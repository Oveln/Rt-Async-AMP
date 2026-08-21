//! K3 soc-timer 计时源（AP 域 0xd4016000 空闲 counter1）。
//!
//! 动机（2026-08-21 延迟战役定案，详见主仓 docs/latency-report/REPORT.md）：
//! SysTimer mtime 读在非背靠背间隔后有 **24.5µs/笔** 的跨时钟域重锁税
//! （热循环 106ns → 231 倍），生产计时链（stamp mark / step SVC 统计 /
//! ISR 时间戳 / 弹性窗时长）每条消息 2-6 笔全是冷读，实测占 D1 rtt
//! ~35µs。本计数器经板上三面探针验证：**12.8MHz 自由运行（mux=0 默认）、
//! 读恒定 277ns、无冷读税**。
//!
//! 资源：AP 域 soc-timer 块 0xd4016000。StarryOS 不使用该块（其
//! clockevent 走 riscv-timer/Sstc，实测 CER=0 整块独占，无共享竞态）。
//! 时钟门在 APBC `APBC_TIMERS1_CLK_RST @0xd4015044`：bit0=bus gate /
//! bit1=func gate / bit2=reset（**高有效**：assert=1/deassert=0，
//! riscv-yocto reset-spacemit.c 定案）/ bit[6:4]=源 mux（0=12.8MHz）。
//! 位定义 = 主线 drivers/clk/spacemit/ccu-k3.c + esOS ccu-spacemit-k3.h。
//! 寄存器布局 = 上游 timer-k1x.c（MMP 血统）：CER=+0x00（bit n=counter n
//! 使能）/ CMR=+0x04（bit n=自由运行模式）/ PLCR(n)=+0x50+(n<<2)=0 自由
//! 运行 / CR(n)=+0x90+(n<<2) 计数值。counter0/2 保留未用。
//!
//! 非 DT probe 的理由：时钟门位于 AP 域 APBC，不在本 crate CCU 驱动
//! （RCPU 域 0xc088xxxx）的管理范围；以 `Board::init` 直连装配（与
//! `spl_handshake` 同级），副作用边界 = 本块内。
//!
//! 使用边界：
//! - 32 位 @12.8MHz 回卷 335s——所有用途为 µs~2s 级**区间差**
//!   （wrapping 语义），不做绝对时刻跨回卷比较；mtimecmp 睡眠唤醒
//!   截止时间仍走 SysTimer（`clint_k3`）。
//! - 时钟源为 PLL1 派生 12.8MHz（AP 电源域）：AP 深度低功耗时停——
//!   本场景 AP 常跑；若未来 AP 换用带 `CONFIG_SPACEMIT_K1X_TIMER` 的
//!   内核（如 Yocto 6.18）会抢占本块（清 CER + 改 1MHz），需届时协调。

const TMR1_BASE: usize = 0xd401_6000;
const APBC_TIMERS1_CLK_RST: usize = 0xd401_5044;

/// 计数频率（mux=0 = pll1_d192_12p8 标称值；板上联标实测 12.798MHz，
/// −0.016% 归 PLL 容差——区间差测量内部自洽，无跨钟对齐需求）。
pub const FREQ_HZ: u32 = 12_800_000;

/// counter1 计数值寄存器偏移（CR(1) = 0x90 + 1×4）。
const TMR_CR1: usize = 0x94;
/// counter1 预载控制寄存器偏移（PLCR(1) = 0x50 + 1×4；0 = 自由运行）。
const TMR_PLCR1: usize = 0x54;

/// 开 APBC 时钟门 + 复位脉冲 + counter1 自由运行化。
///
/// `Board::init` 调用（早于 DT boot，无依赖）。写后回读校验重试 ×8：
/// 门刚开的边界窗口内寄存器写可能未及生效（上游 timer-k1x.c 的
/// timer_write_check 同款防护）。
pub fn init() {
    // SAFETY: 各访问均为本模块文档论证过的目标寄存器纯 MMIO 读写。
    unsafe {
        let gate = (APBC_TIMERS1_CLK_RST as *const u32).read_volatile();
        let keep = gate & !0x7; // 保留源 mux 位 [6:4]
        // 双门开 + 复位脉冲（bit2 高有效：assert=1 → deassert=0）
        (APBC_TIMERS1_CLK_RST as *mut u32).write_volatile(keep | 0x7);
        (APBC_TIMERS1_CLK_RST as *mut u32).write_volatile(keep | 0x3);
        // counter1 自由运行：CMR bit1 + PLCR1=0 + CER bit1（回读校验）
        let cmr = (TMR1_BASE as *const u32).read_volatile();
        (TMR1_BASE as *mut u32).write_volatile(cmr | (1 << 1));
        ((TMR1_BASE + TMR_PLCR1) as *mut u32).write_volatile(0);
        let mut ok = false;
        for _ in 0..8 {
            let cer = (TMR1_BASE as *const u32).read_volatile();
            (TMR1_BASE as *mut u32).write_volatile(cer | (1 << 1));
            if (TMR1_BASE as *const u32).read_volatile() & (1 << 1) != 0 {
                ok = true;
                break;
            }
        }
        if !ok {
            log::error!("[timer_k3] counter1 使能失败（APBC 门/复位异常）");
        }
    }
}

/// 读 counter1（32 位 @12.8MHz，回卷 335s——区间差用 wrapping 语义）。
#[inline]
pub fn now() -> u64 {
    // SAFETY: 纯 MMIO 读，无副作用。
    unsafe { ((TMR1_BASE + TMR_CR1) as *const u32).read_volatile() as u64 }
}

/// tick → ns（编译期常量频率，无额外访存）。
#[inline]
pub fn ticks_to_ns(t: u64) -> u64 {
    t * 1_000_000_000 / FREQ_HZ as u64
}
