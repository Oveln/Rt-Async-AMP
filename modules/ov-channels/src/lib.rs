//! # OV Channel - 双系统共享内存通信库
//!
//! **本仓 vendor 自 crates.io ov-channels 0.2.0**，仅一处本地补丁：窗口
//! 原子（ring 索引 / magic / version / BUSY）从 portable_atomic 改回
//! `core::sync::atomic`（ring.rs / shm.rs / channel.rs 三处 use）。原因：
//! 主仓 K3 专属 target（targets/riscv64imac-k3-none-elf.json，
//! atomic-cas:false）下 portable-atomic 会落入 critical-section 回退——
//! 单核 mstatus 屏蔽**不提供跨核 Acquire/Release 语义**，共享窗索引会
//! 丢 fence 导致跨核陈旧读（L1 litmus：免 fence 读 0/200 新鲜）。core 的
//! load/store 在该 target 上保留且带原生 fence。标准 target 上两者
//! 代码本就相同（portable-atomic 别名 core），QEMU/AP 行为零变化。
//! 上游修复后（窗口原子固定为 core）可移除本 patch 段。
//!
//! 本库提供裸机环境下两个系统之间的高效共享内存通信机制。
//!
//! ## 架构
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    共享内存区域 (如 0xc8000000)                │
//! ├─────────────────────────────────────────────────────────────┤
//! │  Channel 0    │  Channel 1    │  Channel 2    │  ...        │
//! │  ┌─────────────────────────────┐  │  ┌─────────────────────────────┐  │
//! │  │   RingBuffer<Message>        │  │  │   RingBuffer<Message>        │  │
//! │  └─────────────────────────────┘  │  └─────────────────────────────┘  │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## 基础使用示例
//!
//! ```
//! use ov_channels::{Message, MsgType};
//!
//! // 创建通知消息
//! let msg = Message::notification(42);
//! assert_eq!(msg.as_notification(), Some(42));
//!
//! // 创建数据消息
//! let data_msg = Message::data(b"hello");
//! assert_eq!(data_msg.ty(), Some(MsgType::Data));
//!
//! // 创建 RPC 请求
//! let req = Message::request(123, 0, &(42i32)).unwrap();
//! let (rid, mid, args): (u64, u64, i32) = req.as_request().unwrap();
//! assert_eq!(rid, 123);
//! assert_eq!(args, 42);
//!
//! // 创建 RPC 响应
//! let resp = Message::response(123, &42i32).unwrap();
//! let (rid, result): (u64, i32) = resp.as_response().unwrap();
//! assert_eq!(rid, 123);
//! assert_eq!(result, 42);
//! ```

#![no_std]
#![warn(missing_docs)]

// ============================================================================
// 模块声明
// ============================================================================

mod channel;
mod message;
mod ring;
mod shm;

// ============================================================================
// 公共导出
// ============================================================================

// 通道相关
pub use crate::channel::{Channel, Receiver, RecvIter, Sender, SendError};

// 消息相关
pub use crate::message::{Message, MsgType, Payload};

// 共享内存和中断辅助
pub use crate::shm::{SharedMemory, has_pending_interrupt, recv_from_interrupt};

// ============================================================================
// 配置常量 (重新导出以便统一访问)
// ============================================================================

/// 魔术值 - 用于验证共享内存有效性
pub const MAGIC: u16 = 0x4F56; // "OV" in hex

/// 版本号
pub const VERSION: u16 = 1;

/// 每个通道的容量 (必须 >= 2)
pub const CHANNEL_CAPACITY: usize = 128;

/// 消息负载大小 (字节数)
///
/// 255 字节，加上 kind(1字节) 正好 256 字节，无 padding 浪费
pub const PAYLOAD_SIZE: usize = 255;

/// 消息总大小 = 1 (kind) + PAYLOAD_SIZE = 256 字节
pub const MESSAGE_ALIGN: usize = 256;

// ============================================================================
// 通道 ID (放在这里因为它使用了 MAX_CHANNELS)
// ============================================================================

/// 通道标识符
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelId {
    id: u8,
}

impl ChannelId {
    /// 创建新的通道 ID
    #[inline]
    pub const fn new(id: u8) -> Self {
        Self { id }
    }

    /// 获取原始 ID 值
    #[inline]
    pub const fn get(self) -> u8 {
        self.id
    }
}

impl TryFrom<u8> for ChannelId {
    type Error = InvalidChannelId;

    #[inline]
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(Self { id: value })
    }
}

impl TryFrom<usize> for ChannelId {
    type Error = InvalidChannelId;

    #[inline]
    fn try_from(value: usize) -> Result<Self, Self::Error> {
        if value <= u8::MAX as usize {
            Ok(Self { id: value as u8 })
        } else {
            Err(InvalidChannelId)
        }
    }
}

/// 无效的通道 ID
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidChannelId;

impl core::fmt::Display for InvalidChannelId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Channel ID out of range for this SharedMemory")
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    //! # 测试说明
    //!
    //! 所有测试共享一个全局 `TEST_SHM`，因此必须串行运行：
    //! ```sh
    //! cargo test -- --test-threads=1
    //! ```

    use super::*;

    // 使用静态内存作为测试数据 (no_std 环境)
    static mut TEST_SHM: SharedMemory<4> = SharedMemory::new();

    fn test_shm() -> &'static mut SharedMemory<4> {
        unsafe {
            TEST_SHM.init();
            &mut TEST_SHM
        }
    }

    // ChannelId tests

    #[test]
    fn test_channel_id_new() {
        let id = ChannelId::new(0u8);
        assert_eq!(id.get(), 0);
    }

    #[test]
    fn test_channel_id_try_from_u8() {
        assert_eq!(ChannelId::try_from(0u8), Ok(ChannelId::new(0u8)));
        assert_eq!(ChannelId::try_from(255u8), Ok(ChannelId::new(255u8)));
    }

    #[test]
    fn test_channel_id_try_from_usize() {
        assert_eq!(ChannelId::try_from(0usize), Ok(ChannelId::new(0u8)));
        assert_eq!(ChannelId::try_from(255usize), Ok(ChannelId::new(255u8)));
        assert_eq!(ChannelId::try_from(256usize), Err(InvalidChannelId));
    }

    // Message tests

    #[test]
    fn test_message_notification() {
        let msg = Message::notification(42);
        assert_eq!(msg.ty(), Some(MsgType::Notification));
        assert_eq!(msg.as_notification(), Some(42));
    }

    #[test]
    fn test_message_data() {
        let msg = Message::data(b"hello");
        assert_eq!(msg.ty(), Some(MsgType::Data));
        assert_eq!(msg.as_data().unwrap()[0..5], b"hello"[..]);
    }

    #[test]
    fn test_message_request() {
        let msg = Message::request(123, 456, &(1i32, 2i32)).unwrap();
        assert_eq!(msg.ty(), Some(MsgType::Request));
        let (rid, mid, (a, b)): (u64, u64, (i32, i32)) = msg.as_request().unwrap();
        assert_eq!(rid, 123);
        assert_eq!(mid, 456);
        assert_eq!(a, 1);
        assert_eq!(b, 2);
    }

    #[test]
    fn test_message_response() {
        let msg = Message::response(789, &99i32).unwrap();
        assert_eq!(msg.ty(), Some(MsgType::Response));
        let (rid, result): (u64, i32) = msg.as_response().unwrap();
        assert_eq!(rid, 789);
        assert_eq!(result, 99);
    }

    #[test]
    fn test_msgtype_try_from() {
        assert_eq!(MsgType::try_from(0u8), Ok(MsgType::Notification));
        assert_eq!(MsgType::try_from(1u8), Ok(MsgType::Data));
        assert_eq!(MsgType::try_from(2u8), Ok(MsgType::Request));
        assert_eq!(MsgType::try_from(3u8), Ok(MsgType::Response));
        assert_eq!(MsgType::try_from(99u8), Err(99u8));
    }

    // RingBuffer tests

    #[test]
    fn test_ring_empty() {
        use crate::ring::RingBuffer;
        let rb: RingBuffer<4> = RingBuffer::new();
        assert!(rb.is_empty());
        assert_eq!(rb.len(), 0);
        assert_eq!(rb.try_recv(), None);
    }

    #[test]
    fn test_ring_send_recv() {
        use crate::ring::RingBuffer;
        let rb: RingBuffer<4> = RingBuffer::new();
        let msg = Message::notification(1);

        assert!(rb.try_send(&msg));
        assert!(!rb.is_empty());
        assert_eq!(rb.try_recv(), Some(msg));
    }

    #[test]
    fn test_ring_full() {
        use crate::ring::RingBuffer;
        let rb: RingBuffer<4> = RingBuffer::new();

        for i in 0u32..3 {
            assert!(rb.try_send(&Message::notification(i)));
        }

        assert!(!rb.try_send(&Message::notification(99)));
    }

    // Channel tests

    #[test]
    fn test_channel_valid() {
        let ch = Channel::new();
        assert!(ch.is_valid());
        assert_eq!(ch.version(), VERSION);
    }

    #[test]
    fn test_channel_send_recv() {
        let ch = Channel::new();
        let msg = Message::notification(42);

        assert!(ch.try_send(&msg).is_ok());
        assert_eq!(ch.try_recv(), Some(msg));
    }

    #[test]
    fn test_channel_full() {
        let ch = Channel::new();

        for i in 0u32..CHANNEL_CAPACITY as u32 - 1 {
            assert!(ch.try_send(&Message::notification(i)).is_ok());
        }

        assert_eq!(ch.try_send(&Message::notification(99)), Err(SendError::Full));
    }

    // SharedMemory tests

    #[test]
    fn test_shm_new() {
        let shm: SharedMemory<4> = SharedMemory::new();
        shm.init();
        assert!(shm.is_valid());
    }

    #[test]
    fn test_shm_sender_receiver() {
        let shm = test_shm();
        let id = ChannelId::new(0u8);

        let tx = shm.sender(id).unwrap();
        let rx = shm.receiver(id).unwrap();

        let msg = Message::notification(42);
        assert!(tx.try_send(&msg).is_ok());
        assert_eq!(rx.try_recv(), Some(msg));
    }

    #[test]
    fn test_shm_invalid_channel_id() {
        let shm = test_shm();
        assert!(shm.sender(ChannelId::new(0u8)).is_ok());
        assert!(shm.receiver(ChannelId::new(1u8)).is_ok());
        assert!(shm.sender(ChannelId::new(4u8)).is_err());
        assert!(shm.receiver(ChannelId::new(4u8)).is_err());
    }

    #[test]
    fn test_shm_multiple_channels() {
        let shm = test_shm();

        for i in 0u8..4 {
            let tx = shm.sender(ChannelId::new(i)).unwrap();
            let msg = Message::notification(i as u32);
            assert!(tx.try_send(&msg).is_ok());

            let rx = shm.receiver(ChannelId::new(i)).unwrap();
            assert_eq!(rx.try_recv(), Some(Message::notification(i as u32)));
        }
    }

    // Iterator tests

    #[test]
    fn test_receiver_iter() {
        let shm = test_shm();
        let rx = shm.receiver(ChannelId::new(0u8)).unwrap();

        for i in 0u32..5 {
            shm.sender(ChannelId::new(0u8)).unwrap().try_send(&Message::notification(i)).unwrap();
        }

        let mut count = 0u32;
        for msg in rx.iter() {
            assert_eq!(msg.as_notification(), Some(count));
            count += 1;
        }
        assert_eq!(count, 5);
    }

    // Integration tests

    #[test]
    fn test_bidirectional_communication() {
        let shm = test_shm();

        // 系统 A 发送到通道 0
        let tx_a = shm.sender(ChannelId::new(0u8)).unwrap();
        tx_a.try_send(&Message::notification(1)).unwrap();

        // 系统 B 从通道 0 接收
        let rx_b = shm.receiver(ChannelId::new(0u8)).unwrap();
        assert_eq!(rx_b.try_recv(), Some(Message::notification(1)));

        // 系统 B 发送到通道 1
        let tx_b = shm.sender(ChannelId::new(1u8)).unwrap();
        let resp_msg = Message::response(1, &42i32).unwrap();
        tx_b.try_send(&resp_msg).unwrap();

        // 系统 A 从通道 1 接收
        let rx_a = shm.receiver(ChannelId::new(1u8)).unwrap();
        let resp = rx_a.try_recv().unwrap();
        let (rid, result): (u64, i32) = resp.as_response().unwrap();
        assert_eq!(rid, 1);
        assert_eq!(result, 42);
    }

    #[test]
    fn test_rpc_flow() {
        let shm = test_shm();

        // 客户端发送请求到通道 0
        let req_tx = shm.sender(ChannelId::new(0u8)).unwrap();
        let msg = Message::request(1001, 0, &(42i32, 99i32)).unwrap();
        req_tx.try_send(&msg).unwrap();

        // 服务端从通道 0 接收请求
        let req_rx = shm.receiver(ChannelId::new(0u8)).unwrap();
        let req = req_rx.try_recv().unwrap();
        let (rid, _, (a, b)): (u64, u64, (i32, i32)) = req.as_request().unwrap();
        assert_eq!(rid, 1001);
        assert_eq!(a, 42);
        assert_eq!(b, 99);

        // 服务端发送响应到通道 1
        let resp_tx = shm.sender(ChannelId::new(1u8)).unwrap();
        let resp_msg = Message::response(1001, &141i32).unwrap();
        resp_tx.try_send(&resp_msg).unwrap();

        // 客户端从通道 1 接收响应
        let resp_rx = shm.receiver(ChannelId::new(1u8)).unwrap();
        let resp = resp_rx.try_recv().unwrap();
        let (rid, result): (u64, i32) = resp.as_response().unwrap();
        assert_eq!(rid, 1001);
        assert_eq!(result, 141);
    }

    #[test]
    fn test_multiple_systems() {
        let shm = test_shm();

        // 系统 0 -> 系统 1 (通道 0)
        shm.sender(ChannelId::new(0u8)).unwrap().try_send(&Message::notification(0)).unwrap();

        // 系统 1 -> 系统 0 (通道 1)
        shm.sender(ChannelId::new(1u8)).unwrap().try_send(&Message::notification(1)).unwrap();

        // 验证所有消息
        assert_eq!(
            shm.receiver(ChannelId::new(0u8)).unwrap().try_recv(),
            Some(Message::notification(0))
        );
        assert_eq!(
            shm.receiver(ChannelId::new(1u8)).unwrap().try_recv(),
            Some(Message::notification(1))
        );
    }

    #[test]
    fn test_mixed_message_types() {
        let shm = test_shm();
        let tx = shm.sender(ChannelId::new(0u8)).unwrap();

        tx.try_send(&Message::notification(1)).unwrap();
        tx.try_send(&Message::data(b"data")).unwrap();
        tx.try_send(&Message::request(999, 0, &123u32).unwrap()).unwrap();
        tx.try_send(&Message::response(888, &456i32).unwrap()).unwrap();

        let rx = shm.receiver(ChannelId::new(0u8)).unwrap();
        let mut iter = rx.iter();

        assert_eq!(iter.next().unwrap().ty(), Some(MsgType::Notification));
        assert_eq!(iter.next().unwrap().ty(), Some(MsgType::Data));
        assert_eq!(iter.next().unwrap().ty(), Some(MsgType::Request));
        assert_eq!(iter.next().unwrap().ty(), Some(MsgType::Response));
        assert_eq!(iter.next(), None);
    }

}
