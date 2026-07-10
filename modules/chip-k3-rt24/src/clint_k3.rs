//! K3 RT24 SysTimer（CLINT 风格）驱动 —— mtime / mtimecmp / MSIP。
//!
//! RT24 的"CLINT"实为一个 SysTimer 块 `0xe4000000`，mtime/mtimecmp/MSIP 三个
//! 寄存器都在里面，但采用**非标准布局**：per-hart 窗口步长 = `hart << 27`
//!（标准 SiFive CLINT 是 mtimecmp `hart*8` / msip `hart*4`）。
//!
//! 寄存器地址（rcpu1 = hart 1，win = base + (1<<27) = `0xec000000`）：
//! - **MSIP**    `win + 0x0`      = `0xec000000`（**上板实测**：写1→MSI，mip=0x8）
//! - **mtimecmp** `win + 0x4000`  = `0xec004000`（esOS clint.h）
//! - **mtime**   `base + 0xbff8`  = `0xe400bff8`（全局，实测读到递增值；不用 hart 窗口）
//!
//! 频率 24 MHz（esOS `SOC_TIMER_FREQ`）。
//!
//! 两个零大小单例分别 impl `Timer` / `Ipi`，共享 `BASE`/`WIN` 全局原子。
//! hart id 取自 FDT `boot_cpuid_phys`（dtc 从 `/cpus` 推导），与 QEMU 侧 driver
//! 同机制，故同一份代码既能跑 rcpu0（boot_cpuid=0）也能跑 rcpu1（=1）。

use core::sync::atomic::{AtomicUsize, Ordering};

use fdt_parser::Node;
use platform::device::{Driver, Ipi, Timer};
use tock_registers::interfaces::{Readable, Writeable};
use tock_registers::registers::{ReadOnly, ReadWrite};
use tock_registers::register_structs;

/// SysTimer 基址。
const SYSTIMER_BASE: usize = 0xe400_0000;
/// per-hart 窗口步长 = `hart << 27`（RT24 专属，非标准 CLINT）。
const HART_SHIFT: u32 = 27;

/// 窗口内偏移。
const OFF_MSIP: usize = 0x0;
const OFF_MTIMECMP: usize = 0x4000;
/// mtime 全局共享，用 base+0xbff8（与标准 CLINT 一致），不带 hart 窗口。
const OFF_MTIME: usize = 0xBFF8;

/// 时钟频率（Hz）。
const FREQ_HZ: u32 = 24_000_000;

register_structs! {
    /// mtimecmp 寄存器，拆成 hi/lo 两个 32 位字段。
    ///
    /// RISC-V 真板写 mtimecmp 应先写高 32 位再写低 32 位，避免中间出现
    /// 很小的临时值伪触发定时器中断。
    pub MtimecmpRegs {
        (0x00 => lo: ReadWrite<u32>),
        (0x04 => hi: ReadWrite<u32>),
        (0x08 => @END),
    }
}

register_structs! {
    /// mtime 寄存器（64 位全局单调计数器）。
    pub MtimeRegs {
        (0x00 => count: ReadOnly<u64>),
        (0x08 => @END),
    }
}

register_structs! {
    /// MSIP 寄存器（单 u32：写 1 触发 MSI，写 0 清除）。
    pub MsipRegs {
        (0x00 => msip: ReadWrite<u32>),
        (0x04 => @END),
    }
}

/// probe 写入的 per-hart 窗口基址 = `base + (hart << 27)`。
/// 0 表示尚未 probe。
static WIN: AtomicUsize = AtomicUsize::new(0);

// ── Timer 单例 ─────────────────────────────────────────────────────

/// SysTimer Timer 单例（零大小）。
pub struct K3SysTimer;

/// 全局单例，供 probe 注册进 registry。
pub static TIMER: K3SysTimer = K3SysTimer;

impl Timer for K3SysTimer {
    fn freq_hz(&self) -> u32 {
        FREQ_HZ
    }

    fn now(&self) -> u64 {
        // 全局 mtime（base+0xbff8），所有 hart 共享同一递增计数。
        let addr = SYSTIMER_BASE + OFF_MTIME;
        // SAFETY: 固定地址 0xe400bff8，amp.toml 校验，指向有效 MMIO 区，
        // 单 hart 串行访问（tock-registers 内部用 volatile）。
        let regs: &MtimeRegs = unsafe { &*(addr as *const MtimeRegs) };
        regs.count.get()
    }

    fn set_deadline(&self, tick: u64) {
        let addr = WIN.load(Ordering::Acquire) + OFF_MTIMECMP;
        // SAFETY: addr = WIN + 0x4000，WIN 来自 probe 写入的 DT reg + hart 偏移，
        // 指向有效 MMIO 区，单 hart 串行访问。
        let regs: &MtimecmpRegs = unsafe { &*(addr as *const MtimecmpRegs) };
        // 先写高 32 位再写低 32 位，避免伪瞬态触发。
        regs.hi.set((tick >> 32) as u32);
        regs.lo.set(tick as u32);
    }
}

impl Driver for K3SysTimer {
    fn compatible(&self) -> &'static [&'static str] {
        &["spacemit,k3-systimer", "riscv,clint0"]
    }

    fn probe(&self, node: &Node<'_>) {
        let reg = node
            .reg()
            .expect("k3 systimer: missing reg property")
            .next()
            .expect("k3 systimer: empty reg");
        let base = reg.address as usize;
        debug_assert_eq!(
            base, SYSTIMER_BASE,
            "k3 systimer: DT base {:#x} != expected {:#x}",
            base, SYSTIMER_BASE
        );

        // hart id 取自 FDT boot_cpuid_phys（dtc 推导自 /cpus，rcpu1=1）。
        let hart = node.fdt().boot_cpuid_phys() as usize;
        let win = base + (hart << HART_SHIFT);
        WIN.store(win, Ordering::Release);

        platform::driver::TIMER.set(&TIMER);

        log::info!(
            "K3 SysTimer probed: base={:#x}, hart={}, win={:#x}",
            base,
            hart,
            win
        );
    }
}

// ── Ipi 单例 ───────────────────────────────────────────────────────

/// SysTimer MSIP 单例（零大小）。
pub struct K3MsIP;

/// 全局单例，供 probe 注册进 registry。
pub static MSIP: K3MsIP = K3MsIP;

impl Ipi for K3MsIP {
    unsafe fn send(&self) {
        // 写本 hart 的 MSIP=1 触发 MachineSoft。地址 WIN+0x0 已上板实测确认。
        let win = WIN.load(Ordering::Acquire);
        if win == 0 {
            return; // WIN 未 probe，静默跳过
        }
        // SAFETY: addr = WIN + 0x0，WIN 来自 probe 写入的 DT reg + hart 偏移，
        // 指向有效 MMIO 区。Ipi::send 的 unsafe 由调用者保证上下文。
        let regs: &MsipRegs = unsafe { &*((win + OFF_MSIP) as *const MsipRegs) };
        regs.msip.set(1);
    }

    unsafe fn clear(&self) {
        let win = WIN.load(Ordering::Acquire);
        if win == 0 {
            return;
        }
        // SAFETY: 同上。
        let regs: &MsipRegs = unsafe { &*((win + OFF_MSIP) as *const MsipRegs) };
        regs.msip.set(0);
    }
}

impl Driver for K3MsIP {
    fn compatible(&self) -> &'static [&'static str] {
        &["spacemit,k3-systimer-msip", "riscv,clint0-msip"]
    }

    fn probe(&self, node: &Node<'_>) {
        let reg = node
            .reg()
            .expect("k3 msip: missing reg property")
            .next()
            .expect("k3 msip: empty reg");
        let base = reg.address as usize;
        debug_assert_eq!(
            base, SYSTIMER_BASE,
            "k3 msip: DT base {:#x} != expected {:#x}",
            base, SYSTIMER_BASE
        );

        // 与 Timer probe 同算 WIN；二者共享同一全局原子（同 base、同 hart）。
        let hart = node.fdt().boot_cpuid_phys() as usize;
        let win = base + (hart << HART_SHIFT);
        WIN.store(win, Ordering::Release);

        platform::driver::IPI.set(&MSIP);

        log::info!("K3 MSIP probed: win={:#x} (msip @ {:#x})", win, win + OFF_MSIP);
    }
}
