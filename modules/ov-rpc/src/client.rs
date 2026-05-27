//! RPC 客户端

use ov_channels::{ChannelId, Message, SharedMemory};
use portable_atomic::{AtomicU64, Ordering};

use crate::RequestId;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn alloc_request_id() -> RequestId {
    NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

/// RPC 客户端
///
/// 向指定 channel 发送请求，从另一个 channel 接收响应。
/// 通过 `request_id` 匹配请求和响应。
pub struct RpcClient {
    shm_addr: usize,
    req_channel: ChannelId,
    resp_channel: ChannelId,
}

impl RpcClient {
    /// 创建 RPC 客户端
    ///
    /// - `shm_addr`: 共享内存物理地址
    /// - `req_channel`: 发送请求的 channel ID
    /// - `resp_channel`: 接收响应的 channel ID
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

    /// 发起 RPC 调用（仅发送请求，不等待响应）
    ///
    /// 返回 `RequestId` 用于后续 [`wait_response`](Self::wait_response)。
    pub fn call_async<Args: serde::Serialize>(
        &self,
        method_id: u64,
        args: &Args,
    ) -> Result<RequestId, ov_channels::SendError> {
        let rid = alloc_request_id();
        let msg = Message::request(rid, method_id, args)
            .map_err(|_| ov_channels::SendError::Invalid)?;

        let shm = self.shm();
        let tx = shm
            .sender(self.req_channel)
            .map_err(|_| ov_channels::SendError::Invalid)?;
        tx.try_send(&msg)?;

        Ok(rid)
    }

    /// 等待指定 `request_id` 的响应
    ///
    /// 扫描 `resp_channel` 中所有待处理消息，寻找匹配的响应。
    /// 不匹配的响应会被丢弃。
    pub fn wait_response<T: serde::de::DeserializeOwned>(
        &self,
        request_id: RequestId,
    ) -> Option<T> {
        let shm = self.shm();
        let Ok(rx) = shm.receiver(self.resp_channel) else {
            return None;
        };

        for msg in rx.iter() {
            if let Some((rid, result)) = msg.as_response::<T>() {
                if rid == request_id {
                    return Some(result);
                }
            }
        }

        None
    }

    /// 发起 RPC 调用并等待响应（便捷方法）
    ///
    /// **注意**：在单线程测试中，需要先让服务端处理请求后再调用此方法，
    /// 否则会立即返回 `None`。推荐使用 [`call_async`](Self::call_async) +
    /// [`wait_response`](Self::wait_response) 的拆分模式。
    pub fn call<T, Args>(&self, method_id: u64, args: &Args) -> Option<T>
    where
        Args: serde::Serialize,
        T: serde::de::DeserializeOwned,
    {
        let rid = self.call_async(method_id, args).ok()?;
        self.wait_response(rid)
    }

    /// 发起 RPC 调用，发送请求后触发 IPI 通知
    #[cfg(feature = "amp")]
    pub fn call_with_notify<T, Args, F>(
        &self,
        method_id: u64,
        args: &Args,
        notify: F,
    ) -> Option<T>
    where
        Args: serde::Serialize,
        T: serde::de::DeserializeOwned,
        F: FnOnce(),
    {
        let rid = self.call_async(method_id, args).ok()?;
        notify();
        self.wait_response(rid)
    }

    /// 尝试接收一次响应（非阻塞轮询）
    pub fn try_recv_response<T: serde::de::DeserializeOwned>(&self) -> Option<(RequestId, T)> {
        let shm = self.shm();
        let rx = shm.receiver(self.resp_channel).ok()?;
        let msg = rx.try_recv()?;
        msg.as_response::<T>()
    }
}

#[cfg(feature = "amp")]
pub struct AmpRpcClient {
    inner: RpcClient,
    notify_fn: unsafe fn(),
}

#[cfg(feature = "amp")]
impl AmpRpcClient {
    pub const fn new(
        shm_addr: usize,
        req_channel: ChannelId,
        resp_channel: ChannelId,
        notify_fn: unsafe fn(),
    ) -> Self {
        Self {
            inner: RpcClient::new(shm_addr, req_channel, resp_channel),
            notify_fn,
        }
    }

    pub fn call<T, Args>(&self, method_id: u64, args: &Args) -> Option<T>
    where
        Args: serde::Serialize,
        T: serde::de::DeserializeOwned,
    {
        let rid = self.inner.call_async(method_id, args).ok()?;
        unsafe {
            (self.notify_fn)();
        }
        self.inner.wait_response(rid)
    }

    pub fn call_async<Args: serde::Serialize>(
        &self,
        method_id: u64,
        args: &Args,
    ) -> Option<RequestId> {
        let rid = self.inner.call_async(method_id, args).ok()?;
        unsafe {
            (self.notify_fn)();
        }
        Some(rid)
    }

    pub fn poll_response<T: serde::de::DeserializeOwned>(
        &self,
        request_id: RequestId,
    ) -> Option<T> {
        self.inner.wait_response(request_id)
    }
}
