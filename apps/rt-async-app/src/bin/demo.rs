//! rt-async-amp 双核 demo
//!
//! hart 0 (M-mode): rt-async 优先级抢占调度，输出到 UART1
//! hart 1 (S-mode): StarryOS，输出到 UART0
//!
//! 共享内存 IPC 位于 0x88000000

#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

extern crate rt_async_app;

use core::pin::Pin;

use executor::priority::Priority;
use executor::spawner::Spawner;
use fugit::ExtU64;
use platform::arch::TrapFrame;
use platform::{Chip, ChipImpl};

#[executor::task]
async fn task_a() {
    let mut count = 0u32;
    loop {
        log::info!("[task_a] tick #{count}");
        count += 1;
        futures::timer::after(500.millis()).await;
    }
}

#[executor::task]
async fn task_b() {
    let mut count = 0u32;
    loop {
        log::info!("[task_b] tock #{count}");
        count += 1;
        futures::timer::after(700.millis()).await;
    }
}

#[executor::task]
async fn task_ipc() {
    rt_async_app::intercom::init();

    let mut tick = 0u32;
    loop {
        rt_async_app::intercom::process_pending();

        if tick.is_multiple_of(10) {
            rt_async_app::intercom::send_notification(tick);
        }

        tick += 1;
        futures::timer::after(1000.millis()).await;
    }
}

#[executor::main]
fn main(spawner: Pin<&'static Spawner<4>>) {
    ChipImpl::put_str("rt-async-amp: hart 0 direct write\n");
    log::info!("rt-async-amp: hart 0 started at 0x80800000");

    spawner.spawn(Priority::new(0), task_a().unwrap());
    spawner.spawn(Priority::new(1), task_b().unwrap());
    spawner.spawn(Priority::new(2), task_ipc().unwrap());

    log::info!("rt-async-amp: 3 tasks spawned, entering scheduler");
}

#[executor::interrupt]
fn MachineTimer(_tf: &mut TrapFrame) {
    futures::timer::handle_timer_isr();
}
