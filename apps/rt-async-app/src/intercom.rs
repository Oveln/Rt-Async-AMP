//! 双核 AMP 通信模块
//!
//! 基于 ov-rpc 框架，在 ov_channels 共享内存通道之上提供类型安全的 RPC。
//!
//! 共享内存布局:
//! - Channel 0: StarryOS -> rt-async (请求/通知)
//! - Channel 1: rt-async -> StarryOS (响应/通知)
//!
//! 约定地址来自 amp.config (通过 chip-qemu_virt_rt 重导出).

use ov_channels::{ChannelId, Message, MsgType, SharedMemory};
use ov_rpc::{define_service, RpcServer};

// ============================================================================
// RPC 服务定义
// ============================================================================

define_service! {
    /// rt-async 侧的 RPC 服务
    RtAsyncRpc {
        ECHO: 0 => fn echo(val: u32) -> u32;
        ADD: 1 => fn add(a: i32, b: i32) -> i32;
    }
}

impl RtAsyncRpc {
    fn echo(val: u32) -> u32 {
        val
    }
    fn add(a: i32, b: i32) -> i32 {
        a.wrapping_add(b)
    }
}

// ============================================================================
// RPC Server 实例
// ============================================================================

static SERVER: RpcServer = RpcServer::new(
    chip_qemu_virt_rt::SHMBASE,
    ChannelId::new(0),
    ChannelId::new(1),
);

// ============================================================================
// 公共 API
// ============================================================================

/// 初始化共享内存（由 rt-async 启动时调用一次）
pub fn init() {
    unsafe {
        let shm = SharedMemory::at(chip_qemu_virt_rt::SHMBASE);
        shm.init();
    }
    log::info!("[InterCom] initialized at {:#x}", chip_qemu_virt_rt::SHMBASE);
}

/// 检查是否有待处理消息
pub fn has_pending() -> bool {
    SERVER.has_pending()
}

/// 处理所有待处理消息（RPC + 通知），返回是否有工作
pub fn process_pending() -> bool {
    let mut n = 0;
    loop {
        match SERVER.process_one::<RtAsyncRpc>() {
            ov_rpc::ProcessResult::NoMessage => break,
            ov_rpc::ProcessResult::Handled => n += 1,
            ov_rpc::ProcessResult::NotRpc(msg) => handle_non_rpc(msg),
        }
    }
    if n > 0 {
        unsafe { chip_qemu_virt_rt::send_ipi_to_linux() };
    }
    n > 0
}

fn handle_non_rpc(msg: Message) {
    match msg.ty() {
        Some(MsgType::Notification) => {
            if let Some(id) = msg.as_notification() {
                log::info!("[InterCom] notification: {}", id);
                send_notification(id);
            }
        }
        Some(MsgType::Data) => {
            if let Some(data) = msg.as_data() {
                log::info!("[InterCom] data: {} bytes", data.len());
            }
        }
        _ => {}
    }
}

/// 向 StarryOS 发送消息
pub fn send_message(msg: Message) {
    unsafe {
        let shm = SharedMemory::at(chip_qemu_virt_rt::SHMBASE);
        if let Ok(tx) = shm.sender(ChannelId::new(1))
            && tx.try_send(&msg).is_ok()
        {
            chip_qemu_virt_rt::send_ipi_to_linux();
        }
    }
}

/// 向 StarryOS 发送通知
pub fn send_notification(id: u32) {
    let msg = Message::notification(id);
    send_message(msg);
}

/// 获取 RPC Server 引用（供外部使用）
pub fn server() -> &'static RpcServer {
    &SERVER
}
