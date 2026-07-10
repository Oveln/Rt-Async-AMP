//! K3 RT24 R_UART0 驱动（PXA 派生 UART，`spacemit,pxa-uart0`）。
//!
//! 实现 platform 的 [`Driver`]（DT probe 入口）+ [`Serial`]（console）。
//! 初始化序列移植自 esos `pxa_uart_initialize()` + 本仓库 `uart.rs`（已点亮）。
//!
//! ## 与时钟/pinmux 的分工
//!
//! UART 的时钟链（ruart_14 上游 gate + UART0 末端 gate）由 CCU driver
//!（[`crate::clock`]）经设备树 `clocks` 属性在 `boot()` 的 driver probe
//! **之前**自动使能；pinmux（GPIO_122/123）由 pinctrl-single driver 经
//! `pinctrl-0` 同样在 probe 前自动应用。本 probe **只做波特率/FIFO/
//! UUE 单元使能**——这些是 PXA-uart IP 自身的配置，与时钟链无关。
//!
//! ## PXA-uart 关键点
//!
//! - 寄存器 stride = **4**（不是标准 16550 的 1）。
//! - **UUE 位**（IER bit6=0x40）+ **MCR OUT2**（0x08）必须置，否则整个 UART
//!   单元 disabled，THR 写入不出波形（PXA 专属，最易漏）。
//! - 波特率：14.48MHz / (16*115200) ≈ 8（DLAB → DLL=8/DLH=0 → 清 DLAB 设 8N1）。

use core::sync::atomic::{AtomicUsize, Ordering};

use fdt_parser::Node;
use platform::device::{Driver, Serial};
use tock_registers::interfaces::{Readable, Writeable};
use tock_registers::registers::{ReadOnly, ReadWrite};
use tock_registers::{register_bitfields, register_structs};

/// probe 写入的 MMIO 基址。0 表示尚未 probe。
static BASE: AtomicUsize = AtomicUsize::new(0);

// ── 寄存器定义（tock-registers，stride = 4）─────────────────────────

register_bitfields![u32,
    /// 中断使能寄存器 IER。
    Ier [
        UUE OFFSET(6) NUMBITS(1) [],  // UART Unit Enable（PXA 专属，最易漏）
    ],
    /// FIFO 控制寄存器 FCR。
    Fcr [
        ENABLE OFFSET(0) NUMBITS(1) [],  // FIFO 使能
        CLR_RX OFFSET(1) NUMBITS(1) [],  // 清 RX FIFO
        CLR_TX OFFSET(2) NUMBITS(1) [],  // 清 TX FIFO
    ],
    /// 线路控制寄存器 LCR。
    Lcr [
        WLEN8 OFFSET(0) NUMBITS(2) [],   // 8 数据位（值 0b11）
        DLAB  OFFSET(7) NUMBITS(1) [],   // 除数锁存访问
    ],
    /// modem 控制寄存器 MCR。
    Mcr [
        OUT2 OFFSET(3) NUMBITS(1) [],    // OUT2（PXA 专属，配合 UUE）
    ],
    /// 线路状态寄存器 LSR。
    Lsr [
        DR   OFFSET(0) NUMBITS(1) [],   // 数据就绪
        THRE OFFSET(5) NUMBITS(1) [],   // 发送保持寄存器空
    ],
];

register_structs! {
    /// PXA-UART 寄存器映射（u32 寄存器，stride = 4）。
    pub PxaUartRegs {
        (0x000 => thr_rbr: ReadWrite<u32>),                        // 发送/接收保持
        (0x004 => ier:     ReadWrite<u32, Ier::Register>),         // 中断使能（DLAB=0）/ DLH（DLAB=1）
        (0x008 => fcr:     ReadWrite<u32, Fcr::Register>),         // FIFO 控制
        (0x00C => lcr:     ReadWrite<u32, Lcr::Register>),         // 线路控制
        (0x010 => mcr:     ReadWrite<u32, Mcr::Register>),         // modem 控制
        (0x014 => lsr:     ReadOnly<u32, Lsr::Register>),          // 线路状态
        (0x018 => @END),
    }
}

// 14.48MHz / (16 * 115200) ≈ 8
const DIVISOR: u32 = 8;

/// 返回寄存器引用。probe 前调用为 panic。
fn regs() -> &'static PxaUartRegs {
    let addr = BASE.load(Ordering::Acquire);
    assert!(addr != 0, "pxa-uart: not probed");
    // SAFETY: addr 来自 probe 写入的 DT reg，指向已验证的 MMIO 区域。
    // 单 hart 串行访问，无别名引用（tock-registers 内部用 volatile）。
    unsafe { &*(addr as *const PxaUartRegs) }
}

/// PXA-UART 单例（零大小）。
pub struct PxaUart;

/// 全局单例，供 probe 注册进 registry。
pub static INSTANCE: PxaUart = PxaUart;

impl Serial for PxaUart {
    fn write(&self, buf: &[u8]) {
        let r = regs();
        for &b in buf {
            // 串口需 \r\n：把 \n 转成 \r\n（与旧 uart.rs::put_str 行为一致），
            // 否则终端按 LF 解释会呈阶梯换行。
            if b == b'\n' {
                // 等 THR 空，写 \r。
                while !r.lsr.is_set(Lsr::THRE) {
                    core::hint::spin_loop();
                }
                r.thr_rbr.set(b'\r' as u32);
            }
            // 等 THR 空，写字节。
            while !r.lsr.is_set(Lsr::THRE) {
                core::hint::spin_loop();
            }
            r.thr_rbr.set(b as u32);
        }
    }

    fn read(&self) -> Option<u8> {
        // 阶段1：轮询读（不接 RX 中断）。SerialRx 异步路径留后续。
        let r = regs();
        if !r.lsr.is_set(Lsr::DR) {
            return None;
        }
        Some(r.thr_rbr.get() as u8)
    }

    fn has_data(&self) -> bool {
        regs().lsr.is_set(Lsr::DR)
    }
}

impl Driver for PxaUart {
    fn compatible(&self) -> &'static [&'static str] {
        &["spacemit,pxa-uart0"]
    }

    fn probe(&self, node: &Node<'_>) {
        let reg = node
            .reg()
            .expect("pxa-uart0: missing reg property")
            .next()
            .expect("pxa-uart0: empty reg");
        let base = reg.address as usize;
        BASE.store(base, Ordering::Release);

        // 波特率：设 DLAB → DLL/DLH → 清 DLAB 设 8N1 → FCR。
        // 时钟链/pinmux 已由 CCU/pinctrl driver 在 probe 前自动配置，此处只配 IP 自身。
        let r = regs();
        r.lcr.write(Lcr::DLAB::SET);                 // 进除数锁存模式
        r.thr_rbr.set(DIVISOR & 0xFF);               // DLL（offset 0x000 与 thr_rbr 共用）
        r.ier.set((DIVISOR >> 8) & 0xFF);            // DLH（offset 0x004 与 ier 共用）
        r.lcr.write(Lcr::WLEN8::SET);                // 清 DLAB，设 8N1
        r.fcr.write(Fcr::ENABLE::SET + Fcr::CLR_RX::SET + Fcr::CLR_TX::SET);

        // UUE 单元使能（PXA 专属，⭐ 最易漏）+ MCR OUT2。
        r.ier.write(Ier::UUE::SET);
        r.mcr.write(Mcr::OUT2::SET);

        // 登记进多实例注册表；默认 console 由 boot() 的 try_derive_console
        // 据 chosen.stdout-path 在首个 Serial probe 后派生（不再由 probe 自命）。
        platform::driver::SERIALS.register(&INSTANCE);

        log::info!("K3 R_UART0 probed: base={:#x}, 115200-8N1", base);
    }
}
