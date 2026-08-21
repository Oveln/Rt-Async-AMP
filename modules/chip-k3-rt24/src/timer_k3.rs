//! K3 本地计时源（RCPU AON 域 AON_TIMER1 @0xc0889000，counter0）。
//!
//! **选型原则（2026-08-22 延迟战役定案，REPORT.md）**：RT24 的高延迟外设
//! 访问根因是跨互连——mtime（SysTimer @0xe4000000）虽是 rcpu1 功能上的
//! 计时器，但物理上是全 SoC 共享块（mtimecmp 按 hart<<27 分区），挂在
//! 簇间互连上，RT24 本地总线上的外设只有 0xc088_xxxx AON 域这批。
//! mtime 间隔读 24.5µs/笔、AP 域 counter1 间隔读 ~13µs/笔（均已板上
//! 证伪，回滚 6f4ec9d）——替代者必须出自本地域。AON_TIMER1~4
//! （0xc0889000 / 0xc088c800 / c900 / ca00）是文档载明的 RCPU 专属
//! 计时器（k3_docs 06_address_map.md §6.2），本模块用 AON_TIMER1。
//!
//! 时钟/复位：`MCU_TIMER1_CLK_RST @0xc088c04c`（17_clock_reset.md
//! RCPU_PMU，基址 0xc088c000）：
//! - bit0 TIMER_SW_RSTN —— 复位值 0 = **保持复位**，写 1 释放（低有效
//!   语义；与 APBC 的 bit2 高有效复位相反——2026-08-21 首次尝试栽在
//!   这里：开门后又写 0x3 把 bit0 清零，外设被按回复位态读全 0）；
//! - bit1 TIMER_FCLK_EN / bit2 TIMER_PCLK_EN —— 双时钟门；
//! - bit[5:4] TIMER_FCLK_SEL —— 0=25.6MHz / 1=12.8MHz / 2=3.2MHz。
//! 开门写 `keep|0x7` 一笔到位（mux 位 [18:8] 保留读回值，默认 SEL=0）。
//!
//! 寄存器布局**文档未载**，按同血脉 APBC OS timer（timer-k1x.c，MMP
//! 血统）假设：CER=+0x00（bit n=counter n 使能）/ CMR=+0x04（bit n=
//! 自由运行）/ PLCR(n)=+0x50+(n<<2) / CR(n)=+0x90+(n<<2)——由探针
//! （intercom TMR_RT_ON 组）板上验证计数递增后方可采信。
//!
//! **状态：探针验证阶段**——生产计时链仍走 clint mtime；间隔读单价
//! 经 TMR_AON_GAPPED 实测且优于 mtime 前不迁移（counter1 教训：
//! 背靠背热读价不代表间隔读价，必须实测 mark 间隔路径）。
//!
//! 使用边界：
//! - 32 位 @25.6MHz 回卷 167s——只做 µs~秒级**区间差**（wrapping 语义），
//!   不做绝对时刻跨回卷比较；mtimecmp 睡眠唤醒截止时间仍走 SysTimer
//!   （`clint_k3`）。
//! - FCLK 由 AON 域电源供给（非 AP PLL 派生），AP 低功耗不影响。
//! - 若 AP 侧未来声明使用 AON_TIMER1（esOS/StarryOS 现均未用，dts
//!   disabled），需跨核协调。

const AON_TMR1_BASE: usize = 0xc088_9000;
const MCU_TIMER1_CLK_RST: usize = 0xc088_c04c;

/// 计数频率标称值（FCLK_SEL=0 = 25.6MHz；实测值由 TMR_AON_CAL 板上
/// 联标校准——区间差测量内部自洽，无跨钟对齐需求）。
pub const FREQ_HZ: u32 = 25_600_000;

/// counter0 计数值寄存器偏移（CR(0) = 0x90，布局假设见模块头）。
const TMR_CR0: usize = 0x90;
/// counter0 预载控制寄存器偏移（PLCR(0) = 0x50；0 = 自由运行）。
const TMR_PLCR0: usize = 0x50;

/// 开时钟门 + 释放软复位 + counter0 自由运行化。
///
/// `Board::init` 调用（早于 DT boot，无依赖；与 `spl_handshake` 同级
/// 直连装配，理由：门在 AON_PMU 区，不在本 crate CCU 驱动的管理范围，
/// 副作用边界 = 本块内）。CER 写后回读校验重试 ×8（门刚开的边界窗口
/// 内寄存器写可能未及生效，上游 timer-k1x.c timer_write_check 同款防护）。
pub fn init() {
    // SAFETY: 各访问均为本模块文档论证过的目标寄存器纯 MMIO 读写。
    unsafe {
        let gate = (MCU_TIMER1_CLK_RST as *const u32).read_volatile();
        // 保留 bit[18:3]（DIV/SEL/其他），低 3 位 = RSTN|FCLK|PCLK 全开，
        // 一笔到位——不再有第二笔写（上一版在此清了 bit0 重新按住复位）
        (MCU_TIMER1_CLK_RST as *mut u32).write_volatile(gate | 0x7);
        // counter0 自由运行：CMR bit0 + PLCR0=0 + CER bit0（回读校验）
        let cmr = (AON_TMR1_BASE + 0x04) as *mut u32;
        cmr.write_volatile(cmr.read_volatile() | 1);
        ((AON_TMR1_BASE + TMR_PLCR0) as *mut u32).write_volatile(0);
        let cer = AON_TMR1_BASE as *mut u32;
        let mut ok = false;
        for _ in 0..8 {
            cer.write_volatile(cer.read_volatile() | 1);
            if cer.read_volatile() & 1 != 0 {
                ok = true;
                break;
            }
        }
        if !ok {
            log::error!("[timer_k3] AON_TIMER1 使能失败（门/软复位异常，布局假设待探针核实）");
        }
    }
}

/// 读 counter0（32 位 @25.6MHz，回卷 167s——区间差用 wrapping 语义）。
#[inline]
pub fn now() -> u64 {
    // SAFETY: 纯 MMIO 读，无副作用。
    unsafe { ((AON_TMR1_BASE + TMR_CR0) as *const u32).read_volatile() as u64 }
}

/// tick → ns（标称频率，无额外访存；精确换算用探针联标值）。
#[inline]
pub fn ticks_to_ns(t: u64) -> u64 {
    t * 1_000_000_000 / FREQ_HZ as u64
}
