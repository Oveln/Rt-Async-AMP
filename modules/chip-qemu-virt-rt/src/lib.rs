//! # QEMU Virt RT 芯片实现（转发 shim）
//!
//! 基于 rt-async 的 `qemu-virt`，主仓库 rt-async-amp 专属：
//! - 跑在 QEMU hart1（M-mode RTOS），与子模块（hart0）不同
//! - UART0 (0x10000000): OpenSBI / StarryOS (hart 0, S-mode)
//! - UART1 (0x10002000): rt-async (hart 1, M-mode RTOS)
//!
//! 本 crate 已不再在 `Chip`/`TimerChip` impl 里硬编码 MMIO 逻辑——具体外设驱动
//! （NS16550A / CLINT timer / CLINT msip / sifive test）抽到 `platform::drivers`
//! 内部，由设备树 probe 实例化。这里仅保留 `extern_trait` 静态分发入口，方法体
//! 转发到 `platform::driver` registry（console / timer / ipi / reset），保持上层
//! （executor / futures / apps）调用 `ChipImpl::*` / `TimerChipImpl::*` 零改动。
//!
//! ## DTB handoff（esos 同款扫描）
//! 与子模块自包含 `include_bytes!` 不同，主仓库 rt-async 跑在 hart1、地址布局
//! 不同，DTB 由 xtask 经 QEMU `-device loader,addr=RTASYNCDTBBASE,file=rt-async.dtb`
//! 摆进内存。`board_init` 从 `RTASYNCDTBBASE` 起按固定步长扫描，认领 root
//! `compatible` 含 `"ov,rt-async"` 的 DTB，交给 `platform::dtb::init_dtb`。

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

/// esos 同款 handoff：从 `RTASYNCDTBBASE` 起按 `SCAN_STEP` 步长扫描内存，
/// 认领 root 节点 `compatible` 含 `"ov,rt-async"` 的 DTB。
///
/// xtask 用 QEMU `-device loader,addr=RTASYNCDTBBASE,file=rt-async.dtb`
/// 把 rt-async 专属 DTB 摆进内存；loader 不保证精确落在 base（4KB 对齐即可），
/// 故按页步长扫若干页兜底。每个候选地址先按 FDT header 大小试解析（`from_bytes`
/// 校验 magic + totalsize），合法后用 `total_size()` 重构正确长度的 `'static`
/// 切片，再校验 root `compatible`。
///
/// 找不到则 panic（板级描述缺失属致命错误）。
fn locate_rtasync_dtb() -> &'static [u8] {
    use fdt_parser::Fdt;

    /// 扫描步长（4KB，与 QEMU loader 对齐）。
    const SCAN_STEP: usize = 0x1000;
    /// 最多扫描的页数（兜底防死循环；rt-async DTB 不会摆太远）。
    const SCAN_PAGES: usize = 16;
    /// 试探用切片长度：FDT header 解析只需前 40 字节，给 4KB 足够且能覆盖
    /// 绝大多数 DTB 的 totalsize。from_bytes 仅校验 header，不要求切片 == totalsize。
    const PROBE_LEN: usize = 0x1000;

    let base = amp::RTASYNCDTBBASE;
    for i in 0..SCAN_PAGES {
        let addr = base + i * SCAN_STEP;
        // SAFETY: 该地址由 QEMU loader 写入 DTB（或为 RAM 空洞）。读 FDT header
        // 大小（40 字节）的裸内存是安全的：QEMU virt 的 RAM 覆盖该区域，
        // 不会触发 fault；非 DTB 地址 from_bytes 返回 Err，跳过即可。
        let probe: &[u8] =
            unsafe { core::slice::from_raw_parts(addr as *const u8, PROBE_LEN) };

        let Ok(fdt) = Fdt::from_bytes(probe) else {
            continue;
        };

        // 用 header 里的 totalsize 重构精确长度的 'static 切片，并据此重建
        // Fdt。这样后续 find_compatible 遍历 struct block 时不会因 probe 切片
        // 短于 totalsize 而越界（probe 仅用于校验 header）。
        let total = fdt.total_size();
        if total == 0 || total > 0x10_0000 {
            // totalsize 异常（0 或超 1MB），跳过防越界。
            continue;
        }
        // SAFETY: totalsize 来自合法 FDT header；该内存为 RAM，'static 有效
        // （rt-async 整个生命周期内 loader 写入的 DTB 都在）。
        let dtb: &[u8] =
            unsafe { core::slice::from_raw_parts(addr as *const u8, total) };

        // 重解析精确长度切片；失败则跳过（header 合法但 body 损坏的极端情况）。
        let Ok(fdt) = Fdt::from_bytes(dtb) else {
            continue;
        };

        // 认领条件：root 节点 compatible 含 "ov,rt-async"。
        // find_compatible 遍历所有节点匹配 compatible 列表；root 节点本身
        // 也被遍历到（all_nodes 含 root）。
        if fdt.find_compatible(&["ov,rt-async"]).next().is_some() {
            return dtb;
        }
    }
    panic!("locate_rtasync_dtb: no DTB with compatible=\"ov,rt-async\" found");
}

#[extern_trait]
impl Chip for QemuVirtRt {
    fn board_init() {
        // 1. esos 同款扫描：从 RTASYNCDTBBASE 认领 compatible="ov,rt-async" 的 DTB。
        let dtb = locate_rtasync_dtb();
        platform::dtb::init_dtb(dtb);

        // 2. 注册板级 driver 列表（用 platform 内置默认列表）。
        let drivers = platform::drivers::default_drivers();
        // SAFETY: drivers 是 'static 切片；board_init 在调度器启动前串行调用一次。
        unsafe { platform::driver::set_drivers(drivers) };

        // 3. 遍历 DT 实例化 driver（probe 各节点 → 填充 registry 槽位）。
        platform::driver::boot();
    }

    fn shutdown() -> ! {
        platform::driver::reset().shutdown()
    }

    fn put_str(s: &str) {
        platform::driver::console().write(s.as_bytes());
    }

    unsafe fn pend() {
        // SAFETY: 调用者（platform::pend）保证上下文合适。
        unsafe { platform::driver::ipi().send() };
    }

    unsafe fn clear_pend() {
        // SAFETY: ISR 早期调用，关中断上下文。
        unsafe { platform::driver::ipi().clear() };
    }
}

#[extern_trait]
impl TimerChip for QemuVirtRt {
    fn freq_hz() -> u32 {
        platform::driver::timer().freq_hz()
    }

    fn now_ticks() -> u64 {
        platform::driver::timer().now()
    }

    fn set_deadline(tick: u64) {
        platform::driver::timer().set_deadline(tick)
    }

    unsafe fn enable_timer_irq() {
        // 先把 deadline 推到最远，避免立刻触发；再开 mie.MTIE。
        // MTIE 属 arch 级配置，不属于 driver model，保留在此。
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
/// 注意：这是给 hart0 发 IPI（不是本 hart1 的 IPI），CLINT MSIP0 在 base+0，
/// 与 driver registry 里的本 hart IPI（base+4）不同，故保留直接 MMIO 写。
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
