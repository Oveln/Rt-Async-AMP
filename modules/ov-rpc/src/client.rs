//! RPC 客户端

use core::sync::atomic::Ordering;

use ov_channels::{
    ChannelId, Message, MessageBuf, MsgType, PAYLOAD_SIZE, RecvOutcome, SharedMemory, stream,
};

use crate::{RequestId, RESPONSE_DATA_MAX, NOTIFY_FLAG, ONE_WAY_FLAG};

/// Errors that can occur when receiving an RPC response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    /// A response was present in the buffer but failed to deserialize.
    ///
    /// The message has been consumed (removed from the buffer); the caller
    /// cannot retry with a different type. 服务端 poison 响应（方法未注册/
    /// 参数反序列化失败/响应超长）同样落到这里——按 rid 关联的调用方
    /// 据此把"等响应挂死"变成快失败。
    DeserializeFailed,
}

// portable-atomic：K3 专属 target（atomic-cas:false）下 core RMW 被 cfg
// 掉，fetch_add 经 CS 回退；标准 target（AP 用户态等）别名 core 原生。
static NEXT_REQUEST_ID: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(1);

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

/// 缓冲的一条响应：rid + 已剥除 rid 的 postcard 结果字节。
///
/// 尺寸上限与服务端 [`Response`](crate::Response) 同预算（大响应经
/// ov-channels 0.3 块层多块到达，poll 时归一化到本结构）。
#[derive(Clone, Copy)]
struct RespEntry {
    rid: RequestId,
    len: usize,
    data: [u8; RESPONSE_DATA_MAX],
}

impl RespEntry {
    const EMPTY: Self = Self {
        rid: 0,
        len: 0,
        data: [0; RESPONSE_DATA_MAX],
    };

    /// 按目标类型解码（postcard 容忍尾随零填充）。
    fn decode<T: serde::de::DeserializeOwned>(&self) -> Option<T> {
        postcard::from_bytes(&self.data[..self.len]).ok()
    }
}

/// RPC 客户端。
///
/// 支持四种调用模式：`call` / `call_poll` / `send` / `urgent`。
///
/// 响应通道经 [`Receiver::try_recv_into`](ov_channels::Receiver::try_recv_into)
/// 接收：单块（≤247B 结果，与 0.2.x 线格式相同）与多块流帧（大响应，
/// ≤2028B）都归一进内部缓冲。
pub struct RpcClient {
    shm_addr: usize,
    req_ch: ChannelId,
    resp_ch: ChannelId,
    urgent_ch: ChannelId,
    buf_len: usize,
    buf: [RespEntry; BUF_CAP],
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
            buf: [RespEntry::EMPTY; BUF_CAP],
        }
    }

    #[inline]
    fn shm(&self) -> &'static SharedMemory<3> {
        unsafe { SharedMemory::<3>::at(self.shm_addr) }
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
        // （原 user-cbo 按行发布/刷新已随 PMA 非缓存窗口撤除——写直达 SRAM，
        // 读恒新鲜，无需任何缓存维护。）
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
        // Full fence: guarantee the request write is ordered before reading BUSY.
        // This prevents a lost-wakeup race between client write and server sleep.
        // （BUSY 读经非缓存窗口恒新鲜，无陈旧行风险。）
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
    /// 经 [`Receiver::try_recv_into`](ov_channels::Receiver::try_recv_into)
    /// 接收：单块响应（裸格式 `rid ++ postcard(T)`）与多块响应（字节流帧
    /// `[len][rid ++ postcard(T)]`，服务端 >247B 结果）都归一化。非
    /// Request/Response kind 的消息（如 notification）照旧取走并丢弃。
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

        // 重组 scratch：多块响应先重组进 MessageBuf（载荷连接视图）再拷出入队
        let mut scratch = MessageBuf::new();
        let mut count = 0;
        while self.buf_len < BUF_CAP {
            let entry = match rx.try_recv_into(&mut scratch) {
                Ok(None) => break,
                // 块链不变量破坏（正确的服务端不可能触发）：终止本轮，
                // 消息留在环里由下一次 poll 重试——不做静默跳过
                Err(_) => break,
                Ok(Some(RecvOutcome::Single(m))) => {
                    // 历史行为：非 Request/Response kind（无 rid）取走丢弃
                    let Some(rid) = m.request_id() else {
                        continue;
                    };
                    let mut e = RespEntry::EMPTY;
                    e.rid = rid;
                    e.len = PAYLOAD_SIZE - 8;
                    e.data[..e.len].copy_from_slice(&m.payload_bytes()[8..]);
                    e
                }
                Ok(Some(RecvOutcome::Multi(mm))) => {
                    // 多块响应：字节流帧，帧内数据 = rid ++ postcard(T)
                    if mm.kind() != MsgType::Response as u8 {
                        continue;
                    }
                    let Some(bytes) = stream::decode(mm.payload()) else {
                        continue;
                    };
                    if bytes.len() < 8 {
                        continue;
                    }
                    let mut e = RespEntry::EMPTY;
                    e.rid = u64::from_le_bytes(bytes[..8].try_into().unwrap());
                    e.len = bytes.len() - 8;
                    e.data[..e.len].copy_from_slice(&bytes[8..]);
                    e
                }
            };
            self.buf[self.buf_len] = entry;
            self.buf_len += 1;
            count += 1;
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
    /// 两种结果都会**消费**该条响应——解码失败的条目（含服务端 poison）
    /// 不留在队首，FIFO 不会因此楔死。
    pub fn recv<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>, RecvError> {
        if self.buf_len == 0 {
            return Ok(None);
        }
        let out = self.buf[0].decode::<T>().ok_or(RecvError::DeserializeFailed);
        self.buf_len -= 1;
        self.buf.copy_within(1..=self.buf_len, 0);
        self.buf[self.buf_len] = RespEntry::EMPTY;
        out.map(Some)
    }

    /// 按 rid 匹配取响应（乱序场景）。
    ///
    /// Returns `Ok(None)` if no matching response is buffered,
    /// `Ok(Some(value))` on successful deserialization, or
    /// `Err(RecvError::DeserializeFailed)` if a matching response was present
    /// but could not be decoded as type `T`. 匹配条目无论成败都**消费**。
    pub fn recv_for<T: serde::de::DeserializeOwned>(
        &mut self,
        request_id: RequestId,
    ) -> Result<Option<T>, RecvError> {
        for i in 0..self.buf_len {
            if self.buf[i].rid == request_id {
                let out = self.buf[i].decode::<T>().ok_or(RecvError::DeserializeFailed);
                self.buf_len -= 1;
                self.buf[i] = self.buf[self.buf_len];
                self.buf[self.buf_len] = RespEntry::EMPTY;
                return out.map(Some);
            }
        }
        Ok(None)
    }

    /// 缓冲区中待处理的响应数量。
    pub fn buffered(&self) -> usize {
        self.buf_len
    }

    /// 发起服务发现（INIT，保留 method 0）。
    ///
    /// 响应载荷是服务描述符原始字节（非 postcard）——用
    /// [`Self::recv_raw_for`] 收取后经 [`descriptor::parse`](crate::descriptor::parse)
    /// 解析。旧固件（无 INIT 拦截）下本调用收到 poison 响应
    /// （`recv_raw_for` 命中后字节解不开描述符）或版本门直接拒绝发送，
    /// 均为快失败。
    pub fn discover<N: FnOnce()>(
        &self,
        notify: N,
    ) -> Result<RequestId, ov_channels::SendError> {
        self.call(crate::METHOD_INIT, &(), notify)
    }

    /// 按 rid 取**原始载荷字节**（rid 之外、不经 postcard 解码，不消费）。
    ///
    /// 服务发现专用：描述符是自有线格式，走 raw 路径而非类型化解码。
    /// 未到达返回 `None`；条目保留在缓冲里直到被 `recv_for`/`recv` 消费
    /// 或覆盖（discover 流程通常即刻解析，无需显式清理）。
    pub fn recv_raw_for(&mut self, request_id: RequestId) -> Option<&[u8]> {
        for i in 0..self.buf_len {
            if self.buf[i].rid == request_id {
                let len = self.buf[i].len;
                return Some(&self.buf[i].data[..len]);
            }
        }
        None
    }
}
