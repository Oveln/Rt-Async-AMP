//! # K3 RT24 rcpu1 芯片实现
//!
//! 为进迭时空 K3 SoC 的 RT24 实时小核（rcpu1，CVA6/RV64GC）提供
//! [`Chip`] / [`TimerChip`] 实现与板级初始化。
//!
//! 初始化序列移植自 esos 的 `os1_rcpu/baremetal/main.c`（已验证）。

#![no_std]

pub mod clock;
