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
use tock_registers::interfaces::{Readable, Writeable};
use tock_registers::registers::ReadWrite;
use tock_registers::register_structs;

#[allow(dead_code)]
mod amp {
    include!(concat!(env!("OUT_DIR"), "/amp_gen.rs"));
}

pub use amp::{SHMBASE, SHMSIZE, UART1IRQ};

register_structs! {
    /// PLIC priority 寄存器（单 u32，读回确认用）。
    pub PlicPrioReg {
        (0x00 => priority: ReadWrite<u32>),
        (0x04 => @END),
    }
}

register_structs! {
    /// CLINT MSIP 寄存器（单 u32，跨 hart IPI 用）。
    pub ClintMsipReg {
        (0x00 => msip: ReadWrite<u32>),
        (0x04 => @END),
    }
}

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
        platform::driver::DRIVERS.set(drivers);

        // 3. 遍历 DT 实例化 driver（probe 各节点 → 填充 registry 槽位）。
        platform::driver::boot();

        // 4. 注册 UART1 RX 中断 handler（仅写 IRQ_TABLE，无 PLIC 竞争）。
        //    PLIC enable/priority 推迟到 [`Board::late_init`]，避免被 hart0
        //    (StarryOS) 较晚的 PLIC 初始化覆盖。
        platform::register_irq(
            UART1IRQ as u32,
            platform::drivers::serial_ns16550a::rx_handler,
        );
    }

    fn late_init() {
        // 配置 UART1 的 PLIC 中断。必须在开全局中断前、hart0 完成其 PLIC
        // 初始化后进行：本板 PLIC 与 hart0 (StarryOS) 共享，priority 寄存器
        // 全局唯一，StarryOS 的 disable_all_sources 会批量清零所有 source
        // priority。hart1 启动远早于 hart0，Board::init/main 里配的优先级会
        // 被覆盖为 0（priority=0 即禁能），导致 RX 中断永不触发。
        setup_console_irq();
    }
}

/// 配置 UART1 的 PLIC 中断（使能 + 优先级）。
///
/// 策略：先基于 CLINT `mtime` 忙等 hart0 (StarryOS) 的启动窗口（其 PLIC
/// 初始化会调 `disable_all_sources` 批量清零所有 source priority），过了
/// 这个窗口再配置并读回确认。读回失败则重试，但设有上限避免死循环。
///
/// 此函数在 `Board::late_init`（`platform::start()` 之前、全局中断关闭）
/// 调用，忙等不影响系统响应。
///
/// **失效条件（需关注）**：当前 `HART0_BOOT_SECS` 是经验值，依赖两点假设——
/// ① StarryOS 启动到 PLIC 初始化稳定在窗口内；② hart0 在该窗口之后不会
/// 再次清零 source priority。若 StarryOS 启动序列或耗时显著变化（如增加
/// 外设 probe、慢速 flash 初始化），需重估此窗口；hart0 若引入运行时
/// 重新 disable_all_sources 的逻辑，本方案需改为事件驱动（如 StarryOS
/// 完成后主动 IPI 通知），否则 priority 会被二次覆盖。
fn setup_console_irq() {
    const TARGET_PRIO: u32 = 2;
    // 等 hart0 启动窗口（含 PLIC 初始化）。3 秒留足裕量。
    const HART0_BOOT_SECS: u64 = 3;
    // 配置后读回确认的重试上限。
    const MAX_CONFIRM_RETRIES: u32 = 20;
    let freq = platform::timer().freq_hz() as u64;
    let start = platform::timer().now();
    let wait_ticks = freq.saturating_mul(HART0_BOOT_SECS);
    while platform::timer().now().wrapping_sub(start) < wait_ticks {
        core::hint::spin_loop();
    }

    platform::intctl().enable_irq(UART1IRQ as u32);
    // 配置并读回确认（裸读 PLIC priority 寄存器，板级自查，不走通用 trait）；
    // hart0 偶发的延迟写需要重试，但有上限。
    let prio_addr = amp::PLICBASE + 4 * UART1IRQ as usize;
    for _ in 0..MAX_CONFIRM_RETRIES {
        platform::intctl().set_priority(UART1IRQ as u32, TARGET_PRIO);
        // SAFETY: prio_addr = PLICBASE + irq*4，amp.toml 校验过的 MMIO 地址，
        // 单 hart 串行读（tock-registers 内部用 volatile）。
        let prio: &PlicPrioReg = unsafe { &*(prio_addr as *const PlicPrioReg) };
        if prio.priority.get() == TARGET_PRIO {
            return;
        }
        for _ in 0..50_000 {
            core::hint::spin_loop();
        }
    }
    log::warn!("setup_console_irq: priority not stable after retries, forcing enable");
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
    // SAFETY: amp::CLINTBASE 是 amp.toml 校验过的 MMIO 地址，单 hart 串行写。
    let msip: &ClintMsipReg = unsafe { &*(amp::CLINTBASE as *const ClintMsipReg) };
    msip.msip.set(1);
}

/// 清除 hart 0 的 MSIP0。
pub unsafe fn clear_ipi_to_linux() {
    // SAFETY: 同上。
    let msip: &ClintMsipReg = unsafe { &*(amp::CLINTBASE as *const ClintMsipReg) };
    msip.msip.set(0);
}
