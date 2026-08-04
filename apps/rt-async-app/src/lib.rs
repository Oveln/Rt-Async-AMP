//! rt-async-amp 双核应用
//!
//! hart 1 (M-mode): rt-async 实时任务
//! hart 0 (S-mode): StarryOS Linux 内核
//! 共享内存 IPC 位于设备树 `ov,rt-async-amp` 节点（由 ov-shm 驱动 probe，
//! 单一真相源 `its/rt-async-shm.dtsi`，与 AP 侧 StarryOS rt_shm 匹配同一节点）

#![no_std]

// 强制链接板级 crate：`#[extern_trait] impl Board` 符号经此保持引用，
// 否则 --gc-sections 会在链接期丢弃 chip-qemu-virt-rt 的 Board 实现
// （原由 intercom 的 `use ...::SHMBASE` 隐式保住）。
#[allow(unused_imports)]
pub use chip_qemu_virt_rt as _;

pub mod intercom;
pub mod ipc_wait;
pub mod uart_wait;
