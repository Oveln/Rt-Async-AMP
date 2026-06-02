//! DELAY 延时抖动精密测试
//!
//! 测量方法：对称"三明治"计时
//!
//!   sync ECHO ──→ rt-async ──→ IPI ──→ drain
//!   T0 = mono_ns()               // rt-async idle
//!   [DELAY(us)?] ──→ rt-async busy-wait
//!   ECHO ──→ rt-async ──→ IPI
//!   T1 = mono_ns()   ←── await_ipi
//!   drain
//!
//!   baseline = T1−T0 (无 DELAY, 纯 RPC 往返开销)
//!   actual   = (T1−T0) − mean(baseline)
//!   jitter   = actual − expected
//!
//! 优化措施：
//! - CLOCK_MONOTONIC 原始纳秒时间戳
//! - SCHED_FIFO 实时调度 + CPU 亲和性（减少 Linux 调度抖动）
//! - 充分预热排冷 + 大样本量 (300 轮)
//! - 统计：min / max / mean / stddev / CV / p50 / p95 / p99 + 直方图

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::IntoRawFd;

use ov_rpc::define_service_client;

#[allow(dead_code)]
mod amp {
    include!(concat!(env!("OUT_DIR"), "/amp_gen.rs"));
}

const RT_SHM_IOC_NOTIFY: libc::c_ulong = amp::RTSHM_IOC_NOTIFY as libc::c_ulong;
const RT_SHM_IOC_AWAIT: libc::c_ulong = amp::RTSHM_IOC_AWAIT as libc::c_ulong;
const RT_SHM_IOC_CLR_PENDING: libc::c_ulong = amp::RTSHM_IOC_CLR_PENDING as libc::c_ulong;
const SHM_SIZE: usize = amp::SHMSIZE;

define_service_client! {
    RtAsyncRpc {
        ECHO: 0 => call echo(val: u32) -> u32;
        ADD:  1 => call add(a: i32, b: i32) -> i32;
        DELAY: 2 => send delay(us: u32);
    }
}

// ── SHM / ioctl ─────────────────────────────────────────────

fn do_ioctl(fd: libc::c_int, cmd: libc::c_ulong, arg: libc::c_ulong) -> io::Result<libc::c_int> {
    let r = unsafe { libc::ioctl(fd, cmd as _, arg) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

struct RtShm {
    fd: libc::c_int,
    ptr: *mut std::ffi::c_void,
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
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
        Ok(Self { fd, ptr })
    }

    #[inline]
    fn shm_addr(&self) -> usize {
        self.ptr as usize
    }
    fn notify(&self) -> io::Result<()> {
        do_ioctl(self.fd, RT_SHM_IOC_NOTIFY, 0).map(|_| ())
    }
    fn clear_pending(&self) -> io::Result<()> {
        do_ioctl(self.fd, RT_SHM_IOC_CLR_PENDING, 0).map(|_| ())
    }
    fn await_ipi(&self) -> io::Result<()> {
        do_ioctl(self.fd, RT_SHM_IOC_AWAIT, 0).map(|_| ())
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

// ── Timing ──────────────────────────────────────────────────

/// CLOCK_MONOTONIC 纳秒时间戳，开销 ~20-40ns (vdso)
#[inline(always)]
fn mono_ns() -> u64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

// ── System tuning ───────────────────────────────────────────

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
        "  CPU{cpu} {} | SCHED_FIFO {}",
        if cpu_ok { "✓" } else { "✗" },
        if rt_ok { "✓" } else { "✗ (需 root)" }
    );
}

#[cfg(not(target_os = "linux"))]
fn apply_realtime(cpu: usize) {
    println!("  CPU{cpu} ✗ | SCHED_FIFO ✗ (非 Linux 主机，仅占位)");
}

// ── Measurement primitives ──────────────────────────────────

/// 单次 ECHO round-trip（预热 / 同步用）
fn echo_sync(c: &mut RtAsyncRpc, rt: &RtShm) {
    let notify = || rt.notify().unwrap();
    let rid = c.echo(0xABAD_u32, notify).unwrap();
    rt.await_ipi().unwrap();
    c.poll_responses();
    let _: u32 = c.recv_for(rid).unwrap().unwrap();
}

/// 对称"三明治"计时。
/// 若 `delay_us = Some(us)`，在 sync 和 ECHO 之间插入 DELAY；
/// 若 `delay_us = None`，则测量纯 RPC 往返开销（baseline）。
///
/// 两种路径使用完全相同的骨架，仅差一条 DELAY send，
/// 因此 baseline 与 DELAY 测量的系统开销几乎完全对称。
fn sandwich(c: &mut RtAsyncRpc, rt: &RtShm, delay_us: Option<u32>) -> u64 {
    let notify = || rt.notify().unwrap();

    // sync: 确保 rt-async 回到 idle 状态
    echo_sync(c, rt);

    // T0: 即将开始测量窗口
    let t0 = mono_ns();

    if let Some(us) = delay_us {
        c.delay(us, notify).unwrap(); // DELAY (one-way, rt-async busy-waits)
    }
    let rid = c.echo(0xEEEE_u32, notify).unwrap(); // ECHO (排在 DELAY 之后)
    rt.await_ipi().unwrap(); // 等待 ECHO response IPI

    // T1: 测量窗口结束
    let t1 = mono_ns();

    c.poll_responses();
    let _: u32 = c.recv_for(rid).unwrap().unwrap();

    t1 - t0
}

// ── Statistics ──────────────────────────────────────────────

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
    let cv = if s.mean > 0.0 {
        s.stddev / s.mean * 100.0
    } else {
        0.0
    };
    println!(
        "  [{label}] n={} min={:.1} p50={:.1} mean={:.1} p95={:.1} p99={:.1} max={:.1} µs  σ={:.2}µs  CV={cv:.2}%",
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
        println!("  {:>7.1}µs │{:>4}│{}", b as f64 / 1e3, cnt, bar);
        b += bucket_ns;
    }
}

// ── Main ────────────────────────────────────────────────────

fn main() {
    println!("╔══════════════════════════════════════════╗");
    println!("║       DELAY Jitter Precision Test        ║");
    println!("╚══════════════════════════════════════════╝\n");

    let rt = RtShm::open().expect("open /dev/rt_shm");
    rt.clear_pending().unwrap();
    let mut c = RtAsyncRpc::new(rt.shm_addr());

    println!("[setup]");
    apply_realtime(2); // pin to core 2 (避开处理中断的 core 0)
    println!();

    const WARMUP: usize = 30;
    const N: usize = 300;
    const DELAYS: &[u32] = &[0, 50, 100, 200, 500, 1000, 5000];

    // ── Warmup ──
    print!("[warmup] {WARMUP} ECHO round-trips... ");
    for _ in 0..WARMUP {
        echo_sync(&mut c, &rt);
    }
    println!("ok\n");

    // ── Baseline: ECHO→ECHO (no DELAY) ──
    println!("[baseline] {N} rounds, no DELAY");
    let bl: Vec<u64> = (0..N)
        .map(|_| sandwich(&mut c, &rt, None))
        .collect();
    let bs = calc(&bl);
    show("baseline", &bs);
    hist(&bl, 500);
    let bl_mean = bs.mean;
    println!("  → baseline mean = {:.1} µs\n", bl_mean / 1e3);

    // ── DELAY tests ──
    for &us in DELAYS {
        println!("{}", "─".repeat(55));
        println!("[DELAY {us}µs] {N} rounds");

        let raw: Vec<u64> = (0..N)
            .map(|_| sandwich(&mut c, &rt, Some(us)))
            .collect();

        // actual = raw − baseline_mean
        let actual: Vec<u64> = raw
            .iter()
            .map(|&r| r.saturating_sub(bl_mean as u64))
            .collect();
        let expected_ns = us as u64 * 1000;

        let rs = calc(&raw);
        let as_ = calc(&actual);
        show("raw sandwich", &rs);
        show("actual (−baseline)", &as_);

        // jitter = |actual − expected|
        let jitter_abs: Vec<u64> = actual
            .iter()
            .map(|&a| (a as i64 - expected_ns as i64).unsigned_abs())
            .collect();
        let js = calc(&jitter_abs);
        let offset_us = as_.mean / 1e3 - us as f64;
        println!(
            "  offset={offset_us:+.1}µs  jitter: mean|Δ|={:.2}µs  max|Δ|={:.2}µs",
            js.mean / 1e3,
            js.max as f64 / 1e3,
        );

        // 直方图：按实际范围自适应 bucket
        let bucket = ((as_.max - as_.min) / 15).max(100);
        hist(&actual, bucket);
    }

    println!("\nDone.");
}
