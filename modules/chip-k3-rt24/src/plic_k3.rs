//! K3 RT24 PLIC 驱动（RISC-V 平台中断控制器，**自定义布局**）。
//!
//! RT24 PLIC @ `0xe0000000`，compatible `riscv,plic0`，但寄存器布局与
//! SiFive 标准 PLIC **不同**（见 esOS `libcpu/risc-v/spacemit/rt24/riscv-plic.h`）：
//!
//! - **per-hart 步长 = `hart << 27`**（标准 SiFive 是 context=hart*2，小步长）。
//! - priority / enable / threshold / claim **都带** per-hart 偏移
//!   （SiFive 的 priority 是全局共享的，K3 不是）。
//! - base+0x0 有一个 **feature 寄存器**（标准 PLIC 此处不可写）。
//!
//! 偏移表（ctx_win = base + (hart << 27)）：
//! - priority:  `ctx_win + 0x0      + source*4`
//! - pending:   `ctx_win + 0x1000   + (source>>5)*4`
//! - enable:    `ctx_win + 0x2000   + (source>>5)*4`
//! - threshold: `ctx_win + 0x200000`
//! - claim/complete: `ctx_win + 0x200004`（共用同一寄存器）
//!
//! 阶段1验收不依赖 PLIC（无外部中断），但写好让后续 UART RX 中断 / 外设
//! 驱动零改动接入。

use core::sync::atomic::{AtomicUsize, Ordering};

use fdt_parser::Node;
use platform::device::{Driver, InterruptController};

/// PLIC 基址。
const PLIC_BASE: usize = 0xe000_0000;
/// per-hart 窗口步长 = `hart << 27`（与 SysTimer 同约定）。
const HART_SHIFT: u32 = 27;

// ── 寄存器偏移（esOS riscv-plic.h，与 SiFive 不同）─────────────
const PRIORITY_BASE: usize = 0x0000;
#[allow(dead_code)]
const PENDING_BASE: usize = 0x1000;
const ENABLE_BASE: usize = 0x2000;
const THRESHOLD_OFFSET: usize = 0x200000;
const CLAIM_OFFSET: usize = 0x200004;

#[inline(always)]
fn write32(addr: usize, val: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, val) };
}

#[inline(always)]
fn read32(addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

/// PLIC 驱动单例（零大小）。
pub struct PlicK3;

/// 全局单例，供 probe 注册进 registry。
pub static PLIC: PlicK3 = PlicK3;

/// probe 写入的基址。
static BASE: AtomicUsize = AtomicUsize::new(0);
/// probe 计算的 per-hart 窗口 = base + (hart << 27)。
static WIN: AtomicUsize = AtomicUsize::new(0);

impl InterruptController for PlicK3 {
    fn enable_irq(&self, irq: u32) {
        let win = WIN.load(Ordering::Acquire);
        let addr = win + ENABLE_BASE + (irq as usize >> 5) * 4;
        let bit = 1u32 << (irq & 0x1f);
        let v = read32(addr);
        write32(addr, v | bit);
    }

    fn disable_irq(&self, irq: u32) {
        let win = WIN.load(Ordering::Acquire);
        let addr = win + ENABLE_BASE + (irq as usize >> 5) * 4;
        let bit = 1u32 << (irq & 0x1f);
        let v = read32(addr);
        write32(addr, v & !bit);
    }

    fn set_priority(&self, irq: u32, prio: u32) {
        // K3 priority 带 per-hart 偏移（与 SiFive 的全局 priority 不同！）。
        let win = WIN.load(Ordering::Acquire);
        let addr = win + PRIORITY_BASE + irq as usize * 4;
        write32(addr, prio);
    }

    fn set_threshold(&self, thr: u32) {
        let win = WIN.load(Ordering::Acquire);
        let addr = win + THRESHOLD_OFFSET;
        write32(addr, thr);
    }

    fn claim(&self) -> u32 {
        let win = WIN.load(Ordering::Acquire);
        let addr = win + CLAIM_OFFSET;
        read32(addr)
    }

    fn complete(&self, irq: u32) {
        let win = WIN.load(Ordering::Acquire);
        let addr = win + CLAIM_OFFSET;
        // K3 claim/complete 共用同一寄存器。
        write32(addr, irq);
    }
}

impl Driver for PlicK3 {
    fn compatible(&self) -> &'static [&'static str] {
        &["riscv,plic0"]
    }

    fn probe(&self, node: &Node<'_>) {
        let reg = node
            .reg()
            .expect("k3 plic: missing reg property")
            .next()
            .expect("k3 plic: empty reg");
        let base = reg.address as usize;
        debug_assert_eq!(
            base, PLIC_BASE,
            "k3 plic: DT base {:#x} != expected {:#x}",
            base, PLIC_BASE
        );

        // hart id 取自 FDT boot_cpuid_phys（rcpu1=1）。win = base + hart<<27。
        let hart = node.fdt().boot_cpuid_phys() as usize;
        let win = base + (hart << HART_SHIFT);
        BASE.store(base, Ordering::Release);
        WIN.store(win, Ordering::Release);

        // 门槛清零（不屏蔽任何中断）。
        write32(win + THRESHOLD_OFFSET, 0);

        // 先注册再打日志（log 经 console 输出，console 未注册会 panic）。
        platform::driver::set_intctl(&PLIC);

        log::info!(
            "K3 PLIC probed: base={:#x}, hart={}, win={:#x}",
            base,
            hart,
            win
        );
    }
}
