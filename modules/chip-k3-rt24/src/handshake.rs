//! K3 RT24 rcpu1 AMP 启动握手。
//!
//! `k3_rproc_start()` 唤醒 rcpu1 后死等 CORE0_BOOT_ENTRY_LO 非 0（~6s）。
//! rcpu1 必须回写 *CORE0* 寄存器（交叉规则：rcpu0 写 CORE1，rcpu1 写 CORE0）。
//! 必须最先做（`Board::init()` 第一步），否则 AP 卡 6s 超时。
//! 见 uboot `drivers/remoteproc/k3-rproc.c` `k3_rproc_start()` case 1。

use tock_registers::interfaces::Writeable;
use tock_registers::registers::WriteOnly;
use tock_registers::register_structs;

/// CORE0 启动入口寄存器（rcpu1 写 CORE0，交叉规则）。
const RCPU_CORE0_BOOT_ENTRY_LO: usize = 0xc088_007c;

register_structs! {
    /// CORE0 启动入口寄存器（单 u32，写 1 通知 AP rcpu1 已就绪）。
    pub BootEntryReg {
        (0x00 => entry_lo: WriteOnly<u32>),
        (0x04 => @END),
    }
}

/// SPL 启动握手回写：通知 AP "rcpu1 已就绪"，解锁 AP 的 6s 轮询。
///
/// `Board::init()` 第一步调用，在 DTB 注入和 driver probe 之前。
pub fn spl_handshake() {
    // SAFETY: 对有效、对齐的 MMIO 地址（amp.toml 校验过的硬编码常量）做单次
    // u32 store。启动最早期单 hart 执行，无别名（该寄存器不被 Rust owning）
    // 也无并发。写此寄存器有明确副作用——解锁 AP 侧 k3_rproc_start() 的 6s
    // 轮询，这正是其目的。
    let regs: &BootEntryReg = unsafe { &*(RCPU_CORE0_BOOT_ENTRY_LO as *const BootEntryReg) };
    regs.entry_lo.set(1u32);
}
