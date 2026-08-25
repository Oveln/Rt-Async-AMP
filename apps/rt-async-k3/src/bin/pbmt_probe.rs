//! PBMT 用户态兑现性实验 · RP 侧伴随固件（与 user-apps/user-test-pbmt 配对）。
//!
//! 背景：板上 OpenSBI banner 列出 svpbmt 且 priv 1.12 ⇒ menvcfg.PBMTE 已置
//! 位；08-16 内核侧 ioremap 别名时延探针判 X100 硅忽略 PBMT（3.1 vs 1.2
//! 周期/读，同为 L1 命中量级）。本固件 + AP 侧独立用户态 bin 在真实用户
//! mmap 路径上复测，以行为级观测为主（观测期零 CBO / 零 ioctl 干扰）：
//! - 写直达性：AP 用户态写 REQ 魔数 → 本固件轮询见数即写回执；
//! - 读新鲜度：本固件周期写计数器，AP 侧裸读观察前进性。
//!
//! 职责（不跑 intercom 协议、不打 IPI——观测期 AP 侧刻意零门铃零 ioctl）：
//! 1. 首写门控 3s：避开 U-Boot k3_clear_sram() 全清（RP 启动 ~1.5s 后）与
//!    boot 链晚期脏行回写（t≈7-24s，见 src/watchdog.rs）；
//! 2. 门控后清一次 flag/ack 残留，随后每 ~100µs 向 cnt 单元写 {seq, mtime}
//!    （clint 直连 busy-wait 节奏——Slot 定时器 ~3µs/次会淹没周期精度；
//!    本固件单任务独占 CPU，忙等无害）；
//! 3. 同拍 Acquire 轮询 flag（fence r,rw + read_volatile——普通 volatile
//!    重读会被前端合并缓冲钉住陈旧值，litmus L1 mode1 实锤），见 REQ 前缀
//!    即写 ack {ACK 魔数, 回显 flag 原值} 并清零 flag（消费即清）。flag
//!    = REQ 前缀(高 32 位) | 每轮 nonce(pid, 低 32 位)：AP 侧只认回显等于
//!    本轮 nonce 的回执——缓存模式下上一轮滞留的 REQ 写会在下一轮 mmap
//!    时被内核整窗作废（CBIE=01 = clean+invalidate）冲刷进 SRAM 迟到触
//!    发回执，nonce 回显使这种跨轮残留不可能被误判为直达。
//!
//! scratch 单元布局（相对窗口基址 +0x18700 = intercom/rtbench 的
//! SHM_SCRATCH_OFF；RP 经本地别名窗访问，见 ov-shm probe 注释）。四单元分
//! 处不同缓存行，落在 MEMBENCH（止于 +0x5ff）与 LITMUS mode-2 哑元
//! （+0x7f8）之间的空闲区：
//!   +0x600 flag  AP→RP  REQ 魔数
//!   +0x680 ack   RP→AP  {ACK 魔数, 检测时 seq}
//!   +0x6c0 cnt   RP→AP  {seq, mtime tick}（~100µs 步长）
//!   +0x700       AP 侧时延自测行，本固件不碰
//! 偏移与魔数和 user-test-pbmt 手工保持同值（两侧无共享 crate、各持最小
//! 依赖；改一处必须同步另一处，两文件头注互引）。

#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

use core::pin::Pin;

use chip_k3_rt24::K3Rt24;
use executor::priority::Priority;
use executor::spawner::Spawner;
use platform::arch::TrapFrame;
use platform::Timer as _;

// 强制链接 chip crate（同 sched_demo，见其注释）。
#[used]
static _FORCE_LINK_CHIP_K3_RT24: K3Rt24 = K3Rt24;

// 编译期生成的本 ELF 构建时间（build.rs → OUT_DIR/build_time.rs）。
include!(concat!(env!("OUT_DIR"), "/build_time.rs"));

// ── scratch 布局（与 user-test-pbmt 同值，双端手工对齐）─────────────────
const SHM_SCRATCH_OFF: usize = 0x18700;
const FLAG_OFF: usize = 0x600;
const ACK_OFF: usize = 0x680;
const CNT_OFF: usize = 0x6c0;
/// AP→RP 请求前缀（高 32 位，"PBMT"；低 32 位 = 每轮 nonce，AP 侧为 pid）。
const REQ_PREFIX: u64 = (u32::from_be_bytes(*b"PBMT") as u64) << 32;
/// RP→AP 回执魔数（"PBMT_ACK"，ack+8 回显 flag 原值）。
const ACK_MAGIC: u64 = u64::from_be_bytes(*b"PBMT_ACK");

/// 首写门控（ms），依据同 watchdog::WRITE_GATE_MS。
const WRITE_GATE_MS: u64 = 3000;
/// counter 写入周期（µs）。
const PERIOD_US: u64 = 100;

#[executor::task]
async fn task_pbmt_probe() {
    let now = || chip_k3_rt24::clint_k3::TIMER.now();
    let freq = chip_k3_rt24::clint_k3::TIMER.freq_hz() as u64;
    let scr = ov_shm::shm::base() + SHM_SCRATCH_OFF;
    log::info!(
        "[pbmt_probe] start (built {}), scratch={:#x}, 首写门控 {}ms",
        BUILD_TIME,
        scr,
        WRITE_GATE_MS
    );

    // SAFETY: scratch 为窗口尾部空闲区（见模块头注），仅本固件与 AP 侧
    // user-test-pbmt 访问；volatile 保序由单任务程序序保证。
    let rd = |off: usize| unsafe { ((scr + off) as *const u64).read_volatile() };
    let wr = |off: usize, v: u64| unsafe { ((scr + off) as *mut u64).write_volatile(v) };

    let t0 = now();
    while now().wrapping_sub(t0) < WRITE_GATE_MS * freq / 1000 {
        core::hint::spin_loop();
    }

    // 清上一轮实验残留（U-Boot 每次启动已全清 SRAM，此处防御性再清）。
    wr(FLAG_OFF, 0);
    wr(ACK_OFF, 0);
    wr(ACK_OFF + 8, 0);
    log::info!("[pbmt_probe] gate open, cnt 周期 {}µs", PERIOD_US);

    let period = PERIOD_US * freq / 1_000_000;
    let mut seq: u64 = 0;
    let mut next = now();
    let mut ack_logged = false;
    let mut last_log = now();
    loop {
        seq += 1;
        // RP 写 SRAM 按程序序直达（litmus 实锤，无需 fence）；ts 为诊断信息。
        wr(CNT_OFF, seq);
        wr(CNT_OFF + 8, now());

        // Acquire 轮询 flag：fence r,rw 强制重新取数（litmus L1 mode1——
        // 普通 volatile 重读会被前端合并缓冲钉住陈旧值）。
        // SAFETY: 纯屏障指令，无内存副作用。
        unsafe { core::arch::asm!("fence r, rw", options(nostack)) };
        let f = rd(FLAG_OFF);
        if f & 0xffff_ffff_0000_0000 == REQ_PREFIX {
            wr(ACK_OFF, ACK_MAGIC);
            wr(ACK_OFF + 8, f); // 回显 flag 原值（含发起轮 nonce）
            wr(FLAG_OFF, 0); // 消费即清：防残留重复触发回执
            if !ack_logged {
                ack_logged = true;
                log::info!("[pbmt_probe] REQ 已见（flag={:#x}, seq={}），回执已写并清 flag", f, seq);
            }
        }

        next = next.wrapping_add(period);
        while now() < next {
            core::hint::spin_loop();
        }

        // 10s 一条活性行（低频，符合中断/串口纪律）。
        if now().wrapping_sub(last_log) >= 10 * freq {
            last_log = now();
            log::info!("[pbmt_probe] alive seq={}", seq);
        }
    }
}

#[executor::main]
fn main(spawner: Pin<&'static Spawner<4>>) {
    platform::console().write(b"\nK3 pbmt_probe (built ");
    platform::console().write(BUILD_TIME.as_bytes());
    platform::console().write(b")\n");
    spawner.spawn(Priority::new(0), task_pbmt_probe().unwrap());
}

// ── ISR ──────────────────────────────────────────────────────────────────
// 节奏走 clint busy-wait，不用 timer future；两 hook 维持调度器语义。
#[executor::interrupt]
fn MachineSoft(_tf: &mut TrapFrame) {}

#[executor::interrupt]
fn MachineTimer(_tf: &mut TrapFrame) {
    // mtimecmp=MAX 清 mtip，防中断风暴（同 rtbench）。
    chip_k3_rt24::clint_k3::TIMER.set_deadline(u64::MAX);
}
