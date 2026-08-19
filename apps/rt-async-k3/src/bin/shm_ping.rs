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
//! 共享内存物理位于 RCPU SRAM 起始（主域视图 0xc0800000 / RP 本地别名 0x0，
//! 0x19000，经 ov-shm DT 节点 probe）。
//! intercom 模块负责 `SharedMemory::<3>::init()` 初始化 ring buffer + `RpcServer`
//! 消息收发（sender/try_send/receiver/try_recv 的细节封装其内）。跨核一致性由
//! AP 内核 rt_shm 驱动的 CBO 缓存同步点保证（X100 上 PTE PBMT 属性不生效，
//! 详见 tgoskits 侧 rt_shm 驱动文档）；rcpu1 对 SRAM 读写本就直达（无缓存）。
//!
//! SRAM 启动期时序约束：窗口头 ~1s 是还在执行的 SPL 代码（SPL text
//! 0xc0801000 起），U-Boot `k3_clear_sram()` 又在 RP 上线 ~1.5s memset 整个
//! 512KB SRAM——但 memset 发生在 bootm 之前，且共享窗初始化的职责现已
//! 迁至 AP 内核 rt_shm probe 期（时机确定性排在全部破坏者之后），本侧经
//! `intercom::wait_ready` 只读等待；magic 自愈看门狗作纵深防御（应对 bootdelay
//! 漂移等残余场景）。
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
use platform::arch::TrapFrame;
use rt_async_k3::watchdog;

// ── 强制链接 chip crate ──────────────────────────────────────────────────
// chip-k3-rt24 的对外符号（`#[extern_trait] Board` 实现）不被本 bin 的常规代码
// 路径引用，rustc 默认不会把 chip rlib 交给链接器（--gc-sections 会剔除）。
// K3Rt24 是 chip crate 的公开零大小类型，保留其实例构成强引用锚点，拉入 rlib。
#[used]
static _FORCE_LINK_CHIP_K3_RT24: K3Rt24 = K3Rt24;

// 编译期生成的本 ELF 构建时间（build.rs → OUT_DIR/build_time.rs）。
include!(concat!(env!("OUT_DIR"), "/build_time.rs"));

// magic 自愈看门狗：共享任务（见 `watchdog.rs` 模块文档——启动链迟到
// 写回会清 magic，无看门狗则 AP 侧 is_valid() 永假）。
//
// ov-channels ping 服务任务：等待 AP 侧 init（见 intercom::wait_ready）→
// 弹性忙等收割 → mailbox4 唤醒。
#[executor::task]
async fn task_shm_ping() {
    // init 职责已迁至 AP 内核 rt_shm probe 期；10s 回退兼容旧内核。
    rt_async_k3::intercom::wait_ready(10, 10_000).await;
    log::info!("[shm-ping] service online, awaiting AP notify (mailbox4, IRQ 69)");

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
    spawner.spawn(Priority::new(3), watchdog::magic_watchdog().unwrap());
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
