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

use core::sync::atomic::{AtomicUsize, Ordering};

use fdt_parser::Node;
use platform::device::{Driver, InterruptController};
use tock_registers::interfaces::{Readable, Writeable};
use tock_registers::registers::ReadWrite;
use tock_registers::register_structs;

/// PLIC 基址。
const PLIC_BASE: usize = 0xe000_0000;
/// per-hart 窗口步长 = `hart << 27`（与 SysTimer 同约定）。
const HART_SHIFT: u32 = 27;

// ── 寄存器偏移（esOS riscv-plic.h，与 SiFive 不同）─────────────
const PRIORITY_BASE: usize = 0x0000;
const ENABLE_BASE: usize = 0x2000;
const THRESHOLD_OFFSET: usize = 0x200000;

register_structs! {
    /// per-context 寄存器组：threshold（偏移 0）+ claim/complete（偏移 4）。
    /// K3 的 threshold 在 ctx_win+0x200000，claim 在 ctx_win+0x200004，连续 8 字节。
    pub PlicContext {
        (0x00 => threshold: ReadWrite<u32>),
        (0x04 => claim_complete: ReadWrite<u32>),
        (0x08 => @END),
    }
}

register_structs! {
    /// PLIC priority 寄存器（单 u32，散落在 ctx_win + irq*4）。
    pub PlicPriority {
        (0x00 => priority: ReadWrite<u32>),
        (0x04 => @END),
    }
}

register_structs! {
    /// PLIC enable 寄存器（单 u32，散落在 ctx_win + 0x2000 + (irq/32)*4）。
    pub PlicEnable {
        (0x00 => enable: ReadWrite<u32>),
        (0x04 => @END),
    }
}

/// PLIC 驱动单例（零大小）。
pub struct PlicK3;

/// 全局单例，供 probe 注册进 registry。
pub static PLIC: PlicK3 = PlicK3;

/// probe 写入的基址。
static BASE: AtomicUsize = AtomicUsize::new(0);
/// probe 计算的 per-hart 窗口 = base + (hart << 27)。
static WIN: AtomicUsize = AtomicUsize::new(0);

impl PlicK3 {
    /// per-context 寄存器组引用。地址 = win + THRESHOLD_OFFSET。
    fn ctx_regs(&self) -> &'static PlicContext {
        let win = WIN.load(Ordering::Acquire);
        // SAFETY: addr = win + 0x200000，win 来自 probe 写入的 DT reg + hart 偏移，
        // 指向有效 MMIO 区，单 hart 串行访问（tock-registers 内部用 volatile）。
        unsafe { &*((win + THRESHOLD_OFFSET) as *const PlicContext) }
    }

    /// 指定中断源的 priority 寄存器引用。地址 = win + irq*4。
    fn priority_regs(&self, irq: u32) -> &'static PlicPriority {
        let win = WIN.load(Ordering::Acquire);
        let addr = win + PRIORITY_BASE + irq as usize * 4;
        // SAFETY: 同上。
        unsafe { &*(addr as *const PlicPriority) }
    }

    /// 指定中断源的 enable 寄存器引用。地址 = win + 0x2000 + (irq/32)*4。
    fn enable_regs(&self, irq: u32) -> &'static PlicEnable {
        let win = WIN.load(Ordering::Acquire);
        let addr = win + ENABLE_BASE + (irq as usize >> 5) * 4;
        // SAFETY: 同上。
        unsafe { &*(addr as *const PlicEnable) }
    }
}

impl InterruptController for PlicK3 {
    fn enable_irq(&self, irq: u32) {
        let bit = 1u32 << (irq & 0x1f);
        // enable 寄存器无位域定义（与 tgoskits 一致），手动 get+set RMW。
        let r = self.enable_regs(irq);
        r.enable.set(r.enable.get() | bit);
    }

    fn disable_irq(&self, irq: u32) {
        let bit = 1u32 << (irq & 0x1f);
        let r = self.enable_regs(irq);
        r.enable.set(r.enable.get() & !bit);
    }

    fn set_priority(&self, irq: u32, prio: u32) {
        // K3 priority 带 per-hart 偏移（与 SiFive 的全局 priority 不同！）。
        self.priority_regs(irq).priority.set(prio);
    }

    fn set_threshold(&self, thr: u32) {
        self.ctx_regs().threshold.set(thr);
    }

    fn claim(&self) -> u32 {
        self.ctx_regs().claim_complete.get()
    }

    fn complete(&self, irq: u32) {
        self.ctx_regs().claim_complete.set(irq);
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
        self.ctx_regs().threshold.set(0);

        platform::driver::INTC.set(&PLIC);

        log::info!(
            "K3 PLIC probed: base={:#x}, hart={}, win={:#x}",
            base,
            hart,
            win
        );
    }
}
