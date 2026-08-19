//! 通道和端点

use core::sync::atomic::Ordering;
use core::fmt;
use core::sync::atomic::AtomicU16;

use crate::message::Message;
use crate::ring::RingBuffer;
use crate::{CHANNEL_CAPACITY, MAGIC, VERSION};

/// 单向通信通道
#[repr(C, align(256))]
pub struct Channel {
    pub(crate) magic: AtomicU16,
    pub(crate) version: AtomicU16,
    buffer: RingBuffer<CHANNEL_CAPACITY>,
}

impl Channel {
    /// 创建新通道
    #[inline]
    pub const fn new() -> Self {
        Self {
            magic: AtomicU16::new(MAGIC),
            version: AtomicU16::new(VERSION),
            buffer: RingBuffer::new(),
        }
    }

    /// 验证通道
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.magic.load(Ordering::Acquire) == MAGIC
    }

    /// 获取版本号
    #[inline]
    pub fn version(&self) -> u16 {
        self.version.load(Ordering::Acquire)
    }

    /// 尝试发送
    #[inline]
    pub fn try_send(&self, msg: &Message) -> Result<(), SendError> {
        if !self.is_valid() {
            return Err(SendError::Invalid);
        }
        if self.buffer.try_send(msg) {
            Ok(())
        } else {
            Err(SendError::Full)
        }
    }

    /// 尝试接收
    #[inline]
    pub fn try_recv(&self) -> Option<Message> {
        self.is_valid().then(|| self.buffer.try_recv()).flatten()
    }

    /// 预读队首消息（不移除）
    #[inline]
    pub fn peek(&self) -> Option<Message> {
        self.is_valid().then(|| self.buffer.peek()).flatten()
    }

    /// 检查是否有待读取消息
    #[inline]
    pub fn has_pending(&self) -> bool {
        self.is_valid() && self.buffer.has_pending()
    }

    /// 检查是否为空
    #[inline]
    pub fn is_empty(&self) -> bool {
        !self.is_valid() || self.buffer.is_empty()
    }

    /// 获取消息数量
    #[inline]
    pub fn len(&self) -> usize {
        self.is_valid().then(|| self.buffer.len()).unwrap_or(0)
    }

    /// 清空
    #[inline]
    pub fn clear(&self) {
        if self.is_valid() {
            self.buffer.clear();
        }
    }
}

/// 发送错误
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    /// 通道无效
    Invalid,
    /// 缓冲区已满
    Full,
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid => write!(f, "Invalid channel"),
            Self::Full => write!(f, "Buffer full"),
        }
    }
}

// ============================================================================
// 发送端和接收端
// ============================================================================

/// 发送端 - 只能发送
#[derive(Clone, Copy)]
pub struct Sender<'a> {
    channel: &'a Channel,
}

impl<'a> Sender<'a> {
    /// 创建发送端
    #[inline]
    pub(crate) fn new(channel: &'a Channel) -> Self {
        Self { channel }
    }

    /// 发送消息
    #[inline]
    pub fn try_send(&self, msg: &Message) -> Result<(), SendError> {
        self.channel.try_send(msg)
    }

    /// 检查是否有效
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.channel.is_valid()
    }
}

/// 接收端 - 只能接收
#[derive(Clone, Copy)]
pub struct Receiver<'a> {
    channel: &'a Channel,
}

impl<'a> Receiver<'a> {
    /// 创建接收端
    #[inline]
    pub(crate) fn new(channel: &'a Channel) -> Self {
        Self { channel }
    }

    /// 接收消息
    #[inline]
    pub fn try_recv(&self) -> Option<Message> {
        self.channel.try_recv()
    }

    /// 预读队首消息（不移除）
    #[inline]
    pub fn peek(&self) -> Option<Message> {
        self.channel.peek()
    }

    /// 检查是否有待读取消息
    #[inline]
    pub fn has_pending(&self) -> bool {
        self.channel.has_pending()
    }

    /// 检查是否为空
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.channel.is_empty()
    }

    /// 获取消息数量
    #[inline]
    pub fn len(&self) -> usize {
        self.channel.len()
    }

    /// 清空
    #[inline]
    pub fn clear(&self) {
        self.channel.clear();
    }

    /// 检查是否有效
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.channel.is_valid()
    }
}

/// 接收迭代器
pub struct RecvIter<'a> {
    receiver: Receiver<'a>,
}

impl<'a> RecvIter<'a> {
    /// 创建迭代器
    #[inline]
    pub fn new(receiver: Receiver<'a>) -> Self {
        Self { receiver }
    }
}

impl<'a> Iterator for RecvIter<'a> {
    type Item = Message;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.receiver.try_recv()
    }
}

impl<'a> Receiver<'a> {
    /// 创建接收迭代器
    #[inline]
    pub fn iter(&self) -> RecvIter<'a> {
        RecvIter::new(*self)
    }
}
