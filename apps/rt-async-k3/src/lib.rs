//! rt-async-k3 —— K3 RT24 rcpu1 双核 AMP 应用
//!
//! 与 AP 侧 StarryOS 经共享内存（ov-channels）+ mailbox4 通知通信。
//! 共享内存基址来自设备树 `ov,rt-async-amp` 节点（ov-shm 驱动 probe）。
//! mailbox4（0xCAC91000，IRQ 69，Rust 变量名 MBX3）为核间信令通道；
//! mailbox3 归 esos(rcpu0) rproc 专用，rt-async 不触碰。

#![no_std]
#![feature(impl_trait_in_assoc_type)]

// 强制链接板级 crate：`#[extern_trait] impl Board` 符号经此保持引用，
// 否则 --gc-sections 会在链接期丢弃 chip-k3-rt24 的 Board 实现。
#[allow(unused_imports)]
pub use chip_k3_rt24 as _;

pub mod intercom;
pub mod robot;
pub mod watchdog;
