//! K3 RT24 R_UARTx 驱动（PXA 派生 UART，`spacemit,pxa-uart0`）——多实例。
//!
//! 实现 platform 的 [`Driver`]（DT probe 入口）+ [`Serial`]（console）。
//! 初始化序列移植自 esos `pxa_uart_initialize()` + 本仓库 `uart.rs`（已点亮）。
//!
//! ## 多实例模型
//!
//! 一个 Driver 单例按 DT 到达顺序（DFS 文档序 = DTS 中 serial 节点顺序）把
//! 各节点分配进固定 slot 池（[`PORT_BASES`]）：
//!
//! | slot | 控制器 | 用途 | 引脚（MUX mode） |
//! |------|--------|------|-------------------|
//! | 0 | R_UART0 @ 0xc088_1000 | console/log | GPIO_122/123（mode4） |
//! | 1 | R_UART3 @ 0xc088_1300 | 机器人底盘口 | GPIO_88/89（mode3） |
//! | 2 | R_UART1 @ 0xc088_1100 | 机器人机械臂口 | GPIO_17/18（mode5） |
//!
//! console 派生（`try_derive_console` 取 SERIALS 首项）= slot 0，与单实例
//! 时代行为一致——**R_UART0 节点必须始终排在 DTS 中其他 serial 节点之前**。
//!
//! 协议路径（机器人控制）必须用 [`PxaUart::write_raw`]：console 的
//! [`Serial::write`] 带 `\n → \r\n` 文本翻译，会破坏透传帧。
//!
//! ## 与时钟/pinmux 的分工
//!
//! 各 UART 的时钟链（ruart_14 上游 gate + 末端 gate）由 CCU driver
//! （[`crate::clock`]）经设备树 `clocks` 属性在 `boot()` 的 driver probe
//! **之前**自动使能；pinmux 由 pinctrl-single driver 经 `pinctrl-0` 同样在
//! probe 前自动应用。本 probe **只配 IP 自身**（波特率/FIFO/UUE 单元使能）。
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

/// 实例池容量：R_UART0（console）+ 底盘口 + 机械臂口。
///
/// 与 SERIALS 注册表容量（4）约束对齐：勿超。
pub const PORT_COUNT: usize = 3;

/// probe 按 DT 到达顺序写入各 slot 的 MMIO 基址。0 表示尚未 probe。
static PORT_BASES: [AtomicUsize; PORT_COUNT] = [const { AtomicUsize::new(0) }; PORT_COUNT];

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

/// 返回指定 slot 的寄存器引用。该 slot 尚未 probe 时 panic。
fn regs_for(slot: usize) -> &'static PxaUartRegs {
    let addr = PORT_BASES[slot].load(Ordering::Acquire);
    assert!(addr != 0, "pxa-uart: port {} not probed", slot);
    // SAFETY: addr 来自 probe 写入的 DT reg，指向已验证的 MMIO 区域。
    // 单 hart 串行访问，无别名引用（tock-registers 内部用 volatile）。
    unsafe { &*(addr as *const PxaUartRegs) }
}

/// 按 DT 到达序初始化一个 slot 的波特率/FIFO/UUE。
fn init_port(slot: usize) {
    // 波特率：设 DLAB → DLL/DLH → 清 DLAB 设 8N1 → FCR。
    // 时钟链/pinmux 已由 CCU/pinctrl driver 在 probe 前自动配置，此处只配 IP 自身。
    let r = regs_for(slot);
    r.lcr.write(Lcr::DLAB::SET);                 // 进除数锁存模式
    r.thr_rbr.set(DIVISOR & 0xFF);               // DLL（offset 0x000 与 thr_rbr 共用）
    r.ier.set((DIVISOR >> 8) & 0xFF);            // DLH（offset 0x004 与 ier 共用）
    r.lcr.write(Lcr::WLEN8::SET);                // 清 DLAB，设 8N1
    r.fcr.write(Fcr::ENABLE::SET + Fcr::CLR_RX::SET + Fcr::CLR_TX::SET);

    // UUE 单元使能（PXA 专属，⭐ 最易漏）+ MCR OUT2。
    r.ier.write(Ier::UUE::SET);
    r.mcr.write(Mcr::OUT2::SET);
}

/// 单个 UART 端口句柄（slot 固定）。
pub struct PxaUart {
    slot: usize,
}

/// 端口对象池：slot 0 = R_UART0（console）、1 = R_UART3（底盘）、2 = R_UART1（臂）。
///
/// 与 DTS 中 serial 节点的文档顺序一一对应（见模块头注表格）。
pub static PORTS: [PxaUart; PORT_COUNT] = [
    PxaUart { slot: 0 },
    PxaUart { slot: 1 },
    PxaUart { slot: 2 },
];

/// 按 slot 取端口对象；该 slot 未 probe（无对应 DTS 节点）时返回 None。
pub fn port(slot: usize) -> Option<&'static PxaUart> {
    if slot < PORT_COUNT && PORT_BASES[slot].load(Ordering::Acquire) != 0 {
        Some(&PORTS[slot])
    } else {
        None
    }
}

impl PxaUart {
    /// 本端口的 slot 编号。
    pub fn slot(&self) -> usize {
        self.slot
    }

    /// 原始写：逐字节阻塞发送，**不做** `\n → \r\n` 翻译。
    ///
    /// 机器人协议（底盘二进制帧 / 舵机 ASCII 帧）是字节透传，
    /// [`Serial::write`] 的文本翻译会破坏帧内容——协议路径必须用本方法。
    pub fn write_raw(&self, buf: &[u8]) {
        let r = regs_for(self.slot);
        for &b in buf {
            // 等 THR 空，写字节。
            while !r.lsr.is_set(Lsr::THRE) {
                core::hint::spin_loop();
            }
            r.thr_rbr.set(b as u32);
        }
    }

    /// 原始读：轮询取单字节，无数据返回 None（阶段1 轮询读，不接 RX 中断）。
    pub fn read_raw(&self) -> Option<u8> {
        let r = regs_for(self.slot);
        if !r.lsr.is_set(Lsr::DR) {
            return None;
        }
        Some(r.thr_rbr.get() as u8)
    }
}

impl Serial for PxaUart {
    fn write(&self, buf: &[u8]) {
        let r = regs_for(self.slot);
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
        self.read_raw()
    }

    fn has_data(&self) -> bool {
        regs_for(self.slot).lsr.is_set(Lsr::DR)
    }
}

/// Driver 单例（DT probe 入口）：每个 `spacemit,pxa-uart0` 节点 probe 一次，
/// 按 DT 到达顺序占用下一个空闲 slot。
pub struct PxaUartDriver;

/// 全局单例，供 K3_DRIVERS 注册。
pub static INSTANCE: PxaUartDriver = PxaUartDriver;

impl Driver for PxaUartDriver {
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

        // 找第一个空闲 slot（DT 到达序 = DTS 文档序；console 派生依赖
        // R_UART0 节点排在最前 → slot 0）。
        let Some(slot) = PORT_BASES
            .iter()
            .position(|b| b.load(Ordering::Relaxed) == 0)
        else {
            log::warn!("pxa-uart: port pool full ({}), ignoring {:#x}", PORT_COUNT, base);
            return;
        };
        PORT_BASES[slot].store(base, Ordering::Release);

        init_port(slot);

        // 登记进多实例注册表；console 由 boot() 的 try_derive_console
        // 据 chosen.stdout-path 在首个 Serial probe 后派生（首项 = slot 0）。
        platform::driver::SERIALS.register(&PORTS[slot]);

        log::info!(
            "K3 R_UART slot {} probed: base={:#x}, 115200-8N1",
            slot,
            base
        );
    }
}
