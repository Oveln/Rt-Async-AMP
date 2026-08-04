//! 跨核通知驱动（compatible `ov,clint-msip-notifier`）。
//!
//! 设备树中通过 notifier 子节点声明通知设备，compatible 区分后端：
//!
//! - `ov,clint-msip-notifier`：QEMU virt，写 CLINT MSIP 寄存器触发对端
//!   （hart0/StarryOS）的 MachineSoft 中断；
//! - K3 真板用硬件 mailbox：`chip-k3-rt24` 实现本模块的
//!   [`PeerNotifier`] trait（包 `MBX4`）并在板级注册，无需改本 crate。
//!
//! DTS 示例（与 AP 侧 StarryOS binding 对称）：
//! ```dts
//! shm@88000000 {
//!     compatible = "ov,rt-async-shm";
//!     reg = <0 0x88000000 0 0x11000>;
//!     notifier@2000000 {
//!         compatible = "ov,clint-msip-notifier";
//!         reg = <0 0x02000000 0 0x4>;   /* 对端 MSIP0 */
//!     };
//! };
//! ```

use core::sync::atomic::{AtomicUsize, Ordering};

use fdt_parser::Node;
use platform::device::Driver;
use platform::Slot;
use tock_registers::interfaces::Writeable;
use tock_registers::registers::ReadWrite;
use tock_registers::register_structs;

/// 向对端核心（AP / StarryOS）发送通知的能力。
///
/// 实现约定：`notify` 返回前须保证共享内存中的数据已写入完成——
/// msip 后端内部用 `fence(Release)` 保证可见性；mailbox 后端同理。
pub trait PeerNotifier: Send + Sync {
    /// 触发对端核心的中断。
    fn notify(&self);
}

/// 已 probe 的通知设备（板级驱动 probe 时注入）。
pub static NOTIFIER: Slot<&'static dyn PeerNotifier> = Slot::new();

/// 取通知设备引用。未注册时返回 `None`（不 panic）。
pub fn try_notifier() -> Option<&'static dyn PeerNotifier> {
    NOTIFIER.get().copied()
}

/// 取通知设备引用。未注册则 panic。
pub fn notifier() -> &'static dyn PeerNotifier {
    try_notifier().expect("ov-shm: no notifier device registered")
}

register_structs! {
    /// CLINT MSIP 寄存器（单 u32：写 1 触发 MSI，写 0 清除）。
    pub MsipReg {
        (0x00 => msip: ReadWrite<u32>),
        (0x04 => @END),
    }
}

/// CLINT MSIP 通知后端（QEMU virt：写对端 MSIP 触发 MachineSoft）。
pub struct ClintMsipNotifier;

/// 全局单例，供板级 driver 列表注册。
pub static CLINT_MSIP: ClintMsipNotifier = ClintMsipNotifier;

/// probe 写入的对端 MSIP 地址。0 表示尚未 probe。
static MSIP_ADDR: AtomicUsize = AtomicUsize::new(0);

impl PeerNotifier for ClintMsipNotifier {
    fn notify(&self) {
        let addr = MSIP_ADDR.load(Ordering::Acquire);
        assert!(addr != 0, "ov-shm: clint-msip notifier not probed");
        // 与旧 send_ipi_to_linux 一致：写 MSIP 前 Release fence，
        // 保证共享内存写入对对端可见。
        core::sync::atomic::fence(Ordering::Release);
        // SAFETY: addr 来自 probe 写入的 DT reg，指向已验证的 MMIO 区域；
        // 单 hart 串行访问，无别名引用（tock-registers 内部用 volatile）。
        let msip: &MsipReg = unsafe { &*(addr as *const MsipReg) };
        msip.msip.set(1);
    }
}

impl Driver for ClintMsipNotifier {
    fn compatible(&self) -> &'static [&'static str] {
        &["ov,clint-msip-notifier"]
    }

    fn probe(&self, node: &Node<'_>) {
        let reg = node
            .reg()
            .expect("clint-msip notifier: missing reg property")
            .next()
            .expect("clint-msip notifier: empty reg");
        MSIP_ADDR.store(reg.address as usize, Ordering::Release);
        NOTIFIER.set(&CLINT_MSIP);
        log::info!("[ov-shm] clint-msip notifier probed at {:#x}", reg.address);
    }
}
