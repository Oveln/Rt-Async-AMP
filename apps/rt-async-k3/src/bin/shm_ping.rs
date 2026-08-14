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
//! 共享内存位于 DDR（0x104430000，经 ov-shm DT 节点 probe）。intercom 模块
//! 负责 `SharedMemory::<3>::init()` 初始化 ring buffer + `RpcServer` 消息收发
//! （sender/try_send/receiver/try_recv 的细节封装其内）。AP 侧 mmap 经
//! svpbmt PBMT=NC 映射（uncached），保证跨核读写一致。
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

// ── 强制链接 chip crate ──────────────────────────────────────────────────
// chip-k3-rt24 的对外符号（`#[extern_trait] Board` 实现）不被本 bin 的常规代码
// 路径引用，rustc 默认不会把 chip rlib 交给链接器（--gc-sections 会剔除）。
// K3Rt24 是 chip crate 的公开零大小类型，保留其实例构成强引用锚点，拉入 rlib。
#[used]
static _FORCE_LINK_CHIP_K3_RT24: K3Rt24 = K3Rt24;

// 编译期生成的本 ELF 构建时间（build.rs → OUT_DIR/build_time.rs）。
include!(concat!(env!("OUT_DIR"), "/build_time.rs"));

/// ov-channels ping 服务任务：init ring buffer → 弹性忙等收割 → mailbox4 唤醒。
#[executor::task]
async fn task_shm_ping() {
    // 初始化 SharedMemory::<3> ring buffer（AP 侧 is_valid() 轮询等待此完成）。
    rt_async_k3::intercom::init();
    log::info!(
        "[shm-ping] SharedMemory<3> inited, awaiting AP notify (mailbox4, IRQ 69)"
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
