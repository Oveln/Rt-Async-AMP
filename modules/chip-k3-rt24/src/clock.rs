//! K3 RT24 rcpu1 CCU（Clock Control Unit）driver。
//!
//! 实现 [`platform::ClockProvider`]。probe 时开 ruart_14 共享上游 gate +
//! 注册进 [`platform::driver::CLOCK`] slot；`enable_for` 经设备树 `clocks`
//! 属性配置各外设末端 gate/mux/reset。
//!
//! ## 关键设计：RT24 小核是 PLL 消费者
//!
//! K3 的 PLL（PLL1~8）在 `0xD4050000`（Main PMU），属 AP 大核电源域，
//! 由 SPL/U-Boot 启动时配好并锁定。RT24 实时小核不碰 PLL，只消费已分频
//! 好的固定频率时钟源。小核侧时钟寄存器全在 `0xC088_xxxx`（手册 17.4.2），
//! 只有 gate/mux/div/reset，无 PLL 编程。故不需完整 CCF。
//!
//! ## RCPU 各外设时钟域基址（手册 17.4.2，PDF p.626 / 手册 p.608）
//!
//! | 域 | 基址 | 外设 |
//! |----|------|------|
//! | RCPU_SYSCTRL  | 0xc088_0000 | 含 ruart_14 上游 DDN gate（+0x3C）|
//! | RCPU_UARTCTRL | 0xc088_1f00 | UART0~5 |
//! | RCPU_I2CCTRL  | 0xc088_6f00 | I2C0~2 |
//! | RCPU_SPICTRL  | 0xc088_5f00 | SSP0~1 |
//!
//! ## CLK_RST 寄存器统一格式（手册 p.689 R_UARTn_CLK_RST，各外设同构）
//!
//! ```text
//! bit[0]   APBCLK    APB 总线时钟使能
//! bit[1]   FNCLK     功能时钟使能
//! bit[2]   RST       复位：0=释放, 1=复位（上电默认复位态）
//! bit[5:4] FNCLKSEL  功能时钟源选择（mux）
//! bit[18:8] FNCLKDIV 功能时钟分频（实际值 = FNCLKDIV + 1）
//! ```

use platform::device::{ClockProvider, Driver};
use platform::driver;

use fdt_parser::Node;

// --- RCPU 时钟域基址（手册 17.4.2）---
const RCPU_UARTCTRL: usize = 0xc088_1f00;
const RCPU_I2CCTRL: usize = 0xc088_6f00;
const RCPU_SPICTRL: usize = 0xc088_5f00;

// --- ruart_14 上游 DDN gate（RCPU_SYSCTRL + 0x3C）---
// 产生 ~14.48MHz 时钟源（ruart_14），多个 UART 共享。bit[31]=gate。
// 不置 bit31 则 ruart_14 被关，下游选它的 UART 都无时钟。
const RUART_14_CLK_CTRL: usize = 0xc088_003c;
const RUART_14_GATE_BIT: u32 = 1u32 << 31;

// --- CLK_RST 统一位域 ---
const APBCLK: u32 = 1 << 0;
const FNCLK: u32 = 1 << 1;
// RST 位写 0 = 释放复位（上电默认 1 = 复位态，需主动释放）。

/// 时钟 ID 常量（来自 its/k3-clock.h，经 DTS clocks = <&ccu K3_CLK_xxx> 引用）。
/// 这里用普通 const 而非 include 头文件（Rust 无 C 预处理器）；值与
/// its/k3-clock.h 的 #define 保持一致。
mod clk_id {
    pub const RUART0: u32 = 0;
    pub const RI2C0: u32 = 10;
    pub const RI2C1: u32 = 11;
    pub const RI2C2: u32 = 12;
    pub const RSSP0: u32 = 20;
    pub const RSSP1: u32 = 21;
}

/// 一个外设时钟的寄存器描述。
struct ClkEntry {
    /// RCPU 时钟域基址。
    base: usize,
    /// 外设在域内的寄存器偏移。
    offset: usize,
    /// 默认 FNCLKSEL（时钟源选择，域语义相关）。
    ///
    /// UART 域 sel=0 → ruart_14(~14.48MHz)；I2C/SSP 域 sel=0 → 24.576MHz
    /// （见手册 p.689/p.692 各域 FNCLKSEL 编码）。
    sel: u32,
}

/// 时钟 ID → 寄存器描述查表。
static CLK_TABLE: &[(u32, ClkEntry)] = &[
    // UART0（FNCLKSEL=0 → ruart_14）
    (clk_id::RUART0, ClkEntry { base: RCPU_UARTCTRL, offset: 0x00, sel: 0 }),
    // I2C0~2（FNCLKSEL=0 → 24.576MHz）
    (clk_id::RI2C0, ClkEntry { base: RCPU_I2CCTRL, offset: 0x00, sel: 0 }),
    (clk_id::RI2C1, ClkEntry { base: RCPU_I2CCTRL, offset: 0x04, sel: 0 }),
    (clk_id::RI2C2, ClkEntry { base: RCPU_I2CCTRL, offset: 0x08, sel: 0 }),
    // SSP0~1（FNCLKSEL=0 → 24.576MHz）
    (clk_id::RSSP0, ClkEntry { base: RCPU_SPICTRL, offset: 0x00, sel: 0 }),
    (clk_id::RSSP1, ClkEntry { base: RCPU_SPICTRL, offset: 0x04, sel: 0 }),
];

#[inline(always)]
fn write32(addr: usize, val: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, val) };
}

#[inline(always)]
fn read32(addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

/// 使能一个外设的功能时钟并释放复位。
///
/// 写 CLK_RST 寄存器：置 APBCLK+FNCLK、选时钟源、RST 位写 0（释放复位）。
fn enable_periph(reg: usize, sel: u32) {
    let v = APBCLK | FNCLK | (sel << 4);
    write32(reg, v);
}

/// K3 CCU 零大小单例。
pub struct Ccu;

/// 全局单例，供 probe 注册进 [`driver::CLOCK`]。
pub static CCU: Ccu = Ccu;

impl Driver for Ccu {
    fn compatible(&self) -> &'static [&'static str] {
        &["spacemit,k3-ccu"]
    }

    fn probe(&self, _node: &Node<'_>) {
        // 1. 开 ruart_14 共享上游 DDN gate（多个 UART 共用，一次性开启）。
        //    保留 num/den（来自 ruart_14_tbl），只置 bit[31] gate 位。
        let v = read32(RUART_14_CLK_CTRL) | RUART_14_GATE_BIT;
        write32(RUART_14_CLK_CTRL, v);

        // 2. 注册进 CLOCK slot，使 boot() 后续节点能调 enable_for。
        driver::CLOCK.set(&CCU);
        log::info!("K3 CCU probed: ruart_14 upstream gate enabled");
    }
}

impl ClockProvider for Ccu {
    fn enable_for(&self, node: &Node<'_>) {
        // 解析 node 的 clocks 属性：fdt_parser 的 node.clocks() 返回
        // ClockRef { node, select, .. }，其中 select 就是 #clock-cells 的
        // ID cell（DTS 里 clocks = <&ccu K3_CLK_xxx> 的第二项）。
        for clock in node.clocks() {
            let id = clock.select as u32;
            if let Some((_, entry)) = CLK_TABLE.iter().find(|(cid, _)| *cid == id) {
                let reg = entry.base + entry.offset;
                enable_periph(reg, entry.sel);
                log::info!("k3 clk: enabled id={} at {:#x}", id, reg);
            } else {
                log::warn!("k3 clk: unknown clock id {}", id);
            }
        }
    }
}
