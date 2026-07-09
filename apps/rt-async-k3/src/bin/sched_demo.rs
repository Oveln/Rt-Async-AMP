//! K3 RT24 rcpu1 调度器+定时器验证 demo。
//!
//! 验收点：在 K3 RT24 上跑通 rt-async 优先级抢占调度器 + SysTimer 定时器
//! + MSIP 自中断（pend）全链路。
//!
//! 运行效果：R_UART0(115200 8N1) 持续输出：
//!   - `H`（高优先级任务 task_high，每 50ms 一次）
//!   - `L`（低优先级任务 task_low，每 50ms 一次，被 task_high 抢占）
//! 看到两者交替（H 出现频率 ≥ L）即说明抢占+定时器正常。
//!
//! 板级初始化（握手+时钟+pinmux+UUE）已在 `platform::init()` 内由
//! `BoardImpl::init()` → `chip-k3-rt24` 的 `Board::init()` 完成（早于本 main）。

#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

// ── 强制链接 chip crate ──────────────────────────────────────────────────
// chip-k3-rt24 的对外符号（`#[extern_trait] Board` 实现）不被本 bin 的常规代码
// 路径引用，rustc 默认不会把 chip rlib 交给链接器（--gc-sections 会剔除）。
// K3Rt24 是 chip crate 的公开零大小类型，保留其实例构成强引用锚点，拉入 rlib。
use chip_k3_rt24::K3Rt24;
#[used]
static _FORCE_LINK_CHIP_K3_RT24: K3Rt24 = K3Rt24;

use core::pin::Pin;

use executor::priority::Priority;
use executor::spawner::Spawner;
use fugit::ExtU64;
use platform::arch::TrapFrame;

/// 高优先级任务：每 50ms 输出 `H`，验证定时器唤醒 + 抢占低优先级。
#[executor::task]
async fn task_high() {
    let mut n = 0u32;
    loop {
        futures::timer::after(50.millis()).await;
        n += 1;
        platform::console().write(b"H");
        // 每 20 次（~1s）打一行带计数，便于确认计数在涨。
        if n % 20 == 0 {
            platform::console().write(b" [high tick=");
            print_dec(n);
            platform::console().write(b"]\n");
        }
    }
}

/// 低优先级任务：每 50ms 输出 `L`，会被 task_high 抢占。
#[executor::task]
async fn task_low() {
    let mut n = 0u32;
    loop {
        futures::timer::after(50.millis()).await;
        n += 1;
        platform::console().write(b"L");
        if n % 20 == 0 {
            platform::console().write(b" [low tick=");
            print_dec(n);
            platform::console().write(b"]\n");
        }
    }
}

#[executor::main]
fn main(spawner: Pin<&'static Spawner<4>>) {
    platform::console().write(b"\nK3 rt-async scheduler demo\n");
    platform::console().write(b"expect alternating H/L (high preempts low)\n\n");

    spawner.spawn(Priority::new(3), task_high().unwrap());
    spawner.spawn(Priority::new(1), task_low().unwrap());

    platform::console().write(b"tasks spawned, scheduler running\n");
}

// ── ISR ──────────────────────────────────────────────────────────────────
// K3 阶段1 无跨核 IPC，MachineSoft 仅作调度器自中断入口（空 hook）。
#[executor::interrupt]
fn MachineSoft(_tf: &mut TrapFrame) {}

// 定时器中断：驱动 TimerQueue 唤醒 after() future。
#[executor::interrupt]
fn MachineTimer(_tf: &mut TrapFrame) {
    futures::timer::handle_timer_isr();
}

// ── 小工具：十进制打印（不依赖 fmt/alloc）────────────────────────────────
fn print_dec(mut v: u32) {
    if v == 0 {
        platform::console().write(b"0");
        return;
    }
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    platform::console().write(&buf[i..]);
}
