//! RPC 客户端

use core::sync::atomic::{AtomicU64, Ordering};

use ov_channels::{ChannelId, Message, SharedMemory};

use crate::{RequestId, NOTIFY_FLAG, ONE_WAY_FLAG};

/// Errors that can occur when receiving an RPC response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    /// A response was present in the buffer but failed to deserialize.
    ///
    /// The message has been consumed (removed from the buffer); the caller
    /// cannot retry with a different type.
    DeserializeFailed,
}

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Response buffer capacity.
///
/// Each `poll_responses()` call drains up to `BUF_CAP` messages from the
/// shared-memory channel into an on-stack array. Any responses that don't
/// fit remain in the channel's ring buffer and will be picked up on the
/// next `poll_responses()` call — nothing is lost.
///
/// Callers that expect more than `BUF_CAP` in-flight RPCs between two
/// consecutive polls should increase this value, or simply poll more
/// frequently. In a no\_std / real-time context a bounded buffer is
/// intentional: unbounded buffering would risk uncontrolled stack growth.
const BUF_CAP: usize = 8;

/// 通道布局约定。
pub mod channel {
    use ov_channels::ChannelId;
    pub const REQ: ChannelId = ChannelId::new(0);
    pub const RESP: ChannelId = ChannelId::new(1);
    pub const URGENT: ChannelId = ChannelId::new(2);
}

/// RPC 客户端。
///
/// 支持四种调用模式：`call` / `call_poll` / `send` / `urgent`。
pub struct RpcClient {
    shm_addr: usize,
    req_ch: ChannelId,
    resp_ch: ChannelId,
    urgent_ch: ChannelId,
    buf_len: usize,
    buf: [(RequestId, Message); BUF_CAP],
}

impl RpcClient {
    /// 创建 RPC 客户端，使用默认通道布局 (CH0/CH1/CH2)。
    pub const fn new(shm_addr: usize) -> Self {
        Self::with_channels(shm_addr, channel::REQ, channel::RESP, channel::URGENT)
    }

    /// 创建 RPC 客户端，自定义通道。
    pub const fn with_channels(
        shm_addr: usize,
        req_ch: ChannelId,
        resp_ch: ChannelId,
        urgent_ch: ChannelId,
    ) -> Self {
        Self {
            shm_addr,
            req_ch,
            resp_ch,
            urgent_ch,
            buf_len: 0,
            buf: [(0, Message::empty()); BUF_CAP],
        }
    }

    #[inline]
    fn shm(&self) -> &'static SharedMemory {
        unsafe { SharedMemory::at(self.shm_addr) }
    }

    fn send_request(
        &self,
        method_id: u64,
        args: &impl serde::Serialize,
        ch: ChannelId,
    ) -> Result<RequestId, ov_channels::SendError> {
        let rid = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let msg = Message::request(rid, method_id, args)
            .map_err(|_| ov_channels::SendError::Invalid)?;

        let shm = self.shm();
        let tx = shm.sender(ch).map_err(|_| ov_channels::SendError::Invalid)?;
        tx.try_send(&msg)?;

        Ok(rid)
    }

    /// 写入请求后检查 BUSY 标志，若服务端不在忙等则自动调用 `notify` 发 IPI。
    fn call_inner<N: FnOnce()>(
        &self,
        method_id: u64,
        args: &impl serde::Serialize,
        notify: N,
    ) -> Result<RequestId, ov_channels::SendError> {
        let rid = self.send_request(method_id, args, self.req_ch)?;
        // Full fence: guarantee the request write is visible before reading BUSY.
        // This prevents a lost-wakeup race between client write and server sleep.
        core::sync::atomic::fence(Ordering::SeqCst);
        if !self.shm().is_busy() {
            notify();
        }
        Ok(rid)
    }

    /// 请求-响应：写入请求后检查 BUSY 标志，**服务端回 IPI**。
    ///
    /// 若 BUSY=0（服务端可能在睡眠），自动调用 `notify` 发送 IPI 唤醒服务端。
    /// 调用者在收到 IPI back 后调用 `poll_responses()` 读取响应。
    pub fn call<Args: serde::Serialize, N: FnOnce()>(
        &self,
        method_id: u64,
        args: &Args,
        notify: N,
    ) -> Result<RequestId, ov_channels::SendError> {
        self.call_inner(method_id | NOTIFY_FLAG, args, notify)
    }

    /// 请求-响应：写入请求后检查 BUSY 标志，**服务端不回 IPI**。
    ///
    /// 若 BUSY=0（服务端可能在睡眠），自动调用 `notify` 发送 IPI 唤醒服务端。
    /// 调用者需要自行 busy-poll (`poll_responses()`) 读取响应。
    pub fn call_poll<Args: serde::Serialize, N: FnOnce()>(
        &self,
        method_id: u64,
        args: &Args,
        notify: N,
    ) -> Result<RequestId, ov_channels::SendError> {
        self.call_inner(method_id, args, notify)
    }

    /// 单向调用：不期待响应，走普通请求通道。
    ///
    /// 若 BUSY=0（服务端可能在睡眠），自动调用 `notify` 发送 IPI 唤醒服务端。
    pub fn send<Args: serde::Serialize, N: FnOnce()>(
        &self,
        method_id: u64,
        args: &Args,
        notify: N,
    ) -> Result<(), ov_channels::SendError> {
        self.send_request(method_id | ONE_WAY_FLAG, args, self.req_ch)?;
        core::sync::atomic::fence(Ordering::SeqCst);
        if !self.shm().is_busy() {
            notify();
        }
        Ok(())
    }

    /// 急停：走高优先级通道 (CH2)，不期待响应。
    ///
    /// 若 BUSY=0（服务端可能在睡眠），自动调用 `notify` 发送 IPI 唤醒服务端。
    pub fn urgent<Args: serde::Serialize, N: FnOnce()>(
        &self,
        method_id: u64,
        args: &Args,
        notify: N,
    ) -> Result<(), ov_channels::SendError> {
        self.send_request(method_id | ONE_WAY_FLAG, args, self.urgent_ch)?;
        core::sync::atomic::fence(Ordering::SeqCst);
        if !self.shm().is_busy() {
            notify();
        }
        Ok(())
    }

    /// Drain up to `BUF_CAP` response messages from `resp_ch` into the
    /// internal buffer and return the number drained.
    ///
    /// If more than `BUF_CAP` responses are pending, only the first
    /// `BUF_CAP` are buffered; the rest stay in the channel and will be
    /// available on the next call. No responses are lost — this is
    /// batching, not dropping.
    ///
    /// When to call: on IPI receipt or inside a busy-poll loop. For
    /// workloads with many concurrent RPCs, poll frequently enough that
    /// the buffer (and the channel behind it) do not fill up and exert
    /// back-pressure on the sender.
    pub fn poll_responses(&mut self) -> usize {
        let shm = self.shm();
        let Ok(rx) = shm.receiver(self.resp_ch) else {
            return 0;
        };

        let mut count = 0;
        while self.buf_len < BUF_CAP {
            let Some(msg) = rx.try_recv() else { break };
            if let Some(rid) = msg.request_id() {
                self.buf[self.buf_len] = (rid, msg);
                self.buf_len += 1;
                count += 1;
            }
        }
        count
    }

    /// FIFO 按序取下一条响应（不按 rid 匹配）。
    ///
    /// 前提：响应按请求顺序到达。
    ///
    /// Returns `Ok(None)` if the buffer is empty, `Ok(Some(value))` on
    /// successful deserialization, or `Err(RecvError::DeserializeFailed)` if
    /// a response was present but could not be decoded as type `T`.
    pub fn recv<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>, RecvError> {
        if self.buf_len == 0 {
            return Ok(None);
        }
        let msg = self.buf[0].1;
        // Parse BEFORE dequeuing so the message is still available on error.
        let (_request_id, result) = msg
            .as_response::<T>()
            .ok_or(RecvError::DeserializeFailed)?;
        self.buf_len -= 1;
        self.buf.copy_within(1..=self.buf_len, 0);
        self.buf[self.buf_len] = (0, Message::empty());
        Ok(Some(result))
    }

    /// 按 rid 匹配取响应（乱序场景）。
    ///
    /// Returns `Ok(None)` if no matching response is buffered,
    /// `Ok(Some(value))` on successful deserialization, or
    /// `Err(RecvError::DeserializeFailed)` if a matching response was present
    /// but could not be decoded as type `T`.
    pub fn recv_for<T: serde::de::DeserializeOwned>(
        &mut self,
        request_id: RequestId,
    ) -> Result<Option<T>, RecvError> {
        for i in 0..self.buf_len {
            if self.buf[i].0 == request_id {
                let msg = self.buf[i].1;
                // Parse BEFORE dequeuing so the message is still available on error.
                let (_rid, result) = msg
                    .as_response::<T>()
                    .ok_or(RecvError::DeserializeFailed)?;
                self.buf_len -= 1;
                self.buf[i] = self.buf[self.buf_len];
                self.buf[self.buf_len] = (0, Message::empty());
                return Ok(Some(result));
            }
        }
        Ok(None)
    }

    /// 缓冲区中待处理的响应数量。
    pub fn buffered(&self) -> usize {
        self.buf_len
    }
}
