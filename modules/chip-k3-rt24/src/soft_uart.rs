//! 软串口 TX（bit-bang）——R.GPIO 输出线 + AON_TIMER1 counter1 比较中断定拍。
//!
//! 用途：机械臂 ZP10S（ASCII 纯写，115200-8N1）独立 TX 通道，占 40pin
//! **pin40**（网络 I2S0_SDOUT → pad GPIO_113 m2 → **R.GPIO[30]**，经载板
//! U2405 电平转换出 3.3V）。底盘 ESP32 与 console 留在 R_UART0（pin29/32）。
//!
//! ## 架构（2026-08-29 定案，"形态 C"）
//!
//! ```text
//! task_arm（生产者）                     ISR（消费者/定拍）
//! ┌────────────┐   SPSC 字节环   ┌──────────────────────────┐
//! │ send(bytes)│ ──────────────▶ │ AON_TIMER1 c1 match 中断 │
//! └────────────┘  ring [u8;128]  │  每 bit 一拍：翻 PSR/PCR │
//!        │ kick（空闲时）         │  compare += bit_ticks     │
//!        └──────────────────────▶│  （Bresenham，绝对 deadline）│
//!                                └──────────────────────────┘
//! ```
//!
//! - **生产者**：任务上下文把字节入环；机器空闲时 kick（拉 start 位 +
//!   编排首个 compare + 开 match IE），即刻返回——发送全程异步。
//! - **消费者**：AON_TIMER1 counter1（`0xc088_9000`，timer_k3 已开本块
//!   时钟门并验证布局；counter0 是探针时钟，**counter1 空闲归本驱动**）
//!   match 比较中断，每拍推进一位。115200-8N1，每字节 start+8+stop 十拍。
//! - **时基**：AON 计数 2MHz（timer_k3 板上联标）→ 500ns/tick，每 bit
//!   17.36 tick，Bresenham（+41600/115200 进位）平均波特率零误差；
//!   绝对 deadline 递增，单拍迟到不累积。
//!
//! ## 中断延迟预算（为何帧内 mask mailbox）
//!
//! RISC-V handler 不嵌套：帧内若有别的 ISR 在跑，bit 沿即迟到。UART 接收
//! 容忍半位（115200 下 4.34µs）的单次偏移。帧内竞争者：
//! - **mailbox IRQ 69**（排空 FIFO，µs×N 条）——超预算，**帧内经 PLIC
//!   disable 单源 mask**（电平源，mask 期间挂起不丢，帧尾恢复即投递）。
//!   代价：恰在帧内到达的 AP→RP 消息延迟 ≤ 帧时长（16 字节 ≈ 1.39ms）；
//!   调度器 tick / P1 任务不受影响（这正是不整帧关中断的原因）。
//! - MachineTimer（调度 tick，短）——不 mask（mask 了任务唤醒就退化），
//!   迟到 ≤1µs 在半位容忍内，且下一拍绝对 deadline 自动对齐。
//!
//! ## 待板验假设（bring-up 首验项）
//!
//! 1. **PLIC 中断号 9**：手册 7.2 int_src[8/9/10] = timer_1/2/3_irq 为
//!    AON_TIMER1 块三计数器（信号名 1 起始 → counter#1 = int_src[9]）。
//!    若不响，按 8/10 各试一次（`TIMER_IRQ` 常量一处改）。
//! 2. **R_GPIO 偏移**：手册 16.8 载 PLR/PDR/PSR/PCR = +0x0/+0x4/+0x8/+0xC。
//!    R.GPIO[30] < 32 落 port0，四偏移直接用、无端口间距歧义（刻意不选
//!    R.GPIO[35]@pin11 的原因——它落 port1，端口块间距手册未载）。
//! 3. AON counter1 与 counter0 同频 2MHz（同块同源，timer_k3 联标过 c0）。
//!
//! ## 约束
//!
//! - **仅 TX**（ZP10S 无应答）。RX 需要 R_GPIO 边沿中断（int_src[49/50]）
//!   + 3x 采样，另立驱动。
//! - 单生产者（task_arm）/消费在 ISR 与 kick 两处但由 RUNNING 标志互斥，
//!   单 hart 下无并发（见各 SAFETY 注释）。
//! - 帧内 mailbox mask 是本驱动对系统时序的唯一侵入，见上。

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicUsize, Ordering};

use fdt_parser::Node;
use platform::device::Driver;
use tock_registers::interfaces::{Readable, Writeable};
use tock_registers::register_structs;
use tock_registers::registers::{ReadOnly, ReadWrite, WriteOnly};

// ── 常量 ─────────────────────────────────────────────────────────────────

/// R_GPIO（手册名 AUD_GPIO）基址。
const RGPIO_BASE: usize = 0xc088_9400;
/// AON_TIMER1 块基址（timer_k3 管 counter0，本驱动用 counter1）。
const AON_TMR1_BASE: usize = 0xc088_9000;

/// 输出线：R.GPIO[30]（<32 → port0，bit30）。pad = GPIO_113 m2 → 40pin pin40。
const LINE: u32 = 30;
/// 波特率。
const BAUD: u32 = 115_200;
/// AON 计数频率（timer_k3 板上联标 2,000,133Hz，取整）。
const TICK_HZ: u32 = 2_000_000;

/// 编译期锚定 Bresenham 常量与频率/波特率的关系（step_bit 的 17/18、
/// 41_600/115_200 换频率/波特率时须同步改）。
const _: () = assert!(TICK_HZ == 115_200 * 17 + 41_600 && BAUD == 115_200);

/// 本驱动 PLIC 中断号（手册 7.2 int_src[9] = timer_2_irq = 本块 counter#1；
/// 板验假设，见模块文档）。
const TIMER_IRQ: u32 = 9;
/// 帧内 mask 的中断源：mailbox4（int_src[69]）。唯一超 bit 预算的 ISR。
const MAILBOX_IRQ: u32 = 69;

/// 字节环容量（一条 ZP10S 指令 16 字节，容量 ≥ 6 条在途指令）。
const RING_CAP: usize = 128;

// ── 寄存器 ───────────────────────────────────────────────────────────────

register_structs! {
    /// R_GPIO port0 寄存器组（手册 16.8：PLR/PDR/PSR/PCR = +0/+4/+8/+C）。
    /// 仅 port0（LINE < 32），端口块间距手册未载、刻意不涉及。
    pub RgpioRegs {
        (0x00 => plr: ReadOnly<u32>),
        (0x04 => pdr: ReadWrite<u32>),
        (0x08 => psr: WriteOnly<u32>),
        (0x0c => pcr: WriteOnly<u32>),
        (0x10 => @END),
    }
}

register_structs! {
    /// AON_TIMER1 块内本驱动涉及的寄存器（手册 16.3.4；CER/CMR/PLCR/TCR
    /// 偏移与 timer_k3 板上已验证的 MMP 布局一致，Match/IER/ICR 同表；
    /// 间隙以 _reserved 幻影字段占位）。
    pub AonTmr1Regs {
        (0x00 => cer: ReadWrite<u32>),      // bit n = counter n 计数使能
        (0x04 => cmr: ReadWrite<u32>),      // bit n = 1 → counter n 自由运行
        (0x08 => _reserved0),
        (0x20 => t1_m0: ReadWrite<u32>),    // TMR_T1_M0：counter#1 match 比较器 0
        (0x24 => _reserved1),
        (0x54 => plcr1: ReadWrite<u32>),    // TPLCR1：MCS=0 自由运行不重载
        (0x58 => _reserved2),
        (0x64 => ier1: ReadWrite<u32>),     // counter#1 中断使能（bit0=match0）
        (0x68 => _reserved3),
        (0x74 => icr1: WriteOnly<u32>),     // counter#1 中断清除（bit0，写 1 清电平）
        (0x78 => _reserved4),
        (0x84 => tsr1: ReadOnly<u32>),      // counter#1 match 状态
        (0x88 => _reserved5),
        (0x94 => tcr1: ReadOnly<u32>),      // counter#1 计数值（32 位 @2MHz）
        (0x98 => @END),
    }
}

/// R_GPIO port0 寄存器引用。probe 断言过 DT reg 后恒有效。
fn rgpio() -> &'static RgpioRegs {
    // SAFETY: 基址为固定 RCPU 本地 MMIO 块（手册 6.2），probe 后只读/只写
    // 本驱动拥有的 LINE 位；单 hart 串行访问。
    unsafe { &*(RGPIO_BASE as *const RgpioRegs) }
}

/// AON_TIMER1 寄存器引用。
fn aon() -> &'static AonTmr1Regs {
    // SAFETY: 固定 RCPU 本地 MMIO 块；CER/CMR 的 bit0 归 timer_k3（counter0），
    // 本驱动只动 bit1（counter1），RMW 无冲突；单 hart 串行访问。
    unsafe { &*(AON_TMR1_BASE as *const AonTmr1Regs) }
}

// ── 发送状态（单 hart 可见性论证见各注释）────────────────────────────────

/// 机器状态：0 = 空闲（kick 可启动），1 = 发送中（ISR 消费中）。
/// 生产者在关 IE 与 RUNNING=1 之间无 ISR 窗口（IE 未开），单写者发布。
static RUNNING: AtomicU8 = AtomicU8::new(0);
/// 当前字节。kick（任务）写 → 开 IE → ISR 读：同核程序序 + MMIO 强序，
/// Relaxed 即安全（无跨核读者）。
static CUR: AtomicU8 = AtomicU8::new(0);
/// 当前位相：0=start，1..=8=data(LSB 先)，9=stop，10=字节边界（仅 ISR 写）。
static PHASE: AtomicU8 = AtomicU8::new(0);
/// Bresenham 余数（ISR/kick 独占写）。
static ACC: AtomicU32 = AtomicU32::new(0);
/// 下一拍绝对 compare deadline（tick，wrapping 语义；ISR/kick 独占写）。
static DL: AtomicU32 = AtomicU32::new(0);
/// 完成计数：转入空闲的次数（≈ 发出的指令条数，诊断用）。
static FRAMES: AtomicU32 = AtomicU32::new(0);

// ── SPSC 字节环 ──────────────────────────────────────────────────────────

/// 字节环：单生产者 = 任务（send），消费 = kick（任务，仅 RUNNING==0 时）
/// 与 ISR（仅 RUNNING==1 时）——两消费者被 RUNNING 标志互斥，单 hart 下
/// 任何时刻至多一方访问 head。
struct TxRing {
    slots: UnsafeCell<[u8; RING_CAP]>,
    head: AtomicUsize, // 消费者推进
    tail: AtomicUsize, // 生产者推进
    dropped: AtomicU32,
}

// SAFETY：单 hart + SPSC 索引 Release/Acquire 发布序（同 robot.rs ArmRing
// 论证）；消费者侧 kick/ISR 互斥由 RUNNING 保证。
unsafe impl Sync for TxRing {}

static RING: TxRing = TxRing {
    slots: UnsafeCell::new([0; RING_CAP]),
    head: AtomicUsize::new(0),
    tail: AtomicUsize::new(0),
    dropped: AtomicU32::new(0),
};

impl TxRing {
    fn push(&self, b: u8) -> bool {
        let t = self.tail.load(Ordering::Relaxed);
        let h = self.head.load(Ordering::Acquire);
        if t.wrapping_sub(h) >= RING_CAP {
            // 单写者（生产者任务），load+store 即可（无 CAS 依赖）。
            let d = self.dropped.load(Ordering::Relaxed) + 1;
            self.dropped.store(d, Ordering::Relaxed);
            return false;
        }
        // SAFETY：单生产者，tail 未发布，该槽独占。
        unsafe { (*self.slots.get())[t % RING_CAP] = b };
        self.tail.store(t.wrapping_add(1), Ordering::Release);
        true
    }

    fn pop(&self) -> Option<u8> {
        let h = self.head.load(Ordering::Relaxed);
        let t = self.tail.load(Ordering::Acquire);
        if h == t {
            return None;
        }
        // SAFETY：tail 已推进（Acquire），槽数据已发布且消费者独占。
        let b = unsafe { (*self.slots.get())[h % RING_CAP] };
        self.head.store(h.wrapping_add(1), Ordering::Release);
        Some(b)
    }

    fn is_empty(&self) -> bool {
        self.head.load(Ordering::Acquire) == self.tail.load(Ordering::Acquire)
    }
}

// ── 位时序 ───────────────────────────────────────────────────────────────

/// Bresenham 一步：deadline 前进一个 bit（17/18 tick 交替，平均精确）。
/// 2_000_000 = 115_200×17 + 41_600，余数每步 +41_600、满 115_200 进一位
/// （+18 tick）。32 位 wrapping：环回 35.8min 周期内 deadline 恒近距，
/// wrapping 减法语义正确。
#[inline]
fn step_bit(dl: u32, acc: u32) -> (u32, u32) {
    let acc2 = acc + 41_600;
    if acc2 >= 115_200 {
        (dl.wrapping_add(18), acc2 - 115_200)
    } else {
        (dl.wrapping_add(17), acc2)
    }
}

/// 线电平：1 → PSR 置位，0 → PCR 清零（单 bit 写，无 RMW）。
#[inline]
fn set_line(high: bool) {
    let bit = 1u32 << LINE;
    if high {
        rgpio().psr.set(bit);
    } else {
        rgpio().pcr.set(bit);
    }
}

/// 写 compare 并防等值漏拍：若新 deadline 已到/紧邻当前计数（写入生效
/// 前计数可能越过），推到 now+2——误差 ≤1 tick（500ns），下一拍绝对
/// deadline 自动吸收，不累积。
fn write_match_guarded(dl: u32) {
    let now = aon().tcr1.get();
    // wrapping 语义：dl 在 now 之前（或相等）→ 差值绕成大数。
    let mut dl = dl;
    let ahead = dl.wrapping_sub(now);
    if ahead < 2 || ahead > 0x8000_0000 {
        dl = now.wrapping_add(2);
    }
    aon().t1_m0.set(dl);
}

// ── 对外 API（任务侧）───────────────────────────────────────────────────

/// 异步发送：字节入环（满则截断），机器空闲时 kick 启动。返回接受数。
///
/// 调用上下文：任务（当前仅 task_arm——单生产者假设）。即刻返回，
/// 波形由 ISR 后台生成；完成情况查 [`idle`] / [`stats`]。
pub fn send(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let mut n = 0;
    for &b in bytes {
        if !RING.push(b) {
            break;
        }
        n += 1;
    }
    if RUNNING.load(Ordering::Acquire) == 0 {
        // 启动：先 mask mailbox（帧内抖动预算，见模块文档），再 kick。
        // mask 与 kick 之间无本驱动 ISR（IE 关、RUNNING=0），单 hart 下
        // 原子有效。
        platform::driver::intctl().disable_irq(MAILBOX_IRQ);
        kick();
    }
    n
}

/// 空闲 = 不在发送且环已清空。
pub fn idle() -> bool {
    RUNNING.load(Ordering::Acquire) == 0 && RING.is_empty()
}

/// 诊断：(完成轮数, 环溢出丢弃字节数)。
pub fn stats() -> (u32, u32) {
    (
        FRAMES.load(Ordering::Relaxed),
        RING.dropped.load(Ordering::Relaxed),
    )
}

/// 启动机器（调用前提：RUNNING==0，IE 关——无 ISR 并发）。
fn kick() {
    let intctl = platform::driver::intctl();
    let Some(b) = RING.pop() else {
        // 防御：环空（send 已保证非空，理论不可达）。恢复 mailbox。
        intctl.enable_irq(MAILBOX_IRQ);
        return;
    };
    CUR.store(b, Ordering::Relaxed);
    PHASE.store(0, Ordering::Relaxed);
    // start 位立即落线，首拍 deadline 从当下起算。
    set_line(false);
    let now = aon().tcr1.get();
    let (dl, acc) = step_bit(now, 0);
    DL.store(dl, Ordering::Relaxed);
    ACC.store(acc, Ordering::Relaxed);
    write_match_guarded(dl);
    let aon = aon();
    aon.icr1.set(1); // 清可能残留的 match0 电平
    // RUNNING 先于 IE 置位：若首拍 deadline 极近、IE 开后立即触发，ISR 读
    // 到的 RUNNING 必须已是 1（否则防御分支关 IE，帧永不动身）。
    RUNNING.store(1, Ordering::Release);
    aon.ier1.set(aon.ier1.get() | 1); // 开 match0 中断（此后 ISR 接管）
}

// ── ISR ─────────────────────────────────────────────────────────────────

/// counter#1 match 中断：推进一位。经 platform::irq 分发表调用
/// （claim/complete 由框架包办；电平源须先清 ICR 再 complete）。
unsafe fn on_timer_irq(_irq: u32) {
    let aon = aon();
    aon.icr1.set(1); // 清电平中断源（迟于 complete 会死循环重入）

    if RUNNING.load(Ordering::Relaxed) == 0 {
        // 迟到尾巴（帧结束关 IE 与本拍之间不应发生，防御）。
        aon.ier1.set(aon.ier1.get() & !1);
        return;
    }

    let phase = PHASE.load(Ordering::Relaxed) + 1;
    if phase <= 8 {
        // 数据位 D0..D7（LSB 在前）。
        let bit = (CUR.load(Ordering::Relaxed) >> (phase - 1)) & 1;
        set_line(bit == 1);
        PHASE.store(phase, Ordering::Relaxed);
    } else if phase == 9 {
        // stop 位（线回高；此后若无后续字节，高电平即空闲态）。
        set_line(true);
        PHASE.store(phase, Ordering::Relaxed);
    } else {
        // phase == 10：字节边界。有后续字节则立即落下一字节的 start 位，
        // 否则收尾（单 hart：ISR 与任务不并发，环空即终态——send() 在
        // 帧内入环的字节会在更早的字节边界被本处取走，无滞留）。
        match RING.pop() {
            Some(b) => {
                CUR.store(b, Ordering::Relaxed);
                PHASE.store(0, Ordering::Relaxed);
                set_line(false);
            }
            None => {
                aon.ier1.set(aon.ier1.get() & !1);
                RUNNING.store(0, Ordering::Release);
                FRAMES.store(FRAMES.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
                // 帧结束：恢复 mailbox（mask 期间的电平挂起此刻投递）。
                platform::driver::intctl().enable_irq(MAILBOX_IRQ);
                return;
            }
        }
    }

    // 排下一拍。
    let (dl, acc) = step_bit(DL.load(Ordering::Relaxed), ACC.load(Ordering::Relaxed));
    DL.store(dl, Ordering::Relaxed);
    ACC.store(acc, Ordering::Relaxed);
    write_match_guarded(dl);
}

// ── Driver（DT probe）───────────────────────────────────────────────────

/// 软串口 driver 单例（零大小）。
pub struct SoftUart;

/// 全局单例，供 K3_DRIVERS 注册。
pub static INSTANCE: SoftUart = SoftUart;

impl Driver for SoftUart {
    fn compatible(&self) -> &'static [&'static str] {
        &["spacemit,k3-soft-uart"]
    }

    fn probe(&self, node: &Node<'_>) {
        let reg = node
            .reg()
            .expect("k3 soft-uart: missing reg property")
            .next()
            .expect("k3 soft-uart: empty reg");
        debug_assert_eq!(
            reg.address as usize, RGPIO_BASE,
            "k3 soft-uart: DT base {:#x} != expected {:#x}",
            reg.address, RGPIO_BASE
        );

        // pad mux 已由 boot() 据本节点 pinctrl-0 应用（GPIO_113 m2 =
        // R.GPIO[30] → 40pin pin40）。R_GPIO：输出方向 + 空闲高电平。
        let bit = 1u32 << LINE;
        let rg = rgpio();
        rg.pdr.set(rg.pdr.get() | bit);
        rg.psr.set(bit);

        // AON_TIMER1 counter#1：自由运行（CMR bit1=1、PLCR1 MCS=0，不
        // preload/重载——每拍由 ISR 重写 match 绝对 deadline），开始计数
        // （CER bit1；本块时钟门已由 timer_k3::init 在 Board::init 开启）。
        // match0 IE 保持关（kick 时才开）。
        let aon = aon();
        aon.cmr.set(aon.cmr.get() | (1 << 1));
        aon.plcr1.set(0);
        aon.cer.set(aon.cer.get() | (1 << 1));
        aon.icr1.set(1);
        aon.ier1.set(aon.ier1.get() & !1);

        // 中断挂接：注册 handler + PLIC 使能（priority=2 高于 mailbox 的
        // 1——两源同 pending 时 bit 沿先claim）。PLIC 已 probe：本节点在
        // DTS 中排在 intc@e0000000 之后（DFS 先序）。
        platform::irq::register_irq(TIMER_IRQ, on_timer_irq);
        let intctl = platform::driver::intctl();
        intctl.set_priority(TIMER_IRQ, 2);
        intctl.enable_irq(TIMER_IRQ);

        log::info!(
            "[soft-uart] probed: rgpio line {} @pin40, {}-8N1, aon_tmr1 c1 match irq {}",
            LINE,
            BAUD,
            TIMER_IRQ
        );
    }
}
