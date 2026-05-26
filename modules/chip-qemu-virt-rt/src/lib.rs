//! # QEMU Virt RT 芯片实现
//!
//! 基于 rt-async 的 `qemu-virt`，使用自定义 QEMU 的 UART1:
//! - UART0 (0x10000000): OpenSBI / StarryOS (hart 0, S-mode)
//! - UART1 (0x10002000): rt-async (hart 1, M-mode RTOS)

#![no_std]
#![allow(unreachable_code)]

use extern_trait::extern_trait;
use platform::{Chip, TimerChip};

mod amp {
    include!(concat!(env!("OUT_DIR"), "/amp_gen.rs"));
}

pub use amp::{SHMBASE, SHMSIZE};

pub struct QemuVirtRt;

#[extern_trait]
impl Chip for QemuVirtRt {
    fn shutdown() -> ! {
        unsafe {
            core::ptr::write_volatile(0x100_000 as *mut u32, 0x5555);
        }
        loop {}
    }

    fn put_str(s: &str) {
        for &byte in s.as_bytes() {
            unsafe {
                core::ptr::write_volatile(amp::UART1BASE as *mut u8, byte);
            }
        }
    }

    unsafe fn pend() {
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        unsafe { core::ptr::write_volatile((amp::CLINTBASE + 4) as *mut u32, 1) };
    }

    unsafe fn clear_pend() {
        unsafe { core::ptr::write_volatile((amp::CLINTBASE + 4) as *mut u32, 0) };
    }
}

#[extern_trait]
impl TimerChip for QemuVirtRt {
    fn freq_hz() -> u32 {
        10_000_000
    }

    fn now_ticks() -> u64 {
        unsafe { core::ptr::read_volatile(0x200_BFF8 as *const u64) }
    }

    fn set_deadline(tick: u64) {
        unsafe { core::ptr::write_volatile((amp::CLINTBASE + 0x4008) as *mut u64, tick) };
    }

    unsafe fn enable_timer_irq() {
        Self::set_deadline(u64::MAX);
        unsafe { riscv::register::mie::set_mtimer() };
    }
}

/// 向 hart 0 (StarryOS) 发送 IPI (写 MSIP0)
///
/// # Safety
/// 调用前应确保共享内存中的数据已写入完成。
/// 内部使用 `fence(Release)` 保证写 CLINT 之前的所有内存操作对 hart 0 可见。
pub unsafe fn send_ipi_to_linux() {
    core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
    unsafe { core::ptr::write_volatile(amp::CLINTBASE as *mut u32, 1) };
}

/// 清除 hart 0 的 MSIP0
pub unsafe fn clear_ipi_to_linux() {
    unsafe { core::ptr::write_volatile(amp::CLINTBASE as *mut u32, 0) };
}
