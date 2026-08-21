//! 双核 AMP 通信模块
//!
//! 基于 ov-rpc 框架，在 ov_channels 共享内存通道之上提供类型安全的 RPC。
//!
//! 共享内存布局:
//! - Channel 0: StarryOS -> rt-async (请求/通知)
//! - Channel 1: rt-async -> StarryOS (响应/通知)
//! - Channel 2: 急停（单向高优先级）
//!
//! 弹性忙等: 处理完消息后，服务端会在一段弹性时间内忙等，
//! 期间设置 BUSY 标志，客户端据此跳过不必要的 IPI。
//!
//! 共享内存基址与跨核通知设备均来自设备树（`ov-shm` crate probe），
//! 不再从 amp.toml 编译期常量获取。
//!
//! K3 平台：共享内存物理位于 RCPU SRAM 起始（主域视图 0xc0800000，
//! `ov,rt-async-amp` 节点，0x19000；②c 后 RP 侧 DT 故意写本地别名窗口
//! 基址 0x0——同一物理 SRAM，冷读 7× 快且绕开 M2F 桥），跨核通知经
//! mailbox4 硬件中断（IRQ 69）替代 QEMU 的 MachineSoft IPI。共享窗初始化
//! 的职责在 **AP 内核 rt_shm 驱动 probe 期**（时机确定性排在 SPL /
//! U-Boot memset / bootm 缓存 flush 三个窗口破坏者之后），本侧经
//! [`wait_ready`] 只读等待；magic 自愈看门狗（watchdog.rs）作纵深防御。
//!
//! # 延迟插桩（测试基础设施）
//!
//! 请求被本模块察觉的方式有四种（发现路径，决定延迟构成）：
//!
//! | 标签 | 路径 | 延迟构成 |
//! |------|------|----------|
//! | D1 | 睡眠中被 mailbox IRQ 唤醒（AP 发了门铃） | ioctl+CBO → mailbox → ISR → latch → 调度 → 读环，最长路径 |
//! | D2 | 弹性自旋轮询发现（AP 读 BUSY=1 跳过门铃） | 仅自旋轮询周期 |
//! | D3 | 批处理循环中追加发现（背靠背请求） | 排队等前序 handler |
//! | D4 | clear_busy 后最终竞争检查发现（fence 闭环） | 剩余自旋 + 重查 |
//!
//! 另有 D5（冗余门铃：弹性窗口期间到达的 IRQ，此时 AP 本应跳过 IPI）以
//! 窗口计数观测。配套 RPC 服务：
//! - `PING`：回显 val + 发现路径标签 + RP 侧 mtime 分段时间戳（t_isr /
//!   t_drain / t_sched / t_seen），供 AP 侧 bench 按路径分桶测延迟；
//! - `STATS`：按索引查计数器（见 [`stat_idx`]，AP 侧镜像对齐义务）；
//! - `MEMBENCH`：RP 侧内存/MMIO 访问微基准（见 [`membench_op`]），检验
//!   「无缓存 SRAM 单笔 load ~3.3µs / 消息 256B 取读 ~105µs」等延迟归因
//!   假设（2026-08-17 D1 分段 69.8/110.5µs 的解释候选）。

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use fugit::ExtU64;
use ov_channels::{ChannelId, Message, MsgType, SharedMemory};
use ov_rpc::{define_service, HandledKind, ProcessResult, RpcServer};

// ============================================================================
// RPC 服务定义
// ============================================================================

define_service! {
    /// rt-async 侧的 RPC 服务。MEMBENCH/LITMUS 为延迟归因战役测量探针，
    /// 经 `probe` feature 门控（见 Cargo.toml 注释）——正常开发不编译。
    RtAsyncRpc {
        ECHO:  0 => call echo(val: u32) -> u32;
        ADD:   1 => call add(a: i32, b: i32) -> i32;
        DELAY: 2 => send delay(us: u32);
        PING:  3 => call ping(val: u64) -> (u64, u8, u64, u64, u64, u64, u64, u64, u64, u64);
        STATS: 4 => call stat(idx: u32) -> u64;
        #[cfg(feature = "probe")]
        MEMBENCH: 5 => call membench(op: u32, arg: u32) -> (u64, u64);
        #[cfg(feature = "probe")]
        LITMUS: 6 => send litmus(op: u32, arg: u32);
    }
}

impl RtAsyncRpc {
    fn echo(val: u32) -> u32 {
        val
    }
    fn add(a: i32, b: i32) -> i32 {
        a.wrapping_add(b)
    }
    /// 精确延时（busy-wait）：在 process_all 中顺序执行，
    /// 保证前后 RPC 指令之间的时序精度。
    fn delay(us: u32) {
        // ①′：clint 直连——timer() 全路径（Slot 查找）~3µs/次（TIMER_NOW
        // 探针实测 3040ns vs 裸 mtime 105ns），延时循环不宜经 Slot。
        use platform::Timer as _;
        let t = &chip_k3_rt24::clint_k3::TIMER;
        let freq = t.freq_hz() as u64;
        let target = t.now() + (us as u64) * freq / 1_000_000;
        while t.now() < target {
            core::hint::spin_loop();
        }
    }
    /// 测量探针：回显 val + 发现路径标签（1..=4 = D1..D4）+ RP 侧 mtime
    /// 分段时间戳（tick，见模块文档）。
    ///
    /// 分段语义：
    /// - `t_isr`：最近一次 mailbox ISR 入口（D1 的唤醒源；D2/D3/D4 路径下
    ///   属于更早或冗余的中断，仅 D1 样本有效）
    /// - `t_drain`：该 ISR 排空舞步完成点（t_sched−t_isr 拆段用，
    ///   [`chip_k3_rt24::mailbox::LAST_ISR_DONE_TS`]）
    /// - `t_sched`：本次 `process_elastic` 入口 ≈ await 返回、任务恢复执行
    /// - `t_seen`：本 handler 入口 ≈ 消息从 ring 取出并完成反序列化
    ///
    /// 多消息在途时（批处理）t_isr/t_drain 会被后续中断覆盖，仅单请求
    /// 在途的测量场景保证精确。
    fn ping(val: u64) -> (u64, u8, u64, u64, u64, u64, u64, u64, u64, u64) {
        // ①′：clint 直连（同 Slot 内同一 TIMER 实例，纯省 3µs 查找开销）。
        use platform::Timer as _;
        let t_seen = chip_k3_rt24::clint_k3::TIMER.now();
        (
            val,
            DISCOVERY.load(Ordering::Relaxed) as u8,
            chip_k3_rt24::mailbox::LAST_IRQ_TS.load(Ordering::Relaxed),
            chip_k3_rt24::mailbox::LAST_ISR_DONE_TS.load(Ordering::Relaxed),
            T_SCHED.load(Ordering::Relaxed),
            t_seen,
            stamp0(ov_rpc::stamp_idx::CH_ENTER),
            stamp0(ov_rpc::stamp_idx::RECV_DONE),
            stamp0(ov_rpc::stamp_idx::IDX_DONE),
            stamp0(ov_rpc::stamp_idx::SERDE_DONE),
        )
    }
    /// 按索引查询插桩计数器（索引表见 [`stat_idx`]）。
    fn stat(idx: u32) -> u64 {
        stats::get(idx)
    }
    /// RP 侧内存/MMIO 访问微基准（延迟归因诊断）。见 [`membench_op`]。
    #[cfg(feature = "probe")]
    fn membench(op: u32, arg: u32) -> (u64, u64) {
        run_membench(op, arg)
    }
    /// 跨核免 fence 顺序性实验（见 [`litmus_op`]）。单向：结果经 STATS
    /// LIT_* 索引轮询读取。
    #[cfg(feature = "probe")]
    fn litmus(op: u32, arg: u32) {
        run_litmus(op, arg)
    }
}

/// dispatch 分解戳读取：probe 开启转发 ov_rpc::stamp，关闭时恒 0
/// （PING 元组 ABI 固定 8 元，无 stamps 构建下尾两位截断为 0）。
#[cfg(not(feature = "probe"))]
fn stamp0(_idx: usize) -> u64 {
    0
}
#[cfg(feature = "probe")]
fn stamp0(idx: usize) -> u64 {
    ov_rpc::stamp::get(idx)
}

// ============================================================================
// 发现路径标签与插桩计数器
// ============================================================================

/// 发现路径标签值（D1..D4，含义见模块文档延迟插桩节）。
pub mod path {
    /// D1：睡眠中被 mailbox IRQ 唤醒后发现
    pub const D1_IRQ_WAKE: usize = 1;
    /// D2：弹性自旋轮询发现（AP 跳过了门铃）
    pub const D2_SPIN_HIT: usize = 2;
    /// D3：批处理循环中追加发现
    pub const D3_BATCH_APPEND: usize = 3;
    /// D4：clear_busy 后最终竞争检查发现（fence 闭环兜底）
    pub const D4_RACE_CLOSE: usize = 4;
}

/// STATS 计数器索引表（ABI：AP 侧 user-test-bench 镜像同值，双端对齐义务）。
pub mod stat_idx {
    /// 已处理消息总数（含非 RPC 消息与未知 method）
    pub const MSGS: u32 = 0;
    /// D1 中断唤醒路径命中（按消息计）
    pub const D1_IRQ_WAKE: u32 = 1;
    /// D2 弹性自旋命中（按消息计）
    pub const D2_SPIN_HIT: u32 = 2;
    /// D3 批处理追加（按消息计）
    pub const D3_BATCH_APPEND: u32 = 3;
    /// D4 竞态闭环补处理（按消息计）
    pub const D4_RACE_CLOSE: u32 = 4;
    /// 弹性窗口期间到达 mailbox IRQ 的窗口数（D5 冗余门铃）
    pub const REDUNDANT_IRQ: u32 = 5;
    /// 响应发送失败数（CH1 满被静默丢弃，ov-rpc 计数）
    pub const RESP_FAIL: u32 = 6;
    /// magic 自愈次数（shm_ping 看门狗）
    pub const HEALS: u32 = 7;
    /// 最近一次完整弹性窗口时长（ns）
    pub const WIN_LAST_NS: u32 = 8;
    /// 完整弹性窗口时长最小值（ns；u64::MAX = 尚无样本）
    pub const WIN_MIN_NS: u32 = 9;
    /// 完整弹性窗口时长最大值（ns）
    pub const WIN_MAX_NS: u32 = 10;
    /// 完整弹性窗口完成数
    pub const WINDOWS: u32 = 11;
    /// 最近一条消息服务时长（ns；消息取出 → 响应写入完成）
    pub const SVC_LAST_NS: u32 = 12;
    /// 消息服务时长最小值（ns；u64::MAX = 尚无样本）
    pub const SVC_MIN_NS: u32 = 13;
    /// 消息服务时长最大值（ns）
    pub const SVC_MAX_NS: u32 = 14;
    /// 当前 mtime tick（采样时刻基准，供漂移研究）
    pub const T_NOW: u32 = 15;
    /// 定时器频率（Hz）
    pub const FREQ_HZ: u32 = 16;
    // ── 以下为 `probe` feature 扩展列（延迟归因战役测量面）──
    /// LITMUS：RP 侧观测到的违例数（op 语义见 [`litmus_op`]）
    #[cfg(feature = "probe")]
    pub const LIT_VIOL: u32 = 17;
    /// LITMUS：RP 侧完成的轮数
    #[cfg(feature = "probe")]
    pub const LIT_ROUNDS: u32 = 18;
    /// LITMUS：RP 侧状态（0=空闲 1=运行 2=完成 3=超时）
    #[cfg(feature = "probe")]
    pub const LIT_STATE: u32 = 19;
    /// dispatch 分解戳：process_channel 入口（转发 ov_rpc::stamp）
    #[cfg(feature = "probe")]
    pub const T_CH_ENTER: u32 = 20;
    /// dispatch 分解戳：try_recv 完成
    #[cfg(feature = "probe")]
    pub const T_RECV_DONE: u32 = 21;
    /// dispatch 分解戳：handler 完成
    #[cfg(feature = "probe")]
    pub const T_HANDLE_DONE: u32 = 22;
    /// dispatch 分解戳：响应写入完成
    #[cfg(feature = "probe")]
    pub const T_RESP_DONE: u32 = 23;
    /// 计数器总数（probe 开 24 / 关 17）
    #[cfg(feature = "probe")]
    pub const COUNT: u32 = 24;
    /// 计数器总数（probe 开 24 / 关 17）
    #[cfg(not(feature = "probe"))]
    pub const COUNT: u32 = 17;
}

/// 插桩计数器存储（仅 IPC 任务上下文读写，Relaxed 足够）。
mod stats {
    // portable-atomic + 单核后端（feature cs-atomics）：fetch_add/min/max
    // 走 mstatus MIE 屏蔽 + 普通访存（~90ns/笔）而非原生 AMO——X100 的
    // Atomics Wrapper 对原子 RMW 序列化 ~2.2µs/笔，每消息 5-8 笔计数是
    // dserde/drx 段的固定成本（2026-08-19 dd 细分段归因）。计数器为
    // 本核本地数据，单核假设 sound。
    use portable_atomic::{AtomicU64, Ordering};

    use super::stat_idx;

    const ZERO: AtomicU64 = AtomicU64::new(0);
    const MAX_INIT: AtomicU64 = AtomicU64::new(u64::MAX);

    // WIN_MIN/SVC_MIN 初值 u64::MAX 表示"尚无样本"（bench 侧判 MAX 显示 N/A）。
    // probe 扩展列（17-23）恒 ZERO 初值。
    #[rustfmt::skip]
    #[cfg(feature = "probe")]
    static C: [AtomicU64; stat_idx::COUNT as usize] = [
        ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO,
        ZERO, MAX_INIT, ZERO, ZERO, ZERO, MAX_INIT, ZERO, ZERO, ZERO,
        ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO,
    ];
    #[rustfmt::skip]
    #[cfg(not(feature = "probe"))]
    static C: [AtomicU64; stat_idx::COUNT as usize] = [
        ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO, ZERO,
        ZERO, MAX_INIT, ZERO, ZERO, ZERO, MAX_INIT, ZERO, ZERO, ZERO,
    ];

    pub fn bump(i: u32) {
        if i < stat_idx::COUNT {
            C[i as usize].fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn set(i: u32, v: u64) {
        if i < stat_idx::COUNT {
            C[i as usize].store(v, Ordering::Relaxed);
        }
    }
    pub fn min(i: u32, v: u64) {
        if i < stat_idx::COUNT {
            C[i as usize].fetch_min(v, Ordering::Relaxed);
        }
    }
    pub fn max(i: u32, v: u64) {
        if i < stat_idx::COUNT {
            C[i as usize].fetch_max(v, Ordering::Relaxed);
        }
    }
    pub fn get(i: u32) -> u64 {
        if i >= stat_idx::COUNT {
            return 0;
        }
        // RESP_FAIL 由 ov-rpc 服务端在其发送失败点递增，此处转发读取。
        if i == stat_idx::RESP_FAIL {
            return ov_rpc::RESP_SEND_FAILS.load(Ordering::Relaxed);
        }
        if i == stat_idx::T_NOW {
            use platform::Timer as _;
            return chip_k3_rt24::clint_k3::TIMER.now();
        }
        if i == stat_idx::FREQ_HZ {
            use platform::Timer as _;
            return chip_k3_rt24::clint_k3::TIMER.freq_hz() as u64;
        }
        // dispatch 分解戳转发（feature "stamps"，未启用时恒 0）。
        #[cfg(feature = "probe")]
        if (stat_idx::T_CH_ENTER..=stat_idx::T_RESP_DONE).contains(&i) {
            return ov_rpc::stamp::get((i - stat_idx::T_CH_ENTER) as usize);
        }
        C[i as usize].load(Ordering::Relaxed)
    }
}

// ============================================================================
// MEMBENCH：RP 侧内存 / MMIO 访问微基准（probe 门控，延迟归因测量面）
// ============================================================================

/// MEMBENCH 操作码（ABI：AP 侧 user-test-bench 镜像同值，双端对齐义务）。
///
/// 背景（2026-08-17 板上数据）：D1 分段 dsched=69.8µs、dseen=110.5µs、
/// 弹性自旋迭代 ~20µs，均远超 614MHz 主频下的预期——候选解释是共享窗
/// SRAM 为无缓存访问（单笔 load µs 级）。本组操作直接测出各类访问的
/// 真实单价，判定物理下限与优化方向：
/// - SHM 组在共享窗尾部空闲区上测（见 [`SHM_SCRATCH_OFF`]）；
/// - LOCAL 组在固件 .bss 上测，作同尺寸基线对照（若固件数据也在同一
///   SRAM 且同样无缓存，两者应同价——这本身就是结论）；
/// - MMBOX 组只读 mailbox **只读状态寄存器**（msg_status / irq_en_set /
///   fifo_status）——绝不读 mbox_msg（读 FIFO 会消费数据）；
/// - RD_MTIME 测 mtime MMIO 读（clint_k3::TIMER.now()，固定地址）。
#[cfg(feature = "probe")]
pub mod membench_op {
    /// SHM 同一行重复 8B 读（arg=次数，0→2000）
    pub const RD_LINE_SHM: u32 = 0;
    /// SHM 256B 整块单次 read_volatile（= try_recv 的消息取读；arg=次数，0→200）
    pub const RD_BLK256_SHM: u32 = 1;
    /// SHM 256B 显式 32×8B 步进读（对照编译器整块取读序列；arg=次数，0→200）
    pub const RD_BLK8_SHM: u32 = 2;
    /// SHM 8B 写 + SeqCst fence（写发布单价；arg=次数，0→2000）
    pub const WR_LINE_SHM: u32 = 3;
    /// 本地 .bss 同一行重复 8B 读（基线；arg=次数，0→2000）
    pub const RD_LINE_LOCAL: u32 = 4;
    /// 本地 256B 整块读（基线；arg=次数，0→200）
    pub const RD_BLK256_LOCAL: u32 = 5;
    /// 本地 8B 写 + fence（基线；arg=次数，0→2000）
    pub const WR_LINE_LOCAL: u32 = 6;
    /// SHM 跨行步进读（arg=stride 字节，0→64；扫满 0x800 空闲区）。
    /// 同行读远快于跨行读 ⇒ 存在某种缓存/合并路径，是别名优化的线索
    pub const RD_STRIDE_SHM: u32 = 7;
    /// mailbox msg_status[ch0] 只读 MMIO 读（arg=次数，0→2000）
    pub const RD_MMBOX_MSGSTAT: u32 = 8;
    /// mailbox irq_en_set（本地 user）MMIO 读（arg=次数，0→2000）
    pub const RD_MMBOX_IRQEN: u32 = 9;
    /// mailbox fifo_status[ch0] MMIO 读（arg=次数，0→2000）
    pub const RD_MMBOX_FIFOSTAT: u32 = 10;
    /// mtime MMIO 读（arg=次数，0→2000）
    pub const RD_MTIME: u32 = 11;
    /// 共享窗 `AtomicU64::load(Acquire)` 循环（arg=次数，0→2000）。
    /// 2026-08-17 mb 实测裸 read_volatile 同地址仅 22ns / 冷访问 ~875ns，
    /// 而 W=2.007s 折算自旋每笔访问 ~3.3µs——差额疑似在内存序（Acquire/
    /// fence），本 op 直接测带序读的真实单价
    pub const AQ_LOAD_SHM: u32 = 12;
    /// 共享窗 `AtomicU64::store(Release)` 循环（arg=次数，0→2000）
    pub const REL_STORE_SHM: u32 = 13;
    /// 纯 `fence(SeqCst)` 循环（arg=次数，0→2000）——隔离 fence 本身
    pub const FENCE_ONLY: u32 = 14;
    /// 精确弹性自旋循环体 `has_pending() || has_urgent()`（arg=次数，0→200）。
    /// 直接对账 W：预测 ~20µs/迭代（6 笔 Acquire 型 load）
    pub const SPIN_ITER: u32 = 15;
    /// 只读寄存器探测（arg=4 对齐的 32 位地址）：连续 1000 次 `read_volatile`
    /// u32，返回 (耗时 ns, 末次读值)。**仅限只读寄存器**——mailbox 的
    /// mbox_msg/FIFO 读会消费数据，禁止探测。用于：RCPU_PMU 时钟配置回读
    /// （0xC088C0C0/C4/C8，手册 17.4）、r_mailbox 本地窗口存在性
    /// （0xC076_x000，手册 6.2，REVISION 应为 0xAAAA5555）、RCPU 本地
    /// SRAM 别名（0x0..0x80000 镜像 0xC080_0000，手册 6.2）。
    pub const PEEK_T: u32 = 16;
    /// 本地 .bss `AtomicU64::load(Acquire)` 循环（arg=次数，0→2000）。
    /// 判别原子开销是否与目标地址无关（Atomics Wrapper 按指令序列化）
    /// 还是共享窗特有：与 AQ_LOAD_SHM 同价 ⇒ 指令本身贵
    pub const AQ_LOAD_LOCAL: u32 = 17;
    /// 真实取读路径探测：对空 ch0 循环 `receiver(0).try_recv()`（纯读：
    /// magic Acquire + 双索引 Acquire，空则不触块读；arg=次数，0→200）。
    /// dseen（111.5µs）前缀成分的直接对照
    pub const RECV_EMPTY: u32 = 18;
    /// `platform::timer().now()` ×1000（含 platform Slot 查找路径），对照
    /// RD_MTIME（裸 clint MMIO 107ns）分离 slot/原子开销——dseen/ddisp 里
    /// 每次 timer() 调用的真实成本
    pub const TIMER_NOW: u32 = 19;
    // ── L0 归因闭合组（2026-08-20）：dseen 107.8µs 中 fence 理论只解释
    // ~31µs（手数代码 ~14 笔 × 热循环 2.2µs），~75µs 无主。三假设：
    // H1 单笔"冷"fence 贵于热循环（吞吐≠延迟）；H2 postcard/Message 构解
    // 本身贵；H3 其他调用链成本。以下探针逐一钉死。──
    /// 跨行 16 址（stride 64B）轮转 Acquire 读（arg=次数，0→2000）。
    /// 对照 AQ_LOAD_SHM 同址热循环：区分"同址合并效应"与真实跨址单价
    pub const AQ_DISTINCT_SHM: u32 = 20;
    /// 跨行 Acquire 读 + 每笔间隔一次 mtime 读（完成点，强制前笔落地）。
    /// 单笔"冷"真实单价（每样本含 1 笔 mtime 读 ~0.1-0.3µs，判读扣除）。
    /// H1 判定：cold ≫ hot(2198ns) ⇒ 真实路径 14 笔 × cold 才是 dseen 构成
    pub const COLD_AQ_SHM: u32 = 21;
    /// scratch 复刻 try_recv 全序列 ×N（arg=次数，0→200）：magic Acquire +
    /// read Acquire + write Acquire + 256B read_volatile + read Release。
    /// drx 段（45.6µs）窗口成本的隔离对账
    pub const RECV_SEQ: u32 = 22;
    /// scratch 复刻 try_send 全序列 ×N（arg=次数，0→200）：magic Acquire +
    /// write Acquire + read Acquire + 256B write_volatile + write Release。
    /// 响应发送路径的窗口成本（dserde/ddisp 段成分）
    pub const SEND_SEQ: u32 = 23;
    /// 本地 postcard 双向构解 ×N（arg=次数，0→200）：Message::request 构造
    /// + method_id + as_request 反序列化 + Message::response 构造 +
    /// as_response 反序列化（PING 真实形状）。H2 判定：dispatch 段本地成本
    pub const POSTCARD_RT: u32 = 24;
    /// 真实通道空 try_recv ×200（arg=通道号 0/1/2）。对照 RECV_EMPTY（固定
    /// ch0）补测 ch2——drain_all 每消息批前后各查一次 ch2（3 笔 Acquire）
    pub const RECV_EMPTY_CH: u32 = 25;
    /// 门铃全成本 ×N（arg=次数，0→100）：notifier().notify() = fence +
    /// mailbox MMIO 写。注意：N 次假唤醒 AP 侧 AWAIT（bench 循环耐受，
    /// 本轮 console 会多 N 条空唤醒痕迹，不影响协议）
    pub const NOTIFY_N: u32 = 26;
    /// 真实 ch0 一来一回 ×N（arg=次数，0→200）：try_send 小消息 + try_recv
    /// 收回（净零）。探针期间 AP 阻塞在 MEMBENCH 响应等待（单请求在途），
    /// ch0 无并发生产者。H5：真实通道上下文 vs scratch 复刻（recv_seq +
    /// send_seq）的溢价——D2 热态 svc 实测比探针合计仍高 ~85µs 的定位面
    pub const SELF_ROUND: u32 = 27;
    /// 真实 ch0：try_send 1 条 → peek ×N（magic+双索引 Acquire + 槽读，无
    /// Release/无索引推进）→ try_recv 清 1 条。与 SELF_ROUND 对照分离
    /// read Release 与索引推进的成本
    pub const SELF_PEEK: u32 = 28;
    /// H8 新鲜写衰减：自旋 try_recv 直到收到一条（arg=超时 ms，0→200），
    /// 返回**成功那一笔**的计时。bench 编排（fresh_scan）：fire 本 op →
    /// 延迟 D → 写 dummy notification（不门铃）。成功笔即"读 AP 于 ~D
    /// 前写入的消息"的单笔价格；D→∞ 应回落 recv_seq 价（~11.7µs），短 D
    /// 若 ≈ drx（43.6µs）⇒ H8：读 AP 新鲜写的行有确定性税（posted 写落地）
    pub const FRESH_WAIT_RECV: u32 = 29;
    /// L0 终拆后续：op 上下文直接调 dispatch 完整路径（`RtAsyncRpc::handle`
    /// = 宏 match + postcard args 反序列化 + handler + response 序列化）
    /// ×N（arg=次数，0→200）。drest 34µs vs 本探针的差额 = "op 上下文 vs
    /// process_channel 上下文"的纯上下文税；若本探针也 ~30µs ⇒ postcard/
    /// 宏本身在真实形状下慢（此前 postcard_rt 双向 9.4µs 是热循环价）
    pub const DISPATCH_N: u32 = 30;
    /// 间隔版 mtime 读：N 轮 {t0=now(); 忙等 ~20µs; t1=now()}，返回 Σ 轮内
    /// now() 计时。测"间隔 20µs 的单笔 mtime MMIO 读"单价（RD_MTIME 的
    /// 106ns 是背靠背热循环价；真实路径的 stamp mark 每条消息 4-6 次、
    /// 间隔 µs~百 ms——若冷读 µs 级，mark 链即是隐藏税）
    pub const NOW_GAPPED: u32 = 31;
    /// rdcycle CSR 间隔读对照（NOW_GAPPED 的 CPU 本地版）：t0/t1 用
    /// `csrr cycle`，忙等固定圈数（~20µs @614MHz 假设）。返回每轮 cycle
    /// 差（ck=圈数）。NOW_GAPPED 实测间隔 mtime 读 ~24µs/笔（跨域同步器
    /// 重锁，2026-08-21 定案 dslot/drest 超额真凶）——若 rdcycle 无此税，
    /// stamp 时钟源迁 rdcycle 可每消息省 ~40-60µs 且消除测量假象
    /// mcycle CSR 间隔读对照（NOW_GAPPED 的 CPU 本地版）：t0/t1 用
    /// mcycle，忙等固定圈数 12000。返回每轮 cycle 差（ck=圈数）。
    /// NOW_GAPPED 实测间隔 mtime 读 ~24µs/笔（跨域同步器重锁，
    /// 2026-08-21 定案 dslot/drest 超额真凶）——mcycle 是否同税由本组探针
    /// （GAPPED/HOT/CAL）三面定案，决定 stamp 时钟源迁移方案
    pub const CYCLE_GAPPED: u32 = 32;
    /// mcycle 热连读 ×1000 单价（返回首尾差 cycle 数，ck=1000）。
    /// 判别"仅冷读慢"vs"读本身慢"
    pub const CYCLE_HOT: u32 = 33;
    /// mcycle↔mtime 频率联标：~5ms 忙等（mtime 判据），返回 (cycle 差,
    /// mtime 差 ticks)。板上 cycle_gapped 每轮 wall ≈6.18ms 且反推 mcycle
    /// 恰 24MHz——联标判 mcycle 是否真核频计数还是与 mtime 同源
    pub const CYCLE_CAL: u32 = 34;
    /// soc-timer counter1 自由运行化（一次性）：AP 域 0xd4016000 块（AP dts
    /// timer-id 0，counter0=AP 广播用；counter1/2 空闲且块时钟常开）。
    /// 置 CMR bit1 + PLCR1=0 + CER bit1（回读校验重试，CER 与 AP 共享）。
    /// 寄存器布局镜像上游 timer-k1x.c（MMP 血统）：CR(n)=+0x90+(n<<2) 即
    /// DS 所称 TCCRn。mtime 冷读税（24.5µs/笔）的替换候选
    pub const TMR_SETUP: u32 = 35;
    /// counter1（TMR_CR1=0xd4016094）热连读 ×N（默认 4000，mtime 括号计时）
    pub const TMR_HOT: u32 = 36;
    /// NOW_GAPPED 同构 + t1 前插入 1 笔 counter1 读：每轮与 NOW_GAPPED 的
    /// 差 = 候选"间隔 ~20µs 冷读"边际成本 Δ。Δ≈0 ⇒ 免跨域税可换源
    pub const TMR_GAPPED: u32 = 37;
    /// counter1↔mtime 5ms 频率联标 + 单调性哨兵（c1 递增且未被 AP 重编程
    /// 打断——AP 广播只动 counter0）。返回 (counter ticks, mtime ticks)
    pub const TMR_CAL: u32 = 38;
    /// 候选 B 侦查：d4014000 块（K1 dts timer0；K3 AP dts 无节点——若存在
    /// 即无共享独立块）。若 K3 无此块/时钟未开，读可能总线错误挂死固件：
    /// 排表尾，挂死不影响已打印结果
    pub const TMR_B_SCAN: u32 = 39;
    /// 开 TIMERS1（0xd4016000）APBC 时钟门 + 去复位，再置 counter1 自由
    /// 运行并 1ms 计数验证。寄存器/位 = 主线 ccu-k3.c + k3-syscon.h：
    /// `APBC_TIMERS1_CLK_RST` @0xd4015044，bit0=bus gate / bit1=func gate /
    /// bit2=reset（极性未定，两变体自动试）/ bit[6:4]=源 mux（0=12.8MHz）。
    /// 首轮板上实锤：StarryOS 不用该块（CER=0 全零、写不进），时钟常闭
    pub const TMR_CLKON: u32 = 40;
}

/// 共享窗尾部空闲区偏移（MEMBENCH 专用 scratch）。
///
/// ov-channels 布局：0x100 头 + 3×0x8200 通道 = 0x18700；窗口总 0x19000，
/// 尾部 0x900 不被协议使用（AP 内核 flush 会扫全窗但不解释内容，写
/// scratch 无害）。偏移 256 对齐（基址按 0x10000 对齐），满足块操作。
#[cfg(feature = "probe")]
const SHM_SCRATCH_OFF: usize = 0x18700;
/// scratch 可用长度（留 0x100 边界裕量，防布局假设偏差越窗）。
#[cfg(feature = "probe")]
const SHM_SCRATCH_LEN: usize = 0x800;

/// 256B 对齐块——与 `ov_channels::Message` 同尺寸/对齐，`read_volatile`
/// 生成与 try_recv 消息取读等价的访存序列。
#[cfg(feature = "probe")]
#[repr(C, align(256))]
struct Blk256([u8; 256]);

/// MEMBENCH 本地基线 scratch。
// SAFETY: 仅 run_membench（IPC 任务上下文，单线程执行）访问，无并发。
#[cfg(feature = "probe")]
static mut MEMBENCH_LOCAL: Blk256 = Blk256([0; 256]);

/// 执行一次微基准，返回 (耗时 ns, 校验和)。
///
/// 校验和累积读到的值，防止无副作用的读被优化掉；耗时含循环控制与
/// 校验和开销（614MHz 下 ~ns 级，远小于被测的 µs 级单价）。未知 op
/// 返回 (0, 0)。
#[cfg(feature = "probe")]
fn run_membench(op: u32, arg: u32) -> (u64, u64) {
    use membench_op as M;

    let n = |default: u32| if arg == 0 { default as usize } else { arg as usize };
    // SAFETY: MEMBENCH_LOCAL 见其定义处注释；SHM scratch 指向窗口尾部
    // 空闲区（只被本函数读写）。
    let shm_line = (SHM_BASE.load(Ordering::Acquire) + SHM_SCRATCH_OFF) as *const u64;
    let local_line = (&raw const MEMBENCH_LOCAL) as *const u64;
    let shm_blk = shm_line as *const Blk256;
    let local_blk = local_line as *const Blk256;

    let mut ck: u64 = 0;
    let t0 = platform::timer().now();
    match op {
        M::RD_LINE_SHM | M::RD_LINE_LOCAL => {
            let p = if op == M::RD_LINE_SHM { shm_line } else { local_line };
            for _ in 0..n(2000) {
                // SAFETY: p 指向有效 8B（scratch 区/本地 static）。
                ck = ck.wrapping_add(unsafe { p.read_volatile() });
            }
        }
        M::RD_BLK256_SHM | M::RD_BLK256_LOCAL => {
            let p = if op == M::RD_BLK256_SHM { shm_blk } else { local_blk };
            for _ in 0..n(200) {
                // SAFETY: p 256 对齐、指向有效 256B。
                let b = unsafe { p.read_volatile() };
                let w: [u8; 8] = b.0[0..8].try_into().unwrap();
                ck = ck.wrapping_add(u64::from_le_bytes(w));
            }
        }
        M::RD_BLK8_SHM => {
            for _ in 0..n(200) {
                let mut s = 0u64;
                for i in 0..32 {
                    // SAFETY: shm_line + i*8 保持在 256B 块内。
                    s = s.wrapping_add(unsafe { shm_line.add(i * 8).read_volatile() });
                }
                ck = ck.wrapping_add(s);
            }
        }
        M::WR_LINE_SHM | M::WR_LINE_LOCAL => {
            let p = if op == M::WR_LINE_SHM { shm_line as *mut u64 } else { local_line as *mut u64 };
            let iters = n(2000);
            for i in 0..iters {
                // SAFETY: p 指向有效 8B 写目标。
                unsafe { p.write_volatile(i as u64) };
                core::sync::atomic::fence(Ordering::SeqCst);
            }
            ck = iters as u64;
        }
        M::RD_STRIDE_SHM => {
            let stride = if arg == 0 { 64 } else { arg as usize }.max(8);
            let base = shm_line as *const u8;
            let mut off = 0usize;
            while off + 8 <= SHM_SCRATCH_LEN {
                // SAFETY: off+8 保持在 scratch 界内。
                ck = ck.wrapping_add(unsafe { (base.add(off) as *const u64).read_volatile() });
                off += stride;
            }
        }
        M::RD_MMBOX_MSGSTAT => {
            for _ in 0..n(2000) {
                ck = ck.wrapping_add(chip_k3_rt24::mailbox::MBX3.msg_count(0) as u64);
            }
        }
        M::RD_MMBOX_IRQEN => {
            for _ in 0..n(2000) {
                ck = ck.wrapping_add(chip_k3_rt24::mailbox::MBX3.irq_enabled(0) as u64);
            }
        }
        M::RD_MMBOX_FIFOSTAT => {
            for _ in 0..n(2000) {
                ck = ck.wrapping_add(chip_k3_rt24::mailbox::MBX3.fifo_is_empty(0) as u64);
            }
        }
        M::RD_MTIME => {
            use platform::Timer as _;
            for _ in 0..n(2000) {
                // clint_k3::TIMER.now() = 固定地址 mtime MMIO 读（无 slot 查找）。
                ck = ck.wrapping_add(chip_k3_rt24::clint_k3::TIMER.now());
            }
        }
        M::AQ_LOAD_SHM => {
            // SAFETY: scratch 8B 对齐；重解释为原子对象且仅本函数访问
            // （单线程 IPC 任务上下文）。
            let a = unsafe { &*(shm_line as *const AtomicU64) };
            for _ in 0..n(2000) {
                ck = ck.wrapping_add(a.load(Ordering::Acquire));
            }
        }
        M::REL_STORE_SHM => {
            // SAFETY: 同上。
            let a = unsafe { &mut *(shm_line as *mut AtomicU64) };
            let iters = n(2000);
            for i in 0..iters {
                a.store(i as u64, Ordering::Release);
            }
            ck = iters as u64;
        }
        M::FENCE_ONLY => {
            for _ in 0..n(2000) {
                core::sync::atomic::fence(Ordering::SeqCst);
            }
        }
        M::SPIN_ITER => {
            // 精确复刻 process_elastic 自旋循环体（纯读，不消费消息）。
            let iters = n(200);
            for _ in 0..iters {
                if server().has_pending() || server().has_urgent() {
                    ck = ck.wrapping_add(1);
                }
            }
            ck = ck.wrapping_add(iters as u64);
        }
        M::PEEK_T => {
            if arg & 3 != 0 {
                return (0, 0); // 未对齐地址拒绝（misaligned load 会 fault）
            }
            let p = arg as usize as *const u32;
            let mut v = 0u32;
            for _ in 0..1000 {
                // SAFETY: 调用方（bench 探针清单）保证地址已映射且只读；
                // 这里不做映射检查——探测不存在的窗口会 LoadFault（诊断
                // 场景可接受，重启即恢复）。
                v = unsafe { p.read_volatile() };
            }
            let ns = ticks_to_ns(platform::timer().now().saturating_sub(t0));
            return (ns, v as u64);
        }
        M::AQ_LOAD_LOCAL => {
            // SAFETY: MEMBENCH_LOCAL 8B 对齐，仅本函数访问。
            let a = unsafe { &*(local_line as *const AtomicU64) };
            for _ in 0..n(2000) {
                ck = ck.wrapping_add(a.load(Ordering::Acquire));
            }
        }
        M::RECV_EMPTY => {
            let iters = n(200);
            let shm = unsafe { SharedMemory::<3>::at(SHM_BASE.load(Ordering::Acquire)) };
            let rx = shm.receiver(ChannelId::new(0)).expect("ch0 receiver");
            let mut got = 0u64;
            for _ in 0..iters {
                if rx.try_recv().is_some() {
                    got += 1;
                }
            }
            ck = ck.wrapping_add(got);
        }
        M::TIMER_NOW => {
            for _ in 0..n(1000) {
                ck = ck.wrapping_add(platform::timer().now());
            }
        }
        // ── L0 归因闭合组。scratch 布局（区内偏移，litmus 只用 +0x00..0x40
        // 与 +0x7f8，且与本组顺序执行不并发）：AQ 对象阵 16×64B @+0x40..0x440；
        // 协议复刻 magic u16 @+0x480（独行）、read/write @+0x4C0/+0x4C8
        // （同行，对齐真实 RingBuffer +0x100/+0x108）、槽 256B @+0x500。──
        M::AQ_DISTINCT_SHM | M::COLD_AQ_SHM => {
            let base_arr = shm_line as usize + 0x40;
            let iters = n(if op == M::AQ_DISTINCT_SHM { 2000 } else { 64 });
            let cold = op == M::COLD_AQ_SHM;
            for i in 0..iters {
                // SAFETY: base_arr + (i%16)*64 ∈ scratch [+0x40,+0x440)，
                // 仅本函数访问；对象按 AtomicU64 重解释，仅本核读。
                let a = unsafe { &*((base_arr + (i % 16) * 64) as *const AtomicU64) };
                ck = ck.wrapping_add(a.load(Ordering::Acquire));
                if cold {
                    // clint 直连：每笔间隔的完成点（判读扣除 ~0.1-0.3µs/样本）
                    use platform::Timer as _;
                    ck = ck.wrapping_add(chip_k3_rt24::clint_k3::TIMER.now());
                }
            }
        }
        M::RECV_SEQ | M::SEND_SEQ => {
            use core::sync::atomic::AtomicU16;
            let cap = ov_channels::CHANNEL_CAPACITY;
            // SAFETY: 各偏移落 scratch [+0x480,+0x600)，仅本函数访问；索引
            // 初写归零防垃圾值（此后每轮 store 自维持）。
            unsafe {
                let magic = &*((shm_line as usize + 0x480) as *const AtomicU16);
                let ridx = &*((shm_line as usize + 0x4C0) as *const AtomicUsize);
                let widx = &*((shm_line as usize + 0x4C8) as *const AtomicUsize);
                let slot = (shm_line as usize + 0x500) as *mut Blk256;
                ridx.store(0, Ordering::Relaxed);
                widx.store(1, Ordering::Relaxed); // write−read=1：恒"有消息"
                for _ in 0..n(200) {
                    ck = ck.wrapping_add(magic.load(Ordering::Acquire) as u64);
                    if op == M::RECV_SEQ {
                        // Channel::try_recv 全序列（不判空：测稳态有消息路径；
                        // 槽用固定址——单请求在途时真实路径也近乎同槽）
                        let read = ridx.load(Ordering::Acquire);
                        let _write = widx.load(Ordering::Acquire);
                        let b = slot.read_volatile();
                        ck = ck.wrapping_add(b.0[0] as u64);
                        ridx.store((read + 1) % cap, Ordering::Release);
                    } else {
                        // Channel::try_send 全序列（不判满）
                        let write = widx.load(Ordering::Acquire);
                        let _read = ridx.load(Ordering::Acquire);
                        slot.write_volatile(Blk256([ck as u8; 256]));
                        widx.store((write + 1) % cap, Ordering::Release);
                    }
                }
            }
        }
        M::POSTCARD_RT => {
            // PING 真实形状的本地构解双向（无窗口访问）——dserde 本地成本。
            let req_args = (0xA5A5_u64,);
            for k in 0..n(200) {
                let msg = Message::request(k as u64, 3, &req_args).expect("req serialize");
                let m = msg.method_id().unwrap_or(0);
                let (rid, _mid, a): (u64, u64, (u64,)) =
                    msg.as_request().expect("req deserialize");
                let resp = Message::response(rid, &(a.0, 1_u8, 0, 0, 0, 0, 0, 0))
                    .expect("resp serialize");
                let (_r2, _tuple): (u64, (u64, u8, u64, u64, u64, u64, u64, u64)) =
                    resp.as_response().expect("resp deserialize");
                ck = ck.wrapping_add(m.wrapping_add(rid));
            }
        }
        M::RECV_EMPTY_CH => {
            // arg=通道号（0/1/2），固定 200 轮——补测 drain 的 ch2 前后检查。
            let ch = (arg % 3) as u8;
            let shm = unsafe { SharedMemory::<3>::at(SHM_BASE.load(Ordering::Acquire)) };
            let rx = shm.receiver(ChannelId::new(ch)).expect("receiver");
            let mut got = 0u64;
            for _ in 0..200 {
                if rx.try_recv().is_some() {
                    got += 1;
                }
            }
            ck = ck.wrapping_add(got);
        }
        M::NOTIFY_N => {
            // 门铃全成本（fence + mailbox MMIO 写）。副作用：N 次假唤醒 AP
            // 侧 AWAIT——bench 循环耐受，仅本轮 console 多空唤醒痕迹。
            let iters = n(100);
            for _ in 0..iters {
                ov_shm::notifier::notifier().notify();
            }
            ck = iters as u64;
        }
        M::SELF_ROUND | M::SELF_PEEK => {
            // 真实 ch0 自往返：AP 此刻阻塞在 MEMBENCH 响应等待（同步 RPC），
            // ch0 无并发生产者；成对 send/recv 每轮净零，不污染协议。
            let shm3 = unsafe { SharedMemory::<3>::at(SHM_BASE.load(Ordering::Acquire)) };
            let tx = shm3.sender(ChannelId::new(0)).expect("ch0 sender");
            let rx = shm3.receiver(ChannelId::new(0)).expect("ch0 receiver");
            if op == M::SELF_ROUND {
                let msg = Message::notification(0x50);
                let mut fails = 0u64;
                for _ in 0..n(200) {
                    if tx.try_send(&msg).is_err() {
                        fails += 1;
                    }
                    if rx.try_recv().is_none() {
                        fails += 1;
                    }
                }
                ck = ck.wrapping_add(fails);
            } else {
                // peek 循环：槽读与 Acquire 单价分离 Release/推进
                if tx.try_send(&Message::notification(0x51)).is_err() {
                    ck = ck.wrapping_add(1);
                }
                for _ in 0..n(200) {
                    if rx.peek().is_some() {
                        ck = ck.wrapping_add(1);
                    }
                }
                let _ = rx.try_recv();
            }
        }
        M::FRESH_WAIT_RECV => {
            // H8 衰减扫描的 RP 半边：每笔 try_recv 单独计时，只报成功笔。
            // 空转笔对 magic/索引行的反复 Acquire 与生产 D2 自旋同构（行
            // 状态"本核刚读过"）；槽行只在成功笔首次读——新鲜度保留。
            use platform::Timer as _;
            let shm3 = unsafe { SharedMemory::<3>::at(SHM_BASE.load(Ordering::Acquire)) };
            let rx = shm3.receiver(ChannelId::new(0)).expect("ch0 receiver");
            let freq = chip_k3_rt24::clint_k3::TIMER.freq_hz() as u64;
            let deadline = chip_k3_rt24::clint_k3::TIMER.now()
                + if arg == 0 { 200 } else { arg.min(1000) as u64 } * freq / 1000;
            loop {
                let t0 = chip_k3_rt24::clint_k3::TIMER.now();
                let got = rx.try_recv().is_some();
                let dt = chip_k3_rt24::clint_k3::TIMER.now() - t0;
                if got {
                    return (ticks_to_ns(dt), 1);
                }
                if chip_k3_rt24::clint_k3::TIMER.now() > deadline {
                    // 超时快照（D=100µs got=0 两轮确定性复现，2026-08-21）：
                    // RP 视角 read/write 索引 + 队首槽 kind 首字节。r==w ⇒ AP
                    // 发布未落地或回卷到相等（cache.rs 登记的同行回卷竞态
                    // 变可达？）；ck 打包 (w<<32|r) 供 bench 交叉。
                    // SAFETY: 共享窗已初始化（本 op 前序已 probe 认领），
                    // 仅取 ch0 基址做偏移计算，不解引用。
                    let base = unsafe {
                        shm3.channel_unchecked(ChannelId::new(0)) as *const ov_channels::Channel
                            as usize
                    };
                    // SAFETY: 纯 volatile 读共享窗（read@+0x100/write@+0x108，
                    // 槽区 @+0x200 起，均在本通道 0x8200 内）。
                    let r = unsafe { ((base + 0x100) as *const u64).read_volatile() };
                    let w = unsafe { ((base + 0x108) as *const u64).read_volatile() };
                    let kind = unsafe {
                        ((base + 0x200 + ((r as usize) % 128) * 256) as *const u8).read_volatile()
                    };
                    log::info!("[mb] fresh timeout: r={r} w={w} slot[kind]={kind}");
                    return (0, (w << 32) | r);
                }
            }
        }
        M::DISPATCH_N => {
            // op 上下文完整 dispatch（PING 形状：args (u64,)，response 10 元）
            let msg = Message::request(0x5A5A, 3, &(7u64,)).expect("req serialize");
            for k in 0..n(200) {
                match <RtAsyncRpc as ov_rpc::RpcHandler>::handle(3, msg) {
                    Ok(Some(r)) => {
                        let (_rid, _v): (u64, (u64, u8, u64, u64, u64, u64, u64, u64, u64, u64)) =
                            r.as_response().expect("resp shape");
                    }
                    _ => {
                        ck = ck.wrapping_add(k as u64);
                    }
                }
            }
            ck = ck.wrapping_add(1);
        }
        M::NOW_GAPPED => {
            // 单笔"间隔 mtime 读"单价：每轮 t0/now 间隔 ~20µs 忙等。
            use platform::Timer as _;
            let freq = chip_k3_rt24::clint_k3::TIMER.freq_hz() as u64;
            let gap = freq / 50_000; // 20µs
            let mut total = 0u64;
            let t_all = chip_k3_rt24::clint_k3::TIMER.now();
            for _ in 0..n(200) {
                let t0 = chip_k3_rt24::clint_k3::TIMER.now();
                let mut t = chip_k3_rt24::clint_k3::TIMER.now();
                while t < t0 + gap {
                    core::hint::spin_loop();
                    t = chip_k3_rt24::clint_k3::TIMER.now();
                }
                // 忙等后首读 ≈ 冷读；计入总计时（扣 gap 由 bench 判读）
                let t1 = chip_k3_rt24::clint_k3::TIMER.now();
                total = total.wrapping_add(t1.wrapping_sub(t0));
            }
            let span = chip_k3_rt24::clint_k3::TIMER.now() - t_all;
            let _ = span;
            return (ticks_to_ns(total), ticks_to_ns(gap));
        }
        M::CYCLE_GAPPED => {
            // mcycle 间隔读（对照 NOW_GAPPED 的 ~24µs/笔）：忙等固定圈数
            // 12000。板上实测每轮 wall ≈6.18ms（≫忙等预期）且反推 mcycle
            // 恰以 24MHz 计数——mcycle 冷读亦巨慢或与 mtime 同源，热单价
            // 与真实频率由 CYCLE_HOT/CYCLE_CAL 联标定案。
            let mut total = 0u64;
            for _ in 0..n(200) {
                let t0 = read_mcycle();
                for _ in 0..12_000 {
                    core::hint::spin_loop();
                }
                let t1 = read_mcycle();
                total = total.wrapping_add(t1.wrapping_sub(t0));
            }
            return (total, 12_000u64);
        }
        M::CYCLE_HOT => {
            // mcycle 热连读 ×1000：判别"仅冷读慢" vs "读本身慢"
            let mut sink = 0u64;
            let c0 = read_mcycle();
            for _ in 0..1000 {
                sink = sink.wrapping_add(read_mcycle());
            }
            let c1 = read_mcycle();
            let _ = sink;
            return (c1.wrapping_sub(c0), 1000u64);
        }
        M::CYCLE_CAL => {
            // mcycle↔mtime 频率联标（~5ms 忙等，mtime 判据）：返回
            // (cycle 差, mtime 差 ticks)。判 mcycle 是核频计数还是与
            // mtime 同源 24MHz。
            use platform::Timer as _;
            let m0 = chip_k3_rt24::clint_k3::TIMER.now();
            let c0 = read_mcycle();
            let target = m0 + 120_000; // 5ms @ 24MHz
            while chip_k3_rt24::clint_k3::TIMER.now() < target {
                core::hint::spin_loop();
            }
            let c1 = read_mcycle();
            let m1 = chip_k3_rt24::clint_k3::TIMER.now();
            return (c1.wrapping_sub(c0), m1.wrapping_sub(m0));
        }
        M::TMR_SETUP => {
            // soc-timer counter1 自由运行化。CER 与 AP 共享（其对 bit0 做
            // 读改写）：写后回读校验 + 重试，跨核 RMW 竞态一次性收敛；
            // CMR/PLCR(1) AP 初始化后不再触碰，直接写安全。
            use platform::Timer as _;
            let t0 = chip_k3_rt24::clint_k3::TIMER.now();
            let ccr = tmr1_rd(TMR_CCR);
            let cmr = tmr1_rd(TMR_CMR);
            tmr1_wr(TMR_CMR, cmr | (1 << 1));
            tmr1_wr(TMR_PLCR1, 0);
            let mut retries = 0u64;
            for _ in 0..8 {
                let cer = tmr1_rd(TMR_CER);
                tmr1_wr(TMR_CER, cer | (1 << 1));
                if tmr1_rd(TMR_CER) & (1 << 1) != 0 {
                    break;
                }
                retries += 1;
            }
            let cer = tmr1_rd(TMR_CER);
            log::info!(
                "[mb] tmr_setup: cer={:#x} cmr={:#x} ccr={:#x} cr0={:#x} cr1={:#x} retries={retries}",
                cer,
                tmr1_rd(TMR_CMR),
                ccr,
                tmr1_rd(TMR_CR0),
                tmr1_rd(TMR_CR1)
            );
            let ns = ticks_to_ns(chip_k3_rt24::clint_k3::TIMER.now() - t0);
            // ck 打包 (cer<<32 | retries)；ns 含本 op 首笔 mtime 冷读税，
            // 仅作参考（寄存器快照在 RP console log）
            return (ns, ((cer as u64) << 32) | retries);
        }
        M::TMR_HOT => {
            // counter1 热连读 ×n（默认 4000）。mtime 括号首尾 2 笔为冷读
            // （~24.5µs/笔），4000 次摊薄后 ~12ns/笔，判读时扣除
            use platform::Timer as _;
            let t0 = chip_k3_rt24::clint_k3::TIMER.now();
            let mut sink = 0u64;
            for _ in 0..n(4000) {
                sink = sink.wrapping_add(tmr1_rd(TMR_CR1) as u64);
            }
            let t1 = chip_k3_rt24::clint_k3::TIMER.now();
            let _ = sink;
            return (ticks_to_ns(t1.wrapping_sub(t0)), 4000);
        }
        M::TMR_GAPPED => {
            // NOW_GAPPED 同构，仅在 t1 前插入 1 笔 counter1 读：每轮与
            // NOW_GAPPED 的差 = 候选冷读边际成本 Δ（两 op 结构仅差此一笔）
            use platform::Timer as _;
            let freq = chip_k3_rt24::clint_k3::TIMER.freq_hz() as u64;
            let gap = freq / 50_000; // 20µs
            let mut total = 0u64;
            for _ in 0..n(64) {
                let t0 = chip_k3_rt24::clint_k3::TIMER.now();
                let mut t = chip_k3_rt24::clint_k3::TIMER.now();
                while t < t0 + gap {
                    core::hint::spin_loop();
                    t = chip_k3_rt24::clint_k3::TIMER.now();
                }
                // 待测：间隔 ~20µs 后的候选冷读
                ck = ck.wrapping_add(tmr1_rd(TMR_CR1) as u64);
                let t1 = chip_k3_rt24::clint_k3::TIMER.now();
                total = total.wrapping_add(t1.wrapping_sub(t0));
            }
            return (ticks_to_ns(total), ticks_to_ns(gap));
        }
        M::TMR_CAL => {
            // counter1↔mtime 5ms 联标：返回 (counter ticks, mtime ticks)。
            // c1−c0 ≈ 5000（1MHz）且单调 ⇒ 自由运行、未被 AP 重编程打断
            use platform::Timer as _;
            let m0 = chip_k3_rt24::clint_k3::TIMER.now();
            let c0 = tmr1_rd(TMR_CR1) as u64;
            let target = m0 + 120_000; // 5ms @ 24MHz
            let mut m = chip_k3_rt24::clint_k3::TIMER.now();
            while m < target {
                core::hint::spin_loop();
                m = chip_k3_rt24::clint_k3::TIMER.now();
            }
            let c1 = tmr1_rd(TMR_CR1) as u64;
            let m1 = chip_k3_rt24::clint_k3::TIMER.now();
            return (c1.wrapping_sub(c0), m1.wrapping_sub(m0));
        }
        M::TMR_B_SCAN => {
            // d4014000 侦查（候选 B）：两次 CR0 读隔 1ms + CER/CR1 快照。
            // K3 无此块/时钟未开时本读可能总线错误挂死——排表尾即为此
            let a = tmr0_rd(TMR_CR0);
            let cer = tmr0_rd(TMR_CER);
            let cr1 = tmr0_rd(TMR_CR1);
            use platform::Timer as _;
            let t0 = chip_k3_rt24::clint_k3::TIMER.now();
            let target = t0 + 24_000; // 1ms @ 24MHz
            while chip_k3_rt24::clint_k3::TIMER.now() < target {
                core::hint::spin_loop();
            }
            let b = tmr0_rd(TMR_CR0);
            // ns 打包 (第二次 CR0 << 32 | 第一次 CR0)，ck 打包 (CER<<32|CR1)
            return (
                ((b as u64) << 32) | (a as u64),
                ((cer as u64) << 32) | (cr1 as u64),
            );
        }
        M::TMR_CLKON => {
            // 开 TIMERS1 时钟门 + 去复位（复位极性未定，bit2 两变体自动
            // 试，以 CER 写入回读为判定），随后 counter1 自由运行化 +
            // 1ms 计数验证（mux 保留读回值，默认 0 = 12.8MHz）。
            use platform::Timer as _;
            let apbc0 = apbc_rd(APBC_TIMERS1_CLK_RST);
            let keep = apbc0 & !0x7; // 保留 mux 位 [6:4]
            let enable_c1 = || -> (u32, u64) {
                let cmr = tmr1_rd(TMR_CMR);
                tmr1_wr(TMR_CMR, cmr | (1 << 1));
                tmr1_wr(TMR_PLCR1, 0);
                let mut retries = 0u64;
                for _ in 0..8 {
                    let cer = tmr1_rd(TMR_CER);
                    tmr1_wr(TMR_CER, cer | (1 << 1));
                    if tmr1_rd(TMR_CER) & (1 << 1) != 0 {
                        break;
                    }
                    retries += 1;
                }
                (tmr1_rd(TMR_CER), retries)
            };
            // 变体 A：bit2=1 视为"释放复位"（MMP 惯例）
            apbc_wr(APBC_TIMERS1_CLK_RST, keep);
            apbc_wr(APBC_TIMERS1_CLK_RST, keep | 0x7);
            let (mut cer, mut retries) = enable_c1();
            let mut variant = 0u64;
            if cer & (1 << 1) == 0 {
                // 变体 B：bit2=0 视为"释放复位"（保持时钟门开）
                variant = 1;
                apbc_wr(APBC_TIMERS1_CLK_RST, keep | 0x3);
                let (c2, r2) = enable_c1();
                cer = c2;
                retries += r2;
            }
            let apbc1 = apbc_rd(APBC_TIMERS1_CLK_RST);
            // 1ms 计数验证（12.8MHz 预期 Δ≈12800）
            let c0 = tmr1_rd(TMR_CR1) as u64;
            let t0 = chip_k3_rt24::clint_k3::TIMER.now();
            let target = t0 + 24_000; // 1ms @ 24MHz
            while chip_k3_rt24::clint_k3::TIMER.now() < target {
                core::hint::spin_loop();
            }
            let c1 = tmr1_rd(TMR_CR1) as u64;
            let d = c1.wrapping_sub(c0);
            log::info!(
                "[mb] tmr_clkon: apbc {:#x}->{:#x} (variant={variant}) cer={:#x} cr1 Δ={d} retries={retries}",
                apbc0,
                apbc1,
                cer,
            );
            return (apbc1 as u64, ((cer as u64) << 32) | (d & 0xffff_ffff));
        }
        _ => return (0, 0),
    }
    let ns = ticks_to_ns(platform::timer().now().saturating_sub(t0));
    (ns, ck)
}

/// 读 mcycle CSR（0xB00）。用 mcycle 而非 cycle（0xC00）：本核 M 态未实现
/// 用户别名（csrr cycle 触发 Illegal Instruction 打挂固件，板上实锤
/// 2026-08-21）；mcycle 经 rtbench 时钟标定实证可用。
#[cfg(feature = "probe")]
#[inline]
fn read_mcycle() -> u64 {
    let c: u64;
    // SAFETY: 纯 CSR 读（mcycle 计数器），无副作用。
    unsafe { core::arch::asm!("csrr {}, mcycle", out(reg) c, options(nostack)) };
    c
}

// ============================================================================
// soc-timer（AP 域通用定时器，mtime 冷读税替换候选，2026-08-21）
// ============================================================================
//
// 候选来源：docs-chip K3 DS §2.9.3（9× 32 位 TCCRn 向上计数器）+ tgoskits
// AP dts `timer@d4016000`（spacemit,soc-timer，AP 用 counter0 做广播，计数
// 1MHz）+ 上游 timer-k1x.c 驱动（MMP 血统寄存器布局）。AP 只动 counter0，
// counter1/2 空闲且块时钟常开（AP 依赖它）——RP 零门控共享。
//
// 寄存器偏移（timer-k1x.c）：CER=+0x00（bit n=counter n 使能，AP 跨核 RMW
// 仅动 bit0）/ CMR=+0x04（bit n=自由运行模式）/ CCR=+0x0c（块级时钟选择：
// fastclk 或 32kHz，AP 已选 fastclk）/ PLCR(n)=+0x50+(n<<2)（0=自由运行）/
// CR(n)=+0x90+(n<<2)（32 位计数器值，即 DS 的 TCCRn）。
#[cfg(feature = "probe")]
const TMR1_BASE: usize = 0xd401_6000;
/// d4014000 块（K1 dts timer0；K3 存在性未知——TMR_B_SCAN 侦查）
#[cfg(feature = "probe")]
const TMR0_BASE: usize = 0xd401_4000;
#[cfg(feature = "probe")]
const TMR_CER: usize = 0x00;
#[cfg(feature = "probe")]
const TMR_CMR: usize = 0x04;
#[cfg(feature = "probe")]
const TMR_CCR: usize = 0x0c;
#[cfg(feature = "probe")]
const TMR_PLCR1: usize = 0x54;
#[cfg(feature = "probe")]
const TMR_CR0: usize = 0x90;
#[cfg(feature = "probe")]
const TMR_CR1: usize = 0x94;

/// 读 d4016000（AP 共享块）寄存器。
#[cfg(feature = "probe")]
#[inline]
fn tmr1_rd(off: usize) -> u32 {
    // SAFETY: 纯 MMIO 读，无副作用。
    unsafe { ((TMR1_BASE + off) as *const u32).read_volatile() }
}

/// 写 d4016000（AP 共享块）寄存器。仅 probe 探针上下文调用。
#[cfg(feature = "probe")]
#[inline]
fn tmr1_wr(off: usize, v: u32) {
    // SAFETY: 纯 MMIO 写（探针已论证的目标寄存器）。
    unsafe { ((TMR1_BASE + off) as *mut u32).write_volatile(v) }
}

/// 读 d4014000（候选 B，存在性未知）寄存器。
#[cfg(feature = "probe")]
#[inline]
fn tmr0_rd(off: usize) -> u32 {
    // SAFETY: 纯 MMIO 读，无副作用。
    unsafe { ((TMR0_BASE + off) as *const u32).read_volatile() }
}

// APBC（AP 域外设时钟控制器，主线 k3.dtsi syscon_apbc@0xd4015000）。
// 位定义 = 主线 drivers/clk/spacemit/ccu-k3.c：*_CLK_RST 寄存器
// bit0=bus gate / bit1=func gate / bit2=reset / bit[6:4]=源 mux。
#[cfg(feature = "probe")]
const APBC_BASE: usize = 0xd401_5000;
/// TIMERS1（0xd4016000）时钟/复位寄存器偏移（k3-syscon.h）
#[cfg(feature = "probe")]
const APBC_TIMERS1_CLK_RST: usize = 0x44;

#[cfg(feature = "probe")]
#[inline]
fn apbc_rd(off: usize) -> u32 {
    // SAFETY: 纯 MMIO 读，无副作用。
    unsafe { ((APBC_BASE + off) as *const u32).read_volatile() }
}

/// 写 APBC 寄存器。仅 probe 探针上下文（TMR_CLKON 开 TIMERS1 时钟）。
#[cfg(feature = "probe")]
#[inline]
fn apbc_wr(off: usize, v: u32) {
    // SAFETY: 纯 MMIO 写（探针已论证的目标寄存器）。
    unsafe { ((APBC_BASE + off) as *mut u32).write_volatile(v) }
}

/// 当前发现路径标签（D1..D4）。仅 IPC 任务写，handler 读。
static DISCOVERY: AtomicUsize = AtomicUsize::new(path::D1_IRQ_WAKE);

// ============================================================================
// LITMUS：跨核免 fence 顺序性实验（probe 门控，fence 豁免判定，2026-08-17）
// ============================================================================
//
// 背景：K3 实测 fence/AMO 恒 ~2.2µs（fence 矩阵），而普通 load/store
// 16-120ns。第一轮 L1 结果（rounds=0）实锤：**RP 同址重复普通读被前端
// 合并缓冲钉死陈旧值**（60ms 内 200 次跨核写全未看见；生产 D2 自旋能用
// 是因为 Acquire 的 fence 每次强制重取）。因此本组 v2 的目标改为：
// 测出"读新鲜度"的最便宜刷新原语：
//
// - L1（消费侧读新鲜度）：AP 顺序发布 (round, data)，RP 按读模式轮询：
//   mode0 纯 volatile（已证陈旧，作回归对照）；mode1 每读前 fence r,rw
//   （生产 Acquire 等价，预期恢复全量观测）；mode2 先读 scratch 尾部
//   另一行再读目标（测合并项逐出）。正序组 viol=0 即该模式的读新鲜度
//   成立；反序对照验证检测器。
// - L2（生产侧 store-store + 门铃跨域）：RP 免 fence 写 (data, round)
//   后**裸门铃**（绕过 notify 的 fence），AP IRQ 醒来读校验。
//   正序组 0 违例 ⇒ notify fence 可省；反序对照同上。
// - L3（Dekker/store-buffering）：双方免 fence 各写 flag 后读对方，
//   统计读到旧值次数。预期两侧都高 ⇒ clear_busy 后的 SeqCst fence
//   必须保留（或换硬件 spinlock）。
//
// scratch 布局（SHM_SCRATCH_OFF 起）：
//   +0x00 L1.ap_round / +0x08 L1.ap_data / +0x10 L1.rp_echo（RP 回显）
//   +0x20 L2.rp_round / +0x28 L2.rp_data
//   +0x30 L3.ap_flag / +0x38 L3.rp_flag / +0x7f8 L1 mode2 邻址 dummy
//
// 全部 RP 侧访问刻意用纯 volatile（这正是被测对象）；计数经 STATS
// LIT_* 索引读取。AP 侧驱动见 user-test-bench `lit` 场景（镜像对齐义务）。

/// LITMUS 操作码（ABI：AP 侧 user-test-bench 镜像同值）。
#[cfg(feature = "probe")]
pub mod litmus_op {
    /// L1：RP 轮询消费 arg 毫秒（0→100），校验 AP 发布的 (round,data)
    pub const L1_POLL: u32 = 0;
    /// L2：RP 发布 arg 低 16 位轮数，bit16=1 正序（data 先）否则反序对照
    pub const L2_PUBLISH: u32 = 1;
    /// L3：Dekker，arg 轮（0→1000，上限 100k）
    pub const L3_DEKKER: u32 = 2;
}

/// L1 数据混淆函数（AP 侧镜像）。
#[cfg(feature = "probe")]
fn lit_mix_l1(r: u64) -> u64 {
    r ^ 0xA5A5_5A5A_5A5A_5A5A
}

/// L2 数据混淆函数（AP 侧镜像）。
#[cfg(feature = "probe")]
fn lit_mix_l2(k: u64) -> u64 {
    k.wrapping_mul(2654435761)
}

/// 执行一次 LITMUS 实验（结果入 STATS LIT_*，见模块文档）。
#[cfg(feature = "probe")]
fn run_litmus(op: u32, arg: u32) {
    use litmus_op as L;
    use platform::Timer as _;
    // clint 直连（Slot 路径 ~3µs/次会淹没轮询循环）。
    let now = || chip_k3_rt24::clint_k3::TIMER.now();
    let freq = chip_k3_rt24::clint_k3::TIMER.freq_hz() as u64;
    let base = SHM_BASE.load(Ordering::Relaxed) + SHM_SCRATCH_OFF;
    // SAFETY: scratch 为窗口尾部空闲区（见 SHM_SCRATCH_OFF 注释），仅本
    // 实验与 AP 侧 bench 访问；volatile 保序由编译器保证单线程程序序。
    let rd = |off: usize| unsafe { ((base + off) as *const u64).read_volatile() };
    let wr = |off: usize, v: u64| unsafe { ((base + off) as *mut u64).write_volatile(v) };

    stats::set(stat_idx::LIT_VIOL, 0);
    stats::set(stat_idx::LIT_ROUNDS, 0);
    stats::set(stat_idx::LIT_STATE, 1);

    match op {
        L::L1_POLL => {
            // arg = ms（低 16 位，0→100）| 读模式（bit16-17：0 纯读 /
            // 1 每读前 fence / 2 先读邻址再读目标）。
            let ms = if arg & 0xffff == 0 { 100 } else { (arg & 0xffff) as u64 };
            let mode = (arg >> 16) & 3;
            let deadline = now() + ms * freq / 1000;
            let mut last = rd(0x00);
            let mut rounds = 0u64;
            let mut viol = 0u64;
            loop {
                let r = match mode {
                    // SAFETY: 纯屏障指令（生产 Acquire 读的等价刷新）。
                    1 => unsafe {
                        core::arch::asm!("fence r, rw", options(nostack));
                        rd(0x00)
                    },
                    // 邻址逐出探测：先读 scratch 尾部另一行再读目标。
                    2 => {
                        let _ = rd(0x7f8);
                        rd(0x00)
                    }
                    _ => rd(0x00),
                };
                if r != last {
                    let d = rd(0x08);
                    // 复读 round：若读取期间 AP 已前进到下一轮，本样本的
                    // data 无法归因（可能是更新一轮的数据），跳过防假阳性。
                    if rd(0x00) != r {
                        last = rd(0x00);
                        continue;
                    }
                    if d != lit_mix_l1(r) {
                        viol += 1;
                    }
                    last = r;
                    rounds += 1;
                    wr(0x10, r);
                }
                if now() >= deadline {
                    break;
                }
            }
            stats::set(stat_idx::LIT_VIOL, viol);
            stats::set(stat_idx::LIT_ROUNDS, rounds);
            // 轮数是否齐全由 bench 对照判读；RP 侧只有超时一种结束方式。
            stats::set(stat_idx::LIT_STATE, 3);
        }
        L::L2_PUBLISH => {
            let rounds = (arg & 0xffff).max(1) as u64;
            let proper = arg & (1 << 16) != 0;
            use platform::device::Mailbox as _;
            // 节奏 500µs：AP IRQ→读校验 ~150µs/轮 + 内核路径余量；
            // 第一轮曾在 200µs 节奏下 AP 侧 await 挂死（v1 事故），放宽。
            let pace = freq / 2000;
            for k in 0..rounds {
                let d = lit_mix_l2(k);
                if proper {
                    wr(0x28, d);
                    wr(0x20, k);
                } else {
                    wr(0x20, k);
                    wr(0x28, d);
                }
                // 裸门铃：绕过 PeerNotifier::notify 的 fence iorw,iorw——
                // 「数据/索引落 SRAM 先于 mailbox FIFO 写」正是本组要测的。
                chip_k3_rt24::mailbox::MBX3.signal(1);
                let until = now() + pace;
                while now() < until {
                    core::hint::spin_loop();
                }
            }
            stats::set(stat_idx::LIT_ROUNDS, rounds);
            stats::set(stat_idx::LIT_STATE, 2);
        }
        L::L3_DEKKER => {
            let rounds = (if arg == 0 { 1000 } else { arg as u64 }).min(100_000);
            let mut stale = 0u64;
            let mut last_ap = rd(0x30);
            for k in 0..rounds {
                wr(0x38, k + 1);
                let ap = rd(0x30);
                if ap == last_ap {
                    stale += 1;
                } else {
                    last_ap = ap;
                }
            }
            stats::set(stat_idx::LIT_VIOL, stale);
            stats::set(stat_idx::LIT_ROUNDS, rounds);
            stats::set(stat_idx::LIT_STATE, 2);
        }
        _ => {
            stats::set(stat_idx::LIT_STATE, 0);
        }
    }
}

/// 本次 process_elastic 入口时间戳（≈ await 返回、任务恢复执行，D1 的 t_sched）。
static T_SCHED: AtomicU64 = AtomicU64::new(0);

/// 唤醒周期汇总日志的门控计数（portable-atomic：K3 专属 target 下 core
/// RMW 被 cfg 掉，CS 后端 ~90ns/笔；标准 target 上别名 core 原生）。
static LOG_COUNT: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);

/// 非_rpc 消息日志的门控计数（通知回显是压力测试热路径）。
static NOTIF_LOG_COUNT: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);

/// 热路径日志周期：115200 阻塞 UART 每条毫秒级，逐条打印既污染延迟
/// 数据又拉长竞态窗口，故每 N 个事件才打一条。
const LOG_PERIOD: u64 = 4096;

// ============================================================================
// RPC Server 实例
// ============================================================================

/// `ov_shm::shm::base()` 在 `init()` 时落地。`RpcServer::new` 是 `const fn`
/// 且仅存基址，故运行期每次调用构造即可（无共享内存访问）。
/// `init()` 必须先于其他 `intercom` 函数调用，否则读到未初始化的共享内存。
// 哨兵 usize::MAX：init 前的「未初始化」标记——②c 后基址本身可为 0
// （RCPU 本地别名窗口），不能用 0 做哨兵。
static SHM_BASE: AtomicUsize = AtomicUsize::new(usize::MAX);

/// 弹性忙等自旋上限。
///
/// 每次无消息后自旋此次数；若期间收到新消息则重新处理。
/// **K3 实测（2026-08-17，STATS WIN_*）：100000 次自旋 ≈ 2.0 s**——
/// 每次迭代含 2 组无缓存 SRAM 读（ch0/ch2 索引），单次 ~20µs，远高于
/// 按主频估算的预期。后果：间隔 < 2s 的稳态流量下 RP 永不入睡（全 D2），
/// 空闲后也要自旋满 2s 才 WFI。若需缩短（功耗/唤醒权衡），按比例调小
/// （如 2500 ≈ 50ms）；实测窗口时长以 STATS 的 WIN_* 计数器为准。
const ELASTIC_SPIN_LIMIT: u32 = 100000;

// ============================================================================
// 公共 API
// ============================================================================

/// 装配 dispatch 分解戳时钟（clint mtime 直连——Slot 路径 ~3µs/次）。
/// 见 ov-rpc feature "stamps"（经 `probe` 传递）。
#[cfg(feature = "probe")]
fn install_stamp_clock() {
    use platform::Timer as _;
    ov_rpc::stamp::set_clock(|| chip_k3_rt24::clint_k3::TIMER.now());
}

/// 初始化共享内存（boot 期职责已迁至 AP 内核 rt_shm 驱动 probe 期，
/// 见 [`wait_ready`]；本函数现仅服务于 magic 自愈看门狗与旧内核回退）。
pub fn init() {
    #[cfg(feature = "probe")]
    install_stamp_clock();
    let base = ov_shm::shm::base();
    unsafe {
        let shm = SharedMemory::<3>::at(base);
        shm.init();
    }
    // 先 init 再发布基址：`MachineSoft` ISR 可能在本函数执行期间触发
    // （`ipc_wait::notify_from_isr` → `has_pending`），若基址先行发布
    // 而共享内存尚未初始化，ISR 会读到未初始化的 ring buffer 索引。
    SHM_BASE.store(base, Ordering::Release);
    log::info!("[InterCom] initialized at {:#x}", base);
}

/// 等待 AP 侧 rt_shm 驱动完成共享窗初始化（init 职责已迁至 AP 内核侧）。
///
/// 窗口的三个破坏者——SPL 执行期、U-Boot `k3_clear_sram()` memset（在
/// bootm 之前）、bootm 换核缓存 flush 写回——全部先于 AP 内核启动，故本
/// 函数等到 valid 即确定性处于安全期。本侧全程只读（RP 直达 SRAM 无
/// 缓存，读到即真值）；3s 读门控防 SPL 文本字节构成伪 magic（届时窗已被
/// U-Boot memset 清零）。
///
/// `fallback_ms` 内未检测到 valid（如对端为无 probe 期 init 的旧内核）
/// 则回退本地 [`init`] 并打警告——新固件 + 旧内核组合不致永久等待。
pub async fn wait_ready(poll_ms: u64, fallback_ms: u64) {
    // 读门控：窗口头 ~1s 是 SPL 代码、~1.5s 被 U-Boot memset。
    futures::timer::after(crate::watchdog::WRITE_GATE_MS.millis()).await;

    let base = ov_shm::shm::base();
    assert!(base != usize::MAX, "[InterCom] ov-shm 共享窗未 probe（DT 节点缺失？）");
    // SAFETY: base 来自 DT probe 的保留区，映射与设备同生命周期，跨
    // await 持有安全；本函数对其只读（is_valid）。
    let shm = unsafe { SharedMemory::<3>::at(base) };

    let start = platform::timer().now();
    let freq = platform::timer().freq_hz() as u64;
    loop {
        if shm.is_valid() {
            // 关键：此路径不经过本地 init()，必须在此发布基址——
            // server()/process_elastic 依赖 SHM_BASE，漏存则它们始终读到
            // 哨兵值（has_pending 恒 false，RPC 静默失联）。
            #[cfg(feature = "probe")]
            install_stamp_clock();
            SHM_BASE.store(base, Ordering::Release);
            log::info!("[InterCom] AP-side init detected, service online");
            return;
        }
        let elapsed_ns = (platform::timer().now() - start) * 1_000_000_000 / freq;
        if elapsed_ns >= fallback_ms * 1_000_000 {
            log::warn!(
                "[InterCom] AP 侧 {fallback_ms}ms 未 init（旧内核？），回退本地 init"
            );
            init();
            return;
        }
        futures::timer::after(poll_ms.millis()).await;
    }
}

/// magic 自愈看门狗（shm_ping）每次 re-init 前调用，记录自愈事件数。
pub fn note_magic_heal() {
    stats::bump(stat_idx::HEALS);
}

/// 检查是否有待处理消息
///
/// # Preconditions
///
/// `init()` must have been called before this function, otherwise this will
/// read from uninitialized shared memory.
///
/// 注意：`MachineSoft` ISR（`ipc_wait::notify_from_isr`）会早于 `task_ipc`
/// 首次 poll 调用本函数（spawn 时 `pend()` 已写 MSIP，`intercom::init()` 尚未
/// 执行）。此时 `SHM_BASE` 仍为哨兵值，构造 `RpcServer` 会解引用无效地址
/// （LoadFault）。故未 init 时安全返回 false（无待处理消息）。
pub fn has_pending() -> bool {
    // Relaxed 足够：SHM_BASE 由 init（同 hart 串行）一次写入后只读，
    // 同 hart 程序序保证可见——Acquire 读在此 SoC 上是 ~2.1µs 的 fence
    // 开销，而本函数在弹性轮询循环里每迭代调用（D2 路径热）。
    let base = SHM_BASE.load(Ordering::Relaxed);
    if base == usize::MAX {
        return false;
    }
    RpcServer::new(base).has_pending()
}

/// 向对端核心发送通知 IPI（经 ov-shm 的 notifier 设备，DT 配置后端）。
///
/// 对端 IPI handler 唤醒任何阻塞在 `AWAIT` 的任务，该任务随后直接检查
/// CH1 的 ring buffer 判断消息是否可用——ring buffer 是唯一真相源，
/// 无需中间计数器。
#[inline]
fn send_notify_ipi() {
    ov_shm::notifier::notifier().notify();
}

/// 弹性忙等处理：处理所有消息并在弹性窗口内自旋等待更多请求。
///
/// 工作流程:
/// 1. 设置 BUSY 标志
/// 2. 循环处理所有待处理消息，每个 Notify 响应立即发 IPI
/// 3. 无消息时弹性自旋等待 `ELASTIC_SPIN_LIMIT` 次
/// 4. 自旋期间若收到新消息，重新处理
/// 5. 弹性窗口过期后，清除 BUSY 并做最终竞争检查
///
/// # IPI 策略
///
/// 每个 Notify 响应写入 CH1 后立即调用 `send_notify_ipi()`：直接写
/// MSIP0 触发 Linux 侧中断。Linux IPI handler 仅唤醒阻塞任务，由
/// `await_ipi` 直接读取 CH1 ring buffer 判断是否有消息——无需中间计数器，
/// 彻底消除了计数与实际消息数不匹配的死锁风险。
///
/// # 插桩（测试基础设施）
///
/// 入口设 D1 标签 + `T_SCHED`；批处理续批设 D3；自旋命中设 D2；最终
/// 竞争检查设 D4；完整耗尽的窗口实测时长入 WIN_* 计数器；窗口期间到达
/// 的 mailbox IRQ 计入 REDUNDANT_IRQ（D5 冗余门铃）。每条消息的服务时长
/// 入 SVC_* 计数器。处理语义与 `RpcServer::process_all` 严格一致
/// （先排空 urgent，再排空普通），仅叠加计数与时间戳。
///
/// 返回本次唤醒周期消费的消息数量。
///
/// # Preconditions
///
/// `init()` must have been called before this function, otherwise this will
/// access uninitialized shared memory.
pub fn process_elastic() -> usize {
    let shm = unsafe { SharedMemory::<3>::at(ov_shm::shm::base()) };

    // D1 分段基准：本函数入口 ≈ MBX3.recv().await 返回、IPC 任务恢复执行。
    // ①′：clint 直连（每唤醒周期 3 次 timer()，Slot 路径 3µs/次）。
    use platform::Timer as _;
    let t_enter = chip_k3_rt24::clint_k3::TIMER.now();
    T_SCHED.store(t_enter, Ordering::Relaxed);
    DISCOVERY.store(path::D1_IRQ_WAKE, Ordering::Relaxed);

    // 1. 标记忙等
    shm.set_busy();
    let irq_ts_at_entry = chip_k3_rt24::mailbox::LAST_IRQ_TS.load(Ordering::Relaxed);

    let mut total_count = 0usize;

    loop {
        // 2. 处理所有待处理消息，每个 Notify 立即回 IPI
        let n = drain_all();
        total_count += n;

        if n > 0 {
            // 有工作完成，立即检查更多（不经自旋）；后续批次为 D3 追加
            DISCOVERY.store(path::D3_BATCH_APPEND, Ordering::Relaxed);
            continue;
        }

        // 3. 无消息，弹性自旋
        let win_start = chip_k3_rt24::clint_k3::TIMER.now();
        let mut spun = 0u32;
        while spun < ELASTIC_SPIN_LIMIT {
            if server().has_pending() || server().has_urgent() {
                break;
            }
            spun += 1;
            core::hint::spin_loop();
        }

        if spun < ELASTIC_SPIN_LIMIT {
            // 自旋期间收到新消息，重新处理（该批为 D2 自旋命中）
            DISCOVERY.store(path::D2_SPIN_HIT, Ordering::Relaxed);
            continue;
        }

        // 4. 弹性窗口完整耗尽，准备睡眠：记录实测窗口时长（S0 标定数据源）
        let win_ns = ticks_to_ns(chip_k3_rt24::clint_k3::TIMER.now().saturating_sub(win_start));
        stats::set(stat_idx::WIN_LAST_NS, win_ns);
        stats::min(stat_idx::WIN_MIN_NS, win_ns);
        stats::max(stat_idx::WIN_MAX_NS, win_ns);
        stats::bump(stat_idx::WINDOWS);
        break;
    }

    // D5 冗余门铃观测：弹性窗口期间（BUSY=1）到达过 mailbox IRQ——AP 端
    // 此刻本应读到 BUSY=1 跳过 IPI，窗口内来 IRQ 即冗余/竞态门铃。
    // 注意这是"按窗口"的布尔计数（多次 IRQ 计 1），精确发生率以 AP 侧
    // `sent_ipi && tag != D1` 交叉统计为准。
    if chip_k3_rt24::mailbox::LAST_IRQ_TS.load(Ordering::Relaxed) != irq_ts_at_entry {
        stats::bump(stat_idx::REDUNDANT_IRQ);
    }

    // 5. 清除 BUSY 标志（Release 语义）
    shm.clear_busy();

    // 6. 全内存屏障 + 最终竞争检查
    //    防止客户端写请求与清除 BUSY 之间的竞争：
    //    如果客户端在 clear_busy() 之后才读到 BUSY=0，则客户端会发 IPI；
    //    如果客户端在 clear_busy() 之前读了 BUSY=1（跳过 IPI），
    //    则此处的 fence 保证我们能看到客户端的请求。
    core::sync::atomic::fence(Ordering::SeqCst);

    if server().has_pending() || server().has_urgent() {
        // 竞争窗口内收到请求，重新处理（D4 竞态闭环）。
        // 不再设置 BUSY：服务端即将睡眠，客户端看到 BUSY=0 后会发 IPI 唤醒。
        DISCOVERY.store(path::D4_RACE_CLOSE, Ordering::Relaxed);
        let n = drain_all();
        total_count += n;
    }

    // 热路径日志门控：每 LOG_PERIOD 个唤醒周期打一条汇总
    let cycles = LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if cycles % LOG_PERIOD == 0 {
        log::info!(
            "[InterCom] {} wake cycles, {} msgs total, last window {} msgs",
            cycles,
            stats::get(stat_idx::MSGS),
            total_count
        );
    }
    total_count
}

/// 排空 urgent 与普通通道（语义对齐 `RpcServer::process_all`：先全部
/// urgent，再全部普通），每条消息经 [`step`] 插桩。返回消费的消息数。
fn drain_all() -> usize {
    let mut count = 0usize;
    while step(true) {
        count += 1;
    }
    while step(false) {
        count += 1;
    }
    count
}

/// 处理一条消息并插桩：服务时长（取出 → 响应写入完成）、按发现路径标签
/// 计数、Notify 响应回 IPI、非 RPC 消息转 [`handle_non_rpc`]。
/// 返回是否消费了一条消息。
///
/// Notify 的 IPI 在 process_one 返回（响应已写入 CH1）之后发出，
/// 与 `process_all` 的 on_notify 时机一致。
fn step(urgent: bool) -> bool {
    // ①′：clint 直连（每消息 2 次 timer() + ticks_to_ns 内 1 次 freq）。
    use platform::Timer as _;
    let t0 = chip_k3_rt24::clint_k3::TIMER.now();
    let r = if urgent {
        server().process_urgent::<RtAsyncRpc>()
    } else {
        server().process_one::<RtAsyncRpc>()
    };
    if matches!(r, ProcessResult::NoMessage) {
        return false;
    }

    let svc_ns = ticks_to_ns(chip_k3_rt24::clint_k3::TIMER.now().saturating_sub(t0));
    stats::set(stat_idx::SVC_LAST_NS, svc_ns);
    stats::min(stat_idx::SVC_MIN_NS, svc_ns);
    stats::max(stat_idx::SVC_MAX_NS, svc_ns);
    stats::bump(stat_idx::MSGS);
    match DISCOVERY.load(Ordering::Relaxed) {
        path::D1_IRQ_WAKE => stats::bump(stat_idx::D1_IRQ_WAKE),
        path::D2_SPIN_HIT => stats::bump(stat_idx::D2_SPIN_HIT),
        path::D3_BATCH_APPEND => stats::bump(stat_idx::D3_BATCH_APPEND),
        path::D4_RACE_CLOSE => stats::bump(stat_idx::D4_RACE_CLOSE),
        _ => {}
    }

    match r {
        ProcessResult::Handled(HandledKind::Notify) => send_notify_ipi(),
        ProcessResult::NotRpc(msg) => handle_non_rpc(msg),
        _ => {}
    }
    true
}

/// mtime tick → ns。freq_hz() 返回编译期常量（24MHz），无额外 MMIO。
fn ticks_to_ns(t: u64) -> u64 {
    // ①′：clint 直连（与 Slot 内同一 TIMER 实例；被每消息/每窗口调用）。
    use platform::Timer as _;
    t * 1_000_000_000 / chip_k3_rt24::clint_k3::TIMER.freq_hz() as u64
}

fn handle_non_rpc(msg: Message) {
    match msg.ty() {
        Some(MsgType::Notification) => {
            if let Some(id) = msg.as_notification() {
                // 通知回显是压力测试热路径，逐条打印以毫秒级阻塞 UART。
                let n = NOTIF_LOG_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                if n % LOG_PERIOD == 0 {
                    log::info!("[InterCom] notification #{} (id={}, latest)", n, id);
                }
                send_notification(id);
            }
        }
        Some(MsgType::Data) => {
            if let Some(data) = msg.as_data() {
                log::info!("[InterCom] data: {} bytes", data.len());
            }
        }
        _ => {}
    }
}

/// 向 StarryOS 发送消息
///
/// # Preconditions
///
/// `init()` must have been called before this function, otherwise this will
/// access uninitialized shared memory.
pub fn send_message(msg: Message) {
    unsafe {
        let shm = SharedMemory::<3>::at(ov_shm::shm::base());
        match shm.sender(ChannelId::new(1)) {
            Ok(tx) => {
                if let Err(e) = tx.try_send(&msg) {
                    log::warn!("[InterCom] send failed: {:?}", e);
                } else {
                    send_notify_ipi();
                }
            }
            Err(e) => {
                log::warn!("[InterCom] sender acquisition failed: {:?}", e);
            }
        }
    }
}

/// 向 StarryOS 发送通知
pub fn send_notification(id: u32) {
    let msg = Message::notification(id);
    send_message(msg);
}

/// 获取 RPC Server（基于当前 SHM 基址构造）
///
/// # Preconditions
///
/// `init()` must have been called before using the returned server to
/// process messages, otherwise shared memory will be uninitialized.
pub fn server() -> RpcServer {
    RpcServer::new(SHM_BASE.load(Ordering::Acquire))
}
