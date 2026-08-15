//! K3 RT24 rcpu1 双核 IPC demo。
//!
//! 验收点：与 AP 侧 StarryOS（/dev/rt_shm 用户态 user-test-*）双向 RPC：
//!   - 收：AP 写 mailbox4（0xCAC91000）FIFO ch0 → rcpu1 IRQ 69 → mbox_isr
//!     → MBX3.recv().await 唤醒
//!   - 发：intercom::send_notify_ipi() → MBX3.signal(1)（ch1）→ AP IRQ 217
//! 共享内存 0xc0800000（RCPU SRAM，ov,rt-async-amp 节点，ov-shm 驱动 probe）。
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
use fugit::ExtU64;
use platform::arch::TrapFrame;

// ── 强制链接 chip crate ──────────────────────────────────────────────────
// chip-k3-rt24 的对外符号（`#[extern_trait] Board` 实现）不被本 bin 的常规代码
// 路径引用，rustc 默认不会把 chip rlib 交给链接器（--gc-sections 会剔除）。
// K3Rt24 是 chip crate 的公开零大小类型，保留其实例构成强引用锚点，拉入 rlib。
#[used]
static _FORCE_LINK_CHIP_K3_RT24: K3Rt24 = K3Rt24;

// 编译期生成的本 ELF 构建时间（build.rs → OUT_DIR/build_time.rs）。
include!(concat!(env!("OUT_DIR"), "/build_time.rs"));

/// 双核 IPC 服务任务：弹性忙等处理 + mailbox4 外部中断唤醒。
#[executor::task]
async fn task_ipc() {
    // SRAM 写入门控（同 shm_ping.WRITE_GATE_MS）：窗口头 ~1s 是还在执行的
    // SPL 代码、~1.5s 被 U-Boot k3_clear_sram() 整窗 memset，提前 init 写入
    // 的 magic 会被抹掉、AP 侧 is_valid() 永假。
    futures::timer::after(3000.millis()).await;
    rt_async_k3::intercom::init();

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
