//! 共享内存和中断辅助函数

use core::sync::atomic::Ordering;

#[cfg(feature = "flags")]
use core::sync::atomic::AtomicBool;

use crate::channel::{Channel, Receiver, Sender};
use crate::message::Message;
use crate::{ChannelId, InvalidChannelId, MAGIC, VERSION};

/// 共享内存区域
///
/// `N` 为通道数量，由调用方在编译时确定。
/// 启用 `flags` feature 后包含 `busy` 标志用于 AMP 弹性忙等协调。
#[repr(C, align(256))]
pub struct SharedMemory<const N: usize = 2> {
    #[cfg(feature = "flags")]
    busy: AtomicBool,
    channels: [Channel; N],
}

impl<const N: usize> SharedMemory<N> {
    /// 创建新的共享内存
    #[inline]
    pub const fn new() -> Self {
        const CHANNEL: Channel = Channel::new();
        Self {
            #[cfg(feature = "flags")]
            busy: AtomicBool::new(false),
            channels: [CHANNEL; N],
        }
    }

    /// 获取指定地址的共享内存
    ///
    /// # Safety
    /// 地址必须指向有效的共享内存区域
    #[inline]
    pub unsafe fn at(addr: usize) -> &'static Self {
        &*(addr as *const Self)
    }

    /// 验证共享内存
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.channels.iter().all(|c| c.is_valid())
    }

    /// 初始化共享内存
    #[inline]
    pub fn init(&self) {
        #[cfg(feature = "flags")]
        self.busy.store(false, Ordering::Release);
        for ch in &self.channels {
            ch.magic.store(MAGIC, Ordering::Release);
            ch.version.store(VERSION, Ordering::Release);
            ch.clear();
        }
    }

    /// 返回通道数量
    #[inline]
    pub const fn channel_count(&self) -> usize {
        N
    }

    // ------------------------------------------------------------------
    // BUSY flag API (requires "flags" feature)
    // ------------------------------------------------------------------

    /// 标记服务端正在忙等待（弹性忙等窗口内）。
    ///
    /// 由 rt-async 在进入弹性忙等之前调用。
    #[cfg(feature = "flags")]
    #[inline]
    pub fn set_busy(&self) {
        self.busy.store(true, Ordering::Release);
    }

    /// 清除忙等待标志（服务端即将进入 WFI）。
    ///
    /// 调用者必须在 `clear_busy()` 之后插入 **`fence(SeqCst)`** 并
    /// 重新检查 `has_pending()`，以防清标志和客户端写请求之间的竞争。
    #[cfg(feature = "flags")]
    #[inline]
    pub fn clear_busy(&self) {
        self.busy.store(false, Ordering::Release);
    }

    /// 检查服务端是否正在忙等待。
    ///
    /// 客户端在写入请求后调用此方法：返回 `true` 表示服务端正在轮询，
    /// 无需发送 IPI；返回 `false` 表示服务端可能正在睡眠，需要 IPI。
    #[cfg(feature = "flags")]
    #[inline]
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    /// 获取发送端
    ///
    /// # Error
    /// 如果通道 ID 无效（>= N），返回 `InvalidChannelId`
    #[inline]
    pub fn sender(&self, id: ChannelId) -> Result<Sender<'_>, InvalidChannelId> {
        if (id.get() as usize) < N {
            Ok(Sender::new(&self.channels[id.get() as usize]))
        } else {
            Err(InvalidChannelId)
        }
    }

    /// 获取接收端
    ///
    /// # Error
    /// 如果通道 ID 无效（>= N），返回 `InvalidChannelId`
    #[inline]
    pub fn receiver(&self, id: ChannelId) -> Result<Receiver<'_>, InvalidChannelId> {
        if (id.get() as usize) < N {
            Ok(Receiver::new(&self.channels[id.get() as usize]))
        } else {
            Err(InvalidChannelId)
        }
    }

    /// 获取通道 (低级 API)
    ///
    /// # Safety
    /// 通道 ID 必须有效（< N）
    #[inline]
    pub unsafe fn channel_unchecked(&self, id: ChannelId) -> &Channel {
        &self.channels[id.get() as usize]
    }
}

// ============================================================================
// 中断辅助函数
// ============================================================================

/// 在中断处理中快速接收消息
///
/// # Safety
/// shared_memory 必须指向有效的共享内存区域，channel_id 必须有效（< N）
#[inline]
pub unsafe fn recv_from_interrupt<const N: usize>(
    shared_memory: usize,
    channel_id: ChannelId,
) -> Option<Message> {
    let shm = SharedMemory::<N>::at(shared_memory);
    shm.channel_unchecked(channel_id).try_recv()
}

/// 检查通道是否有待读取消息
///
/// # Safety
/// shared_memory 必须指向有效的共享内存区域，channel_id 必须有效（< N）
#[inline]
pub unsafe fn has_pending_interrupt<const N: usize>(shared_memory: usize, channel_id: ChannelId) -> bool {
    let shm = SharedMemory::<N>::at(shared_memory);
    shm.channel_unchecked(channel_id).has_pending()
}
