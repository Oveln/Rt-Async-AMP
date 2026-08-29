//! K3 RT24 rcpu1 机器人控制固件（AKA-00 底盘 + 机械臂）。
//!
//! 在 ipc_demo 的 intercom 服务（ECHO/PING/STATS + 机器人语义 RPC）之上，
//! 增加 [`crate::robot`] 的两个 P1 协议任务（高于 RPC 服务 P2，定时器抢占
//! 保证弹性自旋窗口内仍按节拍运行）：
//!
//! ```text
//! P1  task_chassis ── R_UART0(slot0，与 console 共口) ── 40pin pin29(TX)/pin32(RX)
//! P1  task_arm     ── 软串口 TX（R.GPIO[30] @AON_TIMER1 c1 定拍）── 40pin pin40
//! P2  task_ipc     ── 共享窗 + mailbox4（intercom：RPC 分发/异步完成）
//! P3  watchdog     ── magic 自愈（同 ipc_demo）
//! ```
//!
//! AP 侧配套程序：user-apps/robot-ctl（CLI + serve JSON 行协议，供 Python
//! 调用）。接线与使用方法见 README「机器人控制」小节（双通道方案 +
//! 备选 M.2 飞线双串口，2026-08-27 原理图定案 / 08-29 臂分离软串口）。

#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

extern crate rt_async_k3;

use core::pin::Pin;

use chip_k3_rt24::K3Rt24;
use executor::priority::Priority;
use executor::spawner::Spawner;
use platform::arch::TrapFrame;

// ── 强制链接 chip crate ──────────────────────────────────────────────────
// chip-k3-rt24 的对外符号（`#[extern_trait] Board` 实现）不被本 bin 的常规代码
// 路径引用，rustc 默认不会把 chip rlib 交给链接器（--gc-sections 会剔除）。
// K3Rt24 是 chip crate 的公开零大小类型，保留其实例构成强引用锚点，拉入 rlib。
#[used]
static _FORCE_LINK_CHIP_K3_RT24: K3Rt24 = K3Rt24;

// 编译期生成的本 ELF 构建时间（build.rs → OUT_DIR/build_time.rs）。
include!(concat!(env!("OUT_DIR"), "/build_time.rs"));

#[executor::main]
fn main(spawner: Pin<&'static Spawner<4>>) {
    platform::console().write(b"\nK3 rt-async robot_ctrl (built ");
    platform::console().write(BUILD_TIME.as_bytes());
    platform::console().write(b")\n");

    // 机器人协议任务（P1：高于 RPC 服务，timer ISR 抢占保证弹性自旋窗口
    // 内仍按 10ms 节拍运行——异步完成的响应延迟不受自旋窗口影响）。
    spawner.spawn(Priority::new(1), rt_async_k3::robot::task_chassis().unwrap());
    spawner.spawn(Priority::new(1), rt_async_k3::robot::task_arm().unwrap());
    // intercom RPC 服务任务（P2，同 ipc_demo）。
    spawner.spawn(Priority::new(2), task_ipc().unwrap());
    // magic 自愈看门狗（P3，最低）。
    spawner.spawn(Priority::new(3), rt_async_k3::watchdog::magic_watchdog().unwrap());
    platform::console().write(b"tasks spawned, scheduler running\n");
}

/// 双核 IPC 服务任务：等待 AP 侧 init + 弹性忙等处理 + mailbox4 外部中断唤醒。
#[executor::task]
async fn task_ipc() {
    // 共享窗 init 已迁至 AP 内核 rt_shm probe 期，本侧只读等待。
    rt_async_k3::intercom::wait_ready(10, 10_000).await;

    loop {
        // 弹性忙等处理所有消息，每个 Notify 响应立即回 IPI；机器人语义
        // 请求中的 acall 类（INIT/GRAB/RELEASE）在此转交 P1 任务异步完成。
        let _count = rt_async_k3::intercom::process_elastic();

        // 弹性窗口过期，等待 AP 经 mailbox4 发来的新消息（IRQ 69）。
        chip_k3_rt24::mailbox::MBX3.recv().await;
    }
}

// ── ISR ──────────────────────────────────────────────────────────────────
// K3 无跨核 MachineSoft 通知路径（通知走 mailbox 外部中断），空 hook。
#[executor::interrupt]
fn MachineSoft(_tf: &mut TrapFrame) {}

// 定时器中断：驱动 TimerQueue 唤醒 after() future（也是 P1 任务的抢占源）。
#[executor::interrupt]
fn MachineTimer(_tf: &mut TrapFrame) {
    futures::timer::handle_timer_isr();
}
