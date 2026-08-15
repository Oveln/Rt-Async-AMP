//! K3 共享内存 ov-channels ping 验证（rp 端）。
//!
//! 与 AP 侧 StarryOS 用户态 `user-test-ipc`（Rust，用 ov-channels sender/receiver）
//! 配对，走完整的 ov-channels 消息机制（非自定义裸槽）：
//!
//! - **收**：AP 经 ch0（StarryOS→rt-async）写消息 → mailbox4 FIFO → rcpu1 IRQ 69
//!   → mbox_isr → `MBX3.recv().await` 唤醒 → `process_elastic` 收割 ring buffer。
//! - **发**：`process_elastic` 处理每条消息后经 ch1（rt-async→StarryOS）回写
//!   响应，`send_notify_ipi()` → mailbox4 signal → AP IRQ 唤醒 AWAIT。
//!
//! 共享内存位于 RCPU SRAM 起始（0xc0800000，0x19000，经 ov-shm DT 节点 probe）。
//! intercom 模块负责 `SharedMemory::<3>::init()` 初始化 ring buffer + `RpcServer`
//! 消息收发（sender/try_send/receiver/try_recv 的细节封装其内）。跨核一致性由
//! AP 内核 rt_shm 驱动的 CBO 缓存同步点保证（X100 上 PTE PBMT 属性不生效，
//! 详见 tgoskits 侧 rt_shm 驱动文档）；rcpu1 对 SRAM 读写本就直达（无缓存）。
//!
//! SRAM 启动期时序约束：窗口头 ~1s 是还在执行的 SPL 代码（SPL text 0xc0801000
//! 起），U-Boot `k3_clear_sram()` 又在 RP 上线 ~1.5s memset 整个 512KB SRAM——
//! 故所有窗口写入延迟到 3s 后（两个任务同款门控），magic 自愈作后备（应对
//! bootdelay 漂移与启动链迟到写回）。
//!
//! 与 `ipc_demo` 共用 intercom 机制；本 bin 作为 ov-channels 链路的 ping 验证入口。

#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

extern crate rt_async_k3;

use core::pin::Pin;

use chip_k3_rt24::K3Rt24;
use executor::priority::Priority;
use executor::spawner::Spawner;
use fugit::ExtU64;
use ov_channels::SharedMemory;
use platform::arch::TrapFrame;

// ── 强制链接 chip crate ──────────────────────────────────────────────────
// chip-k3-rt24 的对外符号（`#[extern_trait] Board` 实现）不被本 bin 的常规代码
// 路径引用，rustc 默认不会把 chip rlib 交给链接器（--gc-sections 会剔除）。
// K3Rt24 是 chip crate 的公开零大小类型，保留其实例构成强引用锚点，拉入 rlib。
#[used]
static _FORCE_LINK_CHIP_K3_RT24: K3Rt24 = K3Rt24;

// 编译期生成的本 ELF 构建时间（build.rs → OUT_DIR/build_time.rs）。
include!(concat!(env!("OUT_DIR"), "/build_time.rs"));

/// 共享窗写入门控：SPL 执行期（头 ~1s）与 U-Boot `k3_clear_sram()`（~1.5s）
/// 都会破坏窗口内容，任何窗口写入必须等到上线 3s 之后。
const WRITE_GATE_MS: u64 = 3000;

/// magic 自愈看门狗 + 存活心跳。
///
/// AP 启动链（BootROM/SPL/U-Boot 曾 cacheable 写 SRAM）的迟到脏行写回可能在
/// RP init 之后清掉某通道 magic（板上实锤：ch1/ch2.magic 在 t≈7~24s 被清一次，
/// AP 侧另以 boot 时 invalidate 作废这些脏行）。本任务每秒检查三通道 magic，
/// 丢失即幂等重跑 `intercom::init()` 恢复全部头——必须赶在 AP 内核 rt_shm
/// probe 之前把 valid 变 true。注意 re-init 会顺带清空各通道未读消息（magic
/// 丢失时内容本已不可信，可接受）。
#[executor::task]
async fn task_magic_watchdog() {
    futures::timer::after((WRITE_GATE_MS).millis()).await;

    // SAFETY: `SharedMemory::at` 返回指向 DT 保留区的 &'static 引用，跨 await
    // 持有安全（映射与设备同生命周期）；本任务只读 magic（is_valid）与调用
    // 幂等 init。
    let shm = unsafe { SharedMemory::<3>::at(ov_shm::shm::base()) };
    // 自愈上限 8 次：启动链写回是有限事件（板上实测仅 1 次），无上限会在
    // "窗口持续被外部破坏"的病态场景下无限 re-init 刷日志。
    let mut heals: u32 = 0;
    let mut secs: u64 = 0;
    loop {
        futures::timer::after(1000.millis()).await;
        secs += 1;

        // 不设"先见过 valid"前置：看门狗 t=4s 才首查（3s 门控 + 1s 周期），
        // 若迟到写回恰落在 3s init 与首查之间，前置条件会让自愈永不触发。
        // !is_valid 即幂等 re-init，heals 上限兜住病态循环。
        if !shm.is_valid() && heals < 8 {
            heals += 1;
            log::info!(
                "[shm-ping] t={secs}s 三通道 magic 丢失（启动链迟到写回）→ 自愈 re-init（第 {heals} 次）"
            );
            rt_async_k3::intercom::init();
        }

        if secs % 10 == 0 {
            log::info!("[shm-ping] alive t={secs}s");
        }
    }
}

/// ov-channels ping 服务任务：延迟 3s → init ring buffer → 弹性忙等收割 → mailbox4 唤醒。
#[executor::task]
async fn task_shm_ping() {
    // 写入门控见 WRITE_GATE_MS；读路径不受 SPL/U-Boot 影响，但 init 会写
    // busy + 三通道头，必须同样等门。
    futures::timer::after(WRITE_GATE_MS.millis()).await;

    // 初始化 SharedMemory::<3> ring buffer（AP 侧 is_valid() 轮询等待此完成）。
    rt_async_k3::intercom::init();
    log::info!(
        "[shm-ping] t=3s SharedMemory<3> inited, awaiting AP notify (mailbox4, IRQ 69)"
    );

    loop {
        // 弹性忙等处理 ch0 所有待处理消息（notification + RPC），
        // 每条响应立即经 ch1 回写 + send_notify_ipi() 通知 AP。
        // 弹性窗口过期后返回，进入下面的 mailbox 阻塞等待。
        let _count = rt_async_k3::intercom::process_elastic();

        // 弹性窗口过期，等 AP 经 mailbox4 发来的新消息（IRQ 69）。
        chip_k3_rt24::mailbox::MBX3.recv().await;
    }
}

#[executor::main]
fn main(spawner: Pin<&'static Spawner<4>>) {
    platform::console().write(b"\nK3 shm ov-channels ping (built ");
    platform::console().write(BUILD_TIME.as_bytes());
    platform::console().write(b")\n");

    spawner.spawn(Priority::new(2), task_shm_ping().unwrap());
    // executor 语义：数值小 = 优先级高（Priority 的 Ord 反转内层比较）。
    // 看门狗是后台守护，须低于服务任务（P2），不抢占消息处理。
    spawner.spawn(Priority::new(3), task_magic_watchdog().unwrap());
    platform::console().write(b"tasks spawned, scheduler running\n");
}

// ── ISR ──────────────────────────────────────────────────────────────────
// K3 无跨核 MachineSoft 通知路径（通知走 mailbox 外部中断），空 hook。
#[executor::interrupt]
fn MachineSoft(_tf: &mut TrapFrame) {}

// 定时器中断：驱动 TimerQueue 唤醒 after() future。
#[executor::interrupt]
fn MachineTimer(_tf: &mut TrapFrame) {
    futures::timer::handle_timer_isr();
}
