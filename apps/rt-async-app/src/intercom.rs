//! 双核 AMP 通信模块
//!
//! 共享内存布局:
//! - Channel 0: StarryOS -> rt-async (请求/通知)
//! - Channel 1: rt-async -> StarryOS (响应/通知)
//!
//! 约定地址来自 amp.config (通过 chip-qemu-virt-rt 重导出).

use ov_channal::{ChannelId, Message, MsgType, SharedMemory};

/// 初始化共享内存（由 rt-async 启动时调用一次）
pub fn init() {
    unsafe {
        let shm = SharedMemory::at(chip_qemu_virt_rt::SHMBASE);
        shm.init();
    }
    log::info!("[InterCom] initialized at {:#x}", chip_qemu_virt_rt::SHMBASE);
}

/// 检查 StarryOS 是否发来消息
pub fn has_pending() -> bool {
    unsafe {
        let shm = SharedMemory::at(chip_qemu_virt_rt::SHMBASE);
        shm.receiver(ChannelId::new(0))
            .is_ok_and(|rx| rx.has_pending())
    }
}

/// 接收并处理所有待处理消息
pub fn process_pending() {
    unsafe {
        let shm = SharedMemory::at(chip_qemu_virt_rt::SHMBASE);
        if let Ok(rx) = shm.receiver(ChannelId::new(0)) {
            for msg in rx.iter() {
                handle_message(msg);
            }
        }
    }
}

fn handle_message(msg: Message) {
    match msg.ty() {
        Some(MsgType::Notification) => {
            if let Some(id) = msg.as_notification() {
                log::info!("[InterCom] notification: {}", id);
                send_notification(id);
            }
        }
        Some(MsgType::Request) => {
            if let Some(method_id) = msg.method_id() {
                log::info!("[InterCom] request: method={}", method_id);
                handle_request(method_id, msg);
            }
        }
        Some(MsgType::Data) => {
            if let Some(data) = msg.as_data() {
                log::info!("[InterCom] data: {} bytes", data.len());
            }
        }
        _ => {
            log::warn!("[InterCom] unknown message type");
        }
    }
}

fn handle_request(method_id: u64, msg: Message) {
    match method_id {
        0 => {
            // ECHO: 返回相同的 request_id
            if let Some((rid, _, ())) = msg.as_request::<()>() {
                let resp = Message::response(rid, &0u32).unwrap();
                send_message(resp);
            }
        }
        1 => {
            // ADD: 两个 i32 相加
            if let Some((rid, _, (a, b))) = msg.as_request::<(i32, i32)>() {
                let result = a + b;
                let resp = Message::response(rid, &result).unwrap();
                send_message(resp);
            }
        }
        _ => {
            log::warn!("[InterCom] unknown method: {}", method_id);
        }
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
