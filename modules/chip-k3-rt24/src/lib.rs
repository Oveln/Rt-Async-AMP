//! # K3 RT24 rcpu1 芯片实现
//!
//! 为进迭时空 K3 SoC 的 RT24 实时小核（rcpu1，CVA6/RV64GC）提供
//! [`Chip`] / [`TimerChip`] 实现与板级初始化。
//!
//! 初始化序列移植自 esos 的 `os1_rcpu/baremetal/main.c`（已验证）。

#![no_std]
#![allow(unreachable_code)]

pub mod clock;
pub mod uart;

use extern_trait::extern_trait;
use platform::{Chip, TimerChip};

/// K3 RT24 rcpu1 芯片类型（零大小，仅作 trait impl 载体）。
pub struct K3Rt24;

#[extern_trait]
impl Chip for K3Rt24 {
    fn board_init() {
        clock::early_init(); // 步骤 1-4（握手回写+时钟链+pinmux）
        uart::init();        // 步骤 5-6（波特率/8N1/FCR+UUE）
    }

    fn shutdown() -> ! {
        loop {}
    }

    fn put_str(s: &str) {
        for &b in s.as_bytes() {
            if b == b'\n' {
                uart::putc(b'\r'); // 串口需 \r\n
            }
            uart::putc(b);
        }
    }

    unsafe fn pend() {}

    unsafe fn clear_pend() {}
}

/// TimerChip stub（方案 A）：minimal 无定时器任务，rtimer 留后续。
/// `enable_timer_irq()` 为空操作，不产生中断。
#[extern_trait]
impl TimerChip for K3Rt24 {
    fn freq_hz() -> u32 {
        0
    }

    fn now_ticks() -> u64 {
        0
    }

    fn set_deadline(_tick: u64) {}

    unsafe fn enable_timer_irq() {}
}
