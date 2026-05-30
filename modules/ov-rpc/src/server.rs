//! RPC 服务端

use ov_channels::{ChannelId, Message, SharedMemory};

use crate::{MethodId, strip_flags, is_one_way, wants_notify};

/// 通道布局约定。
pub mod channel {
    use ov_channels::ChannelId;
    pub const REQ: ChannelId = ChannelId::new(0);
    pub const RESP: ChannelId = ChannelId::new(1);
    pub const URGENT: ChannelId = ChannelId::new(2);
}

/// 反序列化失败时返回的错误指示符。
///
/// 由 [`define_service!`](crate::define_service) 宏在 payload 无法解码时产生。
/// 服务端据此发送错误响应，防止客户端在两方调用上永久阻塞。
pub struct DeserializeFailed;

/// RPC 请求处理 trait。
///
/// 推荐使用 [`define_service!`](crate::define_service) 宏自动生成实现。
pub trait RpcHandler {
    /// 处理一个 RPC 请求。
    ///
    /// `method` 已去除协议 flag，是实际的 method ID。
    /// - 返回 `Ok(Some(response))` — 序列化结果，写回响应通道
    /// - 返回 `Ok(None)` — 方法未知或单向调用已完成（无响应）
    /// - 返回 `Err(DeserializeFailed)` — method 已匹配但 payload 反序列化失败
    fn handle(method: MethodId, msg: Message) -> Result<Option<Message>, DeserializeFailed>;
}

/// 处理结果的附带信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandledKind {
    /// interrupt 模式，需要回 IPI
    Notify,
    /// busy-poll 模式，不回 IPI
    Quiet,
    /// 单向调用，无响应发送
    OneWay,
}

/// [`RpcServer::process_one`] / [`RpcServer::process_urgent`] 的返回结果。
#[derive(Debug)]
pub enum ProcessResult {
    /// Channel 中无待处理消息
    NoMessage,
    /// RPC 请求已处理
    Handled(HandledKind),
    /// RPC 请求已知但未被处理（方法未知或 handler 返回 None）
    Unhandled(MethodId),
    /// 非 RPC 消息，交由调用者处理
    NotRpc(Message),
}

/// RPC 服务端。
///
/// ```text
/// CH0 (req_ch):    Client ──▶ 本端  (普通请求)
/// CH1 (resp_ch):   本端  ──▶ Client (响应)
/// CH2 (urgent_ch): Client ──▶ 本端  (急停)
/// ```
pub struct RpcServer {
    shm_addr: usize,
    req_ch: ChannelId,
    resp_ch: ChannelId,
    urgent_ch: ChannelId,
}

impl RpcServer {
    /// 使用默认通道布局创建。
    pub const fn new(shm_addr: usize) -> Self {
        Self::with_channels(shm_addr, channel::REQ, channel::RESP, channel::URGENT)
    }

    /// 自定义通道创建。
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
        }
    }

    #[inline]
    fn shm(&self) -> &'static SharedMemory {
        unsafe { SharedMemory::at(self.shm_addr) }
    }

    fn process_channel<H: RpcHandler>(&self, ch: ChannelId) -> ProcessResult {
        let shm = self.shm();
        let Ok(rx) = shm.receiver(ch) else {
            return ProcessResult::NoMessage;
        };

        let Some(msg) = rx.try_recv() else {
            return ProcessResult::NoMessage;
        };

        let Some(raw_method) = msg.method_id() else {
            return ProcessResult::NotRpc(msg);
        };

        let one_way = is_one_way(raw_method);
        let notify = wants_notify(raw_method);
        let method = strip_flags(raw_method);

        let resp = match H::handle(method, msg) {
            Ok(Some(resp)) => resp,
            Ok(None) => {
                // Method matched but was one-way (send/urgent), or method ID unknown.
                return ProcessResult::Unhandled(method);
            }
            Err(_) => {
                // Method matched but payload deserialization failed.
                #[cfg(feature = "logging")]
                log::warn!("[RpcServer] deserialization failed for method {}", method);
                if !one_way {
                    // Send an error response so the client doesn't hang forever.
                    if let Ok(tx) = shm.sender(self.resp_ch) {
                        if tx.try_send(&Message::notification(0)).is_err() {
                            #[cfg(feature = "logging")]
                            log::warn!("[RpcServer] failed to send error response for method {}", method);
                        }
                    }
                }
                return ProcessResult::Unhandled(method);
            }
        };

        if !one_way {
            if let Ok(tx) = shm.sender(self.resp_ch) {
                if tx.try_send(&resp).is_err() {
                    #[cfg(feature = "logging")]
                    log::warn!("[RpcServer] failed to send response for method {}", method);
                }
            } else {
                #[cfg(feature = "logging")]
                log::warn!("[RpcServer] failed to acquire response channel for method {}", method);
            }
        }

        ProcessResult::Handled(if one_way {
            HandledKind::OneWay
        } else if notify {
            HandledKind::Notify
        } else {
            HandledKind::Quiet
        })
    }

    /// 处理急停通道 (CH2) 的一条消息。
    pub fn process_urgent<H: RpcHandler>(&self) -> ProcessResult {
        self.process_channel::<H>(self.urgent_ch)
    }

    /// 处理普通通道 (CH0) 的一条消息。
    pub fn process_one<H: RpcHandler>(&self) -> ProcessResult {
        self.process_channel::<H>(self.req_ch)
    }

    /// 先处理所有急停，再处理所有普通消息。
    ///
    /// 非 RPC 消息通过 `on_other` 回调。
    /// 返回 `(handled_count, should_notify)`。
    pub fn process_all<H: RpcHandler, F: FnMut(Message)>(
        &self,
        mut on_other: F,
    ) -> (usize, bool) {
        let mut count = 0;
        let mut should_notify = false;

        loop {
            match self.process_urgent::<H>() {
                ProcessResult::NoMessage => break,
                ProcessResult::Handled(HandledKind::OneWay) => count += 1,
                ProcessResult::Handled(kind) => {
                    count += 1;
                    if kind == HandledKind::Notify {
                        should_notify = true;
                    }
                }
                ProcessResult::Unhandled(_) => {}
                ProcessResult::NotRpc(msg) => on_other(msg),
            }
        }

        loop {
            match self.process_one::<H>() {
                ProcessResult::NoMessage => break,
                ProcessResult::Handled(kind) => {
                    count += 1;
                    if kind == HandledKind::Notify {
                        should_notify = true;
                    }
                }
                ProcessResult::Unhandled(_) => {}
                ProcessResult::NotRpc(msg) => on_other(msg),
            }
        }

        (count, should_notify)
    }

    /// 检查急停通道是否有待处理消息。
    pub fn has_urgent(&self) -> bool {
        let shm = self.shm();
        shm.receiver(self.urgent_ch)
            .is_ok_and(|rx| rx.has_pending())
    }

    /// 检查普通通道是否有待处理消息。
    pub fn has_pending(&self) -> bool {
        let shm = self.shm();
        shm.receiver(self.req_ch)
            .is_ok_and(|rx| rx.has_pending())
    }
}
