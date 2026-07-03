//! # QEMU Virt RT 芯片实现
//!
//! 基于 rt-async 的 `qemu-virt`，使用自定义 QEMU 的 UART1:
//! - UART0 (0x10000000): OpenSBI / StarryOS (hart 0, S-mode)
//! - UART1 (0x10002000): rt-async (hart 1, M-mode RTOS)

#![no_std]
#![allow(unreachable_code)]

use extern_trait::extern_trait;
use platform::{Chip, TimerChip};

#[allow(dead_code)]
mod amp {
    include!(concat!(env!("OUT_DIR"), "/amp_gen.rs"));
}

pub use amp::{PLICBASE, SHMBASE, SHMSIZE, UART1IRQ};

// ── NS16550A UART 寄存器偏移 ──────────────────────────────────────────────
mod uart {
    pub const RBR_THR: usize = 0;
    pub const IER: usize = 1;
    pub const IIR_FCR: usize = 2;
    pub const LCR: usize = 3;
    pub const LSR: usize = 5;

    pub const IER_ERBFI: u8 = 0x01;

    pub const FCR_ENABLE: u8 = 0x01;
    pub const FCR_CLEAR_RX: u8 = 0x02;
    pub const FCR_CLEAR_TX: u8 = 0x04;

    pub const LSR_DR: u8 = 0x01;
    pub const LSR_THRE: u8 = 0x20;
}

// ── PLIC 寄存器布局 (context 2 = hart 1 M-mode) ─────────────────────────
mod plic {
    use super::amp;

    pub const ENABLE: usize = amp::PLICBASE + 0x2000 + 2 * 0x80;
    pub const THRESHOLD: usize = amp::PLICBASE + 0x200000 + 2 * 0x1000;
    pub const CLAIM: usize = amp::PLICBASE + 0x200004 + 2 * 0x1000;
}

pub struct QemuVirtRt;

#[extern_trait]
impl Chip for QemuVirtRt {
    fn board_init() {}

    fn shutdown() -> ! {
        unsafe {
            core::ptr::write_volatile(0x100_000 as *mut u32, 0x5555);
        }
        loop {}
    }

    fn put_str(s: &str) {
        for &byte in s.as_bytes() {
            unsafe {
                while core::ptr::read_volatile((amp::UART1BASE + uart::LSR) as *const u8)
                    & uart::LSR_THRE
                    == 0
                {}
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
        unsafe { core::ptr::read_volatile((amp::CLINTBASE + 0xBFF8) as *const u64) }
    }

    fn set_deadline(tick: u64) {
        unsafe { core::ptr::write_volatile((amp::CLINTBASE + 0x4008) as *mut u64, tick) };
    }

    unsafe fn enable_timer_irq() {
        Self::set_deadline(u64::MAX);
        unsafe { riscv::register::mie::set_mtimer() };
    }
}

/// 初始化 UART1：启用 FIFO + 接收中断
pub fn uart_init() {
    let base = amp::UART1BASE;
    unsafe {
        core::ptr::write_volatile((base + uart::IIR_FCR) as *mut u8, uart::FCR_ENABLE | uart::FCR_CLEAR_RX | uart::FCR_CLEAR_TX);
        core::ptr::write_volatile((base + uart::IER) as *mut u8, uart::IER_ERBFI);
    }
}

/// UART1 是否有数据可读 (LSR.DR)
pub fn uart_has_data() -> bool {
    unsafe { core::ptr::read_volatile((amp::UART1BASE + uart::LSR) as *const u8) & uart::LSR_DR != 0 }
}

/// 从 UART1 RBR 读取一个字节
pub fn uart_read_byte() -> u8 {
    unsafe { core::ptr::read_volatile(amp::UART1BASE as *const u8) }
}

/// 初始化 PLIC：设置 UART1_IRQ 优先级、启用中断、设阈值为 0
pub fn plic_init() {
    let irq = amp::UART1IRQ;
    unsafe {
        let prio_reg = (amp::PLICBASE + 4 * irq as usize) as *mut u32;
        core::ptr::write_volatile(prio_reg, 1);
        let word = irq / 32;
        let bit = irq % 32;
        let enable_reg = (plic::ENABLE + word * 4) as *mut u32;
        core::ptr::write_volatile(enable_reg, core::ptr::read_volatile(enable_reg) | (1u32 << bit));
        core::ptr::write_volatile(plic::THRESHOLD as *mut u32, 0);
    }
}

/// PLIC claim：返回当前最高优先级的 pending 中断号
pub fn plic_claim() -> u32 {
    unsafe { core::ptr::read_volatile(plic::CLAIM as *const u32) }
}

/// PLIC complete：通知 PLIC 中断处理完毕
pub fn plic_complete(irq: u32) {
    unsafe { core::ptr::write_volatile(plic::CLAIM as *mut u32, irq) };
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
