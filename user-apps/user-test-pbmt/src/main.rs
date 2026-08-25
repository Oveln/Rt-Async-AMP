//! PBMT 用户态兑现性最小实验（与 RP 固件 `apps/rt-async-k3/src/bin/
//! pbmt_probe.rs` 配对，独立 bin、不依赖 user-test-bench）。
//!
//! 问题：PBMTE 已开（板上 OpenSBI banner 列出 svpbmt + priv 1.12 ⇒
//! menvcfg.PBMTE=1）前提下，用户态 mmap 共享窗（叶子 PTE 带 PBMT=NC，经
//! `DeviceMmap::Physical` → `MappingFlags::UNCACHED` 编码）的读写是否兑现
//! 为非缓存访问，还是仍走缓存。08-16 内核 ioremap 别名时延探针判后者
//! （3.1 vs 1.2 周期/读，同为 L1 命中量级）；本实验在用户态路径、以行为
//! 级观测复测。
//!
//! 四阶段，观测期零 ioctl / 零 cbo——内核 CBO 同步点完全不参与（A'/D 的
//! 单行 cbo.inval 只作用于各自单元，不触碰被观测行）：
//! - 阶段A 写直达性：写 REQ 值（前缀 | 本轮 nonce=pid）到 flag，1s 后
//!   **首读** ack。ack 行从未被本进程读过（mmap 时内核整窗作废后无驻留
//!   副本），首读 L1/L2 均未命中必取 SRAM 真值——判定不依赖待测属性本身。
//!   RP 回显 flag 原值，回显等于本轮 nonce 才算直达——跨轮残留不可能
//!   误判（见阶段A 注释）。
//! - 阶段A' 迟到回执：3s 后作废 ack 行重读。写若被缓存滞留，脏行被逐出
//!   迟到送达 SRAM 时 RP 会补回执——显形即"写被缓存"的直接证据；未见
//!   不构成反证（脏行可能长期不逐出）。
//! - 阶段B RP 活性：首读 cnt，seq=0 说明 pbmt_probe 未运行/未过门控，
//!   实验无效。
//! - 阶段C 读新鲜度：3s 内每 1ms 裸读 cnt.seq。非缓存读每次直达 SRAM，
//!   seq 平滑前进（RP 以 ~100µs 步长推进）；缓存读行驻留 L1/L2、陈旧行
//!   对全 hart 一致（线程迁移救不了），首读后冻结。
//! - 阶段D 时延佐证：同址重读三回路 ns/读（3 轮取最小）——(a) DDR 匿名
//!   页校准循环自身开销（L1 命中量级）；(b) 窗口行裸读；(c) 窗口行每读先
//!   cbo.inval——本系统"非缓存访问 SRAM"的代价下界锚点。
//!
//! 运行：板上 wget 本 bin 后直接运行；每上电至少跑 3 遍（进程重启即重新
//! mmap，内核整窗作废，状态干净）。现亦用作 PMA 固件（opensbi-k3
//! feat/pma-audio-io）的验收/回归检测：非缓存生效与否即固件配置是否在岗。

use std::ffi::c_void;
use std::hint::black_box;
use std::time::{Duration, Instant};

// ── 窗口与 scratch 布局（与 pbmt_probe.rs 同值，双端手工对齐）──────────
const SHM_SIZE: usize = rtshm_abi::K3_SHM_SIZE;
const SCRATCH_OFF: usize = 0x18700;
const FLAG_OFF: usize = 0x600;
const ACK_OFF: usize = 0x680;
const CNT_OFF: usize = 0x6c0;
const LAT_OFF: usize = 0x700;
const REQ_PREFIX: u64 = (u32::from_be_bytes(*b"PBMT") as u64) << 32;
const ACK_MAGIC: u64 = u64::from_be_bytes(*b"PBMT_ACK");
/// cache line（X100 `riscv,cbom-block-size` = 64，同 ov-rpc CACHE_LINE）。
const CACHE_LINE: usize = 64;

/// 单行 cbo.inval + fence——与 ov-rpc `cache::refresh` 同编码
/// （`.insn i 15,2,x0,rs1,0` = cbo.inval；senvcfg.CBIE=01 下按
/// clean+invalidate 执行）。独立 bin 不引协议 crate，就地内联这一条。
#[cfg(target_arch = "riscv64")]
#[inline]
fn line_inval(addr: usize) {
    unsafe {
        core::arch::asm!(
            ".insn i 15, 2, x0, {addr}, 0",
            addr = in(reg) addr & !(CACHE_LINE - 1),
            options(nostack)
        );
        core::arch::asm!("fence rw, rw", options(nostack, preserves_flags));
    }
}

/// 读 scratch 单元（`off` 相对 SCRATCH_OFF）。**必须**经此助手访问单元——
/// 首版曾在调用点漏加 SCRATCH_OFF，误读写 ch0 槽区（窗口 0x600 一带），
/// 全部观测归零报废。
#[inline]
fn rd_cell(shm: *mut c_void, off: usize) -> u64 {
    unsafe { ((shm as usize + SCRATCH_OFF + off) as *const u64).read_volatile() }
}

/// 写 scratch 单元（`off` 相对 SCRATCH_OFF）。
#[inline]
fn wr_cell(shm: *mut c_void, off: usize, v: u64) {
    unsafe { ((shm as usize + SCRATCH_OFF + off) as *mut u64).write_volatile(v) }
}

/// N 次同址 volatile 读，3 轮取最小，返回 ns/读。`inval` = 每读先作废该行
/// （仅 riscv64）。异或累积 + black_box 防优化裁剪。
fn lat_loop(addr: usize, n: u64, inval: bool) -> f64 {
    let mut best = f64::INFINITY;
    for _ in 0..3 {
        let mut acc: u64 = 0;
        let t = Instant::now();
        for _ in 0..n {
            #[cfg(target_arch = "riscv64")]
            if inval {
                line_inval(addr);
            }
            acc ^= unsafe { (addr as *const u64).read_volatile() };
        }
        black_box(acc);
        best = best.min(t.elapsed().as_nanos() as f64 / n as f64);
    }
    best
}

/// 打开 /dev/rt_shm 并 mmap 全窗。内核 mmap 钩子会先整窗 cbo 作废
/// （tgoskits rt_shm `mmap()`），返回后窗口行无驻留副本。fd 故意持有
/// 不关——实验进程生命周期即映射生命周期。
fn open_window() -> *mut c_void {
    use std::fs::OpenOptions;
    use std::os::unix::io::IntoRawFd;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/rt_shm")
        .expect("打开 /dev/rt_shm 失败");
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
        panic!("mmap 失败: {}", std::io::Error::last_os_error());
    }
    ptr
}

fn main() {
    // 尽力钉到 CPU2（避开 IRQ 常驻的 core0），失败不致命：行为判定本身
    // 不依赖绑核（L2 陈旧行对所有 hart 一致），绑核只为时延回路稳定。
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(2, &mut set);
        let _ = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }

    let shm = open_window();
    println!(
        "[pbmt] 窗口 vaddr={:p} size={:#x}（mmap 时内核已整窗作废；配对固件 pbmt_probe）",
        shm, SHM_SIZE
    );

    // ── 阶段A：写直达性 ─────────────────────────────────────────────
    // flag = REQ 前缀 | 本轮 nonce(pid)。nonce 回显使跨轮残留不可能误判：
    // 缓存模式下上一轮滞留的 REQ 写会在本轮 mmap 时被内核整窗作废
    // （CBIE=01 = clean+invalidate）冲刷进 SRAM 迟到触发回执，但回显的
    // 是上一轮 nonce，与本轮比对不上。
    let nonce = unsafe { libc::getpid() } as u64;
    let req_val = REQ_PREFIX | nonce;
    let ta = Instant::now();
    wr_cell(shm, FLAG_OFF, req_val);
    std::thread::sleep(Duration::from_secs(1));
    let ack_magic = rd_cell(shm, ACK_OFF);
    let ack_echo = rd_cell(shm, ACK_OFF + 8);
    let write_direct = ack_magic == ACK_MAGIC && ack_echo == req_val;
    println!(
        "[pbmt] 阶段A 写路径: nonce={:#x} ack={:#x} echo={:#x} ({}ms) → {}",
        nonce,
        ack_magic,
        ack_echo,
        ta.elapsed().as_millis(),
        if write_direct {
            "回执回显本轮 nonce——用户写直达 SRAM（NC 兑现）"
        } else if ack_magic == ACK_MAGIC {
            "回执为其它轮残留（nonce 不匹配）——本轮写未直达"
        } else {
            "1s 内无回执（未直达）"
        }
    );

    // ── 阶段A'：迟到回执诊断 ────────────────────────────────────────
    std::thread::sleep(Duration::from_secs(3));
    #[cfg(target_arch = "riscv64")]
    line_inval(shm as usize + SCRATCH_OFF + ACK_OFF);
    let ack2_magic = rd_cell(shm, ACK_OFF);
    let ack2_echo = rd_cell(shm, ACK_OFF + 8);
    println!(
        "[pbmt] 阶段A' 迟到回执: ack={:#x} echo={:#x} → {}",
        ack2_magic,
        ack2_echo,
        if write_direct {
            "（阶段A 已直达，不适用）"
        } else if ack2_magic == ACK_MAGIC && ack2_echo == req_val {
            "本轮迟到回执显形——写确曾被缓存滞留（仅逐出送达）"
        } else if ack2_magic == ACK_MAGIC {
            "迟到回执为其它轮残留——同为本轮未直达的佐证"
        } else {
            "未见（不构成反证：脏行可能未逐出）"
        }
    );

    // ── 阶段B：RP 活性锚点 ──────────────────────────────────────────
    let seq0 = rd_cell(shm, CNT_OFF);
    let ts0 = rd_cell(shm, CNT_OFF + 8);
    println!("[pbmt] 阶段B RP活性: seq0={} ts0={}", seq0, ts0);
    if seq0 == 0 {
        println!("[pbmt] ⚠ 计数器为 0——pbmt_probe 固件未运行或未过门控，实验无效");
        std::process::exit(1);
    }

    // ── 阶段C：读新鲜度 ─────────────────────────────────────────────
    let tc = Instant::now();
    let mut last = seq0;
    let mut changes: u64 = 0;
    let mut max_plateau_ms: u128 = 0;
    let mut plateau_since = tc;
    while tc.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(1));
        let s = rd_cell(shm, CNT_OFF);
        if s != last {
            changes += 1;
            max_plateau_ms = max_plateau_ms.max(plateau_since.elapsed().as_millis());
            plateau_since = Instant::now();
            last = s;
        }
    }
    max_plateau_ms = max_plateau_ms.max(plateau_since.elapsed().as_millis());
    let read_nc = changes >= 300 && max_plateau_ms <= 100;
    let read_frozen = max_plateau_ms >= 1000;
    println!(
        "[pbmt] 阶段C 读路径: 末值={} 变化={} 最长平台期={}ms → {}",
        last,
        changes,
        max_plateau_ms,
        if read_nc {
            "seq 平滑前进（非缓存读）"
        } else if read_frozen {
            "首读后冻结（缓存读）"
        } else {
            "混合轨迹，按原始数据人工判读"
        }
    );

    // ── 阶段D：时延佐证 ─────────────────────────────────────────────
    let ddr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert!(ddr != libc::MAP_FAILED, "DDR 匿名页映射失败");
    // 先写一次落页，防缺页异常进回路。
    unsafe { (ddr as *mut u64).write_volatile(1) };
    let lat_addr = shm as usize + SCRATCH_OFF + LAT_OFF;
    let ns_a = lat_loop(ddr as usize, 1_000_000, false);
    let ns_b = lat_loop(lat_addr, 1_000_000, false);
    let ns_c = lat_loop(lat_addr, 100_000, true);
    println!(
        "[pbmt] 阶段D 时延(ns/读,3轮最小): DDR对照 a={:.2} 窗口裸读 b={:.2} 窗口逐读inval c={:.2}",
        ns_a, ns_b, ns_c
    );

    // ── 汇总判定 ────────────────────────────────────────────────────
    let ratio_ba = ns_b / ns_a;
    println!(
        "[pbmt] 判定: 写路径={} | 读路径={} | 时延 b/a={:.1}×（c/a={:.1}×）",
        if write_direct { "直达" } else { "未直达" },
        if read_nc {
            "非缓存"
        } else if read_frozen {
            "缓存冻结"
        } else {
            "混合"
        },
        ratio_ba,
        ns_c / ns_a
    );
    if write_direct && read_nc {
        println!("[pbmt] 结论: 用户态映射非缓存生效（机制 = PMA 固件翻转 entry 或 PBMT 兑现；对照启动日志 K3 PMA 行归因）");
    } else if !write_direct && read_frozen {
        println!("[pbmt] 结论: 用户态映射读写走缓存——PBMT 与 PMA 均未生效（固件未带 PMA 补丁，或硅忽略；对照启动日志有无 K3 PMA 行）");
    } else {
        println!("[pbmt] 结论: 部分兑现/混合，按上方原始数据人工判读（阶段A/A'/C/D 各自独立成立）");
    }
}
