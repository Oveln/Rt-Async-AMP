//! rt-async-amp 双核 demo
//!
//! hart 1 (M-mode): rt-async 优先级抢占调度，输出到 UART1
//! hart 0 (S-mode): StarryOS，输出到 UART0
//!
//! 共享内存 IPC 位于 0x88000000
//!
#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

extern crate rt_async_app;

use core::pin::Pin;

use executor::priority::Priority;
use executor::spawner::Spawner;
use platform::arch::TrapFrame;
use platform::{Chip, ChipImpl};

#[executor::task]
async fn task_ipc() {
    rt_async_app::intercom::init();

    loop {
        rt_async_app::ipc_wait::WaitForMessage.await;
        while rt_async_app::intercom::process_pending() {}
    }
}

#[executor::main]
fn main(spawner: Pin<&'static Spawner<4>>) {
    ChipImpl::put_str("rt-async-amp: rt-async started\n");
    log::info!("rt-async-amp: hart 1 (rt-async) started");

    spawner.spawn(Priority::new(2), task_ipc().unwrap());

    log::info!("rt-async-amp: task spawned, entering scheduler");
}

#[executor::interrupt]
fn MachineSoft(_tf: &mut TrapFrame) {
    rt_async_app::ipc_wait::notify_from_isr();
}

#[executor::interrupt]
fn MachineTimer(_tf: &mut TrapFrame) {
    futures::timer::handle_timer_isr();
}
