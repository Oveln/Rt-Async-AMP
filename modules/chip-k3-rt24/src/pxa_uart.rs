//! K3 RT24 R_UART0 驱动（PXA 派生 UART，`spacemit,pxa-uart0`）。
//!
//! 实现 platform 的 [`Driver`]（DT probe 入口）+ [`Serial`]（console）。
//! 初始化序列移植自 esos `pxa_uart_initialize()` + 本仓库 `uart.rs`（已点亮）。
//!
//! ## 与时钟/pinmux 的分工
//!
//! UART 的上游时钟链（ruart_14 DDN gate `0xc088003c` bit31）+ 末端 gate
//!（`0xc0881f00`=0x3）+ pinmux（GPIO_122/123）已在 `clock::early_init()` 里
//! 做完（`Board::init` 第一步，先于本 probe）。本 probe **只做波特率/FIFO/
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

/// probe 写入的 MMIO 基址。0 表示尚未 probe。
static BASE: AtomicUsize = AtomicUsize::new(0);

// ── NS16550/PXA 兼容寄存器偏移（stride = 4）──────────────────────
const THR: usize = 0x000; // 发送保持（读为 RBR）
const IER: usize = 0x004; // 中断使能（DLAB=0）；DLH（DLAB=1）
const FCR: usize = 0x008; // FIFO 控制
const LCR: usize = 0x00C; // 线路控制
const MCR: usize = 0x010; // modem 控制
const LSR: usize = 0x014; // 线路状态
const DLL: usize = 0x000; // 除数低（DLAB=1）
const DLH: usize = 0x004; // 除数高（DLAB=1）

// PXA-uart 专属使能位——不置 UUE，整个 UART 单元 disabled，THR 写入不出波形。
const UART_IER_UUE: u32 = 0x40; // UART Unit Enable
const UART_MCR_OUT2: u32 = 0x08;

const LCR_DLAB: u32 = 0x80; // 设波特率时置
const LCR_8N1: u32 = 0x03; // 8 数据位、1 停止位、无校验
const FCR_ENABLE_CLEAR: u32 = 0x07; // 使能 FIFO + 清 RX/TX

const LSR_THR_EMPTY: u32 = 0x20; // THR 空（可写）
const LSR_DATA_READY: u32 = 0x01; // RX 数据就绪（可读）

// 14.48MHz / (16 * 115200) ≈ 8
const DIVISOR: u32 = 8;

#[inline(always)]
fn write32(addr: usize, val: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, val) };
}

#[inline(always)]
fn read32(addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

/// 轮询 LSR bit5（THR 空）后写 THR（与旧 uart.rs::putc 一致）。
#[inline(always)]
fn putc(base: usize, c: u8) {
    while read32(base + LSR) & LSR_THR_EMPTY == 0 {
        core::hint::spin_loop();
    }
    write32(base + THR, c as u32);
}

/// PXA-UART 单例（零大小）。
pub struct PxaUart;

/// 全局单例，供 probe 注册进 registry。
pub static INSTANCE: PxaUart = PxaUart;

impl Serial for PxaUart {
    fn write(&self, buf: &[u8]) {
        let base = BASE.load(Ordering::Acquire);
        for &b in buf {
            // 串口需 \r\n：把 \n 转成 \r\n（与旧 uart.rs::put_str 行为一致），
            // 否则终端按 LF 解释会呈阶梯换行。
            if b == b'\n' {
                putc(base, b'\r');
            }
            putc(base, b);
        }
    }

    fn read(&self) -> Option<u8> {
        // 阶段1：轮询读（不接 RX 中断）。SerialRx 异步路径留后续。
        let base = BASE.load(Ordering::Acquire);
        if read32(base + LSR) & LSR_DATA_READY == 0 {
            return None;
        }
        Some(read32(base + THR) as u8)
    }

    fn has_data(&self) -> bool {
        let base = BASE.load(Ordering::Acquire);
        read32(base + LSR) & LSR_DATA_READY != 0
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
        // 时钟链/pinmux 已在 clock::early_init() 完成，此处只配 IP 自身。
        write32(base + LCR, LCR_DLAB);
        write32(base + DLL, DIVISOR & 0xFF);
        write32(base + DLH, (DIVISOR >> 8) & 0xFF);
        write32(base + LCR, LCR_8N1);
        write32(base + FCR, FCR_ENABLE_CLEAR);

        // UUE 单元使能（PXA 专属，⭐ 最易漏）+ MCR OUT2。
        write32(base + IER, UART_IER_UUE);
        write32(base + MCR, UART_MCR_OUT2);

        // 登记进多实例注册表；默认 console 由 boot() 的 try_derive_console
        // 据 chosen.stdout-path 在首个 Serial probe 后派生（不再由 probe 自命）。
        // 本 probe 内的 log::info! 因 console 尚未派生会经 try_console 静默丢弃；
        // 后续节点（如 PLIC）probe 时 console 已就绪，日志正常输出。
        platform::driver::SERIALS.register(&INSTANCE);

        log::info!("K3 R_UART0 probed: base={:#x}, 115200-8N1", base);
    }
}
