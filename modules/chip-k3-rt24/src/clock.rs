//! K3 RT24 rcpu1 时钟链 + pinmux + SPL 握手常量与初始化。
//!
//! 移植自 esos `os1_rcpu/baremetal/main.c`（已验证），对应
//! 设计文档 §1.5 的步骤 1-4。

// 步骤1：SPL 启动握手。k3_rproc_start() 唤醒 rcpu1 后死等
// CORE0_BOOT_ENTRY_LO 非 0（~6s）。rcpu1 必须回写 *CORE0* 寄存器
// （交叉规则：rcpu0 写 CORE1，rcpu1 写 CORE0）。必须最先做，否则 AP 卡 6s。
// 见 drivers/remoteproc/k3-rproc.c k3_rproc_start() case 1。
pub const RCPU_CORE0_BOOT_ENTRY_LO: usize = 0xc088_007c;

// 步骤2：RCPU_UART_NM_CLK_14M_CTRL（0xc0880000+0x3C，BASE_TYPE_RCPU reg 3）。
// 上游 DDN 分频器，产生 ruart_14(~14.48MHz)——UART0 末端 mux 的输入 0。
//   bit[31]    gate (1=使能分频器)
//   bit[30:16] den  (0x64，来自 ruart_14_tbl)
//   bit[15:0]  num  (0x6a1，来自 ruart_14_tbl)
// 不置 bit31 则 ruart_14 被关，UART0 即便自身 gate 开了也无时钟。
// 见 ccu-spacemit-k3.c:422 ruart_14_tbl / ruart_14。
pub const RUART_14_CLK_CTRL: usize = 0xc088_003c;
pub const RUART_14_GATE_BIT: u32 = 1u32 << 31;

// 步骤3：RCPU1_UART0_CLK_RST（0xc0881f00，CCU reg-block index 4 offset 0）。
//   bit[1:0]  gate (0x3=使能)
//   bit[5:4]  mux  (0=ruart_14 ~14.48MHz)
//   bit[18:8] div  (0=/1)
// 见 ccu-spacemit-k3.c:442 ruart0_clk。
pub const UART0_CLK_RST: usize = 0xc088_1f00;
pub const UART0_CLK_RST_ENABLE: u32 = 0x0000_0003;

// 步骤4：pinmux。ruart0_3_cfg 用 "pinctrl-single,pins"（offset/value 对，
// 每 pin 一个寄存器），故 GPIO_n 寄存器 = PINCTRL_BASE + n*4。
//   GPIO_122 (0x1e8) -> UART0_TX,  GPIO_123 (0x1ec) -> UART0_RX
// 值 = MUX_MODE4 | EDGE_NONE | PULL_UP | PAD_DS8 = 0xD044（per ruart0_3_cfg）。
pub const PINCTRL_BASE: usize = 0xd401_e000;
pub const UART0_TX_PIN: usize = 122;
pub const UART0_RX_PIN: usize = 123;
pub const UART0_PIN_VAL: u32 = 0xD044;

#[inline(always)]
pub(crate) fn write32(addr: usize, val: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, val) };
}

#[inline(always)]
pub(crate) fn read32(addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

/// 握手回写 + 上游 ruart_14 gate + UART0 末端 gate + pinmux（步骤 1-4）。
///
/// `Chip::board_init()` 第一步。握手必须最先（解锁 AP 的 6s 轮询）。
pub fn early_init() {
    // 1. SPL 握手回写（最先，解锁 AP）
    write32(RCPU_CORE0_BOOT_ENTRY_LO, 1);

    // 2. 使能上游 ruart_14 DDN gate（保留 num/den，只置 bit31）
    let v = read32(RUART_14_CLK_CTRL) | RUART_14_GATE_BIT;
    write32(RUART_14_CLK_CTRL, v);

    // 3. 使能 UART0 末端 gate（gate=0x3、mux=ruart_14、div=/1）
    write32(UART0_CLK_RST, UART0_CLK_RST_ENABLE);

    // 4. pinmux：GPIO_122=TX, GPIO_123=RX
    write32(PINCTRL_BASE + UART0_TX_PIN * 4, UART0_PIN_VAL);
    write32(PINCTRL_BASE + UART0_RX_PIN * 4, UART0_PIN_VAL);
}
