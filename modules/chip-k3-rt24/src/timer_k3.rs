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
//! **板上验证（2026-08-22 探针 TMR_RT_ON/AON_CAL/HOT/GAPPED）**：
//! - 计数递增，实测频率 **2.0001MHz**（tmr_aon_cal 联标 10005 ticks/5ms；
//!   文档标称 SEL=0 应为 25.6MHz 但实测 2MHz，SEL 读回 0——文档位定义
//!   与实况不符，按实测为准）；
//! - 背靠背读 **4ns/笔**（全系统最快：mtime 106 / mailbox 寄存器 148 /
//!   counter1 277ns）；
//! - 间隔读税 **~1.9µs/笔**（tmr_aon_gapped wall−Σaon 摊 200 轮，含
//!   mtime 括号冷读摊薄 ~245ns）——mtime 的 1/13，换源达标。
//!
//! 时钟/复位：`MCU_TIMER1_CLK_RST @0xc088c04c`（17_clock_reset.md
//! RCPU_PMU，基址 0xc088c000）：
//! - bit0 TIMER_SW_RSTN —— 复位值 0 = **保持复位**，写 1 释放（低有效
//!   语义；与 APBC 的 bit2 高有效复位相反——2026-08-21 首次尝试栽在
//!   这里：开门后又写 0x3 把 bit0 清零，外设被按回复位态读全 0）；
//! - bit1 TIMER_FCLK_EN / bit2 TIMER_PCLK_EN —— 双时钟门；
//! - bit[5:4] TIMER_FCLK_SEL —— 文档称 0=25.6MHz，实测全 0 下 2MHz。
//! 开门写 `gate|0x7` 一笔到位（保留其余位读回值）。
//!
//! 寄存器布局文档未载，按同血脉 APBC OS timer（timer-k1x.c，MMP 血统）
//! 假设并经板上计数验证成立：CER=+0x00（bit n=counter n 使能）/
//! CMR=+0x04（bit n=自由运行）/ PLCR(n)=+0x50+(n<<2) / CR(n)=+0x90+(n<<2)。
//!
//! **生产计时链迁移二度证伪并回滚（2026-08-22 板上）**：迁移后 rtt
//! 240→251.5（svc 148/dslot 40.5，与 counter1 轮 146/40.4 几乎同形）。
//! 根因：**mtime 的 24.5µs 冷读税在生产路径从未被支付**——stamp 点前后
//! 交织的 SHM/fence 流量把 mtime 路径保温（"异设备读令 mtime 变热"
//! 效应的另一面），基线无税可省；而任何替换计时器的读在真实间隔
//! （跨域流量填充）下仍付 ~10µs 级。**探针税 ≠ 生产税**：下方探针
//! 实测的 1.9µs 是纯本地 spin 间隔条件下的值。生产链回 mtime；本
//! 模块保留开门 + `now()` 供探针（tmr_aon_* 组）与未来用途。
//! mtimecmp 睡眠唤醒截止时间仍走 SysTimer（`clint_k3`）。
//!
//! 使用边界：
//! - 32 位 @2MHz 回卷 **35.8min**——所有用途为 µs~秒级**区间差**
//!   （wrapping 语义），不做绝对时刻跨回卷比较。
//! - 分辨率 500ns/tick：µs 级区间差的量化误差 ≤0.5µs，dd 分段（数十
//!   µs）足够；亚 µs 精度需求请用 mcycle 热读（17ns，CSR 本地）。
//! - esOS/StarryOS 均未使用 AON_TIMER1（dts disabled）；若 AP 侧未来
//!   声明使用需跨核协调。

const AON_TMR1_BASE: usize = 0xc088_9000;
const MCU_TIMER1_CLK_RST: usize = 0xc088_c04c;

/// 计数频率（tmr_aon_cal 板上联标实测 2,000,133Hz，−0.007% 取整；区间差
/// 测量内部自洽，无跨钟对齐需求）。
pub const FREQ_HZ: u32 = 2_000_000;

/// counter0 计数值寄存器偏移（CR(0) = 0x90，板上计数验证成立）。
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
        // 低 3 位 = RSTN|FCLK|PCLK 全开一笔到位，其余位保留读回值——
        // 不再有第二笔写（2026-08-21 版在此清了 bit0 重新按住复位）
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
            log::error!("[timer_k3] AON_TIMER1 使能失败（门/软复位异常）");
        }
    }
}

/// 读 counter0（32 位 @2MHz，回卷 35.8min——区间差用 wrapping 语义）。
#[inline]
pub fn now() -> u64 {
    // SAFETY: 纯 MMIO 读，无副作用。
    unsafe { ((AON_TMR1_BASE + TMR_CR0) as *const u32).read_volatile() as u64 }
}

/// tick → ns（实测频率，无额外访存）。
#[inline]
pub fn ticks_to_ns(t: u64) -> u64 {
    t * 1_000_000_000 / FREQ_HZ as u64
}
