//! K3 RT24 rcpu1 UART0 驱动（PXA 派生 UART，`spacemit,pxa-uart0`）。
//!
//! 移植自 esos `os1_rcpu/baremetal/main.c` + `pxa_uart.h`/`pxa_uart_initialize()`，
//! 对应设计文档 §1.5 的步骤 5-7。

pub const UART0_BASE: usize = 0xc088_1000;

// NS16550 兼容寄存器偏移
const THR: usize = 0x000; // 发送保持
const IER: usize = 0x004; // 中断使能（DLAB=0 时）；DLH（DLAB=1 时）
const FCR: usize = 0x008; // FIFO 控制
const LCR: usize = 0x00C; // 线路控制
const MCR: usize = 0x010; // modem 控制
const LSR: usize = 0x014; // 线路状态
const DLL: usize = 0x000; // 除数低（DLAB=1）
const DLH: usize = 0x004; // 除数高（DLAB=1）

// PXA-uart 专属使能位——不置 UUE，整个 UART 单元 disabled，THR 写入不出波形。
// 见 esos pxa_uart.h:35,52 与 pxa_uart_initialize()。
const UART_IER_UUE: u32 = 0x40; // UART Unit Enable
const UART_MCR_OUT2: u32 = 0x08;

const LCR_DLAB: u32 = 0x80; // 设波特率时置
const LCR_8N1: u32 = 0x03; // 8 数据位、1 停止位、无校验
const FCR_ENABLE_CLEAR: u32 = 0x07; // 使能 FIFO + 清 RX/TX

const LSR_THR_EMPTY: u32 = 0x20; // THR 空（可写）

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

/// 配置波特率/FIFO/帧格式 + UUE 单元使能（步骤 5-6）。
///
/// `_board_init()` 第二步调用（在 `clock::early_init()` 之后）。
/// 步骤6（UUE）是 PXA-uart 最易漏的关键位。
pub fn init() {
    // 5. 波特率：设 DLAB → DLL/DLH → 清 DLAB 设 8N1 → FCR
    write32(UART0_BASE + LCR, LCR_DLAB);
    write32(UART0_BASE + DLL, DIVISOR & 0xFF);
    write32(UART0_BASE + DLH, (DIVISOR >> 8) & 0xFF);
    write32(UART0_BASE + LCR, LCR_8N1);
    write32(UART0_BASE + FCR, FCR_ENABLE_CLEAR);

    // 6. UUE 单元使能（PXA 专属，⭐ 最易漏）
    write32(UART0_BASE + IER, UART_IER_UUE);
    write32(UART0_BASE + MCR, UART_MCR_OUT2);
}

/// 轮询 LSR bit5（THR 空）后写 THR（步骤 7）。
pub fn putc(c: u8) {
    while read32(UART0_BASE + LSR) & LSR_THR_EMPTY == 0 {}
    write32(UART0_BASE + THR, c as u32);
}
