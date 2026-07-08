//! # QEMU Virt RT 芯片实现
//!
//! 主仓库 rt-async-amp 专属板级 crate。rt-async 跑在 QEMU hart1（M-mode RTOS），
//! UART1 (0x10002000) 作为 console。
//!
//! ## 职责
//!
//! - [`Board`] 实现：DTB handoff + driver 注册 + DT 遍历 + IRQ 注册
//! - AMP 共享内存常量和 hart0 IPI（`send_ipi_to_linux`）
//!
//! 全部外设驱动（NS16550A / CLINT / PLIC / sifive-test）已迁移到
//! `platform::drivers` 内部模块，经设备树 probe 实例化。

#![no_std]
#![allow(unreachable_code)]

use extern_trait::extern_trait;
use platform::Board;

#[allow(dead_code)]
mod amp {
    include!(concat!(env!("OUT_DIR"), "/amp_gen.rs"));
}

pub use amp::{SHMBASE, SHMSIZE, UART1IRQ};

pub struct QemuVirtRt;

/// esos 同款 handoff：从 `RTASYNCDTBBASE` 起按 `SCAN_STEP` 步长扫描内存，
/// 认领 root 节点 `compatible` 含 `"ov,rt-async"` 的 DTB。
fn locate_rtasync_dtb() -> &'static [u8] {
    use fdt_parser::Fdt;

    const SCAN_STEP: usize = 0x1000;
    const SCAN_PAGES: usize = 16;
    const PROBE_LEN: usize = 0x1000;

    let base = amp::RTASYNCDTBBASE;
    for i in 0..SCAN_PAGES {
        let addr = base + i * SCAN_STEP;
        let probe: &[u8] =
            unsafe { core::slice::from_raw_parts(addr as *const u8, PROBE_LEN) };

        let Ok(fdt) = Fdt::from_bytes(probe) else {
            continue;
        };

        let total = fdt.total_size();
        if total == 0 || total > 0x10_0000 {
            continue;
        }
        let dtb: &[u8] =
            unsafe { core::slice::from_raw_parts(addr as *const u8, total) };

        let Ok(fdt) = Fdt::from_bytes(dtb) else {
            continue;
        };

        if fdt.find_compatible(&["ov,rt-async"]).next().is_some() {
            return dtb;
        }
    }
    panic!("locate_rtasync_dtb: no DTB with compatible=\"ov,rt-async\" found");
}

#[extern_trait]
impl Board for QemuVirtRt {
    fn init() {
        // 1. esos 同款扫描 + DTB 注入。
        let dtb = locate_rtasync_dtb();
        platform::dtb::init_dtb(dtb);

        // 2. 注册板级 driver 列表（platform 内置默认列表）。
        let drivers = platform::drivers::default_drivers();
        platform::driver::set_drivers(drivers);

        // 3. 遍历 DT 实例化 driver（probe 各节点 → 填充 registry 槽位）。
        platform::driver::boot();

        // 4. 注册 UART1 RX 中断 handler。
        platform::register_irq(
            UART1IRQ as u32,
            platform::drivers::serial_ns16550a::rx_handler,
        );
        platform::intctl().enable_irq(UART1IRQ as u32);
        platform::intctl().set_priority(UART1IRQ as u32, 2);
    }
}

// ── AMP 专用：向 hart 0 (StarryOS) 发送 IPI ───────────────────────────

/// 向 hart 0 (StarryOS) 发送 IPI (写 MSIP0)。
///
/// 与 driver registry 的本 hart IPI（base + hart*4）不同，这是跨 hart 的
/// AMP 通知——写 CLINT base+0 触发 hart0 的 MachineSoft 中断。
///
/// # Safety
/// 调用前应确保共享内存中的数据已写入完成。
/// 内部使用 `fence(Release)` 保证内存可见性。
pub unsafe fn send_ipi_to_linux() {
    core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
    unsafe { core::ptr::write_volatile(amp::CLINTBASE as *mut u32, 1) };
}

/// 清除 hart 0 的 MSIP0。
pub unsafe fn clear_ipi_to_linux() {
    unsafe { core::ptr::write_volatile(amp::CLINTBASE as *mut u32, 0) };
}
