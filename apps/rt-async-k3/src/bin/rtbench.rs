//! RT24 微架构基准（rtbench）：上电本地自跑，结果打 R_UART0 串口。
//!
//! 背景（2026-08-17 延迟战役 + 代码生成对照实验）：
//! - K3 上 Acquire 读 = `ld + fence r,rw`，2.2µs 的成本在 **fence 等待未完成
//!   访存事务排空**（无缓存 SRAM 完整总线往返），不是原子指令本身；
//! - RMW（fetch_or/swap/...）才编译成真 AMO 指令（~2.2µs/条）；
//! - 拟议优化②a（atomic-cas:false + portable-atomic critical-section 后端）
//!   把原子操作变成 `csrrci mstatus` + 普通 ld/sd，零 fence 零 AMO。
//!
//! 本程序在**不依赖 AP**的前提下实测上述单价与更多微架构指标，为优化
//! 路径定量：
//! 1. 时钟标定（mcycle/mtime 比值 → 核心真实频率，491.52 vs 614.4 之争）
//! 2. 时间戳与驱动基建（mtime / mcycle / Slot 路径 / ①去 Acquire 后的驱动访问器）
//! 3. fence 矩阵：变体 × 前置操作 × 目标（SHM / 本地 .bss）× 距离
//! 4. AMO 矩阵：变体（relaxed/aq/rl/aqrl、w/d、lr、lr+sc）× 目标
//! 5. critical-section 后端仿真（csrrci + 普通 ld/sd，含真实 with() 路径）
//! 6. SRAM 访问模式（同行 / 顺序行 / 冷步进 / 256B 块 / 写 / 读写依赖 / 别名窗）
//! 7. 取指与 I$ 存在性（热调用 vs 散布 16 函数冷调用，pass1 vs pass2）
//! 8. trap 与 WFI 延迟（MSIP 自环、mtimecmp 唤醒）
//!
//! 判读基线（历史板上数据）：mtime 读 107ns、同行读 22ns、冷读 ~875ns、
//! 256B 块读 ~1166ns、ld+fence ~2198ns、sd+fence ~2223ns、AMO ~2.2µs。

#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

extern crate rt_async_k3;

use core::hint::black_box;
use core::pin::Pin;

use chip_k3_rt24::K3Rt24;
use executor::priority::Priority;
use executor::spawner::Spawner;
use platform::arch::TrapFrame;
use platform::Timer as _;

// 强制链接 chip crate（同 ipc_demo）。
#[used]
static _FORCE_LINK_CHIP_K3_RT24: K3Rt24 = K3Rt24;

include!(concat!(env!("OUT_DIR"), "/build_time.rs"));

// ── 布局常量 ─────────────────────────────────────────────────────────────

/// 共享窗尾部空闲区偏移（0x100 头 + 3×0x8200 通道 = 0x18700，与 MEMBENCH 同区）。
const SHM_SCRATCH_OFF: usize = 0x18700;

/// mtime MMIO 地址（0xe400bff8，24MHz）。
const MTIME_ADDR: usize = 0xe400_bff8;

// ── 本地 scratch ─────────────────────────────────────────────────────────

#[repr(C, align(256))]
struct Blk256([u8; 256]);

// SAFETY: 仅本基准任务上下文（含显式关中断段）访问，无并发。
static mut LOCAL_BLK: Blk256 = Blk256([0; 256]);

// ── 计时基建 ─────────────────────────────────────────────────────────────

/// mtime tick（24MHz，41.7ns 分辨率）。
#[inline]
fn mtime() -> u64 {
    chip_k3_rt24::clint_k3::TIMER.now()
}

/// mcycle CSR 读（若 SoC 未实现则恒 0，程序会探测并停用周期列）。
#[inline]
fn mcycle() -> u64 {
    let c: u64;
    // SAFETY: 纯 CSR 读，无副作用。
    unsafe { core::arch::asm!("csrr {0}, mcycle", out(reg) c, options(nostack)) };
    c
}

/// 定时器频率（Hz，编译期常量路径）。
fn freq_hz() -> u64 {
    chip_k3_rt24::clint_k3::TIMER.freq_hz() as u64
}

// ── 结果表 ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct Row {
    name: &'static str,
    ns: u64,
    cyc: u64,
}

const MAX_ROWS: usize = 48;
// SAFETY: 仅本任务顺序写、打印阶段读。
static mut ROWS: [Option<Row>; MAX_ROWS] = [None; MAX_ROWS];
static mut ROW_IDX: usize = 0;
/// mcycle 是否可用（时钟标定节判定）。
static mut MCYCLE_OK: bool = false;

fn record(name: &'static str, ns: u64, cyc: u64) {
    // SAFETY: 单任务上下文顺序访问。
    unsafe {
        if ROW_IDX < MAX_ROWS {
            ROWS[ROW_IDX] = Some(Row { name, ns, cyc });
            ROW_IDX += 1;
        }
    }
}

/// 计时一段基准（fn 指针防内联，循环开销 ~ns 级）。关中断执行。
fn timeit(name: &'static str, addr: usize, iters: usize, f: fn(usize, usize)) {
    // SAFETY: 测量段内无跨上下文依赖；时长 ms 级，关中断安全。
    unsafe { platform::disable_interrupts() };
    let t0 = mtime();
    let c0 = mcycle();
    f(iters, addr);
    let c1 = mcycle();
    let t1 = mtime();
    // SAFETY: 恢复测量前中断状态。
    unsafe { platform::enable_interrupts() };
    let ns = (t1.saturating_sub(t0) as u128 * 1_000_000_000
        / (freq_hz() as u128 * iters as u128)) as u64;
    let cyc = c1.saturating_sub(c0) / iters as u64;
    record(name, ns, cyc);
}

/// 同 [`timeit`] 但不关中断——critical-section 仿真需要 MIE=1 的真实路径
/// （csrrci 保存的 MIE 位决定恢复分支）。仅在 AP 空闲时运行。
fn timeit_live(name: &'static str, addr: usize, iters: usize, f: fn(usize, usize)) {
    let t0 = mtime();
    let c0 = mcycle();
    f(iters, addr);
    let c1 = mcycle();
    let t1 = mtime();
    let ns = (t1.saturating_sub(t0) as u128 * 1_000_000_000
        / (freq_hz() as u128 * iters as u128)) as u64;
    let cyc = c1.saturating_sub(c0) / iters as u64;
    record(name, ns, cyc);
}

// ── 第 2 节：时间戳与驱动基建 ────────────────────────────────────────────

fn b_mtime_read(n: usize, a: usize) {
    let p = a as *const u64;
    let mut v = 0u64;
    for _ in 0..n {
        // SAFETY: p 指向 mtime MMIO（只读计数器）。
        v = v.wrapping_add(unsafe { p.read_volatile() });
    }
    black_box(v);
}

fn b_mcycle_read(n: usize, _a: usize) {
    let mut v = 0u64;
    for _ in 0..n {
        v = v.wrapping_add(mcycle());
    }
    black_box(v);
}

fn b_timer_slot(n: usize, _a: usize) {
    let mut v = 0u64;
    for _ in 0..n {
        v = v.wrapping_add(platform::timer().now());
    }
    black_box(v);
}

fn b_shm_base(n: usize, _a: usize) {
    let mut v = 0usize;
    for _ in 0..n {
        v = v.wrapping_add(ov_shm::shm::base());
    }
    black_box(v);
}

fn b_mbox_msgstat(n: usize, _a: usize) {
    let mut v = 0u32;
    for _ in 0..n {
        v = v.wrapping_add(chip_k3_rt24::mailbox::MBX3.msg_count(0));
    }
    black_box(v);
}

fn b_mbox_irqen(n: usize, _a: usize) {
    let mut v = false;
    for _ in 0..n {
        v ^= chip_k3_rt24::mailbox::MBX3.irq_enabled(0);
    }
    black_box(v);
}

fn b_mbox_enwr(n: usize, _a: usize) {
    // mailbox irq_en_set 幂等写（已置位位再写同值）——门铃类 MMIO 写单价。
    use platform::device::Mailbox as _;
    for _ in 0..n {
        chip_k3_rt24::mailbox::MBX3.enable_new_msg_irq(0);
    }
}

fn b_alu(n: usize, _a: usize) {
    let mut x = 1u64;
    for _ in 0..n {
        x = x.wrapping_mul(3).wrapping_add(1);
    }
    black_box(x);
}

// ── 第 3 节：fence 矩阵 ──────────────────────────────────────────────────
//
// 假设待证：fence 成本 = 等待前置未完成访存事务排空。判别式：
//   ld + fence(紧邻)   ≈ 2.2µs（fence 等 ld 完成）
//   ld + 32 拍延迟 + fence ≈ ? （ld 已完成则 fence 应回落）
//   纯 fence（无事可等）≈ 2ns

fn f_fence_rrw(n: usize, _a: usize) {
    for _ in 0..n {
        // SAFETY: 纯屏障指令。
        unsafe { core::arch::asm!("fence r, rw", options(nostack)) };
    }
}

fn f_fence_rww(n: usize, _a: usize) {
    for _ in 0..n {
        // SAFETY: 同上。
        unsafe { core::arch::asm!("fence rw, w", options(nostack)) };
    }
}

fn f_fence_rwrw(n: usize, _a: usize) {
    for _ in 0..n {
        // SAFETY: 同上（MEMBENCH fence_only 误测 2ns 的复核——那次疑似
        // 被编译器消除；本变体单独成环，四变体独立计价）。
        unsafe { core::arch::asm!("fence rw, rw", options(nostack)) };
    }
}

fn f_fence_iorw(n: usize, _a: usize) {
    for _ in 0..n {
        // SAFETY: 同上（notify 发布点同款）。
        unsafe { core::arch::asm!("fence iorw, iorw", options(nostack)) };
    }
}

fn f_ld(n: usize, a: usize) {
    for _ in 0..n {
        // SAFETY: a 为有效 8B 读地址（scratch/MMIO）。
        unsafe {
            core::arch::asm!(
                "ld {v}, 0({p})",
                v = out(reg) _,
                p = in(reg) a,
                options(nostack)
            );
        }
    }
}

fn f_ld_fence_rrw(n: usize, a: usize) {
    for _ in 0..n {
        // SAFETY: 同上。
        unsafe {
            core::arch::asm!(
                "ld {v}, 0({p})",
                "fence r, rw",
                v = out(reg) _,
                p = in(reg) a,
                options(nostack)
            );
        }
    }
}

fn f_ld_fence_rw(n: usize, a: usize) {
    for _ in 0..n {
        // SAFETY: 同上。
        unsafe {
            core::arch::asm!(
                "ld {v}, 0({p})",
                "fence rw, rw",
                v = out(reg) _,
                p = in(reg) a,
                options(nostack)
            );
        }
    }
}

fn f_ld_delay_fence(n: usize, a: usize) {
    for _ in 0..n {
        // SAFETY: 同上；32 条 addi 模拟 ld 与 fence 间的时间距离。
        unsafe {
            core::arch::asm!(
                "ld {v}, 0({p})",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "addi {d}, {d}, 1",
                "fence r, rw",
                v = out(reg) _,
                p = in(reg) a,
                d = out(reg) _,
                options(nostack)
            );
        }
    }
}

fn f_sd_fence_rw(n: usize, a: usize) {
    for _ in 0..n {
        // SAFETY: a 为有效 8B 写地址（scratch）。
        unsafe {
            core::arch::asm!(
                "sd {v}, 0({p})",
                "fence rw, rw",
                v = in(reg) 0u64,
                p = in(reg) a,
                options(nostack)
            );
        }
    }
}

/// release store 代码生成（fence rw,w; sd）。
fn f_fence_w_sd(n: usize, a: usize) {
    for _ in 0..n {
        // SAFETY: 同上。
        unsafe {
            core::arch::asm!(
                "fence rw, w",
                "sd {v}, 0({p})",
                v = in(reg) 0u64,
                p = in(reg) a,
                options(nostack)
            );
        }
    }
}

/// notify() 发布对（sd; fence iorw,iorw）——门铃前同步点的真实形态。
fn f_sd_fence_iorw(n: usize, a: usize) {
    for _ in 0..n {
        // SAFETY: 同上。
        unsafe {
            core::arch::asm!(
                "sd {v}, 0({p})",
                "fence iorw, iorw",
                v = in(reg) 0u64,
                p = in(reg) a,
                options(nostack)
            );
        }
    }
}

// ── 第 4 节：AMO 矩阵 ────────────────────────────────────────────────────

fn f_amoswap_rel(n: usize, a: usize) {
    for _ in 0..n {
        // SAFETY: a 为有效 8B 对齐地址；amoswap 无序（aq=rl=0）。
        unsafe {
            core::arch::asm!(
                "amoswap.d {t}, {v}, 0({p})",
                t = out(reg) _,
                v = in(reg) 1u64,
                p = in(reg) a,
                options(nostack)
            );
        }
    }
}

fn f_amoswap_aq(n: usize, a: usize) {
    for _ in 0..n {
        // SAFETY: 同上。
        unsafe {
            core::arch::asm!(
                "amoswap.d.aq {t}, {v}, 0({p})",
                t = out(reg) _,
                v = in(reg) 1u64,
                p = in(reg) a,
                options(nostack)
            );
        }
    }
}

fn f_amoswap_rl(n: usize, a: usize) {
    for _ in 0..n {
        // SAFETY: 同上。
        unsafe {
            core::arch::asm!(
                "amoswap.d.rl {t}, {v}, 0({p})",
                t = out(reg) _,
                v = in(reg) 1u64,
                p = in(reg) a,
                options(nostack)
            );
        }
    }
}

fn f_amoswap_aqrl(n: usize, a: usize) {
    for _ in 0..n {
        // SAFETY: 同上。
        unsafe {
            core::arch::asm!(
                "amoswap.d.aqrl {t}, {v}, 0({p})",
                t = out(reg) _,
                v = in(reg) 1u64,
                p = in(reg) a,
                options(nostack)
            );
        }
    }
}

fn f_amoadd_aqrl(n: usize, a: usize) {
    for _ in 0..n {
        // SAFETY: 同上（fetch_add 现行代码生成形态）。
        unsafe {
            core::arch::asm!(
                "amoadd.d.aqrl {t}, {v}, 0({p})",
                t = out(reg) _,
                v = in(reg) 1u64,
                p = in(reg) a,
                options(nostack)
            );
        }
    }
}

fn f_amoor_w_aqrl(n: usize, a: usize) {
    let mut acc = 0u32;
    for _ in 0..n {
        // SAFETY: 同上（executor State::fetch_or 现行形态，32 位）。
        unsafe {
            core::arch::asm!(
                "amoor.w.aqrl {t}, {v}, 0({p})",
                t = out(reg) acc,
                v = in(reg) 2u32,
                p = in(reg) a,
                options(nostack)
            );
        }
    }
    black_box(acc);
}

fn f_lr_aq(n: usize, a: usize) {
    for _ in 0..n {
        // SAFETY: lr 不写目标，仅建立保留集。
        unsafe {
            core::arch::asm!(
                "lr.d.aq {t}, 0({p})",
                t = out(reg) _,
                p = in(reg) a,
                options(nostack)
            );
        }
    }
}

fn f_cas_loop(n: usize, a: usize) {
    let mut done = 0usize;
    for _ in 0..n {
        // SAFETY: 标准 lr/sc CAS 重试环（compare_exchange 现行形态）。
        unsafe {
            core::arch::asm!(
                "1: lr.d {t}, 0({p})",
                "sc.d {r}, {v}, 0({p})",
                "bnez {r}, 1b",
                t = out(reg) _,
                r = out(reg) _,
                v = in(reg) 1u64,
                p = in(reg) a,
                options(nostack)
            );
        }
        done += 1;
    }
    black_box(done);
}

// ── 第 5 节：critical-section 后端仿真 ───────────────────────────────────
//
// 拟议优化②a 的目标代码形态（portable-atomic critical-section 后端）：
// csrrci（保存并关 MIE）→ 普通 ld/sd → 条件恢复。需 MIE=1 路径（timeit_live）。

fn f_cs_load(n: usize, a: usize) {
    for _ in 0..n {
        // SAFETY: 复刻 riscv crate single-hart CS：保存 MIE 并清除、
        // 普通 ld、仅当原 MIE=1 时恢复。地址为有效 8B 读目标。
        unsafe {
            core::arch::asm!(
                "csrrci {s}, mstatus, 8",
                "ld {v}, 0({p})",
                "andi {b}, {s}, 8",
                "beqz {b}, 2f",
                "csrsi mstatus, 8",
                "2:",
                s = out(reg) _,
                b = out(reg) _,
                v = out(reg) _,
                p = in(reg) a,
                options(nostack)
            );
        }
    }
}

fn f_cs_rmw(n: usize, a: usize) {
    for _ in 0..n {
        // SAFETY: 同上，RMW 形态（读-改-写全在 CS 内）。
        unsafe {
            core::arch::asm!(
                "csrrci {s}, mstatus, 8",
                "ld {v}, 0({p})",
                "addi {v}, {v}, 1",
                "sd {v}, 0({p})",
                "andi {b}, {s}, 8",
                "beqz {b}, 2f",
                "csrsi mstatus, 8",
                "2:",
                s = out(reg) _,
                b = out(reg) _,
                v = out(reg) _,
                p = in(reg) a,
                options(nostack)
            );
        }
    }
}

fn f_cs_real(n: usize, a: usize) {
    let p = a as *const u64;
    let mut v = 0u64;
    for _ in 0..n {
        critical_section::with(|_| {
            // SAFETY: p 为有效 8B 读地址；with() 内为单核互斥上下文。
            v = v.wrapping_add(unsafe { p.read_volatile() });
        });
    }
    black_box(v);
}

// ── 第 6 节：SRAM 访问模式 ───────────────────────────────────────────────

fn b_rd_same(n: usize, a: usize) {
    let p = a as *const u64;
    let mut v = 0u64;
    for _ in 0..n {
        // SAFETY: p 为有效 8B 读地址。
        v = v.wrapping_add(unsafe { p.read_volatile() });
    }
    black_box(v);
}

/// 顺序行扫（0x800 scratch，64B 步进，n=轮数）。
fn b_rd_seq_lines(n: usize, a: usize) {
    let base = a as *const u64;
    let mut v = 0u64;
    for _ in 0..n {
        let mut off = 0usize;
        while off < 0x800 {
            // SAFETY: off+8 界内。
            v = v.wrapping_add(unsafe { base.add(off).read_volatile() });
            off += 64;
        }
    }
    black_box(v);
}

/// 冷步进扫（512B 步进，n=轮数）。
fn b_rd_stride512(n: usize, a: usize) {
    let base = a as *const u64;
    let mut v = 0u64;
    for _ in 0..n {
        let mut off = 0usize;
        while off < 0x800 {
            // SAFETY: off+8 界内。
            v = v.wrapping_add(unsafe { base.add(off).read_volatile() });
            off += 512;
        }
    }
    black_box(v);
}

fn b_rd_blk256(n: usize, a: usize) {
    let p = a as *const Blk256;
    let mut v = 0u64;
    for _ in 0..n {
        // SAFETY: p 256 对齐、指向有效 256B。
        let b = unsafe { p.read_volatile() };
        let w: [u8; 8] = b.0[0..8].try_into().unwrap();
        v = v.wrapping_add(u64::from_le_bytes(w));
    }
    black_box(v);
}

/// 大区冷读扫（RCPU 本地窗口 0x19000..0x39000 空闲 SRAM，128KB = 2048 行，
/// 64B 步进）。
/// **v2 事故记录（2026-08-17）**：本测试曾对 RMAP 跨域路径
/// （0xC0800100..0xC0818700 全协议区）顺序背靠背扫 966 行 × 30 轮——
/// 板上把跨域互连直接挂死（X100 域冻结、U-Boot 无响应、软复位不恢复，
/// 需断电）。小脚印（0x800 内）跨域访问全部历史安全，全区间背靠背
/// 顺序读是该桥/SRAM 控制器的未记载死锁触发模式。v3 改扫本地端口
/// （不过 M2F 桥，物理上同一 SRAM，冷价等效且不碰 AP 域）。
fn b_rd_cold_sweep(n: usize, a: usize) {
    let base = a as *const u64;
    let mut v = 0u64;
    for _ in 0..n {
        let mut off = 0usize;
        while off < 0x20000 {
            // SAFETY: 本地窗口 0x19000..0x39000 为空闲 SRAM（协议窗仅
            // 0x0..0x19000），只读无害。
            v = v.wrapping_add(unsafe { base.add(off).read_volatile() });
            off += 64;
        }
    }
    black_box(v);
}

fn b_wr_same(n: usize, a: usize) {
    let p = a as *mut u64;
    for i in 0..n {
        // SAFETY: p 为有效 8B 写地址。
        unsafe { p.write_volatile(i as u64) };
    }
}

fn b_wr_then_rd_same(n: usize, a: usize) {
    let p = a as *mut u64;
    let mut v = 0u64;
    for i in 0..n {
        // SAFETY: 同一地址写后读（store→load 转发探测）。
        unsafe {
            p.write_volatile(i as u64);
            v = v.wrapping_add(p.read_volatile());
        }
    }
    black_box(v);
}

fn b_wr_then_rd_diff(n: usize, a: usize) {
    let p = a as *mut u64;
    let q = (a + 0x400) as *const u64;
    let mut v = 0u64;
    for i in 0..n {
        // SAFETY: 写 p 后读 1KB 外的 q（无转发，须等写完成？）。
        unsafe {
            p.write_volatile(i as u64);
            v = v.wrapping_add(q.read_volatile());
        }
    }
    black_box(v);
}

// ── 第 7 节：取指与 I$ 存在性 ────────────────────────────────────────────
//
// 16 个互不相同的 #[inline(never)] 小函数（返回不同常量防 ICF 合并），
// 指针表循环调用。热（恒调 1 个）vs 散布（轮询 16 个）每调用差额 =
// 冷取指单价；pass2 立即重跑，若显著回落 ⇒ 存在 I$（被自旋逐出），
// 持平 ⇒ 无 I$（dseen/svc 的 ~85µs 残余主嫌疑成立）。

// 各函数读一次 LOCAL_BLK 首字（volatile，编译器不可折叠其值）再混入
// 各异常量——纯度被破坏防循环折叠，常量互异防 ICF 合并。

#[inline(never)]
fn ic00() -> u64 { ic_feed() + 0x0100 }
#[inline(never)]
fn ic01() -> u64 { ic_feed() + 0x0101 }
#[inline(never)]
fn ic02() -> u64 { ic_feed() + 0x0102 }
#[inline(never)]
fn ic03() -> u64 { ic_feed() + 0x0103 }
#[inline(never)]
fn ic04() -> u64 { ic_feed() + 0x0104 }
#[inline(never)]
fn ic05() -> u64 { ic_feed() + 0x0105 }
#[inline(never)]
fn ic06() -> u64 { ic_feed() + 0x0106 }
#[inline(never)]
fn ic07() -> u64 { ic_feed() + 0x0107 }
#[inline(never)]
fn ic08() -> u64 { ic_feed() + 0x0108 }
#[inline(never)]
fn ic09() -> u64 { ic_feed() + 0x0109 }
#[inline(never)]
fn ic10() -> u64 { ic_feed() + 0x010a }
#[inline(never)]
fn ic11() -> u64 { ic_feed() + 0x010b }
#[inline(never)]
fn ic12() -> u64 { ic_feed() + 0x010c }
#[inline(never)]
fn ic13() -> u64 { ic_feed() + 0x010d }
#[inline(never)]
fn ic14() -> u64 { ic_feed() + 0x010e }
#[inline(never)]
fn ic15() -> u64 { ic_feed() + 0x010f }

#[inline(never)]
fn ic_feed() -> u64 {
    // SAFETY: LOCAL_BLK 仅本任务访问；volatile 读破坏调用方纯度。
    unsafe { (&raw const LOCAL_BLK).cast::<u64>().read_volatile() }
}

#[allow(clippy::type_complexity)]
static CALLS: [fn() -> u64; 16] = [
    ic00, ic01, ic02, ic03, ic04, ic05, ic06, ic07, ic08, ic09, ic10, ic11, ic12, ic13, ic14,
    ic15,
];

fn b_call_hot(n: usize, _a: usize) {
    let mut v = 0u64;
    for _ in 0..n {
        v = v.wrapping_add(CALLS[0]());
    }
    black_box(v);
}

fn b_call_spread(n: usize, _a: usize) {
    let mut v = 0u64;
    for _ in 0..n / 16 {
        for i in 0..16 {
            v = v.wrapping_add(CALLS[i]());
        }
    }
    black_box(v);
}

// ── 第 8 节：trap 与 WFI 延迟 ────────────────────────────────────────────

// SAFETY: 仅 ISR 与测试段（天然互斥）访问。
static mut MSIP_ENTRY_TICK: u64 = 0;
static mut WFI_ENTRY_TICK: u64 = 0;

/// pend() 无陷入成本（关中断：MSIP 置位但不触发）。
fn b_pend_off(n: usize, _a: usize) {
    for _ in 0..n {
        // SAFETY: 关中断段内 pend 不触发陷入；随后 clear_pend 清 MSIP
        // 与 PEND_MARKER，恢复中断状态。
        unsafe {
            platform::disable_interrupts();
            platform::pend();
            platform::clear_pend();
            platform::enable_interrupts();
        }
    }
}

/// MSIP 自环（开中断）：t0 → pend() → [trap → ISR 戳 → clear_pend →
/// try_preempt → mret] → stamp 变化。v1 版测得 entry=0 的教训：MSIP 写是
/// 发布事务，陷落有迟滞——直接取 t2 会早于 trap。v2 自旋等 stamp 变化
/// （上限 200µs），miss 单列；前置 drain 残留 pending + mip/mstatus 自检。
fn bench_msip_roundtrip(rounds: usize) {
    let mstatus: u64;
    let mip: u64;
    // SAFETY: 纯 CSR 读。
    unsafe {
        core::arch::asm!("csrr {0}, mstatus", out(reg) mstatus, options(nostack));
        core::arch::asm!("csrr {0}, mip", out(reg) mip, options(nostack));
    }
    log::info!(
        "[rtbench] msip precheck: MIE={} msip_pending={}",
        mstatus & 8 != 0,
        mip & 8 != 0
    );
    // 排空残留：关中断下清 MSIP + PEND_MARKER（wfi 对 pending 但被屏蔽
    // 的中断也会立即醒——不清残留会污染 wfi_wake）。
    // SAFETY: 平衡的关/开中断段。
    unsafe {
        platform::disable_interrupts();
        platform::clear_pend();
        platform::enable_interrupts();
    }
    let mut entry_min = u64::MAX;
    let mut entry_sum = 0u64;
    let mut resume_min = u64::MAX;
    let mut resume_sum = 0u64;
    let mut miss = 0u64;
    let lim_ticks = freq_hz() / 5000; // 200µs
    for _ in 0..rounds {
        // SAFETY: ISR 单点写、对齐 u64 单拷贝读，撕裂不可能。
        let prev = unsafe { MSIP_ENTRY_TICK };
        let t0 = mtime();
        // SAFETY: 任务上下文开中断 pend，立即陷入 MachineSoft。
        unsafe { platform::pend() };
        let mut ti;
        loop {
            // SAFETY: 同上。
            ti = unsafe { MSIP_ENTRY_TICK };
            if ti != prev || mtime().saturating_sub(t0) > lim_ticks {
                break;
            }
        }
        let t2 = mtime();
        if ti == prev {
            miss += 1;
            continue;
        }
        let e = ti.saturating_sub(t0);
        let r = t2.saturating_sub(ti);
        entry_min = entry_min.min(e);
        entry_sum += e;
        resume_min = resume_min.min(r);
        resume_sum += r;
    }
    let ok = rounds as u64 - miss;
    let f = freq_hz();
    let ns = |t: u64| t * 1_000_000_000 / f;
    if ok == 0 {
        log::info!("[rtbench] msip_rtt: 全部 miss（trap 未触发或 stamp 未变）");
        return;
    }
    log::info!(
        "[rtbench] msip_rtt x{rounds} (miss={miss}): entry(pend+trap) avg {} ns / min {} ns, \
         resume(clear+preempt+mret) avg {} ns / min {} ns",
        ns(entry_sum / ok),
        ns(entry_min),
        ns(resume_sum / ok),
        ns(resume_min),
    );
}

/// WFI 唤醒（开中断）：set mtimecmp=now+100µs → wfi → [trap → 戳 →
/// mtimecmp=MAX] → 恢复。v2：前置 drain MSIP + stamp 变化校验（miss 计
/// 假醒——WFI 对 pending-but-masked 中断也立即返回）。
fn bench_wfi_wake(rounds: usize) {
    // SAFETY: 平衡的关/开中断段，清残留 MSIP。
    unsafe {
        platform::disable_interrupts();
        platform::clear_pend();
        platform::enable_interrupts();
    }
    let delta = freq_hz() / 10_000; // 100µs
    let mut wake_min = u64::MAX;
    let mut wake_sum = 0u64;
    let mut tot_sum = 0u64;
    let mut miss = 0u64;
    for _ in 0..rounds {
        // SAFETY: ISR 单点写、对齐读安全。
        let prev = unsafe { WFI_ENTRY_TICK };
        let t0 = mtime();
        chip_k3_rt24::clint_k3::TIMER.set_deadline(t0 + delta);
        // SAFETY: wfi 等中断，MachineTimer 唤醒。
        unsafe { core::arch::asm!("wfi", options(nostack)) };
        let t2 = mtime();
        // SAFETY: 同上。
        let ti = unsafe { WFI_ENTRY_TICK };
        if ti == prev {
            miss += 1;
            continue;
        }
        let w = ti.saturating_sub(t0 + delta);
        wake_min = wake_min.min(w);
        wake_sum += w;
        tot_sum += t2.saturating_sub(t0);
    }
    let ok = rounds as u64 - miss;
    let f = freq_hz();
    let ns = |t: u64| t * 1_000_000_000 / f;
    if ok == 0 {
        log::info!("[rtbench] wfi_wake: 全部 miss（假醒——pending 残留或 stamp 未变）");
        return;
    }
    log::info!(
        "[rtbench] wfi_wake x{rounds} (miss={miss}): wake_err avg {} ns / min {} ns, total avg {} ns",
        ns(wake_sum / ok),
        ns(wake_min),
        ns(tot_sum / ok),
    );
}

// ── 第 9 节：硬件 spinlock（手册 16.7，0xCAC9_1C00）─────────────────────
//
// 手册明言锁获取 <200 周期（~815ns @245.76MHz）——比 fence（~540 周期）
// 快 2.7×，且是全文档唯一背书"保证多核数据一致性"的原语；Dekker fence
// 的替代候选。寄存器：LOCK_N=+0x4N（读 0=获锁置 1，写 0=释放）、
// VER=+0x100（期望 0x312E3030）、SSTATUS=+0x104（=32 单元）。

/// spinlock 基址（AP 域，RCPU 经 IOPMP 跨域访问，与 mailbox4 同路径）。
const SPINLOCK_BASE: usize = 0xCAC9_1C00;

fn bench_spinlock() {
    // SAFETY: 只读常量寄存器；窗口不可达则 LoadFault（诊断可接受，排在
    // 汇总表之后保证其余数据已打出）。
    let ver = unsafe { (SPINLOCK_BASE as *const u32).add(0x100 / 4).read_volatile() };
    let sst = unsafe { (SPINLOCK_BASE as *const u32).add(0x104 / 4).read_volatile() };
    let st = unsafe { (SPINLOCK_BASE as *const u32).add(0x108 / 4).read_volatile() };
    log::info!(
        "[rtbench] spinlock VER={ver:#010x} SSTATUS={sst} STATUS={st:#010x}"
    );
    // v3 事故记录：曾在此对 LOCK31 做 2000 次 lock+unlock（读=获锁/写=释放，
    // 带状态翻转的跨域 APB 读）——板上互连再次楔死（输出止于上行 STATUS，
    // 断电恢复）。跨域可背靠背的只有无状态读；带副作用的读一律禁止。
    log::info!("[rtbench] spinlock lock/unlock 单价测试已移除（v3 事故）");
}

// ── 时钟标定 ─────────────────────────────────────────────────────────────

fn calibrate_clock() -> u64 {
    let target_ticks = freq_hz() / 10; // 100ms
    let t0 = mtime();
    let c0 = mcycle();
    while mtime().saturating_sub(t0) < target_ticks {
        black_box(0);
    }
    let c1 = mcycle();
    let t1 = mtime();
    let core_hz = (c1.saturating_sub(c0)) as u128 * freq_hz() as u128
        / (t1.saturating_sub(t0)) as u128;
    log::info!(
        "[rtbench] core clock = {} Hz ({:.2} MHz); mcycle {}",
        core_hz,
        core_hz as f64 / 1e6,
        if c1 > c0 { "ok" } else { "DEAD (cyc column invalid)" },
    );
    // SAFETY: 单任务上下文。
    unsafe { MCYCLE_OK = c1 > c0 };
    core_hz as u64
}

// ── 主流程 ───────────────────────────────────────────────────────────────

#[executor::task]
async fn task_rtbench() {
    log::info!("[rtbench] start (built {})", BUILD_TIME);

    let core_hz = calibrate_clock();
    let _ = core_hz;

    // ②c 后生产 base() 已是 RCPU 本地别名（0x0..0x80000 镜像主域）。
    // 「shm=共享窗」测试项显式用主域地址，保持与历史数据可比（主域端口
    // vs 本地别名端口的对照正是 ②c 的依据）；生产路径单价看 alias 测试项。
    let prod_base = ov_shm::shm::base();
    let shm = 0xC080_0000usize;
    let shm_line = shm + SHM_SCRATCH_OFF;
    let shm_blk = shm_line;
    let alias_line = SHM_SCRATCH_OFF; // 0x0..0x80000 本地别名窗
    // SAFETY: LOCAL_BLK 仅本任务访问。
    let local_line = (&raw const LOCAL_BLK) as usize;
    let local_blk = local_line;

    log::info!(
        "[rtbench] prod_base={:#x} main_domain={:#x} scratch={:#x}",
        prod_base,
        shm,
        shm_line
    );

    // —— 第 2 节：时间戳与驱动基建 ——
    log::info!("[rtbench] == sec2: timestamp & driver infra ==");
    timeit("mtime_read", MTIME_ADDR, 5000, b_mtime_read);
    timeit("mcycle_read", 0, 5000, b_mcycle_read);
    timeit("timer_slot_now", 0, 2000, b_timer_slot);
    timeit("shm_base_fn", 0, 5000, b_shm_base);
    timeit("mbox_msgstat_drv", 0, 2000, b_mbox_msgstat);
    timeit("mbox_irqen_drv", 0, 2000, b_mbox_irqen);
    timeit("mbox_enwr_mmio", 0, 2000, b_mbox_enwr);
    timeit("alu_mul", 0, 100_000, b_alu);

    // —— 第 3 节：fence 矩阵 ——
    log::info!("[rtbench] == sec3: fence matrix (shm {}) ==", shm_line);
    timeit("fence_r_rw_pure", 0, 1000, f_fence_rrw);
    timeit("fence_rw_w_pure", 0, 1000, f_fence_rww);
    timeit("fence_rw_rw_pure", 0, 1000, f_fence_rwrw);
    timeit("fence_iorw_pure", 0, 1000, f_fence_iorw);
    timeit("ld_ctrl", shm_line, 5000, f_ld);
    timeit("ld+fence_r_rw", shm_line, 2000, f_ld_fence_rrw);
    timeit("ld+fence_rw_rw", shm_line, 2000, f_ld_fence_rw);
    timeit("ld+32delay+fence_r_rw", shm_line, 2000, f_ld_delay_fence);
    timeit("mtime_ld+fence_r_rw", MTIME_ADDR, 2000, f_ld_fence_rrw);
    timeit("sd+fence_rw_rw", shm_line, 2000, f_sd_fence_rw);
    timeit("fence_rw_w+sd(release)", shm_line, 2000, f_fence_w_sd);
    timeit("sd+fence_iorw(notify)", shm_line, 2000, f_sd_fence_iorw);
    log::info!("[rtbench] == sec3b: fence matrix (local {:#x}) ==", local_line);
    timeit("ld+fence_r_rw_local", local_line, 2000, f_ld_fence_rrw);
    timeit("fence_rw_w+sd_local", local_line, 2000, f_fence_w_sd);

    // —— 第 4 节：AMO 矩阵 ——
    log::info!("[rtbench] == sec4: AMO matrix (shm) ==",);
    timeit("amoswap.d_rel", shm_line, 1000, f_amoswap_rel);
    timeit("amoswap.d_aq", shm_line, 1000, f_amoswap_aq);
    timeit("amoswap.d_rl", shm_line, 1000, f_amoswap_rl);
    timeit("amoswap.d_aqrl", shm_line, 1000, f_amoswap_aqrl);
    timeit("amoadd.d_aqrl", shm_line, 1000, f_amoadd_aqrl);
    timeit("amoor.w_aqrl", shm_line, 1000, f_amoor_w_aqrl);
    timeit("lr.d_aq", shm_line, 1000, f_lr_aq);
    timeit("lr+sc_cas", shm_line, 1000, f_cas_loop);
    log::info!("[rtbench] == sec4b: AMO matrix (local) ==");
    timeit("amoswap.d_rel_local", local_line, 1000, f_amoswap_rel);
    timeit("amoswap.d_aqrl_local", local_line, 1000, f_amoswap_aqrl);
    timeit("lr.d_aq_local", local_line, 1000, f_lr_aq);

    // —— 第 5 节：critical-section 后端仿真（MIE=1 真实路径）——
    log::info!("[rtbench] == sec5: critical-section backend emu ==");
    timeit_live("cs_emu_load_shm", shm_line, 2000, f_cs_load);
    timeit_live("cs_emu_load_local", local_line, 2000, f_cs_load);
    timeit_live("cs_emu_rmw_shm", shm_line, 2000, f_cs_rmw);
    timeit_live("cs_real_with_shm", shm_line, 2000, f_cs_real);

    // —— 第 6 节：SRAM 访问模式 ——
    log::info!("[rtbench] == sec6: SRAM access patterns ==");
    timeit("rd_same_line_shm", shm_line, 5000, b_rd_same);
    timeit("rd_seq_lines_shm(/32)", shm_line, 200, b_rd_seq_lines);
    timeit("rd_stride512_shm(/4)", shm_line, 200, b_rd_stride512);
    // 冷读大扫走 RCPU 本地窗口（v2 事故改道，见 b_rd_cold_sweep 注释）。
    timeit("rd_cold_sweep_local(/2048)", 0x19000, 20, b_rd_cold_sweep);
    timeit("rd_blk256_shm", shm_blk, 500, b_rd_blk256);
    timeit("wr_same_shm_nofence", shm_line, 5000, b_wr_same);
    timeit("wr_then_rd_same_shm", shm_line, 2000, b_wr_then_rd_same);
    timeit("wr_then_rd_diff_shm", shm_line, 2000, b_wr_then_rd_diff);
    timeit("rd_same_line_local", local_line, 5000, b_rd_same);
    timeit("rd_blk256_local", local_blk, 500, b_rd_blk256);
    timeit("rd_same_line_alias", alias_line, 5000, b_rd_same);

    // —— 第 7 节：取指与 I$ 存在性 ——
    log::info!("[rtbench] == sec7: icall spread (cold fetch) ==");
    timeit("call_hot", 0, 20_000, b_call_hot);
    timeit("call_spread_p1", 0, 20_000, b_call_spread);
    timeit("call_spread_p2", 0, 20_000, b_call_spread);

    // —— 第 8 节：trap 与 WFI ——
    log::info!("[rtbench] == sec8: trap & wfi ==");
    timeit("pend_ints_off", 0, 500, b_pend_off);
    bench_msip_roundtrip(100);
    bench_wfi_wake(50);

    // —— 汇总表 ——
    log::info!("[rtbench] ===== summary (ns/op, cyc/op) =====");
    // SAFETY: 单任务上下文，只读遍历。
    unsafe {
        for i in 0..ROW_IDX {
            if let Some(r) = &ROWS[i] {
                if MCYCLE_OK {
                    log::info!("[rtbench] {:<26} {:>8} {:>8}", r.name, r.ns, r.cyc);
                } else {
                    log::info!("[rtbench] {:<26} {:>8} {:>8}", r.name, r.ns, "-");
                }
            }
        }
    }
    log::info!("[rtbench] done. sleeping.");

    // —— 第 9 节：硬件 spinlock（排最后：窗口不可达时 LoadFault，
    // 保证汇总表已打出）——
    bench_spinlock();
    log::info!("[rtbench] all sections complete.");
}

#[executor::main]
fn main(spawner: Pin<&'static Spawner<4>>) {
    platform::console().write(b"\nK3 rtbench (built ");
    platform::console().write(BUILD_TIME.as_bytes());
    platform::console().write(b")\n");
    spawner.spawn(Priority::new(0), task_rtbench().unwrap());
}

// ── ISR ──────────────────────────────────────────────────────────────────

// MSIP 自环测试的入口戳（调度器语义由 executor-macro 包裹层维持）。
#[executor::interrupt]
fn MachineSoft(_tf: &mut TrapFrame) {
    // SAFETY: ISR 上下文单点写。
    unsafe { MSIP_ENTRY_TICK = mtime() };
}

// WFI 唤醒测试的入口戳 + 停表（mtimecmp=MAX 清 mtip，防中断风暴）。
#[executor::interrupt]
fn MachineTimer(_tf: &mut TrapFrame) {
    // SAFETY: 同上。
    unsafe { WFI_ENTRY_TICK = mtime() };
    chip_k3_rt24::clint_k3::TIMER.set_deadline(u64::MAX);
}

// K3 无跨核 MachineSoft 通知路径，MachineExternal 用 platform 默认分发。
