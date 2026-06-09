//! rt-async-amp 双核应用
//!
//! hart 1 (M-mode): rt-async 实时任务
//! hart 0 (S-mode): StarryOS Linux 内核
//! 共享内存 IPC 位于 0x8800_0000

#![no_std]

pub mod intercom;
pub mod ipc_wait;
pub mod uart_wait;
