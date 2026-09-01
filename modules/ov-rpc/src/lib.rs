//! # ov-rpc: 实时 AMP RPC 框架
//!
//! 基于 `ov_channels` 共享内存通道，为 AMP 双核系统提供类型安全的 RPC 调用。
//!
//! ## 通道布局
//!
//! ```text
//! CH0: 普通请求  Client ──▶ Server
//! CH1: 普通响应  Server ──▶ Client
//! CH2: 急停通道  Client ──▶ Server (单向, 高优先级)
//! ```
//!
//! ## 调用模式
//!
//! - `call`       — 请求-响应，自动根据 BUSY 标志决定是否发 IPI 唤醒服务端，**服务端逐请求回 IPI**
//! - `call_poll`  — 请求-响应，自动根据 BUSY 标志决定是否发 IPI 唤醒服务端，**服务端不回 IPI**，调用者自行 poll
//! - `send`       — 单向，不期待响应
//! - `urgent`     — 急停，走 CH2，不期待响应
//!
//! ## 消息尺寸分层（v0.2.0，依赖 ov-channels 0.3 块层）
//!
//! | 方向 | postcard 上限 | 线格式 |
//! |------|---------------|--------|
//! | 请求参数（client→server） | 239B（255 − rid 8 − method 16） | 恒单块（与 0.2.x 相同） |
//! | 响应结果（server→client） | ≤ 247B | 单块，与 0.2.x 逐字节相同 |
//! | 响应结果（server→client） | 247B < n ≤ 2028B | 字节流帧 2..=8 块**原子发布** |
//! | 响应结果（server→client） | > 2028B | [`RpcError::ResponseTooLarge`]（配置错误） |
//!
//! 错误路径（方法未注册 / 参数反序列化失败 / 响应超长）服务端对双向调用
//! 回 **poison 响应**（Response kind + 原 rid + 不可解码载荷），客户端
//! 得到 `RecvError::DeserializeFailed`——不会挂死等超时。
//!
//! ## 服务发现
//!
//! method id 0 保留（[`METHOD_INIT`]）：服务端在 dispatch 前拦截 INIT，
//! 把 [`RpcHandler::descriptor`]（`define_service!` const 生成）经响应
//! 通路回发。**方法表从 1 起编号**。客户端经
//! [`RpcClient::discover`](crate::RpcClient::discover) +
//! [`RpcClient::recv_raw_for`](crate::RpcClient::recv_raw_for) +
//! [`descriptor::parse`] 消费。

#![cfg_attr(not(test), no_std)]
#![warn(missing_docs)]

pub mod cache;
mod client;
pub mod descriptor;
mod macros;
mod server;

/// Method ID 类型
pub type MethodId = u64;
/// Request ID 类型
pub type RequestId = u64;

/// 保留 method id 0：服务发现（INIT）。
///
/// 服务端在 dispatch 前拦截，把 [`RpcHandler::descriptor`] 生成的服务
/// 描述符经大响应通路回发（见 [`descriptor`] 模块文档）。**方法表从 1
/// 起编号**——0 被协议占用。旧固件（未拦截）下 INIT 落到未知方法，
/// 客户端收到 poison 响应（或 ov-channels 版本门直接拒绝发送），
/// 快失败而非挂死。
pub const METHOD_INIT: MethodId = 0;

pub use client::{RecvError, RpcClient};
pub use ov_channels::SendError;
pub use server::{
    HandledKind, ProcessResult, RESPONSE_DATA_MAX, RESP_SEND_FAILS, Reply, Response, RpcError,
    RpcHandler, RpcServer, send_response,
};

// 为宏内部使用重新导出 paste
#[doc(hidden)]
pub use paste::paste;

// ============================================================================
// 协议约定：method_id bit 分配
// ============================================================================

/// method_id bit 63: 响应后是否回 IPI (用于 `call` 模式)
pub const NOTIFY_FLAG: u64 = 1 << 63;

/// method_id bit 62: 单向调用 (不需要响应)
pub const ONE_WAY_FLAG: u64 = 1 << 62;

// （低 62 位中的 id 0 保留给 INIT 服务发现，见 [`METHOD_INIT`]。）

/// 提取实际 method_id (低 62 位)
#[inline]
pub const fn strip_flags(method_id: u64) -> u64 {
    method_id & !(NOTIFY_FLAG | ONE_WAY_FLAG)
}

/// 是否需要 IPI 回复
#[inline]
pub const fn wants_notify(method_id: u64) -> bool {
    method_id & NOTIFY_FLAG != 0
}

/// 是否单向调用
#[inline]
pub const fn is_one_way(method_id: u64) -> bool {
    method_id & ONE_WAY_FLAG != 0
}
