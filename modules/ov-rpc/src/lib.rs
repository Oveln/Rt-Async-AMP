//! # ov-rpc: 类型安全的 AMP RPC 框架
//!
//! 基于 `ov_channels` 的共享内存通道，为 AMP 双核系统提供类型安全的 RPC 调用。
//!
//! ## 架构
//!
//! ```text
//! ┌──────────────┐     SharedMemory     ┌──────────────┐
//! │   RpcClient  │ ──── Channel 0 ────▶ │  RpcServer   │
//! │  (请求端)    │ ◀─── Channel 1 ────  │  (服务端)    │
//! └──────────────┘                      └──────────────┘
//! ```
//!
//! ## 快速使用
//!
//! ```ignore
//! use ov_rpc::{define_service, RpcServer, RpcClient, ProcessResult};
//!
//! // 1. 声明 RPC 服务
//! define_service! {
//!     pub MyService {
//!         ECHO: 0 => fn echo(val: u32) -> u32;
//!         ADD:  1 => fn add(a: i32, b: i32) -> i32;
//!     }
//! }
//!
//! // 2. 实现业务逻辑
//! impl MyService {
//!     pub fn echo(val: u32) -> u32 { val }
//!     pub fn add(a: i32, b: i32) -> i32 { a + b }
//! }
//!
//! // 3. 服务端处理
//! server.process_all::<MyService, _>(|non_rpc_msg| {
//!     // 处理通知等非 RPC 消息
//! });
//!
//! // 4. 客户端调用
//! let rid = client.call_async(MyService::ADD, &(3i32, 4i32))?;
//! let result: i32 = client.wait_response(rid)?;
//! ```

#![cfg_attr(not(test), no_std)]
#![warn(missing_docs)]

mod client;
mod macros;
mod server;

pub use client::RpcClient;
pub use server::{MethodId, ProcessResult, RequestId, RpcHandler, RpcServer};

#[cfg(feature = "amp")]
pub use client::AmpRpcClient;
