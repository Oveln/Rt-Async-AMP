//! RPC 服务端

use ov_channels::{ChannelId, Message, SharedMemory};

/// Method ID 类型
pub type MethodId = u64;

/// Request ID 类型
pub type RequestId = u64;

/// RPC 请求处理 trait
///
/// 实现此 trait 以定义 RPC 方法集合。
/// 推荐使用 [`define_service!`](crate::define_service) 宏自动生成实现。
///
/// # 手动实现示例
///
/// ```ignore
/// struct MyService;
///
/// impl RpcHandler for MyService {
///     fn handle(method: MethodId, msg: Message) -> Option<Message> {
///         match method {
///             0 => {
///                 let (rid, _, val): (RequestId, MethodId, u32) = msg.as_request()?;
///                 Message::response(rid, &(val * 2)).ok()
///             }
///             _ => None,
///         }
///     }
/// }
/// ```
pub trait RpcHandler {
    /// 处理一个 RPC 请求，返回响应消息
    ///
    /// - `method`: 方法 ID
    /// - `msg`: 原始请求消息（含 request_id、method_id、序列化参数）
    ///
    /// 返回 `Some(response)` 表示成功处理，`None` 表示方法未知或处理失败。
    fn handle(method: MethodId, msg: Message) -> Option<Message>;
}

/// [`RpcServer::process_one`] 的返回结果
#[derive(Debug)]
pub enum ProcessResult {
    /// Channel 中无待处理消息
    NoMessage,
    /// RPC 请求已成功处理
    Handled,
    /// 非 RPC 消息（通知、数据等），已从 channel 中取出，交由调用者处理
    NotRpc(Message),
}

/// RPC 服务端
///
/// 从指定的 channel 接收请求，通过 `RpcHandler` 分发处理，
/// 将响应写入另一个 channel。
///
/// # 通道布局
///
/// ```text
/// RpcServer { req_channel: 0, resp_channel: 1 }
///
/// Channel 0 (req_channel):  对端 ──▶ 本端  (接收请求)
/// Channel 1 (resp_channel): 本端 ──▶ 对端  (发送响应)
/// ```
pub struct RpcServer {
    shm_addr: usize,
    req_channel: ChannelId,
    resp_channel: ChannelId,
}

impl RpcServer {
    /// 创建 RPC 服务端
    ///
    /// - `shm_addr`: 共享内存物理地址
    /// - `req_channel`: 接收请求的 channel ID
    /// - `resp_channel`: 发送响应的 channel ID
    pub const fn new(shm_addr: usize, req_channel: ChannelId, resp_channel: ChannelId) -> Self {
        Self {
            shm_addr,
            req_channel,
            resp_channel,
        }
    }

    #[inline]
    fn shm(&self) -> &'static SharedMemory {
        unsafe { SharedMemory::at(self.shm_addr) }
    }

    /// 处理一条消息
    ///
    /// 从 `req_channel` 取出一条消息：
    /// - 若是 RPC 请求：调用 `H::handle` 处理并将响应写入 `resp_channel`，返回 [`ProcessResult::Handled`]
    /// - 若是非 RPC 消息（通知、数据）：不处理，以 [`ProcessResult::NotRpc`] 返回给调用者
    /// - 若 channel 为空：返回 [`ProcessResult::NoMessage`]
    pub fn process_one<H: RpcHandler>(&self) -> ProcessResult {
        let shm = self.shm();
        let Ok(rx) = shm.receiver(self.req_channel) else {
            return ProcessResult::NoMessage;
        };

        let Some(msg) = rx.try_recv() else {
            return ProcessResult::NoMessage;
        };

        let Some(method_id) = msg.method_id() else {
            return ProcessResult::NotRpc(msg);
        };

        let Some(resp) = H::handle(method_id, msg) else {
            #[cfg(feature = "logging")]
            log::warn!("[RpcServer] unhandled method: {}", method_id);
            return ProcessResult::Handled;
        };

        if let Ok(tx) = shm.sender(self.resp_channel) {
            let _ = tx.try_send(&resp);
        }

        ProcessResult::Handled
    }

    /// 处理 channel 中所有消息
    ///
    /// 循环调用 [`process_one`](Self::process_one)，
    /// 非RPC 消息通过 `on_other` 回调交由调用者处理。
    ///
    /// 返回已处理的 RPC 请求数量。
    pub fn process_all<H: RpcHandler, F: FnMut(Message)>(&self, mut on_other: F) -> usize {
        let mut count = 0;
        loop {
            match self.process_one::<H>() {
                ProcessResult::NoMessage => return count,
                ProcessResult::Handled => count += 1,
                ProcessResult::NotRpc(msg) => on_other(msg),
            }
        }
    }

    /// 检查是否有待处理消息
    pub fn has_pending(&self) -> bool {
        let shm = self.shm();
        shm.receiver(self.req_channel)
            .is_ok_and(|rx| rx.has_pending())
    }
}
