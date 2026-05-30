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
//! - `call`       — 请求-响应，interrupt 模式 (IPI 回复)
//! - `call_quiet` — 请求-响应，busy-poll 模式 (不回 IPI)
//! - `send`       — 单向，不期待响应
//! - `urgent`     — 急停，走 CH2，不期待响应

#![cfg_attr(not(test), no_std)]
#![warn(missing_docs)]

mod client;
mod macros;
mod server;

/// Method ID 类型
pub type MethodId = u64;
/// Request ID 类型
pub type RequestId = u64;

pub use client::{RecvError, RpcClient};
pub use server::{DeserializeFailed, HandledKind, ProcessResult, RpcHandler, RpcServer};

// ============================================================================
// 协议约定：method_id bit 分配
// ============================================================================

/// method_id bit 63: 响应后是否 IPI 通知 (interrupt 模式)
pub const NOTIFY_FLAG: u64 = 1 << 63;

/// method_id bit 62: 单向调用 (不需要响应)
pub const ONE_WAY_FLAG: u64 = 1 << 62;

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
