//! K3 RT24 复位/关机 stub。
//!
//! QEMU virt 有 `sifive,test1`（写 `0x5555` 触发 QEMU 退出），K3 RT24 无对应
//! 复位外设。关机语义退化为 `wfi` 死循环（与 PowerManager 配合的低功耗停机
//! 留后续；阶段1仅需 trait 占位）。
//!
//! 不经 DT probe（无对应节点），由 `Board::init` 直接 `RESET.set`。

use platform::device::Reset;

/// no-op Reset 单例（零大小）。
pub struct ResetStub;

/// 全局单例，由 Board::init 直接注册。
pub static INSTANCE: ResetStub = ResetStub;

impl Reset for ResetStub {
    fn shutdown(&self) -> ! {
        loop {
            // wfi 等中断唤醒；无中断则永久停在此处。
            riscv::asm::wfi();
        }
    }
}
