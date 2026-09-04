//! K3 AP 域 UART5 TX-only 轮询驱动（机械臂 ZP10S 通道，40pin pin3）。
//!
//! ## 为什么是"AP 域 UART + 轮询"
//!
//! RT24 访问 AP 域 UART 是"总线可达、中断不可达"：RCPU 地址图的 "To AP
//! APB" 窗口（0xd400_0000，4MB）覆盖 UART0/2~10（0xd401_7000~0xd401_7800），
//! 但这些 UART 的中断（m1_ffuart0~10，aplic_int[42..51]/[281]）只进 AP 侧
//! APLIC，RT24 中断源表（手册 §7.2）里没有对应线。ZP10S 只收不发，TX 用
//! LSR 轮询即可，RX 不需要。40pin 上唯一布线的 "UART1" 组是 Secure UART1
//! （0xf061_2000），不在 RCPU 任何地址窗口内，地址都发不进去。
//!
//! 选 UART5：TX = pad83（GPIO_83）mode4 = 40pin pin3（网络 I2C3_SDA，直连
//! 不经电平转换器——kit_v02 原理图 p10 SO-DIMM 193 = GPIO[4]_83 {…UART5_TXD
//! (f4)…I2C3_SDA (f6)…} 铁证），RX = pad82/pin5 不用。40pin 其余 AP UART
//! 复用均被占或不可达：pin29/32 = 底盘 R.UART0（pad122/123 兼 AP UART6）、
//! pin27/28 的 UART0 复用是 AP console 控制器、"UART1" 组不可达。
//!
//! ## AP 侧偷脚与自愈（实测，2026-09-03）
//!
//! AP 引导链会把 pad83 重 mux 成 func6（i2c3-2-cfg = pads82/83，挂触摸屏
//! I2C；板上实测 probe 时 0xd044 → AP 起来后 0xc046，写入模式与 k3-pinctrl
//! set_config_value+set_mux 吻合，thief 为 U-Boot 或 StarryOS 的 pinctrl
//! 应用路径）。APBC/SUCCR 不被碰。故 [`send`] 前调 [`pad83_heal`] 自查
//! MFPR，被改即重写并 warn——AP 每次启动偷一次，首个发送自愈后即稳定；
//! 若 warn 反复出现说明对端持续重夺，届时再于 AP 侧禁 i2c3。
//!
//! ## 时钟（自适应 func parent）
//!
//! APBC_UART5_CLK_RST（syscon_apbc 0xd401_5000 + 0x74，对齐内核
//! k3-syscon.h/ccu-k3.c/reset-spacemit.c）：bit0 bus gate、bit1 func gate、
//! bit2 reset（1 挂起/0 释放）、bits[6:4] func 源 mux：0=57.6M、1=14.7456M
//! （MPMU_SUCCR DDN）、2=48M（MPMU_SUCCR_1 DDN）。
//!
//! sel=1/sel=2 的 parent 都是 MPMU 上的 DDN 分频器（无独立 gate、复位无输
//! 出），且引导链对它们的编程各阶段不一致（实测：BROM 给 UART0 选 sel=1
//! 且 SUCCR=14.519M；U-Boot 后续把 SUCCR 重写为精确 14.7456M——num=125/
//! den=24）。SPL/U-Boot/OpenSBI/StarryOS 的 console 全部静态假设 14.7456M
//! 且正常出字 ⇒ UART0 选中的 parent 必活。故 probe **读寄存器自适应**：
//! 读 APBC_UART0 的 sel（BROM 给 console 的选择 = 活源实证）+ SUCCR/
//! SUCCR_1 算实际频率（`Fout = Fin × den/(2×num)`，num=bits[28:16]、
//! den=bits[12:0]；Fin 分别 153.6M/614.4M），优先跟随 UART0 的 sel，其次
//! 任一活的 ≈14.7456M 源（按实际频率算除数），全死才写 SUCCR=0x7D0018
//! （死源无消费者，写它零影响）。两阶段实测下均收敛到 sel=1/DLL=8（U-Boot
//! 重写后恰为零误差 115200）。
//!
//! ## PXA 16550A 关键点（同 [`crate::pxa_uart`]）
//!
//! 寄存器 stride = 4（reg-shift=2）；**UUE**（IER bit6）+ **MCR OUT2**
//! 不置则整个 UART 单元 disabled（PXA 专属，最易漏）。LSR=0x60 是复位值，
//! 不构成存活证据（发送器冻结时寄存器照常可读写）。

use core::sync::atomic::{AtomicUsize, Ordering};

use fdt_parser::Node;
use platform::device::Driver;
use tock_registers::interfaces::{Readable, Writeable};
use tock_registers::registers::{ReadOnly, ReadWrite};
use tock_registers::{register_bitfields, register_structs};

/// UART5 控制器基址（To AP APB 窗口内；DT reg 校验用）。
const BASE: usize = 0xd401_7400;

/// syscon_apbc 基址 + 各 UART CLK_RST offset。
const APBC: usize = 0xd401_5000;
/// APBC_UART5_CLK_RST（本 UART）。
const APBC_UART5: usize = APBC + 0x74;
/// APBC_UART0_CLK_RST（console——读 BROM 选的 sel 作活源参照）。
const APBC_UART0: usize = APBC + 0x00;

/// MPMU（同在 To AP APB 窗口内）。
const MPMU: usize = 0xd405_0000;
/// MPMU_SUCCR：sel=1 parent（slow_uart1 DDN，Fin=153.6MHz）。
const MPMU_SUCCR: usize = MPMU + 0x14;
/// MPMU_SUCCR_1：sel=2 parent（slow_uart2 DDN，Fin=614.4MHz）。
const MPMU_SUCCR_1: usize = MPMU + 0x10b0;
/// SUCCR = num=125/den=24：153.6MHz × 24/250 = 14.7456MHz（零误差 115200 源）。
const SUCCR_14P7456M: u32 = (125 << 16) | 24;

/// driver 单例（零大小）。
pub struct ApUart;

/// 全局单例，供 K3_DRIVERS 注册。
pub static INSTANCE: ApUart = ApUart;

/// probe 完成标志：send() 的前置条件。
static INITED: AtomicUsize = AtomicUsize::new(0);

register_bitfields![u32,
    /// 中断使能寄存器 IER（DLAB=0）。
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
        THRE OFFSET(5) NUMBITS(1) [],   // 发送保持寄存器空
        TEMT OFFSET(6) NUMBITS(1) [],   // 发送器空（含移位寄存器）
    ],
];

register_structs! {
    /// PXA-UART 寄存器映射（u32 寄存器，stride = 4）。
    ApUartRegs {
        (0x000 => thr_rbr: ReadWrite<u32>),                 // 发送/接收保持 + DLL（DLAB=1）
        (0x004 => ier:     ReadWrite<u32, Ier::Register>),  // 中断使能（DLAB=0）/ DLH（DLAB=1）
        (0x008 => fcr:     ReadWrite<u32, Fcr::Register>),  // FIFO 控制
        (0x00C => lcr:     ReadWrite<u32, Lcr::Register>),  // 线路控制
        (0x010 => mcr:     ReadWrite<u32, Mcr::Register>),  // modem 控制
        (0x014 => lsr:     ReadOnly<u32, Lsr::Register>),   // 线路状态
        (0x018 => @END),
    }
}

/// 寄存器引用。probe 后基址恒定。
fn regs() -> &'static ApUartRegs {
    // SAFETY: BASE 来自本文件常量并与 DT reg 校验一致，指向 AP 域 MMIO；
    // 单 hart 任务上下文串行访问，无别名引用（tock-registers 内部 volatile）。
    unsafe { &*(BASE as *const ApUartRegs) }
}

// SAFETY: 以下两个 fn 均为 32 位对齐的单寄存器 volatile 访问，地址为本
// 文件常量（内核头文件对齐的 APBC/MPMU/pinctrl 布局），probe/send 上下文
// 串行调用。
fn mmio_read(addr: usize) -> u32 {
    unsafe { (addr as *const u32).read_volatile() }
}

fn mmio_write(addr: usize, v: u32) {
    unsafe { (addr as *mut u32).write_volatile(v) }
}

/// DDN 分频器实际输出：`Fout = Fin × den / (2×num)`；num/den 任一为 0
/// 视为无输出（复位态）。den ≤ 8191，Fin ≤ 614.4M，u64 乘法不溢出。
fn ddn_rate(reg: u32, fin_hz: u64) -> u64 {
    let num = ((reg >> 16) & 0x1fff) as u64;
    let den = (reg & 0x1fff) as u64;
    if num == 0 || den == 0 {
        return 0;
    }
    fin_hz * den / (2 * num)
}

/// 自适应选择 func parent：返回 (sel, divisor, 决策描述)。
///
/// 优先级：UART0 的 sel（BROM 给 console 的活源实证，且能用同一 parent
/// 零额外写入）→ 任一活的 ≈14.7456M 源（按实际频率算除数）→ 全死则亲手
/// 编程 SUCCR（死源必无消费者，写它零影响）。
fn choose_clock() -> (u32, u32, &'static str) {
    let apbc0 = mmio_read(APBC_UART0);
    let uart0_sel = (apbc0 >> 4) & 0x7;
    let f1 = ddn_rate(mmio_read(MPMU_SUCCR), 153_600_000);
    let f2 = ddn_rate(mmio_read(MPMU_SUCCR_1), 614_400_000);

    // 理想除数（115200 = f/16/DLL）：四舍五入 + ±2% 误差界。
    fn dll_for(f: u64) -> Option<u32> {
        if f == 0 {
            return None;
        }
        let dll = ((f + 921_600) / 1_843_200) as u64;
        if dll == 0 || dll > 0xffff {
            return None;
        }
        let err = (f as i64 - dll as i64 * 1_843_200).unsigned_abs();
        if err * 50 > f {
            return None; // 误差 > 2%
        }
        Some(dll as u32)
    }

    // 1) 跟随 UART0 的 sel（console 正常出字 ⇒ 该源活着且 ≈14.7456M）。
    if uart0_sel == 1 {
        if let Some(dll) = dll_for(f1) {
            return (1, dll, "follow uart0 sel=1");
        }
    } else if uart0_sel == 2 {
        if let Some(dll) = dll_for(f2) {
            return (2, dll, "follow uart0 sel=2");
        }
    }
    // 2) 任一 DDN 活源在 115200 误差界内。
    if let Some(dll) = dll_for(f1) {
        return (1, dll, "succr alive");
    }
    if let Some(dll) = dll_for(f2) {
        return (2, dll, "succr_1 alive");
    }
    // 3) 全死：编程 SUCCR（sel=1 源此刻必无消费者——console 不在用）。
    mmio_write(MPMU_SUCCR, SUCCR_14P7456M);
    (1, 8, "succr dead -> programmed")
}

/// pad83 MFPR 期望值 = 本节点 pinctrl-0 所写（mux4|EDGE_NONE|PULL_UP|DS8）。
const PAD83: usize = 0xd401_e000 + 83 * 4;
const PAD83_EXPECT: u32 = 0xd044;

/// pad83 自查自愈：AP 引导链每次启动会把 pad83 重 mux 成 func6（i2c3-2-cfg，
/// 见模块文档"AP 侧偷脚"），被改则重写并 warn。APBC/SUCCR 不被碰，故只需
/// 盯 MFPR。warn 反复出现 = 对端持续重夺（届时 AP 侧禁 i2c3）。
fn pad83_heal() {
    let pad = mmio_read(PAD83);
    if pad != PAD83_EXPECT {
        mmio_write(PAD83, PAD83_EXPECT);
        log::warn!("[ap-uart] pad83 stolen {:#x} -> re-mux {:#x}", pad, PAD83_EXPECT);
    }
}

impl Driver for ApUart {
    fn compatible(&self) -> &'static [&'static str] {
        &["spacemit,k3-ap-uart"]
    }

    fn probe(&self, node: &Node<'_>) {
        let reg = node
            .reg()
            .expect("k3 ap-uart: missing reg property")
            .next()
            .expect("k3 ap-uart: empty reg");
        debug_assert_eq!(
            reg.address as usize, BASE,
            "k3 ap-uart: DT base {:#x} != expected {:#x}",
            reg.address, BASE
        );

        // pad mux 已由 boot() 据本节点 pinctrl-0 应用（pad83 mode4 =
        // UART5_TXD = 40pin pin3）。先选 func parent（见 choose_clock），
        // 再开时钟链：APBC 写 mux+双 gate，过 reset 位（1 挂起 → 0 释放，
        // 对齐 reset-spacemit.c 语义）。
        let (sel, divisor, note) = choose_clock();
        let on = (sel << 4) | 0b011; // func+bus gate 开、reset 释放
        mmio_write(APBC_UART5, on | 0b100); // reset 挂起（gate 已开）
        for _ in 0..16 {
            core::hint::spin_loop();
        }
        mmio_write(APBC_UART5, on); // 释放复位

        // IP 配置：DLAB → 除数 → 8N1 → FIFO → UUE/OUT2（同 pxa_uart 序列）。
        let r = regs();
        r.lcr.write(Lcr::DLAB::SET);
        r.thr_rbr.set(divisor & 0xFF); // DLL（offset 0x000 与 thr_rbr 共用）
        r.ier.set((divisor >> 8) & 0xFF); // DLH（offset 0x004 与 ier 共用）
        r.lcr.write(Lcr::WLEN8::SET); // 清 DLAB，设 8N1
        r.fcr.write(Fcr::ENABLE::SET + Fcr::CLR_RX::SET + Fcr::CLR_TX::SET);
        r.ier.write(Ier::UUE::SET);
        r.mcr.write(Mcr::OUT2::SET);

        INITED.store(1, Ordering::Release);
        log::info!(
            "[ap-uart] probed: uart5 @ {:#x} tx=pad83(m4)/40pin pin3, 115200-8N1 poll, clk: {} sel={} dll={}",
            BASE,
            note,
            sel,
            divisor
        );
    }
}

/// send() 单次等待的轮询上限：跨桥 LSR 读 ~1.4µs/笔，20 万笔 ≈ 280ms，
/// 正常一帧（15B ≤ FIFO、TEMT ≈ 1.3ms ≈ 千笔级）远用不到；超限说明波特
/// 率时钟没跑——warn 后返回 0（检测+超时诊断退出，不挂死任务）。
const POLL_LIMIT: u32 = 200_000;

/// 阻塞轮询发送整段缓冲（字节透传，无文本翻译）。
///
/// 先自查自愈 pad mux（AP 侧偷脚，见 [`pad83_heal`]），再逐字节等
/// LSR.THRE 写 THR（FIFO 模式下 THRE≈FIFO 未满），末尾等 LSR.TEMT 确保
/// 整帧移位完成——调用方后续的 sleep 才从帧尾起算。115200 下一帧 15 字节
/// ≈1.3ms 阻塞（task_arm 10ms 节拍内）。调用上下文：任务（单写者——当前
/// 仅 task_arm）；未 probe 时返回 0。
pub fn send(buf: &[u8]) -> usize {
    if INITED.load(Ordering::Acquire) == 0 {
        return 0;
    }
    let r = regs();
    pad83_heal();
    for &b in buf {
        let mut spins = 0u32;
        while !r.lsr.is_set(Lsr::THRE) {
            core::hint::spin_loop();
            spins += 1;
            if spins >= POLL_LIMIT {
                log::warn!("[ap-uart] THRE poll timeout, lsr={:#x}", r.lsr.get());
                return 0;
            }
        }
        r.thr_rbr.set(b as u32);
    }
    let mut spins = 0u32;
    while !r.lsr.is_set(Lsr::TEMT) {
        core::hint::spin_loop();
        spins += 1;
        if spins >= POLL_LIMIT {
            log::warn!("[ap-uart] TEMT poll timeout, lsr={:#x}", r.lsr.get());
            return 0;
        }
    }
    buf.len()
}
