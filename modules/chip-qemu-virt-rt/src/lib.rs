//! # QEMU Virt RT 芯片实现
//!
//! 基于 rt-async 的 `qemu-virt`，使用自定义 QEMU 的 UART1:
//! - UART0 (0x10000000): OpenSBI / StarryOS
//! - UART1 (0x10002000): rt-async (M-mode RTOS, hart 0)

#![no_std]
#![allow(unreachable_code)]

use extern_trait::extern_trait;
use platform::{Chip, TimerChip};

/// QEMU virt UART1 基址 (自定义 QEMU: serial@10002000)
const UART_BASE: usize = 0x1000_2000;
/// SiFive Test 关机寄存器
const SIFIVE_TEST_BASE: usize = 0x100_000;
/// CLINT msip (hart 0)
const CLINT_MSIP: usize = 0x2000_000;
/// CLINT mtimecmp (hart 0)
const CLINT_MTIMECMP: usize = 0x200_4000;
/// CLINT mtime
const CLINT_MTIME: usize = 0x200_BFF8;

pub struct QemuVirtRt;

#[extern_trait]
impl Chip for QemuVirtRt {
    fn shutdown() -> ! {
        unsafe {
            core::ptr::write_volatile(SIFIVE_TEST_BASE as *mut u32, 0x5555);
        }
        loop {}
    }

    fn put_str(s: &str) {
        for &byte in s.as_bytes() {
            unsafe {
                core::ptr::write_volatile(UART_BASE as *mut u8, byte);
            }
        }
    }

    unsafe fn pend() {
        unsafe { core::ptr::write_volatile(CLINT_MSIP as *mut u32, 1) };
    }

    unsafe fn clear_pend() {
        unsafe { core::ptr::write_volatile(CLINT_MSIP as *mut u32, 0) };
    }
}

#[extern_trait]
impl TimerChip for QemuVirtRt {
    fn freq_hz() -> u32 {
        10_000_000
    }

    fn now_ticks() -> u64 {
        unsafe { core::ptr::read_volatile(CLINT_MTIME as *const u64) }
    }

    fn set_deadline(tick: u64) {
        unsafe { core::ptr::write_volatile(CLINT_MTIMECMP as *mut u64, tick) };
    }

    unsafe fn enable_timer_irq() {
        Self::set_deadline(u64::MAX);
        unsafe { riscv::register::mie::set_mtimer() };
    }
}

/// 向 hart 1 (StarryOS) 发送 IPI (写 MSIP1)
pub unsafe fn send_ipi_to_linux() {
    unsafe { core::ptr::write_volatile((0x2000_000 + 4) as *mut u32, 1) };
}

/// 清除 hart 1 的 MSIP1
pub unsafe fn clear_ipi_to_linux() {
    unsafe { core::ptr::write_volatile((0x2000_000 + 4) as *mut u32, 0) };
}
