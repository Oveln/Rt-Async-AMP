//! ov-shm —— 跨核共享内存 + 通知（AMP 数据面）驱动 crate。
//!
//! 基于 rt-async driver model，提供两个可被设备树 probe 的驱动：
//!
//! - [`shm::ShmDriver`]：共享内存区域（compatible `ov,rt-async-amp`，
//!   与 AP 侧 StarryOS rt_shm 匹配同一节点），
//!   probe 从节点 `reg` 读取基址与大小，取代旧的 amp.toml 编译期常量；
//! - [`notifier::ClintMsipNotifier`]：跨核通知后端（compatible
//!   `ov,clint-msip-notifier`），probe 从 `reg` 读取对端 MSIP 地址，
//!   [`notifier::PeerNotifier::notify`] 写 1 触发对端核心中断。
//!
//! 通知设备在设备树中通过 notifier 子节点声明（compatible 区分后端）：
//! QEMU virt 用 `ov,clint-msip-notifier`；K3 真板的 mailbox 后端由
//! `chip-k3-rt24` 实现同一 [`notifier::PeerNotifier`] trait 并注册。

#![no_std]

pub mod notifier;
pub mod shm;
