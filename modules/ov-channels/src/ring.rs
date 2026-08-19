//! 环形缓冲区

use core::sync::atomic::Ordering;
use core::sync::atomic::AtomicUsize;

use crate::message::Message;

/// 环形缓冲区
#[repr(C, align(256))]
pub struct RingBuffer<const N: usize> {
    pub(crate) read: AtomicUsize,
    pub(crate) write: AtomicUsize,
    buffer: [Message; N],
}

impl<const N: usize> RingBuffer<N> {
    /// 创建新环形缓冲区
    #[inline]
    pub const fn new() -> Self {
        Self {
            read: AtomicUsize::new(0),
            write: AtomicUsize::new(0),
            buffer: [Message::empty(); N],
        }
    }

    /// 尝试发送
    #[inline]
    pub fn try_send(&self, msg: &Message) -> bool {
        let write = self.write.load(Ordering::Acquire);
        let read = self.read.load(Ordering::Acquire);
        let next = (write + 1) % N;

        if next == read {
            return false;
        }

        // 使用 volatile 写入，因为我们通过 &self 修改共享内存
        unsafe {
            (self.buffer.as_ptr().add(write) as *mut Message).write_volatile(*msg);
        }
        self.write.store(next, Ordering::Release);

        true
    }

    /// 尝试接收（移除队首）
    #[inline]
    pub fn try_recv(&self) -> Option<Message> {
        let read = self.read.load(Ordering::Acquire);
        let write = self.write.load(Ordering::Acquire);

        if read == write {
            return None;
        }

        let msg = unsafe { self.buffer.as_ptr().add(read).read_volatile() };
        self.read.store((read + 1) % N, Ordering::Release);
        Some(msg)
    }

    /// 预读队首消息（不移除）
    #[inline]
    pub fn peek(&self) -> Option<Message> {
        let read = self.read.load(Ordering::Acquire);
        let write = self.write.load(Ordering::Acquire);

        if read == write {
            return None;
        }

        Some(unsafe { self.buffer.as_ptr().add(read).read_volatile() })
    }

    /// 检查是否有待读取消息
    #[inline]
    pub fn has_pending(&self) -> bool {
        self.read.load(Ordering::Acquire) != self.write.load(Ordering::Acquire)
    }

    /// 检查是否为空
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.read.load(Ordering::Acquire) == self.write.load(Ordering::Acquire)
    }

    /// 获取消息数量
    #[inline]
    pub fn len(&self) -> usize {
        let r = self.read.load(Ordering::Acquire);
        let w = self.write.load(Ordering::Acquire);
        w.wrapping_sub(r) % N
    }

    /// 清空
    #[inline]
    pub fn clear(&self) {
        self.read.store(self.write.load(Ordering::Acquire), Ordering::Release);
    }
}
