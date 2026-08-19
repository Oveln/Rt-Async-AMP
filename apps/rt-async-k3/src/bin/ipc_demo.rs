//! K3 RT24 rcpu1 双核 IPC demo。
//!
//! 验收点：与 AP 侧 StarryOS（/dev/rt_shm 用户态 user-test-*）双向 RPC：
//!   - 收：AP 写 mailbox4（0xCAC91000）FIFO ch0 → rcpu1 IRQ 69 → mbox_isr
//!     → MBX3.recv().await 唤醒
//!   - 发：intercom::send_notify_ipi() → MBX3.signal(1)（ch1）→ AP IRQ 217
//! 共享内存物理位于 RCPU SRAM 起始（主域视图 0xc0800000 / RP 本地别名 0x0，
//! ov,rt-async-amp 节点，ov-shm 驱动 probe）。
//!
//! mailbox 通道约定：物理 **mailbox4**（esos 侧 DTB status "disabled"，空闲）
//! 是 rt-async ↔ AP 的核间信令通道，Rust 变量名沿用 `MBX3`（= DT 唯一
//! mailbox 节点）；**mailbox3** 归 esos(rcpu0) 的 rproc 专用（rcpu0 DTB
//! status "okay"），故 DTS 未保留其节点。RT24 双核共享同一 PLIC，若注册
//! esos 在用通道（mailbox3）的 ISR，会被 esos 的 rpmsg 活动触发幽灵中断
//! ——必须避开。收发分通道：AP→RP ch0（本侧 enable_new_msg_irq(0) 接收），
//! RP→AP ch1（notify() → signal(1)）。

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

/// 双核 IPC 服务任务：等待 AP 侧 init + 弹性忙等处理 + mailbox4 外部中断唤醒。
#[executor::task]
async fn task_ipc() {
    // 共享窗 init 已迁至 AP 内核 rt_shm probe 期（时机确定性在 SPL /
    // U-Boot memset / bootm 缓存 flush 之后），本侧只读等待；10s 回退兼容
    // 旧内核（本地 init）。
    rt_async_k3::intercom::wait_ready(10, 10_000).await;

    loop {
        // 弹性忙等处理所有消息，每个 Notify 响应立即回 IPI。
        let _count = rt_async_k3::intercom::process_elastic();

        // 弹性窗口过期，等待 AP 经 mailbox4 发来的新消息（IRQ 69）。
        chip_k3_rt24::mailbox::MBX3.recv().await;
    }
}

#[executor::main]
fn main(spawner: Pin<&'static Spawner<4>>) {
    platform::console().write(b"\nK3 rt-async IPC demo (built ");
    platform::console().write(BUILD_TIME.as_bytes());
    platform::console().write(b")\n");

    spawner.spawn(Priority::new(2), task_ipc().unwrap());
    // magic 自愈看门狗（P3，低于 IPC 服务任务）：启动链迟到脏行写回会在
    // t≈7~24s 清掉通道 magic（板上实锤），无看门狗则 AP 侧 is_valid() 永假、
    // 用户态程序 5s 超时。详见 watchdog.rs 模块文档。
    spawner.spawn(Priority::new(3), rt_async_k3::watchdog::magic_watchdog().unwrap());
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
