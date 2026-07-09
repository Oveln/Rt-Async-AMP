//! # K3 RT24 rcpu1 板级 crate（新 driver model）
//!
//! 为进迭时空 K3 SoC 的 RT24 实时小核（rcpu1，CVA6/RV64GC，614.4MHz）提供
//! [`platform::Board`] 实现与一组 K3 专属 driver（SysTimer / MSIP / PLIC /
//! PXA-UART）。
//!
//! ## 与 QEMU virt（`chip-qemu-virt-rt`）的对应关系
//!
//! 两者都实现 `Board`，经设备树 probe 实例化 driver。差异：
//! - DTB 来源：QEMU 靠 handoff 扫描；**K3 内嵌**（U-Boot 不做 DTB handoff，
//!   见 `uboot-2022.10/drivers/remoteproc/k3-rproc.c`，只 memcpy ELF 段）。
//! - driver 列表：QEMU 用 `default_drivers()`（含 ns16550a/sifive-test）；
//!   K3 组装自己的列表（PXA-UART 等是 K3 专属，放本 crate）。
//!
//! ## 初始化序列（移植自 esos `os1_rcpu/baremetal/main.c`，已验证）
//!
//! `Board::init()`（由 `platform::init()` 调用）：
//! 1. `clock::early_init()` —— SPL 握手回写（解锁 AP 的 6s 轮询）+ UART 时钟
//!    链（ruart_14 DDN gate + UART0 末端 gate）+ pinmux（GPIO_122/123）。
//! 2. `init_dtb` —— 注入内嵌 K3 DTB。
//! 3. `DRIVERS.set` + `boot()` —— DT 遍历，按 compatible 实例化各 driver。
//!
//! 启动握手必须最先（步骤1），否则 AP 卡在 `k3_rproc_start()` 的轮询循环。

#![no_std]
#![allow(unreachable_code)]

pub mod clock;
pub mod clint_k3;
pub mod plic_k3;
pub mod pxa_uart;
pub mod reset_stub;

use extern_trait::extern_trait;
use platform::device::Driver;
use platform::Board;

/// K3 RT24 板类型（零大小，仅作 trait impl 载体）。
pub struct K3Rt24;

/// K3 专属 driver 列表（不复用 `default_drivers()`）。
///
/// K3 没有 ns16550a / sifive-test（QEMU 专属），且 PXA-UART 是 K3 专属驱动
/// 放在本 crate；故在此组装自己的列表注入 registry。
/// reset_stub 无 DT 节点，不经 probe，在 init() 里直接 `RESET.set`。
static K3_DRIVERS: &[&dyn Driver] = &[
    &pxa_uart::INSTANCE,
    &clint_k3::TIMER,
    &clint_k3::MSIP,
    &plic_k3::PLIC,
];

#[extern_trait]
impl Board for K3Rt24 {
    fn init() {
        // 1. SPL 握手回写 + UART 时钟链 + pinmux（最先，解锁 AP 6s 轮询）。
        clock::early_init();

        // 2. 注入内嵌 DTB（U-Boot 不 handoff DTB，只能内嵌进 ELF）。
        //    include_bytes! 把 .dtb 编进 .rodata，作为 PT_LOAD 段随 ELF 加载。
        platform::dtb::init_dtb(include_bytes!("../../../its/rt-async-k3.dtb"));

        // 3. 注册 K3 专属 driver 列表。
        platform::driver::DRIVERS.set(K3_DRIVERS);

        // 4. 遍历 DT 实例化 driver（probe 各节点 → 填充 registry 槽位）。
        platform::driver::boot();

        // 5. reset_stub 无 DT 节点，直接注册（关机=wfi 死循环，trait 占位）。
        platform::driver::RESET.set(&reset_stub::INSTANCE);
    }
}
