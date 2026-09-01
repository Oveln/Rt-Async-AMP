//! 路径分离 IPC 延迟基准（K3 真板；QEMU 固件无 PING/STATS/MEMBENCH
//! 插桩服务，不适用）
//!
//! 与 RP 侧 intercom 插桩（PING/STATS 服务）配对，把每条请求的"发现路径"
//! 分桶测量：RP 察觉请求的方式决定延迟构成——
//!
//! | 标签 | 路径 | 延迟构成 |
//! |------|------|----------|
//! | D1 | 睡眠中被 mailbox IRQ 唤醒（AP 发了门铃） | ioctl+CBO 冲刷 → mailbox → ISR → 调度 → 读环 |
//! | D2 | 弹性自旋轮询发现（AP 读 BUSY=1 跳过门铃） | 仅自旋轮询周期 |
//! | D3 | 批处理循环中追加（背靠背请求） | 排队等前序 handler |
//! | D4 | clear_busy 后 fence 闭环补处理 | 剩余自旋 + 重查 |
//!
//! 每样本记录（AP 时钟）RTT、发送段耗时，与（RP 时钟，经 PING 回传）
//! t_isr/t_sched/t_seen 分段。两套时钟独立报告，不做跨钟对齐。
//!
//! ## 用法
//!
//! ```text
//! user-test-bench <scenario> [iterations] [interval_ns] [warmup]
//!   s0   标定：dump RP 计数器 + 弹性窗口时长 W 及其稳定性
//!   s1   空闲唤醒：间隔默认 2×W，要求 100% D1
//!   s2   自旋命中：间隔默认 W/4（钳 20µs..200µs），要求 ≥90% D2
//!   s4   竞态扫描：间隔在 (0, 2W) 随机均匀（D4 命中率 + 闭环正确性）
//!   s6   边界流：间隔默认 W（D1/D2 混合，量化冗余门铃率）
//!   raw  自由间隔：interval_ns 必填
//!   mb   RP 内存/MMIO 微基准（iterations = 行级操作次数，默认 2000）：
//!        检验「无缓存 SRAM ~3.3µs/笔、256B 消息取读 ~105µs」等延迟归因
//!        假设——dsched 69.8µs / dseen 110.5µs 的解释候选（2026-08-17）
//!   dd   D1 分解交叉测量（iterations = 轮数默认 30，间隔默认 2W）：
//!        PING 6 戳 + 内核门铃前/IRQ 入口戳，钟差无关恒等式拆出门铃
//!        投递 X+Y、AP 回程；ddrain 拆 dsched 为 ISR 舞步 + 派发两段
//! iterations 默认 1000（mb 为 2000、dd 为 30），warmup 默认 50。
//! ```
//!
//! ## 正确性保障（退出码）
//!
//! - 0：全部通过（回显/rid/标签合法/无丢失/计数器对账/场景纯度达标）
//! - 2：看门狗超时（RP 未回包，AWAIT 被SIGALRM 打断——内核 AWAIT 是
//!      interruptible 的，挂死不再需要复位板子）
//! - 3：数据错误（回显/rid/标签非法、消息丢失、RP 计数器对账不符）
//! - 4：发送背压异常（CH0 满，单请求在途不应发生）
//!
//! 端到端对账原理：bench 是 CH0 唯一生产者，每完成一轮（收到响应）
//! 时 RP 已处理的消息总数必等于 bench 累计发送轮数；快照按"MSGS 最后
//! 读、读后立即标记轮数"取值，故 `msgs_delta == rounds_delta` 与
//! `Σd1..d4 == msgs_delta` 必须精确成立——任何丢失/漏计/串包都会破坏。
//!
//! ## 输出
//!
//! stdout：人类可读汇总 + `# ---- csv begin/end ----` 分隔的逐样本 CSV
//! （列：seq,tag,sent_ipi,rtt_ns,send_ns,sysc,ddrain_ns,dsched_ns,ddisp_ns,
//! dseen_ns,isr,drain,sched,seen）。
//! 环境变量 `BENCH_CSV=/tmp/x.csv` 时另存纯 CSV 文件。
//!
//! ## 缓存维护形态（无）
//!
//! 共享窗经 PMA 物理非缓存（opensbi-k3 feat/pma-audio-io 固件翻转 entry，
//! 2026-08-26 板测三判据闭环）：读写直达 SRAM，本程序与内核均不做任何
//! CBO 缓存维护（原 user-cbo 按行维护与内核整窗同步点已全部撤除）。
//! A4 丢发布（X100 cbo.flush 偶发静默丢失在途 store，同核不可检测）按
//! 设计决策（08-23）**不设运行时恢复**：发送路径保持单遍发布，IPI 等待
//! 挂死由既有心跳超时（SIGALRM 10s，退出码 2）给出诊断退出。该问题
//! 是否真实存在由 fresh_scan（mb 场景）检测——超时时交叉 AP 缓存 /
//! AP 失效回读 / RP SRAM 三方索引视角给出逐档判定。

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::IntoRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ov_channels::{ChannelId, Message, SharedMemory};

// ── 协议常量（与 RP 侧 apps/rt-async-k3/src/intercom.rs 镜像，双端对齐义务）──

/// RtAsyncRpc::PING
const M_PING: u64 = 4;
/// RtAsyncRpc::STATS
const M_STATS: u64 = 5;
/// RtAsyncRpc::MEMBENCH
const M_MEMBENCH: u64 = 6;
/// RtAsyncRpc::LITMUS（单向 send：结果经 STATS LIT_* 轮询）
const M_LITMUS: u64 = 7;

/// PING 响应：(val, tag, t_isr, t_drain, t_sched, t_seen)。后四位是 RP 侧
/// mtime 分段时间戳（见固件 intercom::ping 文档；原 stamps 分解戳已删）。
type PingResp = (u64, u8, u64, u64, u64, u64);

/// MEMBENCH 操作码（镜像 intercom::membench_op，双端对齐义务）。
#[allow(dead_code)]
mod mb_op {
    pub const RD_LINE_SHM: u32 = 0;
    pub const RD_BLK256_SHM: u32 = 1;
    pub const RD_BLK8_SHM: u32 = 2;
    pub const WR_LINE_SHM: u32 = 3;
    pub const RD_LINE_LOCAL: u32 = 4;
    pub const RD_BLK256_LOCAL: u32 = 5;
    pub const WR_LINE_LOCAL: u32 = 6;
    pub const RD_STRIDE_SHM: u32 = 7;
    pub const RD_MMBOX_MSGSTAT: u32 = 8;
    pub const RD_MMBOX_IRQEN: u32 = 9;
    pub const RD_MMBOX_FIFOSTAT: u32 = 10;
    pub const RD_MTIME: u32 = 11;
    pub const AQ_LOAD_SHM: u32 = 12;
    pub const REL_STORE_SHM: u32 = 13;
    pub const FENCE_ONLY: u32 = 14;
    pub const SPIN_ITER: u32 = 15;
    pub const PEEK_T: u32 = 16;
    pub const AQ_LOAD_LOCAL: u32 = 17;
    pub const RECV_EMPTY: u32 = 18;
    pub const TIMER_NOW: u32 = 19;
    // L0 归因闭合组（dseen 75µs 无主部分的三假设探针）
    pub const AQ_DISTINCT_SHM: u32 = 20;
    pub const COLD_AQ_SHM: u32 = 21;
    pub const RECV_SEQ: u32 = 22;
    pub const SEND_SEQ: u32 = 23;
    pub const POSTCARD_RT: u32 = 24;
    pub const RECV_EMPTY_CH: u32 = 25;
    pub const NOTIFY_N: u32 = 26;
    pub const SELF_ROUND: u32 = 27;
    pub const SELF_PEEK: u32 = 28;
    pub const FRESH_WAIT_RECV: u32 = 29;
    pub const DISPATCH_N: u32 = 30;
    pub const NOW_GAPPED: u32 = 31;
    pub const CYCLE_GAPPED: u32 = 32;
    pub const CYCLE_HOT: u32 = 33;
    pub const CYCLE_CAL: u32 = 34;
    pub const TMR_SETUP: u32 = 35;
    pub const TMR_HOT: u32 = 36;
    pub const TMR_GAPPED: u32 = 37;
    pub const TMR_CAL: u32 = 38;
    pub const TMR_B_SCAN: u32 = 39;
    pub const TMR_CLKON: u32 = 40;
    pub const TMR_RT_ON: u32 = 41;
    pub const TMR_AON_CAL: u32 = 42;
    pub const TMR_AON_HOT: u32 = 43;
    pub const TMR_AON_GAPPED: u32 = 44;
    pub const TMR_AON_SCAN: u32 = 45;
}

/// MEMBENCH stride 扫描的 RP 侧 scratch 长度（镜像 SHM_SCRATCH_LEN）。
const MB_SCRATCH_LEN: u64 = 0x800;
/// MEMBENCH/LITMUS scratch 偏移（镜像 intercom::SHM_SCRATCH_OFF）。
const MB_SCRATCH_OFF: usize = 0x18700;

/// LITMUS 操作码（镜像 intercom::litmus_op，双端对齐义务）。
mod lit_op {
    pub const L1_POLL: u32 = 0;
    pub const L2_PUBLISH: u32 = 1;
    pub const L3_DEKKER: u32 = 2;
}

/// 发现路径标签：D1 中断唤醒
const TAG_D1: u8 = 1;

/// STATS 计数器索引（镜像 intercom::stat_idx，双端对齐义务）。
#[allow(dead_code)]
mod stat_idx {
    pub const MSGS: usize = 0;
    pub const D1_IRQ_WAKE: usize = 1;
    pub const D2_SPIN_HIT: usize = 2;
    pub const D3_BATCH_APPEND: usize = 3;
    pub const D4_RACE_CLOSE: usize = 4;
    pub const REDUNDANT_IRQ: usize = 5;
    pub const RESP_FAIL: usize = 6;
    pub const HEALS: usize = 7;
    pub const WIN_LAST_NS: usize = 8;
    pub const WIN_MIN_NS: usize = 9;
    pub const WIN_MAX_NS: usize = 10;
    pub const WINDOWS: usize = 11;
    pub const SVC_LAST_NS: usize = 12;
    pub const SVC_MIN_NS: usize = 13;
    pub const SVC_MAX_NS: usize = 14;
    pub const T_NOW: usize = 15;
    pub const FREQ_HZ: usize = 16;
    pub const LIT_VIOL: usize = 17;
    pub const LIT_ROUNDS: usize = 18;
    pub const LIT_STATE: usize = 19;
}

const STAT_COUNT: usize = 20;

const STAT_NAMES: [&str; STAT_COUNT] = [
    "msgs", "d1", "d2", "d3", "d4", "redundant_irq", "resp_fail", "heals",
    "win_last_ns", "win_min_ns", "win_max_ns", "windows",
    "svc_last_ns", "svc_min_ns", "svc_max_ns", "t_now", "freq_hz",
    "lit_viol", "lit_rounds", "lit_state",
];

// K3/QEMU 共享窗大小同值 0x19000（真源设备树，见 rtshm-abi）。
const SHM_SIZE: usize = rtshm_abi::K3_SHM_SIZE;

const CH0: ChannelId = ChannelId::new(0);
const CH1: ChannelId = ChannelId::new(1);

// ── SHM / ioctl ─────────────────────────────────────────────

fn do_ioctl(fd: libc::c_int, cmd: libc::c_ulong, arg: libc::c_ulong) -> io::Result<libc::c_int> {
    let ret = unsafe { libc::ioctl(fd, cmd as _, arg) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

struct RtShm {
    fd: libc::c_int,
    ptr: *mut std::ffi::c_void,
    /// 本轮 syscall 计数（notify/await 各 +1，诊断路径不计）。
    sysc: std::cell::Cell<u32>,
}

impl RtShm {
    fn open() -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open("/dev/rt_shm")?;
        let fd = file.into_raw_fd();
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                SHM_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            let err = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err);
        }
        Ok(Self { fd, ptr, sysc: std::cell::Cell::new(0) })
    }

    fn shm(&self) -> &SharedMemory<3> {
        unsafe { &*(self.ptr as *const SharedMemory<3>) }
    }

    fn shm_ptr(&self) -> usize {
        self.ptr as usize
    }

    /// NOTIFY 门铃（纯门铃，无缓存维护语义——窗口 PMA 物理非缓存）。
    fn notify(&self) -> io::Result<()> {
        self.sysc.set(self.sysc.get() + 1);
        do_ioctl(self.fd, rtshm_abi::IOC_NOTIFY as libc::c_ulong, 0).map(|_| ())
    }

    fn clear_pending(&self) -> io::Result<()> {
        do_ioctl(self.fd, rtshm_abi::IOC_CLR_PENDING as libc::c_ulong, 0).map(|_| ())
    }

    fn await_ipi(&self) -> io::Result<()> {
        self.sysc.set(self.sysc.get() + 1);
        do_ioctl(self.fd, rtshm_abi::IOC_AWAIT as libc::c_ulong, 0).map(|_| ())
    }

    /// 诊断：hexdump ch0/ch1 通道头 0x20 字节（magic "VO"=0x4F56 LE、
    /// version、ring read/write 索引）。空唤醒现场用——区分"用户态陈旧
    /// 视图"（索引相等）与"magic 失效"（try_recv 的 is_valid 门禁拒收，
    /// 而内核 has_pending 不查 magic，两视图检查不对称的根因即在此）。
    fn dump_ch_headers(&self, tag: &str) {
        let base = self.ptr as *const u8;
        for (name, off) in [("ch0", 0x100usize), ("ch1", 0x100 + 0x8200)] {
            let mut line = String::new();
            for i in 0..0x20 {
                let b = unsafe { core::ptr::read_volatile(base.add(off + i)) };
                line.push_str(&format!("{b:02x} "));
            }
            println!("  [{tag}] {name}@+{off:#x}: {line}");
        }
    }

    /// 软件注入 new_msg 自测 mailbox→APLIC→handler 链路，返回 handler 触发
    /// 计数（失败返回 Err）。超时诊断用：注入后 IRQ 不涨 = 中断线挂高
    /// （MSI 电平源 + CLR 竞态，见内核 rt_shm ack_and_clear 注释）。
    fn test_mbox(&self) -> io::Result<libc::c_int> {
        do_ioctl(self.fd, rtshm_abi::IOC_TEST_MBOX as libc::c_ulong, 0)
    }

    /// 读内核延迟插桩双戳（dd 场景用）：`[门铃 MMIO 写前 ns, IRQ 入口 ns]`。
    /// 纯诊断读数，不计入 sysc。
    fn rd_kts(&self) -> io::Result<[u64; 2]> {
        let mut ts = [0u64; 2];
        do_ioctl(
            self.fd,
            rtshm_abi::IOC_RD_KTS as libc::c_ulong,
            ts.as_mut_ptr() as usize as libc::c_ulong,
        )
        .map(|_| ts)
    }
}

impl Drop for RtShm {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr, SHM_SIZE);
            libc::close(self.fd);
        }
    }
}

// ── 计时与系统调优 ───────────────────────────────────────────

/// CLOCK_MONOTONIC 纳秒（vdso，~20-40ns）
#[inline(always)]
fn mono_ns() -> u64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

/// RP mtime tick 差 → ns（freq 来自 STATS，u128 中间量防溢出）。
fn t2ns(delta_ticks: u64, freq_hz: u64) -> u64 {
    if freq_hz == 0 {
        return 0;
    }
    ((delta_ticks as u128 * 1_000_000_000u128) / freq_hz as u128).min(u64::MAX as u128) as u64
}

#[cfg(target_os = "linux")]
fn apply_realtime(cpu: usize) {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe { libc::CPU_SET(cpu, &mut set) };
    let cpu_ok = unsafe {
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set)
    } == 0;
    let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
    param.sched_priority = 80;
    let rt_ok = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) } == 0;
    println!(
        "[setup] CPU{cpu} pin {} | SCHED_FIFO(80) {}",
        if cpu_ok { "✓" } else { "✗" },
        if rt_ok { "✓" } else { "✗ (需 root)" }
    );
}

#[cfg(not(target_os = "linux"))]
fn apply_realtime(cpu: usize) {
    println!("[setup] CPU{cpu} ✗ | SCHED_FIFO ✗ (非 Linux，占位)");
}

/// 绝对期限定速：>500µs 时先 clock_nanosleep 粗睡（留 200µs 裕量），
/// 尾段自旋对齐——兼顾低开销与 µs 级精度（短间隔场景全自旋）。
fn sleep_until(deadline_ns: u64) {
    loop {
        let now = mono_ns();
        if now >= deadline_ns {
            return;
        }
        // 主线程活跃证明：长定速睡眠（s1 默认 2×W≈4s、dd 校准同款）
        // 不刷新心跳会触发看门狗 SIGALRM 风暴——看门狗只该捕捉 AWAIT
        // 挂死（挂死时本循环不执行，心跳自然停滞，检测能力不受影响）。
        HEARTBEAT.store(now, Ordering::Relaxed);
        let remain = deadline_ns - now;
        if remain > 500_000 {
            let sleep_ns = remain - 200_000;
            let ts = libc::timespec {
                tv_sec: (sleep_ns / 1_000_000_000) as libc::c_long,
                tv_nsec: (sleep_ns % 1_000_000_000) as libc::c_long,
            };
            unsafe {
                libc::clock_nanosleep(libc::CLOCK_MONOTONIC, 0, &ts, std::ptr::null_mut());
            }
        } else {
            std::hint::spin_loop();
        }
    }
}

// ── 看门狗：挂死 10s 内可诊断，无需复位板子 ─────────────────
//
// 仅为心跳诊断（SIGALRM 打断阻塞中的 AWAIT）；A4 丢发布不设运行时
// 恢复（模块头注释），检测走 fresh_scan。

static HEARTBEAT: AtomicU64 = AtomicU64::new(0);
static TIMED_OUT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigalrm(_sig: libc::c_int) {
    TIMED_OUT.store(true, Ordering::SeqCst);
}

fn spawn_watchdog() {
    // flags=0（无 SA_RESTART）：SIGALRM 使阻塞中的 AWAIT ioctl 返回 EINTR。
    unsafe {
        let mut act: libc::sigaction = std::mem::zeroed();
        act.sa_sigaction = on_sigalrm as extern "C" fn(libc::c_int) as usize;
        act.sa_flags = 0;
        libc::sigemptyset(&mut act.sa_mask);
        libc::sigaction(libc::SIGALRM, &act, std::ptr::null_mut());
    }
    std::thread::spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let hb = HEARTBEAT.load(Ordering::Relaxed);
        if hb != 0 && mono_ns().saturating_sub(hb) > 10_000_000_000 {
            // 心跳停滞超 10s：发 SIGALRM 打断主线程的 AWAIT。
            // 不退出本循环：主线程若恢复则心跳更新，继续正常监测。
            unsafe { libc::kill(libc::getpid(), libc::SIGALRM) };
        }
    });
}

// ── 测量核心 ─────────────────────────────────────────────────

/// 轮次错误（映射退出码）。
enum BenchErr {
    Io(io::Error),
    Timeout { seq: u64 },
    Decode,
    RidMismatch { want: u64, got: u64 },
    EchoMismatch { want: u64, got: u64 },
    BadTag(u8),
}

/// 单轮测量输出。
struct RoundOut {
    resp: Message,
    sent_ipi: bool,
    /// 请求构造前（测量窗口起点）
    t0: u64,
    /// 写 CH0 + fence +（如发）NOTIFY ioctl 完成
    t_send_end: u64,
    /// AWAIT 返回并取到 rid 匹配响应
    t1: u64,
    /// 本轮 syscall 数（notify/await）
    sysc: u32,
    /// dd 探针：内核门铃 MMIO 写前一瞬的戳（NOTIFY 内部落定，t_send_end
    /// 后读出，不污染 send 段）。probe_kts=false 时为 0。
    kpre: u64,
    /// dd 探针：唤醒本轮 AWAIT 的 mailbox IRQ 在内核 handler 入口的戳
    /// （t1 后读出，不污染 RTT）。probe_kts=false 时为 0。
    kirq: u64,
}

/// 逐样本记录。
struct Sample {
    seq: u64,
    tag: u8,
    sent_ipi: bool,
    rtt_ns: u64,
    send_ns: u64,
    /// 本轮 syscall 数（notify/await）
    sysc: u32,
    /// RP 分段：t_drain − t_isr（ISR 内 mailbox MMIO 排空舞步，仅 D1 有意义）
    d_drain_ns: u64,
    /// RP 分段：t_sched − t_isr（IRQ 进入 → IPC 任务恢复执行，仅 D1 有意义）
    d_sched_ns: u64,
    /// RP 分段：t_sched − t_drain（trap 返回 + 执行器派发 + 任务恢复）
    d_disp_ns: u64,
    /// RP 分段：t_seen − t_sched（任务恢复 → handler 入口）
    d_seen_ns: u64,
    isr_ticks: u64,
    drain_ticks: u64,
    sched_ticks: u64,
    seen_ticks: u64,
}

/// RP 计数器快照（MSGS 最后读；rounds_mark 在读完 MSGS 后立即标记，
/// 两者的对账等式见模块文档）。
struct Snap {
    c: [u64; STAT_COUNT],
    rounds_mark: u64,
}

struct Bench {
    rt: RtShm,
    rid_next: u64,
    rounds_sent: u64,
    backpressure: u64,
    stray: u64,
    spurious_wake: u64,
    last_seq: u64,
    freq_hz: u64,
    /// 前 N 轮逐相位打印（诊断挂点用；stdout 按行刷新）
    verbose_left: u32,
    /// dd 场景开关：round_msg 在发送后/响应后各取一次内核双戳。
    /// 非 dd 场景保持 false（零额外 syscall）。
    probe_kts: bool,
    /// H9 对照模式（env BENCH_SPIN_AWAIT=1）：响应等待跳过 AWAIT syscall，
    /// 纯用户态轮询（零 syscall/零内核原子/零调度）。
    /// rtt/dseen 应声下跌 ⇒ AP 内核路径（syscall 原子 + 调度）与 RP 的
    /// fence 在全局 Atomics Wrapper/互连上排队竞争。
    spin_await: bool,
}

/// 前 N 轮逐相位打印（诊断挂点）。只读字段访问，避开与 tx/rx 的借用冲突。
macro_rules! vlog {
    ($s:expr, $($arg:tt)*) => {
        if $s.verbose_left > 0 {
            println!("  · rd#{} {}", $s.rounds_sent, format!($($arg)*));
        }
    };
}

impl Bench {
    /// 看门狗超时现场诊断：注入 new_msg 自测 mailbox→APLIC→handler 链路。
    /// 注入后 IRQ 计数不涨 = 中断线挂高（MSI 电平源 + CLR 竞态，见内核
    /// rt_shm `ack_and_clear` 注释记录的 2026-08-15 同症状实锤）。
    /// 只读字段 + `&self.rt`（不可变借用，与 tx/rx 借用共存）。
    fn timeout_diag(&self) {
        eprintln!(
            "[TIMEOUT] seq={}（rd#{}）：AWAIT 未醒 | stray={} spurious_wake={}",
            self.last_seq, self.rounds_sent, self.stray, self.spurious_wake
        );
        match self.rt.test_mbox() {
            Ok(n) => eprintln!("  mailbox 自测：触发 {n} 次——中断线存活（挂点在别处）"),
            Err(e) => eprintln!(
                "  mailbox 自测失败: {e}——中断线疑似挂高（MSI 电平源 CLR 竞态）"
            ),
        }
        eprintln!("  对照手段：BENCH_NO_RT=1 跳过 CPU pin 复跑；抓 RP UART 与 dmesg");
    }

    /// 发一条请求（写 CH0 → 按 BUSY 决定是否 NOTIFY），阻塞等待并返回
    /// rid 匹配的响应。手写协议路径（非 ov-rpc 客户端），以捕获 sent_ipi
    /// 决策与各段耗时。
    ///
    /// （原按行缓存维护已随 PMA 非缓存窗口撤除——BUSY 读的是 SRAM 真值，
    /// 单一判定即可，D2 命中轮零 syscall。）
    fn round_msg(&mut self, msg: Message) -> Result<RoundOut, BenchErr> {
        HEARTBEAT.store(mono_ns(), Ordering::Relaxed);
        // 定速睡眠期间的 SIGALRM 已由 sleep_until 刷心跳杜绝，但残留的
        // TIMED_OUT 标志若不清，后续任何良性 EINTR 会误报 Timeout 退出。
        TIMED_OUT.store(false, Ordering::SeqCst);
        self.rt.sysc.set(0);
        let t0 = mono_ns();
        let shm = self.rt.shm();
        let tx = shm.sender(CH0).unwrap();
        let rx = shm.receiver(CH1).unwrap();
        let rid = msg.request_id().expect("request carries rid");
        if self.verbose_left > 0 {
            self.verbose_left -= 1;
        }

        // （原发送前后按行缓存维护已随 PMA 非缓存窗口撤除——读写直达 SRAM。）
        // 背压：单请求在途 CH0 不应满；100ms 内重试失败按异常退出。
        loop {
            match tx.try_send(&msg) {
                Ok(()) => break,
                Err(ov_channels::SendError::Full) => {
                    self.backpressure += 1;
                    if mono_ns().saturating_sub(t0) > 100_000_000 {
                        eprintln!("[FATAL] CH0 持续 Full 超过 100ms（seq={}）", self.last_seq);
                        std::process::exit(4);
                    }
                    std::hint::spin_loop();
                }
                Err(e) => {
                    eprintln!("[FATAL] CH0 send error: {e:?}");
                    std::process::exit(3);
                }
            }
        }
        vlog!(self, "sent rid={rid}");

        // 发送决策（与 ov-rpc client::call_inner 同型的丢失唤醒防护）：
        // fence 排序写与 BUSY 读——真值单判：读得 1 则 RP 确在弹性自旋且
        // 其重查闭环必见请求；读得 0 则门铃唤醒。BUSY 读经非缓存窗口
        // 恒新鲜。
        std::sync::atomic::fence(Ordering::SeqCst);
        let sent_ipi = !shm.is_busy();
        if sent_ipi {
            self.rt.notify().map_err(BenchErr::Io)?;
        }
        vlog!(self, "send decision done (sent_ipi={sent_ipi})");
        let t_send_end = mono_ns();
        // dd 探针：kpre 在 NOTIFY 内部（门铃 MMIO 写前）已落定，此处读出
        // 不影响 send 段计时。非 dd 场景零开销。
        let kpre = if self.probe_kts {
            self.rt.rd_kts().map_err(BenchErr::Io)?[0]
        } else {
            0
        };

        loop {
            vlog!(self, "await entering...");
            // H9 对照（spin_await）：跳过 AWAIT syscall，纯用户态轮询响应。
            // 超时兜底也改本地判定（无 EINTR 可借）。
            let awoke = if self.spin_await { Ok(()) } else { self.rt.await_ipi() };
            if self.spin_await && mono_ns().saturating_sub(t0) > 5_000_000_000 {
                return Err(BenchErr::Timeout { seq: self.last_seq });
            }
            match awoke {
                Ok(()) => {
                    vlog!(self, "await returned");
                }
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => {
                    if TIMED_OUT.load(Ordering::SeqCst) {
                        self.timeout_diag();
                        return Err(BenchErr::Timeout { seq: self.last_seq });
                    }
                    // 未设超时却 EINTR：信号路径异常，打出以便发现"反复
                    // EINTR 空转"类挂死。
                    eprintln!("! rd#{} AWAIT EINTR（无超时标志）: {e}", self.rounds_sent);
                    self.spurious_wake += 1;
                    continue;
                }
                Err(e) => return Err(BenchErr::Io(e)),
            }
            // （原 AWAIT 返回后的按行刷新与消费发布已撤除——非缓存窗口下
            // try_recv 的内部读恒新鲜，read 推进直达 SRAM 对 RP 可见。）
            let mut found: Option<Message> = None;
            loop {
                match rx.try_recv() {
                    Some(m) if m.request_id() == Some(rid) => {
                        found = Some(m);
                        break;
                    }
                    Some(other) => {
                        // 杂散消息：排空并计数——打印首条细节（ty/rid），
                        // 串包类故障的直接证据。
                        if self.stray == 0 {
                            eprintln!(
                                "! rd#{} stray msg: ty={:?} rid={:?}",
                                self.rounds_sent,
                                other.ty(),
                                other.request_id()
                            );
                        }
                        self.stray += 1;
                    }
                    None => break, // 空唤醒：回外层重等
                }
            }
            if let Some(resp) = found {
                let t1 = mono_ns();
                // dd 探针：kirq 在唤醒本轮 AWAIT 的 IRQ handler 入口已落定，
                // t1 之后读出不污染 RTT。
                let kirq = if self.probe_kts {
                    self.rt.rd_kts().map_err(BenchErr::Io)?[1]
                } else {
                    0
                };
                self.rounds_sent += 1;
                return Ok(RoundOut {
                    resp,
                    sent_ipi,
                    t0,
                    t_send_end,
                    t1,
                    sysc: self.rt.sysc.get(),
                    kpre,
                    kirq,
                });
            }
            // spin_await 模式：无数据 = 正常轮询节拍（RP 尚未写完响应），
            // 静默重试——取证 try_recv 会把消息消费在 found 流程之外造成
            // 本轮永久丢失（warmup 首轮实锤）。
            if self.spin_await {
                continue;
            }
            eprintln!(
                "! rd#{} await 返回但 ch1 无本 rid 消息（空唤醒 #{}）",
                self.rounds_sent,
                self.spurious_wake + 1
            );
            self.spurious_wake += 1;
            if self.spurious_wake <= 3 {
                // 现场取证：通道头 hexdump + 重试 try_recv。窗口非缓存，
                // 读恒 SRAM 真值；"magic 非 4f56" = 通道头被破坏（try_recv
                // 拒收、内核 has_pending 仍真，两视图检查不对称）。
                self.rt.dump_ch_headers("空唤醒现场");
                match rx.try_recv() {
                    Some(m) => eprintln!(
                        "  重试取到消息: ty={:?} rid={:?}",
                        m.ty(),
                        m.request_id()
                    ),
                    None => eprintln!("  重试仍空"),
                }
            }
        }
    }

    fn alloc_rid(&mut self) -> u64 {
        self.rid_next += 1;
        self.rid_next
    }

    fn ping_round(&mut self, seq: u64) -> Result<(PingResp, RoundOut), BenchErr> {
        self.last_seq = seq;
        let rid = self.alloc_rid();
        let req = Message::request(rid, M_PING | ov_rpc::NOTIFY_FLAG, &seq)
            .expect("PING request serialize failed");
        let out = self.round_msg(req)?;
        let (rrid, r) = out
            .resp
            .as_response::<PingResp>()
            .ok_or(BenchErr::Decode)?;
        if rrid != rid {
            return Err(BenchErr::RidMismatch { want: rid, got: rrid });
        }
        if r.0 != seq {
            return Err(BenchErr::EchoMismatch { want: seq, got: r.0 });
        }
        if !(1..=4).contains(&r.1) {
            return Err(BenchErr::BadTag(r.1));
        }
        Ok((r, out))
    }

    fn stat_round(&mut self, idx: u32) -> Result<u64, BenchErr> {
        self.last_seq = u64::MAX; // STATS 轮的超时诊断不关联业务 seq
        let rid = self.alloc_rid();
        let req = Message::request(rid, M_STATS | ov_rpc::NOTIFY_FLAG, &idx)
            .expect("STATS request serialize failed");
        let out = self.round_msg(req)?;
        let (rrid, v) = out.resp.as_response::<u64>().ok_or(BenchErr::Decode)?;
        if rrid != rid {
            return Err(BenchErr::RidMismatch { want: rid, got: rrid });
        }
        Ok(v)
    }

    /// 发一条 MEMBENCH 请求并返回 (耗时 ns, 校验和)。op 语义见 [`mb_op`]。
    fn membench_round(&mut self, op: u32, arg: u32) -> Result<(u64, u64), BenchErr> {
        self.last_seq = u64::MAX;
        let rid = self.alloc_rid();
        let req = Message::request(rid, M_MEMBENCH | ov_rpc::NOTIFY_FLAG, &(op, arg))
            .expect("MEMBENCH request serialize failed");
        let out = self.round_msg(req)?;
        let (rrid, r) = out
            .resp
            .as_response::<(u64, u64)>()
            .ok_or(BenchErr::Decode)?;
        if rrid != rid {
            return Err(BenchErr::RidMismatch { want: rid, got: rrid });
        }
        Ok(r)
    }

    /// 发一条 LITMUS 单向指令（不 AWAIT；结果经 STATS LIT_* 轮询）。
    /// 无条件门铃——实验期间 RP 要么弹性自旋要么刚被上一条唤醒。
    fn litmus_send(&mut self, op: u32, arg: u32) -> Result<(), BenchErr> {
        HEARTBEAT.store(mono_ns(), Ordering::Relaxed);
        self.last_seq = u64::MAX;
        let rid = self.alloc_rid();
        let msg = Message::request(rid, M_LITMUS, &(op, arg))
            .expect("LITMUS request serialize failed");
        let shm = self.rt.shm();
        let tx = shm.sender(CH0).unwrap();
        let t0 = mono_ns();
        loop {
            match tx.try_send(&msg) {
                Ok(()) => break,
                Err(ov_channels::SendError::Full) => {
                    if mono_ns().saturating_sub(t0) > 100_000_000 {
                        eprintln!("[FATAL] CH0 持续 Full（litmus send op={op}）");
                        std::process::exit(4);
                    }
                    std::hint::spin_loop();
                }
                Err(e) => {
                    eprintln!("[FATAL] litmus send: {e:?}");
                    std::process::exit(3);
                }
            }
        }
        self.rt.notify().map_err(BenchErr::Io)?;
        Ok(())
    }

    fn snapshot(&mut self) -> Result<Snap, BenchErr> {
        let mut c = [0u64; STAT_COUNT];
        // 采样序：非路径桶（5..16）先行，路径桶 d1..d4 垫后（第 13..16 轮），
        // MSGS 仍最后读。**路径桶不能在第 1 轮读**：测量前的长睡眠
        // （≥2×W）让 RP 睡眠，快照首轮必是 D1 门铃唤醒，而该轮自己的
        // d1++ 发生在 handler 之后（读走旧值之后）——这个 +1 会落进
        // delta 窗口，Σd 恒比 msgs 多 1（2026-08-17 s2 板上实锤：
        // Σd=218 != msgs=217，逐桶反推全部对账后确认无消息丢失）。
        // 挪到第 13+ 轮读后，首轮 bump 早已落定、窗口两侧对称抵消；
        // 各桶窗口大小与读位无关（恒为快照 17 轮 + 测量轮数），分桶
        // 守恒不受读序影响。
        for i in (5..STAT_COUNT).chain(1..=4) {
            c[i] = self.stat_round(i as u32)?;
        }
        // MSGS 最后读 + 立即标记轮数 → 对账等式的采样点（见模块文档）。
        c[stat_idx::MSGS] = self.stat_round(stat_idx::MSGS as u32)?;
        Ok(Snap { c, rounds_mark: self.rounds_sent })
    }
}

// ── 统计 ─────────────────────────────────────────────────────

struct Stats {
    n: usize,
    min: u64,
    max: u64,
    mean: f64,
    stddev: f64,
    p50: u64,
    p95: u64,
    p99: u64,
}

fn calc(data: &[u64]) -> Stats {
    let mut v = data.to_vec();
    v.sort_unstable();
    let n = v.len();
    let mean = v.iter().sum::<u64>() as f64 / n as f64;
    let var = v.iter().map(|&x| (x as f64 - mean).powi(2)).sum::<f64>() / n as f64;
    let pct = |p: f64| {
        let i = ((p / 100.0 * (n - 1) as f64).round() as usize).min(n - 1);
        v[i]
    };
    Stats {
        n,
        min: v[0],
        max: v[n - 1],
        mean,
        stddev: var.sqrt(),
        p50: pct(50.0),
        p95: pct(95.0),
        p99: pct(99.0),
    }
}

fn show(label: &str, s: &Stats) {
    println!(
        "  [{label}] n={} min={:.1} p50={:.1} mean={:.1} p95={:.1} p99={:.1} max={:.1} µs  σ={:.2}µs",
        s.n,
        s.min as f64 / 1e3,
        s.p50 as f64 / 1e3,
        s.mean / 1e3,
        s.p95 as f64 / 1e3,
        s.p99 as f64 / 1e3,
        s.max as f64 / 1e3,
        s.stddev / 1e3,
    );
}

fn hist(data: &[u64], bucket_ns: u64) {
    if data.is_empty() {
        return;
    }
    let mut v = data.to_vec();
    v.sort_unstable();
    let lo = v[0] / bucket_ns * bucket_ns;
    let hi = (v[v.len() - 1] / bucket_ns + 1) * bucket_ns;
    let mut bk: BTreeMap<u64, usize> = BTreeMap::new();
    for &x in &v {
        *bk.entry(x / bucket_ns * bucket_ns).or_insert(0) += 1;
    }
    let peak = *bk.values().max().unwrap_or(&1) as f64;
    let mut b = lo;
    while b <= hi {
        let cnt = *bk.get(&b).unwrap_or(&0);
        let bar = "█".repeat((cnt as f64 / peak * 40.0).round() as usize);
        println!("  {:>8.1}µs │{:>4}│{}", b as f64 / 1e3, cnt, bar);
        b += bucket_ns;
    }
}

/// 简易 LCG（竞态扫描随机间隔用，无需密码学质量）。
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            0
        } else {
            self.next() % bound
        }
    }
}

// ── 场景 ─────────────────────────────────────────────────────

/// 轮次错误统一出口：打印诊断并映射退出码（泛型以兼容 unwrap_or_else）。
fn die<T>(err: BenchErr) -> T {
    match err {
        BenchErr::Timeout { seq } => {
            eprintln!("[TIMEOUT] seq={seq}：RP 未回包（AWAIT 被看门狗 SIGALRM 打断）");
            eprintln!("  排查：RP UART 日志（magic 自愈/panic）、dmesg（rt_shm IRQ 计数）");
            std::process::exit(2);
        }
        BenchErr::Io(e) => {
            eprintln!("[FATAL] ioctl 失败: {e}");
            std::process::exit(2);
        }
        BenchErr::Decode => {
            eprintln!("[FATAL] 响应反序列化失败（数据损坏或协议版本不匹配）");
            std::process::exit(3);
        }
        BenchErr::RidMismatch { want, got } => {
            eprintln!("[FATAL] rid 不匹配：want={want} got={got}（串包）");
            std::process::exit(3);
        }
        BenchErr::EchoMismatch { want, got } => {
            eprintln!("[FATAL] 回显不匹配：want={want} got={got}（数据损坏）");
            std::process::exit(3);
        }
        BenchErr::BadTag(t) => {
            eprintln!("[FATAL] 非法路径标签 tag={t}（RP 插桩版本不匹配？）");
            std::process::exit(3);
        }
    }
}

fn dump_counters(c: &[u64; STAT_COUNT]) {
    for (i, name) in STAT_NAMES.iter().enumerate() {
        let v = c[i];
        let shown = if v == u64::MAX { "N/A".to_string() } else { v.to_string() };
        println!("  [{i:>2}] {name:<14} = {shown}");
    }
}

/// 场景 s0：标定。预热（定速 2ms 促使 RP 完整耗尽弹性窗口）后 dump
/// 全部计数器与窗口时长 W，再按 100ms 间隔采样 10 次观察 W 稳定性。
fn run_s0(b: &mut Bench, warmup: usize) {
    warmup_paced(b, warmup);

    let mut snap = b.snapshot().unwrap_or_else(die);
    println!("[s0] 计数器快照：");
    dump_counters(&snap.c);
    let w = snap.c[stat_idx::WIN_MAX_NS];
    if w == u64::MAX {
        println!("[s0] ⚠ 尚无完整弹性窗口样本（win=N/A）——检查 RP 固件是否含插桩");
    } else {
        println!(
            "[s0] 弹性窗口 W：last={:.1}µs min={:.1}µs max={:.1}µs（{} 次）",
            snap.c[stat_idx::WIN_LAST_NS] as f64 / 1e3,
            snap.c[stat_idx::WIN_MIN_NS] as f64 / 1e3,
            w as f64 / 1e3,
            snap.c[stat_idx::WINDOWS]
        );
    }

    println!("[s0] 窗口稳定性采样（10 × 100ms）：");
    for k in 0..10 {
        sleep_until(mono_ns() + 100_000_000);
        snap = b.snapshot().unwrap_or_else(die);
        println!(
            "  #{k} win_last={:.1}µs win_max={:.1}µs windows={} msgs={} heals={}",
            snap.c[stat_idx::WIN_LAST_NS] as f64 / 1e3,
            snap.c[stat_idx::WIN_MAX_NS] as f64 / 1e3,
            snap.c[stat_idx::WINDOWS],
            snap.c[stat_idx::MSGS],
            snap.c[stat_idx::HEALS],
        );
    }
    println!("[s0] done");
}

/// 预热：定速 2ms/轮，每轮之间让 RP 留在弹性自旋（K3 实测 W≈2s ≫ 2ms，
/// 预热期 RP 不入睡——排冷与相位稳定足够，D1 基线由测量前的长间隔保证）。
/// 前 3 轮逐相位打印（诊断挂点）。
fn warmup_paced(b: &mut Bench, warmup: usize) {
    println!("[warmup] {warmup} 轮 PING，定速 2ms");
    for seq in 0..warmup as u64 {
        let (r, out) = b.ping_round(seq).unwrap_or_else(die);
        let _ = (r, &out);
        sleep_until(out.t0 + 2_000_000);
    }
    println!("[warmup] done");
}

fn resolve_interval(scen: &str, cfg: Option<u64>, w_ns: u64) -> u64 {
    match scen {
        // 2×W 已被 dd 场景板上实证（30/30 纯 D1）：>W 即睡透，纯度校验
        // 本身会兜底（间隔不足时 D2 混入 → 大声失败，重跑即可）。
        "s1" => cfg.unwrap_or(w_ns.saturating_mul(2)),
        "s2" => cfg.unwrap_or((w_ns / 4).clamp(20_000, 200_000)),
        "s6" => cfg.unwrap_or(w_ns),
        "s4" => cfg.unwrap_or(w_ns.saturating_mul(2)),
        _ => cfg.unwrap_or(0),
    }
}

/// 测量场景通用骨架：预热 → W 标定 → 快照 → 测量循环（定速/随机）→
/// 快照 → 对账校验 → 汇总 + CSV。
fn run_measured(b: &mut Bench, scen: &str, n: usize, interval_cfg: Option<u64>, warmup: usize) {
    warmup_paced(b, warmup);

    let cal = b.snapshot().unwrap_or_else(die);
    let freq = cal.c[stat_idx::FREQ_HZ];
    b.freq_hz = freq;
    let win_max = cal.c[stat_idx::WIN_MAX_NS];
    let w_ns = if win_max == u64::MAX {
        println!("[warn] RP 无完整窗口样本，W 用假设值 300µs（先跑 s0 标定）");
        300_000
    } else {
        win_max
    };
    let interval = resolve_interval(scen, interval_cfg, w_ns);
    println!(
        "[cfg] scenario={scen} n={n} interval={} warmup={warmup} freq_hz={freq} W={:.1}µs",
        match interval_cfg {
            Some(v) => format!("{v}ns (指定)"),
            None => format!("{interval}ns (默认)"),
        },
        w_ns as f64 / 1e3,
    );
    // 预计全程：校准 2×interval + n×(单轮 interval + 每轮服务开销 ~0.3ms)；
    // s4 为随机 (0, interval) 均值取半。小时级场景提前可见，避免静默期
    // 被误判为挂死。大样本请显式传 n / interval_ns。
    let per_round = (if scen == "s4" { interval / 2 } else { interval } + 300_000) as u128;
    let total_s = (per_round * n as u128 + interval as u128 * 2) / 1_000_000_000;
    if total_s > 0 {
        println!("[cfg] 预计全程 ≈ {total_s}s（校准 2×interval + n×单轮）");
    } else {
        println!("[cfg] 预计全程 <1s");
    }

    // 进入测量前让 RP 回到确定状态（清空弹性窗口后的睡眠期）
    sleep_until(mono_ns() + interval.max(w_ns * 2));

    let before = b.snapshot().unwrap_or_else(die);
    // 轮 0 与后续轮同条件：before 快照刚发过 17 轮，RP 弹性窗口（W≈2s）
    // 尚未耗尽，立即开跑会让 seq#0 恒落窗内（s1 板上实锤 29/30——
    // 唯一 D2 就是 seq#0，且每轮都复现）。隔一个 interval 再开测。
    sleep_until(mono_ns() + interval);
    let mut samples: Vec<Sample> = Vec::with_capacity(n);
    let mut rng = Lcg(mono_ns() ^ 0x9E3779B97F4A7C15);
    // 进度行：相对场景起点的耗时（绝对时间戳无法看出推进），步长自适应
    // （n≥200 每 50 轮，小样本打 4 行）。
    let t_start = mono_ns();
    let prog_every = (n / 4).clamp(1, 50).max(1);

    for seq in 0..n as u64 {
        if seq > 0 && seq % prog_every as u64 == 0 {
            println!("[prog] {seq}/{n} 轮（+{:.1}s）", (mono_ns() - t_start) as f64 / 1e9);
        }
        let (r, out) = b.ping_round(seq).unwrap_or_else(die);
        let d_drain = t2ns(r.3.saturating_sub(r.2), freq);
        let d_sched = t2ns(r.4.saturating_sub(r.2), freq);
        let d_disp = t2ns(r.4.saturating_sub(r.3), freq);
        let d_seen = t2ns(r.5.saturating_sub(r.4), freq);
        samples.push(Sample {
            seq,
            tag: r.1,
            sent_ipi: out.sent_ipi,
            rtt_ns: out.t1 - out.t0,
            send_ns: out.t_send_end - out.t0,
            sysc: out.sysc,
            d_drain_ns: d_drain,
            d_sched_ns: d_sched,
            d_disp_ns: d_disp,
            d_seen_ns: d_seen,
            isr_ticks: r.2,
            drain_ticks: r.3,
            sched_ticks: r.4,
            seen_ticks: r.5,
        });
        // 定速：s4 随机间隔 (0, 2W)，其余固定 interval
        let pace = if scen == "s4" {
            rng.below(interval.max(1))
        } else {
            interval
        };
        sleep_until(out.t0 + pace);
    }

    let after = b.snapshot().unwrap_or_else(die);

    // svc 尾段分解已移除（2026-08-19 板上证伪）：分三次 stat_round 读
    // 锁存戳会混线——第 2/3 次读到的已是前一次 STATS 自身更新的戳，
    // 差值是消息间隔而非服务时长（曾打出 234µs 的"响应 try_send"，
    // 而整条 svc 才 134µs）。正确做法：内核侧随消息落账派生计数器
    // （T_HANDLE−T_RECV / T_RESP−T_HANDLE），一次读回——随 ②a-v2
    // 固件改动一并上。

    summarize(b, scen, &before, &after, &samples, interval);
}

// ============================================================================
// lit 场景：跨核免 fence 顺序性实验（LITMUS，fence 豁免判定）
// ============================================================================

/// AP 侧写 LITMUS scratch 字（非缓存窗口，写直达 SRAM）。
fn lit_wr64(b: &mut Bench, scr: usize, off: usize, v: u64) {
    HEARTBEAT.store(mono_ns(), Ordering::Relaxed);
    let _ = &*b; // 保留参数形状一致
    unsafe { ((scr + off) as *mut u64).write_volatile(v) };
}

/// AP 侧读 RP 写的 LITMUS scratch 字（非缓存窗口直读 SRAM 真值）。
fn lit_rd64(b: &mut Bench, scr: usize, off: usize) -> u64 {
    let _ = &*b; // 同上
    unsafe { ((scr + off) as *const u64).read_volatile() }
}

/// 读 RP 侧 LITMUS 计数：(viol, rounds, state)。
fn lit_stats(b: &mut Bench) -> (u64, u64, u64) {
    (
        b.stat_round(stat_idx::LIT_VIOL as u32).unwrap_or(0),
        b.stat_round(stat_idx::LIT_ROUNDS as u32).unwrap_or(0),
        b.stat_round(stat_idx::LIT_STATE as u32).unwrap_or(0),
    )
}

/// L1：AP 顺序发布 (data, round)，RP 按读模式轮询消费（op=lit_op::L1_POLL）。
/// proper=true 正序（data 先发布）；mode：0 纯读（v1 已证陈旧，回归对照）、
/// 1 每读前 fence（生产 Acquire 等价，预期恢复全量观测）、
/// 2 先读邻址再读目标（合并项逐出探测）。
fn lit_l1(b: &mut Bench, scr: usize, rounds: u64, proper: bool, mode: u32) {
    let mix = |r: u64| r ^ 0xA5A5_5A5A_5A5A_5A5A;
    let ms = 60u64;
    let mode_name = ["纯读", "fence读", "邻址读"][mode.min(2) as usize];
    lit_wr64(b, scr, 0x08, 0);
    lit_wr64(b, scr, 0x00, u64::MAX);
    let arg = (ms as u32) | (mode << 16);
    b.litmus_send(lit_op::L1_POLL, arg).unwrap_or_else(die);
    std::thread::sleep(std::time::Duration::from_millis(5));
    for k in 0..rounds {
        let d = mix(k);
        if proper {
            lit_wr64(b, scr, 0x08, d);
            lit_wr64(b, scr, 0x00, k);
        } else {
            lit_wr64(b, scr, 0x00, k);
            lit_wr64(b, scr, 0x08, d);
        }
        std::thread::sleep(std::time::Duration::from_micros(100));
    }
    std::thread::sleep(std::time::Duration::from_millis(ms + 30));
    let (viol, got, state) = lit_stats(b);
    let tag = if proper { "正序" } else { "反序(对照)" };
    let verdict = if proper {
        match (mode, viol, got == rounds) {
            (0, _, _) => "陈旧（回归对照，v1 已证：合并缓冲钉死同址普通读）".to_string(),
            (2, 0, false) => format!(
                "邻址读无效（{got}/{rounds}）：读邻行不逐出目标合并项——无免费刷新原语，只能 fence/CBO"
            ),
            (_, 0, true) => format!("PASS：{mode_name} 恢复全量新鲜度（viol=0 rounds 全）"),
            (_, 0, false) => format!("部分新鲜（viol=0 但 rounds 不足：{got}/{rounds}——刷新不完整或太慢）"),
            _ => "FAIL：观察到乱序（viol>0）".to_string(),
        }
    } else if viol > 0 {
        "检测器有效（观测到违例）✓".to_string()
    } else {
        "对照未触发（该读模式下窗口内未见乱序或未观测到）".to_string()
    };
    println!("[L1 {tag}/{mode_name}] RP viol={viol} rounds={got}/{rounds} state={state} —— {verdict}");
}

/// L2：RP 免 fence 发布 (data, round)+裸门铃（绕 notify fence），AP cbo
/// 轮询读校验（v3）。v1/v2 用 AWAIT 等门铃是双重错误：AWAIT 只在 ch1 有
/// 数据时返回 Ok——裸门铃的空唤醒对用户态本就不可见；且板上两版实锤
/// SIGALRM 打不断内核 AWAIT（挂死 + 看门狗风暴，见 k3-latency-attribution
/// 记忆）。轮询直接检验问题本身：RP 两条免 fence 写落地是否保序、
/// AP refresh 读能否全量可见。
fn lit_l2(b: &mut Bench, scr: usize, rounds: u64, proper: bool) {
    let mix = |k: u64| k.wrapping_mul(2654435761);
    lit_wr64(b, scr, 0x28, 0);
    lit_wr64(b, scr, 0x20, u64::MAX);
    let arg = (rounds as u32 & 0xffff) | u32::from(proper) << 16;
    b.litmus_send(lit_op::L2_PUBLISH, arg).unwrap_or_else(die);
    println!(
        "[L2] armed（{} {} 轮，RP 500µs 节奏），AP cbo 轮询中…",
        if proper { "正序" } else { "反序" },
        rounds
    );
    let mut viol = 0u64;
    let mut maxr: Option<u64> = None;
    let mut polls = 0u64;
    let t_end = mono_ns() + rounds * 1_000_000 + 200_000_000;
    while mono_ns() < t_end {
        HEARTBEAT.store(mono_ns(), Ordering::Relaxed);
        polls += 1;
        let r = lit_rd64(b, scr, 0x20);
        if r != u64::MAX && maxr.is_none_or(|m| r > m) {
            let d = lit_rd64(b, scr, 0x28);
            // 复读 round：读取期间 RP 已前进则样本无法归因，跳过防假阳性。
            let r2 = lit_rd64(b, scr, 0x20);
            if r2 != r {
                maxr = Some(r2);
                continue;
            }
            if d != mix(r) {
                viol += 1;
            }
            maxr = Some(r);
            if r >= rounds - 1 {
                break;
            }
        }
        // 跨域读限速：背靠背跨域访问曾楔死互连（k3-m2f-bridge-deadlock）。
        std::thread::sleep(std::time::Duration::from_micros(100));
    }
    std::thread::sleep(std::time::Duration::from_millis(10));
    let (_rv, rp_rounds, state) = lit_stats(b);
    let tag = if proper { "正序" } else { "反序(对照)" };
    let max_seen = maxr.map_or(0, |m| m + 1);
    println!(
        "[L2 {tag}] AP viol={viol} max_round={max_seen}/{rounds} polls={polls} rp_rounds={rp_rounds} state={state} —— {}",
        if proper {
            if viol == 0 && maxr == Some(rounds - 1) {
                "PASS：RP 免 fence 双写落地保序 + refresh 轮询全量可见 ⇒ notify fence 可省"
            } else if viol > 0 {
                "FAIL：落地乱序（round 先于 data 可见）⇒ notify fence 必须保留"
            } else {
                "可见性不足（max_round 未达）——RP 未完成或刷新缺失，看 rp_rounds/state"
            }
        } else if viol > 0 {
            "检测器有效 ✓"
        } else {
            "对照未触发（两写间隔太短，100µs 轮询未踩中——正常，看正序判据）"
        }
    );
}

/// L3：Dekker/store-buffering——双方免 fence 各写 flag 后读对方，统计读到
/// 旧值的轮数。预期两侧均显著非零 ⇒ clear_busy 后的 SeqCst fence 必须保留。
fn lit_l3(b: &mut Bench, scr: usize, rounds: u64) {
    lit_wr64(b, scr, 0x30, 0);
    lit_wr64(b, scr, 0x38, 0);
    b.litmus_send(lit_op::L3_DEKKER, rounds as u32).unwrap_or_else(die);
    let mut stale = 0u64;
    let mut last = 0u64;
    for k in 0..rounds {
        lit_wr64(b, scr, 0x30, k + 1);
        let rp = lit_rd64(b, scr, 0x38);
        if rp == last {
            stale += 1;
        } else {
            last = rp;
        }
        // 跨域读写限速（互连保护，同 L2）。RP 侧本地访问无限速、整组瞬间
        // 跑完，故 AP stale% 是节奏伪影——本组判读以 RP stale 为主。
        std::thread::sleep(std::time::Duration::from_micros(100));
    }
    std::thread::sleep(std::time::Duration::from_millis(10));
    let (rp_stale, rp_rounds, state) = lit_stats(b);
    println!(
        "[L3 Dekker] AP stale={}/{} ({:.0}%, 节奏伪影), RP stale={}/{} ({:.0}%) state={state} —— {}",
        stale,
        rounds,
        stale as f64 * 100.0 / rounds as f64,
        rp_stale,
        rp_rounds,
        rp_stale as f64 * 100.0 / rp_rounds.max(1) as f64,
        if rp_stale > 0 {
            "RP 免 fence 读对端 flag 丢更新（合并缓冲钉死，与 L1 互证）⇒ BUSY 舞步的 RP 侧读必须有 fence（或换硬件 spinlock）"
        } else {
            "RP 侧未见旧读（异常——检查 RP 轮数与 L1 纯读对照）"
        }
    );
}

fn run_lit(b: &mut Bench) {
    let scr = b.rt.shm_ptr() + MB_SCRATCH_OFF;
    println!("[lit] LITMUS scratch @ {:#x}（共享窗尾部空闲区）", scr);
    let l1_rounds = 200u64;
    let l2_rounds = 100u64;
    let l3_rounds = 2000u64;
    // L1 读新鲜度矩阵：纯读（陈旧回归对照）→ fence 读（生产等价）→
    // fence 反序（检测器对照）→ 邻址读（逐出探测）。
    lit_l1(b, scr, l1_rounds, true, 0);
    lit_l1(b, scr, l1_rounds, true, 1);
    lit_l1(b, scr, l1_rounds, false, 1);
    lit_l1(b, scr, l1_rounds, true, 2);
    lit_l2(b, scr, l2_rounds, true);
    lit_l2(b, scr, l2_rounds, false);
    lit_l3(b, scr, l3_rounds);
    println!(
        "[lit] done——判读：fence读是唯一可靠刷新（每读一条 ≈2.1µs，即 Acquire 价；邻址读已证无逐出）；L2 正序 PASS ⇒ RP 免 fence 写序落地保序、notify fence 可省；L3 RP stale 高 ⇒ RP 侧对端读的 fence 保留"
    );
}

/// 对账校验 + 汇总输出。
fn summarize(b: &Bench, scen: &str, before: &Snap, after: &Snap, samples: &[Sample], interval: u64) {
    let n = samples.len();
    let mut failures: Vec<String> = Vec::new();

    // ── 端到端对账（见模块文档：bench 是 CH0 唯一生产者）──
    let msgs_delta = after.c[stat_idx::MSGS].wrapping_sub(before.c[stat_idx::MSGS]);
    let rounds_delta = after.rounds_mark.wrapping_sub(before.rounds_mark);
    if after.c[stat_idx::MSGS] < before.c[stat_idx::MSGS] {
        failures.push(format!(
            "RP msgs 计数回卷（{} → {}）：RP 中途重启/复位？",
            before.c[stat_idx::MSGS], after.c[stat_idx::MSGS]
        ));
    } else if msgs_delta != rounds_delta {
        failures.push(format!(
            "消息数对账失败：RP msgs_delta={msgs_delta} != 本端 rounds_delta={rounds_delta}（丢失/漏计）"
        ));
    }
    let dsum: u64 = (1..=4usize)
        .map(|k| after.c[k].saturating_sub(before.c[k]))
        .sum();
    if dsum != msgs_delta {
        failures.push(format!(
            "路径分桶对账失败：Σd1..d4 delta={dsum} != msgs_delta={msgs_delta}"
        ));
    }
    let resp_fail_d = after.c[stat_idx::RESP_FAIL].saturating_sub(before.c[stat_idx::RESP_FAIL]);
    if resp_fail_d != 0 {
        failures.push(format!("RP 响应发送失败 {resp_fail_d} 次（CH1 满，响应丢失）"));
    }
    let heals_d = after.c[stat_idx::HEALS].saturating_sub(before.c[stat_idx::HEALS]);
    if heals_d != 0 {
        failures.push(format!("测量期间发生 magic 自愈 {heals_d} 次（共享窗被外部破坏）"));
    }

    // ── 样本级完整性（回显/rid/标签已在 ping_round 内逐条断言）──
    if n != samples.len() || samples.iter().any(|s| s.seq >= n as u64) {
        failures.push("样本序号异常".into());
    }

    // ── 标签分布与场景纯度 ──
    let mut buckets: BTreeMap<u8, Vec<u64>> = BTreeMap::new();
    for s in samples {
        buckets.entry(s.tag).or_default().push(s.rtt_ns);
    }
    println!("\n[tag 分布]");
    for (tag, v) in &buckets {
        println!(
            "  D{tag}: {} ({:.1}%)",
            v.len(),
            v.len() as f64 * 100.0 / n as f64
        );
    }
    let d1_n = buckets.get(&TAG_D1).map(|v| v.len()).unwrap_or(0);
    let d2_n = buckets.get(&2).map(|v| v.len()).unwrap_or(0);
    match scen {
        "s1" if d1_n != n => failures.push(format!(
            "s1 纯度失败：D1 {d1_n}/{n}（间隔 {interval}ns 不足 2×W 或窗口异常）"
        )),
        "s2" if (d2_n as f64) < 0.90 * n as f64 => failures.push(format!(
            "s2 纯度失败：D2 {d2_n}/{n} < 90%（间隔 {interval}ns 未落在弹性窗口内）"
        )),
        _ => {}
    }

    // ── 冗余门铃 / 异常组合 ──
    let redundant = samples.iter().filter(|s| s.sent_ipi && s.tag != TAG_D1).count();
    let busy_stale = samples.iter().filter(|s| !s.sent_ipi && s.tag == TAG_D1).count();
    println!("\n[门铃决策]");
    println!(
        "  sent_ipi={} / skipped={}",
        samples.iter().filter(|s| s.sent_ipi).count(),
        samples.iter().filter(|s| !s.sent_ipi).count()
    );
    println!("  冗余门铃（发了 IPI 但 tag≠D1，D5）: {redundant}");
    println!("  BUSY 疑似过期（跳过 IPI 但 tag=D1）: {busy_stale}");
    let avg_sysc = |tag: u8| {
        let v: Vec<u32> = samples.iter().filter(|s| s.tag == tag).map(|s| s.sysc).collect();
        if v.is_empty() {
            f64::NAN
        } else {
            v.iter().sum::<u32>() as f64 / v.len() as f64
        }
    };
    println!(
        "  syscall/轮：D1={:.2} D2={:.2}（D2 命中轮 1，仅 AWAIT）",
        avg_sysc(TAG_D1),
        avg_sysc(2),
    );
    println!(
        "  背压重试 {} 次 | 杂散消息 {} 条 | 空唤醒 {} 次",
        b.backpressure, b.stray, b.spurious_wake
    );
    if b.stray > 0 {
        failures.push(format!("ch1 出现 {} 条杂散消息（串包/残留）", b.stray));
    }

    // ── 延迟分布（按路径分桶）──
    println!("\n[RTT 分布（AP 时钟，全链路）]");
    for (tag, v) in &buckets {
        let s = calc(v);
        show(&format!("D{tag} RTT"), &s);
    }
    let all_rtt: Vec<u64> = samples.iter().map(|s| s.rtt_ns).collect();
    let all = calc(&all_rtt);
    show("overall", &all);

    // ── RP 内部分段（D1 桶：ISR 舞步 / 派发两段拆分；全体：发现→handler）──
    if let Some(v) = buckets.get(&TAG_D1) {
        let d1 = |f: &dyn Fn(&Sample) -> u64| -> Vec<u64> {
            samples.iter().filter(|s| s.tag == TAG_D1).map(f).collect()
        };
        let sched: Vec<u64> = d1(&|s| s.d_sched_ns);
        if !sched.is_empty() && v.len() == sched.len() {
            println!("\n[RP 分段（D1 样本）]");
            show(
                "t_drain−t_isr（ISR 内 MMIO 排空舞步）",
                &calc(&d1(&|s| s.d_drain_ns)),
            );
            show(
                "t_sched−t_drain（trap 返回+执行器派发+任务恢复）",
                &calc(&d1(&|s| s.d_disp_ns)),
            );
            show("t_sched−t_isr（IRQ→任务恢复，上两段之和）", &calc(&sched));
        }
    }
    let seen: Vec<u64> = samples.iter().map(|s| s.d_seen_ns).collect();
    println!("\n[RP 分段（全体）]");
    show("t_seen−t_sched（任务恢复→handler）", &calc(&seen));
    let send: Vec<u64> = samples.iter().map(|s| s.send_ns).collect();
    println!("\n[AP 分段]");
    show("发送段（写+门铃决策）", &calc(&send));

    // 主桶直方图
    let dom_tag = buckets
        .iter()
        .max_by_key(|(_, v)| v.len())
        .map(|(t, _)| *t)
        .unwrap_or(TAG_D1);
    if let Some(v) = buckets.get(&dom_tag) {
        println!("\n[D{dom_tag} RTT 直方图]");
        let s = calc(v);
        let bucket = ((s.max - s.min) / 15).max(100);
        hist(v, bucket);
    }

    // ── RP 计数器 delta ──
    // WIN_LAST/SVC_LAST/T_NOW 是瞬时锁存值（非单调），跨快照做差无意义
    // （会显示 u64 回卷的巨大负数），跳过；MIN/MAX/累加器照常。MIN 类
    // 计数"下降"（新样本更小）是正常语义，按带符号差显示（-765 而非
    // 18446744073709550851）。
    println!("\n[RP 计数器 delta]");
    for i in 0..STAT_COUNT {
        if matches!(i, stat_idx::WIN_LAST_NS | stat_idx::SVC_LAST_NS | stat_idx::T_NOW) {
            continue;
        }
        let d = after.c[i].wrapping_sub(before.c[i]) as i64;
        if d != 0 {
            println!("  {} += {d}", STAT_NAMES[i]);
        }
    }

    // ── CSV ──
    let mut csv = String::from(
        "seq,tag,sent_ipi,rtt_ns,send_ns,sysc,ddrain_ns,dsched_ns,ddisp_ns,dseen_ns,isr,drain,sched,seen\n",
    );
    for s in samples {
        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            s.seq,
            s.tag,
            s.sent_ipi as u8,
            s.rtt_ns,
            s.send_ns,
            s.sysc,
            s.d_drain_ns,
            s.d_sched_ns,
            s.d_disp_ns,
            s.d_seen_ns,
            s.isr_ticks,
            s.drain_ticks,
            s.sched_ticks,
            s.seen_ticks
        ));
    }
    println!("\n# ---- csv begin ----");
    print!("{csv}");
    println!("# ---- csv end ----");
    if let Ok(path) = std::env::var("BENCH_CSV") {
        match std::fs::write(&path, &csv) {
            Ok(()) => println!("[csv] 已写入 {path}"),
            Err(e) => println!("[csv] 写入 {path} 失败: {e}"),
        }
    }

    // ── 结论 ──
    println!("\n[校验] {}", if failures.is_empty() { "全部通过 ✓" } else { "失败 ✗" });
    for f in &failures {
        println!("  ✗ {f}");
    }
    if !failures.is_empty() {
        std::process::exit(3);
    }
}

// ── 场景 mb / dd：延迟归因诊断 ────────────────────────────────

/// 场景 mb：RP 侧内存/MMIO 访问微基准（MEMBENCH RPC）。
///
/// 2026-08-20 起目标 = **L0 归因闭合**：P1 后 dseen 107.8µs（dpre 24.3 +
/// drx 45.6 + dserde 38.0）中 fence 理论只解释 ~31µs（手数代码 ~14 笔 ×
/// 热循环 2.2µs），~75µs 无主。判读规则（探针组见 ops 表 L0 段）：
/// - cold_aq ≫ aq_load(2198ns) ⇒ H1：单笔冷 fence 贵于热循环，真实笔数
///   × cold 单价重新对账 dseen；
/// - postcard_rt 达 µs×10 级 ⇒ H2：构解本身是大头（优化打 postcard/形状）；
/// - 两者都小 ⇒ H3：无主部分在调用链（stamps/timer/dispatch 机械），
///   下一步加段内细分探针。
/// 历史锚点（2026-08-17）：旧"256B 取读 ~105µs"假设已被 RD_BLK256_SHM
/// 证伪（②c 别名窗后 ~0.4µs）；邮箱寄存器价 = 驱动 Acquire 而非 MMIO。
    /// H8 新鲜写衰减扫描：两段式控制"AP 写入"与"RP 读取"
    /// 的间隔 D——① fire MEMBENCH(FRESH_WAIT_RECV)（RP 进入自旋收取）→
    /// ② 自旋延迟 D → ③ 写 dummy notification（不门铃，RP 在 op 内收取）
    /// → ④ 等 FRESH 响应。响应的 ns = RP 成功笔（读 AP 于 ~D 前写入的
    /// 消息）的单笔 try_recv 价格。
    ///
    /// 判据：短 D 显著高于 recv_seq（11.7µs）且随 D 衰减 ⇒ H8 实锤——
    /// 读"AP 新鲜写（posted 写未落地）"的行有确定性税，即 drx 43.6 与
    /// dserde 34.4 中 32µs 级差额的物理来源（dserde 同理：读 AP 刚写的
    /// 请求槽做反序列化）；D→∞ 回落探针价。
    ///
    /// 08-23 起兼作 **A4 存在性检测**（运行时恢复已按设计决策移除）：超时
    /// 档交叉三方索引视角判定——AP 缓存视角（③ 前后快照，w 字段 AP 自有
    /// 恒见新值；r 字段 RP 所有、可能陈旧）、RP SRAM 视角（超时快照，RP
    /// 整个 200ms 自旋期所见）、AP 失效回读（超时后 inval 索引行再读；
    /// 注意该读可能恰好令迟滞脏行落地，属扰动性探测，只作行驻留态参考）。
    /// RP w 落后 AP 缓存 w ⇒ 发布未跨核可见——A4 在位的直接证据。
    fn fresh_scan(b: &mut Bench) {
        use ov_rpc::cache;
        println!("\n[mb] H8 新鲜写衰减扫描（D = dummy 写入到 RP 收取的间隔）");
        let shm = b.rt.shm();
        let ch0 = unsafe { shm.channel_unchecked(CH0) } as *const _ as usize;
        let dummy = Message::notification(0xF00D);
        // A4/PMA 退化检测计数（AP 视角快照 r2/w2/r3/w3 为循环内局部量，
        // 判读见函数文档）。
        let mut a4_detected = 0u32;
        for d_ns in [0u64, 30_000, 100_000, 300_000, 1_000_000, 3_000_000, 10_000_000, 50_000_000] {
            let rid = mono_ns();
            // ① fire 请求（门铃一发拉 RP 进 handler）
            let req = Message::request(rid, M_MEMBENCH | ov_rpc::NOTIFY_FLAG, &(29u32, 200u32))
                .expect("FRESH req serialize");
            shm.sender(CH0).unwrap().try_send(&req).expect("FRESH req send");
            b.rt.notify().expect("notify");
            // ② 精确延迟（自旋，AP 无他事）
            let dl = mono_ns() + d_ns;
            while mono_ns() < dl {
                std::hint::spin_loop();
            }
            // ③ dummy（不门铃——RP 在 op 内自旋收取）。
            let (r2, w2) = cache::ring_indices(ch0);
            shm.sender(CH0).unwrap().try_send(&dummy).expect("dummy send");
            let (r3, w3) = cache::ring_indices(ch0);
            // ④ 等响应（超时兜底由 op 的 arg=200ms 承担；此处 2s 硬超时）
            let rx1 = shm.receiver(CH1).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            let mut got_ns: Option<(u64, u64)> = None;
            while got_ns.is_none() {
                if std::time::Instant::now() > deadline {
                    eprintln!("! FRESH 响应超时（D={d_ns}）");
                    break;
                }
                let _ = b.rt.await_ipi();
                while let Some(m) = rx1.try_recv() {
                    if m.request_id() == Some(rid) {
                        // as_response = (rid, (ns, got))——上一版把 rid 当
                        // ns 打印（输出 133 秒级假值 + 真值错位到 got 列）
                        let (_r, v): (u64, (u64, u64)) =
                            m.as_response().expect("FRESH resp shape");
                        got_ns = Some(v);
                    }
                }
            }
            match got_ns {
                Some((ns, got)) if ns > 0 => println!(
                    "  D={:>8.1}µs → RP 单笔 try_recv {:>7.1} µs（got={got}）",
                    d_ns as f64 / 1e3,
                    ns as f64 / 1e3
                ),
                // ns==0 ⇔ 固件超时：got 字段是 RP 视角快照 (w<<32|r)。
                // A4 判定：RP 整个 200ms 自旋期 w 停在 AP 已发布值之前
                // ⇒ 发布未跨核可见。失效回读再取 AP 侧 SRAM 视角：仍
                // 落后 ⇒ 迟滞脏行驻留缓存（粘滞特征）；已推进 ⇒ 该回读
                // 本身令其落地（扰动性探测），不影响超时档判定。
                Some((_, packed)) => {
                    let r = packed & 0xffff_ffff;
                    let w = packed >> 32;
                    let a4 = w < w3 as u64;
                    if a4 {
                        a4_detected += 1;
                    }
                    let (r4, w4) = cache::ring_indices(ch0);
                    println!(
                        "  D={:>8.1}µs → RP 超时：{}。AP 视角 ({r2},{w2})→({r3},{w3})，RP 200ms 视角 r={r}/w={w}，重读 r={r4}/w={w4}",
                        d_ns as f64 / 1e3,
                        if a4 { "发布未达 RP——A4 复发或 PMA 退化" } else { "RP 已见 w 推进，另查" },
                    );
                }
                // 2s 硬超时（连 RP 的超时响应都未达）：发布或响应链疑似
                // 异常，以重读代行交叉。
                None => {
                    let (r4, w4) = cache::ring_indices(ch0);
                    let a4 = w4 < w3;
                    if a4 {
                        a4_detected += 1;
                    }
                    println!(
                        "  D={:>8.1}µs → 2s 硬超时（RP 超时响应也未达）：{}。AP 视角 ({r2},{w2})→({r3},{w3})，重读 r={r4}/w={w4}",
                        d_ns as f64 / 1e3,
                        if a4 { "A4 特征——发布未落地（PMA 退化时复现）" } else { "发布已落地，响应链另查" },
                    );
                }
            }
        }
        // 短 D 丢发布为概率性（08-23 04:06 轮 0/30/100µs 档都曾丢失），
        // 零复现不构成"问题不存在"的证明。
        println!(
            "  [A4 检测] 本轮 {a4_detected}/8 档判定丢发布{}",
            if a4_detected == 0 { "（未复现——概率性问题，零复现不构成不存在证明）" } else { "——问题在位" }
        );
    }

fn run_mb(b: &mut Bench, line_n: u32) {
    let blk_n = (line_n / 10).max(20);
    let stride = |s: u64| (MB_SCRATCH_LEN / s) as u32; // 读次数
    let spin_n = 200u32;
    // (op, 名称, arg, 次数)。arg：常规 op = 循环次数，stride op = 步长。
    let ops: &[(u32, &str, u32, u32)] = &[
        (mb_op::RD_LINE_SHM, "rd_line_shm      共享窗同行 8B 读", line_n, line_n),
        (mb_op::RD_BLK256_SHM, "rd_blk256_shm    256B 整块读(=try_recv)", blk_n, blk_n),
        (mb_op::RD_BLK8_SHM, "rd_blk8_shm      256B 显式 32×8B", blk_n, blk_n),
        (mb_op::WR_LINE_SHM, "wr_line_shm      共享窗 8B 写+fence", line_n, line_n),
        (mb_op::RD_LINE_LOCAL, "rd_line_local    本地 .bss 同行 8B 读", line_n, line_n),
        (mb_op::RD_BLK256_LOCAL, "rd_blk256_local  本地 256B 整块读", blk_n, blk_n),
        (mb_op::WR_LINE_LOCAL, "wr_line_local    本地 8B 写+fence", line_n, line_n),
        (mb_op::RD_STRIDE_SHM, "rd_stride_shm@64 跨行步进 64B", 64, stride(64)),
        (mb_op::RD_STRIDE_SHM, "rd_stride_shm@512 跨行步进 512B", 512, stride(512)),
        (mb_op::RD_MMBOX_MSGSTAT, "rd_mmbox_msgstat mailbox MSGSTATUS", line_n, line_n),
        (mb_op::RD_MMBOX_IRQEN, "rd_mmbox_irqen  mailbox IRQENABLE", line_n, line_n),
        (mb_op::RD_MMBOX_FIFOSTAT, "rd_mmbox_fifostat mailbox FIFOSTATUS", line_n, line_n),
        (mb_op::RD_MTIME, "rd_mtime         mtime MMIO 读", line_n, line_n),
        (mb_op::AQ_LOAD_SHM, "aq_load_shm      Acquire 原子读", line_n, line_n),
        (mb_op::REL_STORE_SHM, "rel_store_shm    Release 原子写", line_n, line_n),
        (mb_op::FENCE_ONLY, "fence_only       纯 SeqCst fence", line_n, line_n),
        (mb_op::SPIN_ITER, "spin_iter        自旋一轮(pending||urgent)", 0, spin_n),
        (mb_op::AQ_LOAD_LOCAL, "aq_load_local    本地 Acquire 原子读", line_n, line_n),
        (mb_op::RECV_EMPTY, "recv_empty       try_recv 空 ch0 一轮", 0, spin_n),
        (mb_op::TIMER_NOW, "timer_now        platform timer 全路径", 1000, 1000),
        // L0 归因闭合组（arg：RECV_EMPTY_CH=通道号 2，NOTIFY_N=0→默认 100）
        (mb_op::AQ_DISTINCT_SHM, "aq_distinct_shm  跨行16址轮转Acquire", line_n, line_n),
        (mb_op::COLD_AQ_SHM, "cold_aq_shm      跨行Acquire+mtime间隔", 0, 64),
        (mb_op::RECV_SEQ, "recv_seq         复刻try_recv全序列", 0, blk_n),
        (mb_op::SEND_SEQ, "send_seq         复刻try_send全序列", 0, blk_n),
        (mb_op::POSTCARD_RT, "postcard_rt      Message构解双向(PING形状)", 0, blk_n),
        (mb_op::RECV_EMPTY_CH, "recv_empty_ch2   try_recv 空 ch2 一轮", 2, spin_n),
        (mb_op::NOTIFY_N, "notify_n         门铃全成本(fence+MMIO)", 0, 100),
        (mb_op::SELF_ROUND, "self_round       真实ch0 send+recv往返", 0, blk_n),
        (mb_op::SELF_PEEK, "self_peek        真实ch0 peek×N(无Release)", 0, blk_n),
        (mb_op::DISPATCH_N, "dispatch_n       op内完整dispatch(PING)", 0, blk_n),
        (mb_op::NOW_GAPPED, "now_gapped       间隔20µs的mtime读", 0, 64),
        (mb_op::CYCLE_GAPPED, "cycle_gapped     间隔~20µs的mcycle读", 0, 64),
        (mb_op::CYCLE_HOT, "cycle_hot        mcycle热读×1000", 0, 1),
        (mb_op::CYCLE_CAL, "cycle_cal        5ms联标mcycle频率", 0, 1),
        // 计时源替换候选：AP 域 soc-timer 0xd4016000 空闲 counter1（mtime
        // 冷读税 24.5µs/笔 的根治候选，布局=上游 timer-k1x.c）。
        // clkon/rt_on 先行：首轮实锤 AP 域块时钟常闭，须先开门去复位；
        // rt_on 为 RCPU 本地域候选（esOS rtimer0@c0889000，无人用）——主选
        (mb_op::TMR_CLKON, "tmr_clkon        APBC开TIMERS1时钟+复位", 0, 1),
        (mb_op::TMR_RT_ON, "tmr_rt_on        AON_TIMER1开门+计数验证", 0, 1),
        (mb_op::TMR_AON_CAL, "tmr_aon_cal      5ms联标AON_TIMER1频率", 0, 1),
        (mb_op::TMR_AON_HOT, "tmr_aon_hot      AON c0 热读×4000", 0, 4000),
        (mb_op::TMR_AON_GAPPED, "tmr_aon_gapped   AON c0 间隔读×200(判据)", 0, 200),
        (mb_op::TMR_SETUP, "tmr_setup        soc-timer c1 自由运行化", 0, 1),
        (mb_op::TMR_HOT, "tmr_hot          counter1 热读×4000", 0, 4000),
        (mb_op::TMR_GAPPED, "tmr_gapped       间隔20µs读counter1", 0, 64),
        (mb_op::TMR_CAL, "tmr_cal          5ms联标counter1频率", 0, 1),
        // 排表尾：K3 若无 d4014000 块，本 op 读可能总线错误挂死固件
        (mb_op::TMR_B_SCAN, "tmr_b_scan       d4014000侦查(可能挂死)", 0, 1),
    ];
    println!("[mb] RP 内存/MMIO 微基准：行级 ×{line_n}，块级 ×{blk_n}（mtime 计时，含 ~ns 级循环开销）");
    let mut per: Vec<(usize, u64)> = Vec::new();
    // soc-timer 探针的原始 (ns, ck)——判读需要 ck 槽（打包寄存器快照）
    let mut tmr_setup_seen = false;
    let mut tmr_setup_ck: u64 = 0;
    let mut tmr_clkon_seen = false;
    let mut tmr_clkon_ck: u64 = 0;
    let mut tmr_rt_on_seen = false;
    let mut tmr_rt_on_ck: u64 = 0;
    let mut tmr_cal_ck: u64 = 0;
    let mut aon_cal_ck: u64 = 0;
    let mut aon_gapped_raw: (u64, u64) = (0, 0);
    let mut bscan_seen = false;
    let mut bscan_raw: (u64, u64) = (0, 0);
    for (i, &(op, name, arg, count)) in ops.iter().enumerate() {
        let (ns, ck) = b.membench_round(op, arg).unwrap_or_else(die);
        if ns == 0 && ck == 0 {
            if op == mb_op::TMR_B_SCAN {
                // 全零回读本身是有效结论：块不存在/时钟未开（未挂死）
                println!("  {name:<40} 全零回读（d4014000 无此块或时钟未开）");
                bscan_seen = true;
                continue;
            }
            if op == mb_op::TMR_AON_SCAN {
                // 结果只进 RP console log（三块逐一），AP 侧无判读数据
                println!("  {name:<40} 完成（TIMER2/3/4 结果见 RP console log）");
                continue;
            }
            println!("  {name:<40} 未知 op（固件/工具版本不匹配？）");
            continue;
        }
        match op {
            x if x == mb_op::TMR_SETUP => {
                tmr_setup_seen = true;
                tmr_setup_ck = ck;
            }
            x if x == mb_op::TMR_CLKON => {
                tmr_clkon_seen = true;
                tmr_clkon_ck = ck;
            }
            x if x == mb_op::TMR_RT_ON => {
                tmr_rt_on_seen = true;
                tmr_rt_on_ck = ck;
            }
            x if x == mb_op::TMR_CAL => tmr_cal_ck = ck,
            x if x == mb_op::TMR_AON_CAL => aon_cal_ck = ck,
            x if x == mb_op::TMR_AON_GAPPED => aon_gapped_raw = (ns, ck),
            x if x == mb_op::TMR_B_SCAN => {
                bscan_seen = true;
                bscan_raw = (ns, ck);
            }
            _ => {}
        }
        let each = ns / count.max(1) as u64;
        per.push((i, each));
        println!("  {name:<40} {:>9.1} µs 总 / {:>8.2} ns/次  ck={ck:#x}", ns as f64 / 1e3, each as f64);
    }
    let g = |want: usize| per.iter().find(|(j, _)| *j == want).map(|(_, v)| *v);
    // 按 op 查找（tmr 组表序随探针增删调整，positional 索引易漂移）
    let gop = |want: u32| {
        ops.iter()
            .position(|&(o, _, _, _)| o == want)
            .and_then(|i| per.iter().find(|(j, _)| *j == i).map(|(_, v)| *v))
    };
    println!("\n[mb] 判读");
    match (g(0), g(4)) {
        (Some(shm), Some(loc)) => {
            println!(
                "  8B 读：SHM {} ns vs LOCAL {} ns —— {}（22ns 是同地址合并效应而非缓存行驻留：真有驻留则 D2 自旋读陈旧索引，s2 不可能通过）",
                shm,
                loc,
                if (shm as i64 - loc as i64).abs() < shm as i64 / 4 {
                    "同价：固件数据与共享窗访存特性一致"
                } else if loc < shm / 4 {
                    "本地显著快：本地走缓存，共享窗无缓存（或更慢的端口）"
                } else {
                    "本地更慢？检查 .bss 是否也在外设 SRAM"
                }
            );
        }
        _ => {}
    }
    if let (Some(blk), Some(line)) = (g(1), g(0)) {
        println!(
            "  256B 块读 {} ns/次（顺序流水后 ≈ {} × 行价 {}）—— dseen 实测 110.5µs 假设{}验证",
            blk,
            blk / line.max(1),
            line,
            if blk >= 90_000 { "获" } else { "未获" }
        );
    }
    if let (Some(s64), Some(line)) = (g(7), g(0)) {
        println!(
            "  冷访问真实单价：stride@512 {} ns；顺序步进 {} ns（流水收益）；同行 {} ns（合并）",
            g(8).unwrap_or(0),
            s64,
            line
        );
    }
    if let Some(mm) = g(10) {
        println!(
            "  mailbox 状态寄存器读 {} ns/笔 —— 但 PEEK 直读同寄存器仅 ~150-250ns：此价 = 驱动访问器的 Acquire（regs() 每次 base.load(Acquire)，irq_enabled 再加 user_local.load(Acquire)）×2.2µs。ISR 舞步去 Acquire 后 ddrain 11.3µs → ~2-3µs",
            mm,
        );
    }
    if let Some(mt) = g(12) {
        println!("  mtime MMIO 读 {} ns/笔（dsched/ddisp 含 2-4 笔）", mt);
    }
    // 访存序开销判读（索引 13-16：aq/rel/fence/spin）。
    if let Some(aq) = g(13) {
        println!("  Acquire 原子读 {} ns/笔 vs 裸读 22ns —— 差额即内存序开销", aq);
    }
    if let Some(sp) = g(16) {
        println!(
            "  自旋一轮 {} ns（W=2.007s 折算预期 ~20070ns；= 6×Acquire 单价 ⇒ 坐实序开销主导）",
            sp
        );
    }
    if let (Some(aqs), Some(aql)) = (g(13), g(17)) {
        println!(
            "  Acquire 读：SHM {aqs} ns vs 本地 {aql} ns —— {}",
            if (aqs as i64 - aql as i64).abs() < aqs as i64 / 4 {
                "同价：原子指令本身 2.2µs（Atomics Wrapper 序列化），与目标地址无关"
            } else {
                "不同价：窗口特有开销，地址路径相关"
            }
        );
    }
    if let Some(re) = g(18) {
        println!(
            "  try_recv 空 ch0 一轮 {} ns（≈ magic+双索引 3×Acquire + 调用链开销；dseen 111.5µs 的前缀对照——差额大 ⇒ 代码取指/分发机械是大头）",
            re
        );
    }
    if let (Some(tn), Some(mt)) = (g(19), g(12)) {
        println!(
            "  timer() 全路径 {} ns/次 vs 裸 clint {} ns —— 差额 = platform Slot 查找路径的原子开销（dseen/ddisp 各含 1-3 次 timer()）",
            tn, mt
        );
    }
    // L0 归因闭合（索引 20-26）：dseen 107.8µs（dpre 24.3 + drx 45.6 +
    // dserde 38.0）中 fence 理论只解释 ~31µs（~14 笔 × 热循环 2.2µs），
    // ~75µs 无主。三假设：H1 冷 fence 单价贵于热循环 / H2 postcard 构解贵 /
    // H3 其他调用链。以下探针逐一钉死，结论决定 P3（fence 去冗余）与
    // W2（双向轮询）的排序。
    if let (Some(hot), Some(dist), Some(cold), Some(mt)) = (g(13), g(20), g(21), g(12)) {
        let net = cold.saturating_sub(mt);
        let verdict = if net > hot * 3 / 2 {
            "cold≫hot ⇒ H1 成立：dseen 真构成 = ~14 笔 × cold 而非 41 笔 × hot"
        } else {
            "cold≈hot ⇒ H1 证伪：fence 单价与冷热/地址无关，dseen 无主部分另寻他因（冷核执行？dd 冷/热对比）"
        };
        println!(
            "  fence 单价三口径：同址热 {hot} / 跨址 {dist} / 跨址+间隔 {cold}（扣 mtime ≈ {net}）ns —— {verdict}"
        );
    }
    if let (Some(rs), Some(ss), Some(pc)) = (g(22), g(23), g(24)) {
        println!(
            "  协议复刻：recv_seq {rs} / send_seq {ss} ns/次 + postcard 双向 {pc} ns/次 —— 对账 drx 45.6µs（≈recv_seq+stamps）、dserde 38.0µs（≈postcard）；recv_seq ≈ 5×fence 单价可交叉验证"
        );
    }
    if let (Some(e0), Some(e2)) = (g(18), g(25)) {
        println!(
            "  空轮询：ch0 {e0} / ch2 {e2} ns（≈3×Acquire；drain 每消息批前后各一次 ch2 检查，入 dpre 段）"
        );
    }
    if let Some(nn) = g(26) {
        println!(
            "  门铃 notify {nn} ns/次（fence+MMIO 写，ddisp 的 RP 侧成分；本轮 console 的空唤醒痕迹即本探针副作用，非异常）"
        );
    }
    if let (Some(sr), Some(sp), Some(rs), Some(ss)) = (g(27), g(28), g(22), g(23)) {
        println!(
            "  真实通道 vs scratch：self_round {sr} / self_peek {sp} ns vs recv_seq {rs} + send_seq {ss} —— self_round ≈ 两者之和 ⇒ 真实通道无溢价（85µs 无主另寻）；≫ 之和 ⇒ H5：真实通道上下文隐藏税"
        );
    }
    // L0 终拆后续（2026-08-21 板上细拆：didx 7.8/dslot 37.2/dmth 1.1/drest 34.0
    // ——索引读完美对账，缺口= 槽读+Release 与 dispatch/postcard 各 ~34µs）。
    if let (Some(dn), Some(ng)) = (g(29), g(30)) {
        let per_read = (ng.saturating_sub(20_000)) / 2;
        println!(
            "  dispatch_n {dn} ns/次 vs drest 34.0µs —— op 内 ≪ drest ⇒ 纯上下文税（mark 链/代码位置）；≈ drest ⇒ dispatch 本体慢\n  now_gapped 每轮 {ng} ns（含 20µs 忙等 + 2 笔 mtime 读）⇒ 间隔读 ≈ {per_read} ns/笔 vs RD_MTIME 热循环 106ns",
        );
    }
    if let Some(cg) = g(31) {
        // 板上实测每轮 wall ≈6.18ms（≫12k 圈忙等预期）且 total 反推 mcycle
        // 恰以 24MHz 计数——mcycle 间隔读同样有跨域税，热单价/频率由
        // cycle_cal 联标定案。
        let _ = cg;
        println!(
            "  cycle_gapped 每轮 wall ≈6.18ms（≫忙等预期）⇒ mcycle 间隔读同样有跨域税（热读价与频率见 cycle_cal）"
        );
    }
    if let (Some(hot), Some(span)) = (g(32), g(33)) {
        let mhz = span as f64 / 5_000.0; // ck = 5ms 联标 cycle 差
        let hot_ns = hot as f64 / 1000.0 / mhz * 1000.0;
        println!(
            "  cycle_cal：热读 {hot} cycle/千笔（≈{hot_ns:.0} ns/笔 @ {mhz:.2}MHz）；mcycle 频率 = {mhz:.2} MHz —— 热读快且频率≈核频 ⇒ 仅冷读慢（stamp 链可短间隔热身救）；热读慢或=24MHz ⇒ mcycle 与 mtime 同源同税，计时源需第三方案"
        );
    }
    // 计时源替换候选三面判读：AP 域 soc-timer 0xd4016000 空闲 counter1
    // （AP 仅用 counter0 做广播、块时钟常开；布局=上游 timer-k1x.c）。
    // 免冷读税即可承接 step/SVC 计时（mtime 冷读 24.5µs/笔 的根治）。
    // 注意表序 clkon/rt_on 在三面前（先开门，首轮实锤两块时钟均常闭）。
    if tmr_clkon_seen {
        let cer = tmr_clkon_ck >> 32;
        let d = tmr_clkon_ck & 0xffff_ffff;
        println!(
            "  tmr_clkon：CER={cer:#x}（bit1={}）1ms 计数 Δ={d} —— Δ≈12800 ⇒ 12.8MHz 活（mux=0）；Δ≈其他 ⇒ 按 mux 换算；0 ⇒ 仍死（APBC 写也丢=跨域过滤坐实，见 RP console variant）",
            cer & 2 != 0
        );
    }
    if tmr_rt_on_seen {
        let cer = tmr_rt_on_ck >> 32;
        let d = tmr_rt_on_ck & 0xffff_ffff;
        println!(
            "  tmr_rt_on：CER={cer:#x}（bit0={}）1ms 计数 Δ={d} —— Δ≈25600 ⇒ 25.6MHz 活（布局假设成立）；≈12800 ⇒ 12.8MHz（SEL 读回非 0，按 tmr_aon_cal 实测换算）；0 ⇒ 仍死（查 rccu 回读与 aon_scan，RP console）",
            cer & 1 != 0
        );
    }
    if let Some(ac) = gop(mb_op::TMR_AON_CAL) {
        let hz = ac as f64 * 24_000_000.0 / aon_cal_ck.max(1) as f64;
        println!(
            "  tmr_aon_cal：AON_TIMER1 5ms 走 {ac} ticks（mtime 窗口 {aon_cal_ck}）⇒ ≈{hz:.0} Hz（标称 25.6MHz）；单调 ⇒ 自由运行成立"
        );
    }
    if let Some(ah) = gop(mb_op::TMR_AON_HOT) {
        println!(
            "  tmr_aon_hot {ah} ns/笔（mtime 括号计时；对照 counter1 277ns / rd_mtime 热 106ns / mailbox 寄存器 148ns——本地域应 ≤ 150ns）"
        );
    }
    if aon_gapped_raw != (0, 0) {
        let (aon_sum, wall) = aon_gapped_raw;
        // Σaon = AON 自己计的每轮真实耗时（含忙等）；wall = mtime 计的
        // 整段。wall − Σaon 摊 200 轮 = 每轮重锁税（正=有税）
        let hz = if let Some(ac) = gop(mb_op::TMR_AON_CAL) {
            ac as f64 * 24_000_000.0 / aon_cal_ck.max(1) as f64
        } else {
            25_600_000.0
        };
        let wall_ns = t2ns(wall, 24_000_000);
        let aon_ns = ((aon_sum as f64) * 1e9 / hz) as u64;
        let tax = (wall_ns.saturating_sub(aon_ns)) / 200;
        println!(
            "  tmr_aon_gapped：wall {}µs vs Σaon {}µs/200轮 ⇒ 每轮间隔读税 ≈ {tax} ns —— <1µs ⇒ 本地域免跨互连税、换源可行（对齐 mark 间隔复测后迁移）；~13µs ⇒ 与 counter1 同病，弃；另每轮忙等 wall≈{}ns（Σaon/200）",
            wall_ns / 1000,
            aon_ns / 1000,
            aon_ns / 200
        );
    }
    if let (Some(ng), Some(tg)) = (g(30), gop(mb_op::TMR_GAPPED)) {
        let delta = tg as i64 - ng as i64;
        println!(
            "  tmr_gapped 每轮 {tg} vs now_gapped {ng}（结构仅差 1 笔候选读）⇒ 候选冷读 Δ={delta} ns —— |Δ|<1µs ⇒ 免跨域税、计时源可迁；Δ≈24500 ⇒ 与 mtime 同病，弃"
        );
    }
    if let Some(th) = gop(mb_op::TMR_HOT) {
        println!(
            "  tmr_hot {th} ns/笔（mtime 括号冷读税已摊薄 ~12ns；对照 rd_mtime 热 106ns / mailbox 寄存器 148ns）"
        );
    }
    if let Some(tc) = gop(mb_op::TMR_CAL) {
        let hz = tc as f64 * 24_000_000.0 / tmr_cal_ck.max(1) as f64;
        println!(
            "  tmr_cal：counter1 5ms 走 {tc} ticks（mtime 窗口 {tmr_cal_ck} ticks）⇒ ≈{hz:.0} Hz（预期 1MHz = AP dts timer-frequency）；值≈5000 且单调 ⇒ 自由运行、未被 AP 重编程打断"
        );
    }
    if tmr_setup_seen {
        let retries = tmr_setup_ck & 0xffff_ffff;
        let cer = tmr_setup_ck >> 32;
        println!(
            "  tmr_setup：CER={cer:#x}（bit1={}，retries={retries}；cer/cmr/ccr/cr0/cr1 快照见 RP console log）",
            cer & 2 != 0
        );
    }
    if bscan_seen {
        let (ns, ck) = bscan_raw;
        let a = ns & 0xffff_ffff;
        let b = ns >> 32;
        let cer = ck >> 32;
        let cr1 = ck & 0xffff_ffff;
        println!(
            "  tmr_b_scan：d4014000 CR0 {a:#x}→{b:#x}（Δ={}）/CER={cer:#x}/CR1={cr1:#x} —— Δ>0 且≈1000（1ms@1MHz）⇒ 独立块活着（无共享候选）；全 0 或不变 ⇒ 无块/时钟未开",
            b.wrapping_sub(a)
        );
    }
    // H8 新鲜写衰减扫描：drx/dserde 的 32µs 级确定性差额（探针全快、
    // 真实路径全慢、预热不降）的最后候选——读"AP 刚写（posted 未落地）"
    // 的行。
    fresh_scan(b);

    // 寄存器探测（手册 06/17 章来源；顺序：安全在前，未知窗口在后）。
    println!("\n[mb] 寄存器探测（各 1000 次读：末值 + 单价）");
    let peeks: &[(u32, &str)] = &[
        (0x0001_8700, "RCPU 本地 SRAM 别名 +0x18700（应=0x7cf=scratch 写值）"),
        (0xC088_C0C0, "RCPU_BUS_CLK_CTRL（AXI/APB 分频）"),
        (0xC088_C0C4, "RT24_CORE0_CLK_CTRL"),
        (0xC088_C0C8, "RT24_CORE1_CLK_CTRL（本核）"),
        (0xC076_0000, "r_mailbox[0] REVISION（本地窗口存在性）"),
        (0xC076_1000, "r_mailbox[4] REVISION（期望版本串）"),
        (0xCAC9_1000, "mailbox4 主域 REVISION（对照，现用路径）"),
        // 活寄存器经本地窗口 vs 主域——mailbox 迁移的决胜探针：
        // 常量寄存器（REVISION）两条路径都快（~150-180ns），不能代表
        // MSGSTATUS/FIFOSTATUS/IRQENABLE 这类跨时钟域的活状态读。
        (0xC076_10C0, "r_mbox[4] MSGSTATUS[0]（本地窗口·活寄存器）"),
        (0xC076_1080, "r_mbox[4] FIFOSTATUS[0]（本地窗口·活寄存器）"),
        (0xC076_1118, "r_mbox[4] IRQENABLE_SET[u1]（本地窗口·活寄存器）"),
        (0xCAC9_1118, "mailbox4 IRQENABLE_SET[u1]（主域对照，现用）"),
        // 硬件 spinlock（手册 16.7，锁获取 <200 周期——Dekker fence 替代候选）
        (0xCAC9_1D00, "spinlock VER（期望 0x312E3030）"),
        (0xCAC9_1D04, "spinlock SSTATUS（期望 32=单元数）"),
        (0xCAC9_1D08, "spinlock STATUS（各单元占用位图）"),
    ];
    let mut vals: Vec<(u32, u32, u64)> = Vec::new();
    for &(addr, name) in peeks {
        let (ns, v) = b.membench_round(mb_op::PEEK_T, addr).unwrap_or_else(die);
        vals.push((addr, v as u32, ns / 1000));
        println!(
            "  {name:<52} {:#010x} → {:#010x}  {:.1} ns/笔",
            addr,
            v,
            ns as f64 / 1000.0
        );
    }
    let val = |a: u32| vals.iter().find(|(x, _, _)| *x == a).map(|(_, v, _)| *v);
    let cost = |a: u32| vals.iter().find(|(x, _, _)| *x == a).map(|(_, _, ns)| *ns);
    if let Some(v) = val(0xC088_C0C8) {
        let sel = (v >> 4) & 3;
        let div = (v & 3) + 1;
        let src = match sel {
            0 => "rcpu_sys_clk(491.52/614.4 mux，默认档手册未载)",
            1 => "614 MHz",
            2 => "491 MHz",
            _ => "Reserved",
        };
        println!("  ⇒ core1: SEL={sel}({src}) DIV={div}；",);
    }
    if let Some(v) = val(0xC088_C0C0) {
        let axi = ((v >> 3) & 7) + 1;
        let apb = (v & 7) + 1;
        println!("  ⇒ rcpu 总线: AXI=核心/{axi}, APB=AXI/{apb}");
    }
    // mailbox 判读：本地窗口 vs 主域。
    if let (Some(rb), Some(md)) = (cost(0xC076_1118), cost(0xCAC9_1118)) {
        let rb_v = val(0xC076_1118).unwrap_or(0);
        let md_v = val(0xCAC9_1118).unwrap_or(0);
        let mirrored = rb_v == md_v;
        println!(
            "  ⇒ 活寄存器读：本地窗口 {rb} ns vs 主域 {md} ns；EN[u1] 值 本地={rb_v:#x} vs 主域={md_v:#x} —— {}",
            if mirrored {
                "同值，窗口镜像（迁移可选）"
            } else {
                "不同值：本地窗口是独立硬件/不镜像主集群——迁移作废；寄存器本身够快（~150-250ns），该修的是驱动访问器的 Acquire"
            }
        );
    }
}

/// dd 单轮记录。
struct DdRow {
    tag: u8,
    sent_ipi: bool,
    rtt_ns: u64,
    send_ns: u64,
    ddrain_ns: u64,
    ddisp_ns: u64,
    dseen_ns: u64,
    svc_ns: u64,
    /// t_isr(mtime ns) − kpre(内核 ns)：门铃去程 X + 未知钟差常数 ΔE。
    /// **可为负**（mtime 自上电计、内核时钟自内核启动计，epoch 不同），
    /// 且含跨轮钟漂（ppm × 间隔）——只看轮内抖动，不做绝对值结论。
    x_plus_o: i64,
    /// (t_isr−kpre) + (kirq−t_seen)：X + RP 尾段 + Y，钟差无关（真值）。
    s_val: i64,
    /// AP 回程 = (t1−t_send_end) − (t_seen−t_isr) − s_val：IRQ 入口 →
    /// 用户态 mono_ns（handler + 唤醒 + ioctl 返回 + vdso），钟差无关。
    ap_ret: i64,
    /// RTT 闭合残差：rtt − (send+ddrain+ddisp+dseen+s_val+ap_ret)，应 ≈ 0。
    closure: i64,
}

/// 场景 dd：D1 门铃投递分解（AP 内核戳 × RP mtime 戳交叉测量）。
///
/// 每轮（间隔默认 2W 保证 RP 睡眠 → 全 D1）：PING + SVC 探针 + 内核
/// 双戳（NOTIFY 门铃 MMIO 写前 / mailbox IRQ handler 入口）。
///
/// 钟差无关恒等式（两钟 epoch 差 ΔE 在单轮 ~300µs 窗口内漂移 ~ns 级）：
/// - s_val = (t_isr−kpre) + (kirq−t_seen) = X + RP尾段 + Y（去程门铃 +
///   handler 出口→RP 门铃写 + 回程门铃，全部真值）；
/// - ap_ret = R − s_val，其中 R = (t1−t_send_end)−(t_seen−t_isr)；
/// - 闭合：rtt ≈ send + ddrain + ddisp + dseen + s_val + ap_ret。
fn run_dd(b: &mut Bench, n: usize, interval_cfg: Option<u64>, warmup: usize) {
    warmup_paced(b, warmup);

    let cal = b.snapshot().unwrap_or_else(die);
    let freq = cal.c[stat_idx::FREQ_HZ];
    b.freq_hz = freq;
    let w_ns = if cal.c[stat_idx::WIN_MAX_NS] == u64::MAX {
        println!("[warn] RP 无完整窗口样本，W 用 2s 假设值");
        2_000_000_000
    } else {
        cal.c[stat_idx::WIN_MAX_NS]
    };
    let interval = interval_cfg.unwrap_or((w_ns * 2).max(2_000_000_000));
    println!(
        "[dd] n={n} interval={}ns W={:.1}ms freq={freq} （kpre/kirq 探针已开：每轮 +2 ioctl，不计 sysc）",
        interval_cfg.map(|v| format!("{v} (指定)")).unwrap_or_else(|| format!("{interval} (默认 2W)")),
        w_ns as f64 / 1e6,
    );
    // kpre/kirq 双戳探针；spin_await（H9 对照）下它是额外内核活动，会
    // 污染对照条件——自动关闭（kpre/kirq 记 0，X+o/S/APret 恒等式仍自洽）。
    b.probe_kts = !b.spin_await;

    let mut rows: Vec<DdRow> = Vec::with_capacity(n);
    for seq in 0..n as u64 {
        // 先睡再发：保证 RP 处于目标路径状态（大间隔 = D1 睡眠；小间隔 =
        // D2 弹性窗内）。
        sleep_until(mono_ns() + interval);
        let (r, out) = b.ping_round(seq).unwrap_or_else(die);
        // SVC 探针：此时读到的 SVC_LAST 即本轮 PING 的服务时长（STATS
        // 自己的 svc 在其 handler 返回后才落账，不覆盖本次读数）。
        let svc_ns = b.stat_round(stat_idx::SVC_LAST_NS as u32).unwrap_or_else(die);

        let ddrain = t2ns(r.3.saturating_sub(r.2), freq);
        let ddisp = t2ns(r.4.saturating_sub(r.3), freq);
        let dseen = t2ns(r.5.saturating_sub(r.4), freq);
        let t_isr_ns = t2ns(r.2, freq);
        let t_seen_ns = t2ns(r.5, freq);
        // 带符号跨钟差：X+ΔE 可负（epoch 不同），saturating 会破坏 S 恒等式。
        let x_plus_o = t_isr_ns as i64 - out.kpre as i64;
        let s_val = x_plus_o + (out.kirq as i64 - t_seen_ns as i64);
        let r_val = (out.t1 - out.t_send_end) as i64 - (t_seen_ns - t_isr_ns) as i64;
        let ap_ret = r_val - s_val;
        let rtt = out.t1 - out.t0;
        let send = out.t_send_end - out.t0;
        let sum = send as i64 + ddrain as i64 + ddisp as i64 + dseen as i64 + s_val + ap_ret;
        rows.push(DdRow {
            tag: r.1,
            sent_ipi: out.sent_ipi,
            rtt_ns: rtt,
            send_ns: send,
            ddrain_ns: ddrain,
            ddisp_ns: ddisp,
            dseen_ns: dseen,
            svc_ns,
            x_plus_o,
            s_val,
            ap_ret,
            closure: rtt as i64 - sum,
        });
        println!(
            "  rd#{seq} tag=D{} ipi={} rtt={:>7.1} send={:>6.1} ddrain={:>6.1} ddisp={:>6.1} dseen={:>9.1} svc={:>6.1} X+o={:>8.1} S={:>7.1} APret={:>7.1} 闭环={:+.1} µs",
            r.1,
            out.sent_ipi as u8,
            rtt as f64 / 1e3,
            send as f64 / 1e3,
            ddrain as f64 / 1e3,
            ddisp as f64 / 1e3,
            dseen as f64 / 1e3,
            svc_ns as f64 / 1e3,
            x_plus_o as f64 / 1e3,
            s_val as f64 / 1e3,
            ap_ret as f64 / 1e3,
            (rtt as i64 - sum) as f64 / 1e3,
        );
    }
    b.probe_kts = false;
    // svc 尾段分解已移除：三次 stat_round 锁存混线（板上实锤 234µs 假值，
    // 见 run_measured 同款注释）——待内核侧派生计数器随 ②a-v2 一并上。

    // 按 tag 分桶输出。戳有效性（板上实证 2026-08-20）：
    // - svc/rtt/send 用本条消息自己的戳，任何发现路径下有效；
    // - dseen 依赖 T_SCHED（唤醒入口戳，连续流下 process_elastic 不重入
    //   故不刷新——D2/D3 样本线性膨胀至数百 ms）；ddrain/ddisp/X+o/S 依赖
    //   唤醒链戳（t_isr/t_drain），非 D1 下是上一周期残值。后四者仅 D1
    //   桶输出。
    let d1: Vec<&DdRow> = rows.iter().filter(|x| x.tag == TAG_D1 && x.sent_ipi).collect();
    let bad = rows.len() - d1.len();
    if bad > 0 {
        println!("[dd] ⚠ {bad}/{} 轮非 D1 或未发门铃——按 tag 分桶统计（间隔 <W 时 D2 样本同样有分析价值）", rows.len());
    }
    for (tag, name) in [(TAG_D1, "D1"), (2u8, "D2"), (3, "D3"), (4, "D4")] {
        let bucket: Vec<&DdRow> = if tag == TAG_D1 {
            d1.iter().copied().collect()
        } else {
            rows.iter().filter(|x| x.tag == tag).collect()
        };
        if bucket.is_empty() {
            continue;
        }
        let col = |f: &dyn Fn(&&DdRow) -> u64| -> Vec<u64> { bucket.iter().map(f).collect() };
        if tag != TAG_D1 {
            println!("\n[dd 分布（{name} 样本，n={}）——dseen/ddrain/ddisp/X+o/S 依赖唤醒链戳，非 D1 下语义失效，不输出]", bucket.len());
        } else {
            println!("\n[dd 分布（D1 样本，n={}）]", bucket.len());
        }
        show("RTT", &calc(&col(&|x| x.rtt_ns)));
        show("send（AP 用户态发送段）", &calc(&col(&|x| x.send_ns)));
        show("svc（STATS 探针）", &calc(&col(&|x| x.svc_ns)));
        if tag == TAG_D1 {
            show("t_drain−t_isr（ISR 内 MMIO 舞步）", &calc(&col(&|x| x.ddrain_ns)));
            show("t_sched−t_drain（trap+派发+恢复）", &calc(&col(&|x| x.ddisp_ns)));
            show("t_seen−t_sched（取读+handler）", &calc(&col(&|x| x.dseen_ns)));
        }
        // X+o 含未知钟差常数：去 min 归零后看抖动（负值合法，见 DdRow 文档）。
        if tag == TAG_D1 {
            let xs: Vec<i64> = bucket.iter().map(|x| x.x_plus_o).collect();
            let x_min = *xs.iter().min().unwrap_or(&0);
            let xs_rel: Vec<u64> = xs.iter().map(|&v| (v - x_min).max(0) as u64).collect();
            show("X+o−min（去程门铃抖动，常数已归零）", &calc(&xs_rel));
            if bucket.iter().any(|x| x.s_val < 0 || x.ap_ret < 0) {
                println!("  ⚠ 出现负 S/AP 回程样本（戳未配对？内核未含 RD_KTS？），下列统计取 max(0,·)");
            }
            show("S = X+RP尾+Y（钟差无关）", &calc(&col(&|x| x.s_val.max(0) as u64)));
            show("AP 回程 = IRQ→用户态（钟差无关）", &calc(&col(&|x| x.ap_ret.max(0) as u64)));
            let closure: Vec<u64> = bucket.iter().map(|x| x.closure.unsigned_abs()).collect();
            show("闭环残差 |rtt−Σ|（应 ≈0）", &calc(&closure));
        }
    }
    if d1.is_empty() {
        println!("[dd] 无 D1 样本（间隔 <2W 时正常——看上方 D2/D3 桶）");
    }
    println!(
        "\n[dd] 预算恒等式：rtt = send + ddrain + ddisp + dseen + (X+RP尾+Y) + AP回程\n\
         \x20    （X 单独值受钟差常数污染：两钟 epoch 不同且跨轮有 ppm 级漂移，\n\
         \x20     只看抖动；S 与 AP 回程为钟差无关量，可作绝对结论）"
    );
}

fn usage() -> ! {
    eprintln!(
        "用法: user-test-bench <s0|s1|s2|s4|s6|raw|mb|dd|lit> [iterations] [interval_ns] [warmup]\n\
         \x20 s0  标定（dump 计数器 + 弹性窗口 W）\n\
         \x20 s1  空闲唤醒（间隔 2×W，要求 100% D1）\n\
         \x20 s2  自旋命中（间隔 W/4，要求 ≥90% D2）\n\
         \x20 s4  竞态扫描（间隔随机 (0,2W)，D4 命中率）\n\
         \x20 s6  边界流（间隔 W，D1/D2 混合）\n\
         \x20 raw 自由间隔（interval_ns 必填）\n\
         \x20 mb  RP 内存/MMIO 微基准（iterations = 行级次数，默认 2000）\n\
         \x20 dd  D1/D2 分解交叉测量（轮数默认 30，间隔默认 2W）\n\
         \x20 lit 跨核免 fence 顺序性实验（LITMUS，L1/L2/L3 含对照组）"
    );
    std::process::exit(1);
}

fn main() {
    println!("╔══════════════════════════════════════════╗");
    println!("║   Path-separated IPC Latency Bench       ║");
    println!("╚══════════════════════════════════════════╝");

    let args: Vec<String> = std::env::args().collect();
    let scen = args.get(1).cloned().unwrap_or_default();
    if !matches!(scen.as_str(), "s0" | "s1" | "s2" | "s4" | "s6" | "raw" | "mb" | "dd" | "lit") {
        usage();
    }
    let default_n: usize = match scen.as_str() {
        "mb" => 2000,
        "dd" => 30,
        "lit" => 1,
        // 全部场景默认控制在 ~2min：单轮开销 = interval（W≈2s 量级），
        // 大样本用命令行显式传 n。
        "s1" => 25,  // 25×4s ≈ 100s
        "s4" => 50,  // 50×随机(0,2W)均值 ≈ 50×2s ≈ 100s
        "s6" => 50,  // 50×W ≈ 100s
        _ => 1000,   // s2：interval=200µs，1000 轮 ≈ 2s
    };
    let n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(default_n);
    let interval_cfg: Option<u64> = args.get(3).and_then(|s| s.parse().ok());
    let warmup: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(50);
    if scen == "raw" && interval_cfg.is_none() {
        eprintln!("raw 场景必须指定 interval_ns");
        usage();
    }

    let rt = RtShm::open().expect("open /dev/rt_shm 失败");
    rt.clear_pending().expect("CLR_PENDING 失败");

    let shm = rt.shm();
    let timeout = std::time::Duration::from_secs(5);
    let start = std::time::Instant::now();
    while !shm.is_valid() {
        if start.elapsed() > timeout {
            panic!(
                "共享内存 5s 内未 valid——排查：\
                 (1) RP bin 无 magic 看门狗且启动链迟到写回已清 magic（RP UART 无自愈日志）；\
                 (2) RP 固件未运行（应见 '[InterCom] initialized'）；\
                 (3) /dev/rt_shm 未 probe（dmesg）"
            );
        }
        // （原"每 100ms 经 NOTIFY 整窗强刷视图"已撤——窗口 PMA 物理非缓存，
        // 裸轮询即 SRAM 真值，无需任何刷新。）
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    println!("[setup] shm valid");

    // 清理上一轮残留回包
    {
        let rx = shm.receiver(CH1).unwrap();
        let mut stale = 0;
        while let Some(m) = rx.try_recv() {
            stale += 1;
            let _ = m.ty();
        }
        if stale > 0 {
            println!("[setup] 清理上轮残留消息 {stale} 条");
        }
        // （原消费发布已撤——非缓存窗口 read 推进直达 SRAM。）
    }

    if std::env::var("BENCH_NO_RT").is_ok() {
        // 对照诊断开关：不绑核不设 FIFO——区分"CPU2 pin + 跨核唤醒"类问题
        println!("[setup] BENCH_NO_RT=1：跳过 CPU pin / SCHED_FIFO");
    } else {
        apply_realtime(2); // 避开处理 IRQ 的 core0（与 user-test-sched 相同）
    }
    spawn_watchdog();

    let mut b = Bench {
        rt,
        rid_next: 0x5100_0000, // 避开其他 user-app 的 rid=1.. 低位段
        rounds_sent: 0,
        backpressure: 0,
        stray: 0,
        spurious_wake: 0,
        last_seq: 0,
        freq_hz: 0,
        verbose_left: 3,
        probe_kts: false,
        spin_await: std::env::var("BENCH_SPIN_AWAIT").is_ok(),
    };
    if b.spin_await {
        println!("[cfg] spin-await 模式：AWAIT syscall → 纯用户态轮询（H9 对照）");
    }

    match scen.as_str() {
        "s0" => run_s0(&mut b, warmup),
        "mb" => run_mb(&mut b, n as u32),
        "dd" => run_dd(&mut b, n, interval_cfg, warmup),
        "lit" => run_lit(&mut b),
        _ => run_measured(&mut b, &scen, n, interval_cfg, warmup),
    }
}
