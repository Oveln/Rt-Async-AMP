//! rt-async UART console (interrupt-driven RX)
//!
//! hart 1 (M-mode): rt-async priority-preemptive scheduler, UART1 interactive shell
//! hart 0 (S-mode): StarryOS, output to UART0
//!
//! RX strategy: UART1 RX interrupt → NS16550A driver ring buffer → SerialRx Future

#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

extern crate rt_async_app;

use core::fmt::Write;
use core::pin::Pin;

use executor::priority::Priority;
use executor::spawner::Spawner;
use platform::arch::TrapFrame;

const LINE_BUF_SIZE: usize = 128;

struct UartWriter;
impl Write for UartWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        platform::console().write(s.as_bytes());
        Ok(())
    }
}

fn uprint(args: core::fmt::Arguments) {
    let _ = UartWriter.write_fmt(args);
}

macro_rules! uprintln {
    ($($arg:tt)*) => { uprint(format_args!($($arg)*)) }
}

#[executor::task]
async fn task_ipc() {
    rt_async_app::intercom::init();

    loop {
        let _count = rt_async_app::intercom::process_elastic();
        rt_async_app::ipc_wait::WaitForMessage.await;
    }
}

#[executor::task]
async fn task_console() {
    let mut line_buf = [0u8; LINE_BUF_SIZE];
    let mut line_len: usize = 0;

    uprintln!("\r\nrt-async console ready (interrupt-driven). Type 'help' for commands.\r\n");

    loop {
        let byte = futures::serial::SerialRx::new().await;

        match byte {
            0x7f | 0x08 => {
                if line_len > 0 {
                    line_len -= 1;
                    put_str("\x08 \x08");
                }
            }
            b'\r' | b'\n' => {
                put_str("\r\n");
                if line_len > 0 {
                    let line = core::str::from_utf8(&line_buf[..line_len]).unwrap_or("");
                    execute(line);
                }
                put_str("> ");
                line_len = 0;
            }
            0x03 => {
                uprintln!("^C\r\n");
                put_str("> ");
                line_len = 0;
            }
            0x15 => {
                for _ in 0..line_len {
                    put_str("\x08 \x08");
                }
                line_len = 0;
            }
            _ => {
                if byte >= 0x20 && byte < 0x7f && line_len < LINE_BUF_SIZE - 1 {
                    line_buf[line_len] = byte;
                    line_len += 1;
                    let s = [byte];
                    put_str(core::str::from_utf8(&s).unwrap_or("?"));
                }
            }
        }
    }
}

fn execute(line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }

    let (cmd, rest) = split_first_word(line);

    match cmd {
        "echo" => {
            put_str(rest);
            put_str("\r\n");
        }
        "add" => {
            let (a_str, b_str) = split_first_word(rest);
            let a = match parse_i32(a_str) {
                Some(v) => v,
                None => {
                    put_str("error: invalid first operand\r\n");
                    return;
                }
            };
            let b = match parse_i32(b_str) {
                Some(v) => v,
                None => {
                    put_str("error: invalid second operand\r\n");
                    return;
                }
            };
            uprintln!("{} + {} = {}\r\n", a, b, a.wrapping_add(b));
        }
        "help" => {
            put_str("Commands:\r\n");
            put_str("  echo <text>  - echo text back\r\n");
            put_str("  add <a> <b>  - add two integers\r\n");
            put_str("  help         - show this help\r\n");
            put_str("  clear        - clear screen\r\n");
        }
        "clear" => {
            put_str("\x1b[2J\x1b[H");
        }
        _ => {
            put_str("unknown command: ");
            put_str(cmd);
            put_str("\r\n");
        }
    }
}

fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}

fn parse_i32(s: &str) -> Option<i32> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (positive, digits) = if s.starts_with('-') {
        (false, &s[1..])
    } else if s.starts_with('+') {
        (true, &s[1..])
    } else {
        (true, s)
    };
    if digits.is_empty() {
        return None;
    }
    let mut result: i32 = 0;
    for &b in digits.as_bytes() {
        if !(b >= b'0' && b <= b'9') {
            return None;
        }
        result = result.checked_mul(10)?.checked_add((b - b'0') as i32)?;
    }
    Some(if positive { result } else { -result })
}

fn put_str(s: &str) {
    platform::console().write(s.as_bytes());
}

#[executor::main(info)]
fn main(spawner: Pin<&'static Spawner<4>>) {
    log::info!("rt-async-amp: hart 1 console started (interrupt-driven)");

    spawner.spawn(Priority::new(2), task_ipc().unwrap());
    spawner.spawn(Priority::new(1), task_console().unwrap());

    log::info!("rt-async-amp: tasks spawned, entering scheduler");
}

#[executor::interrupt]
fn MachineSoft(_tf: &mut TrapFrame) {
    rt_async_app::ipc_wait::notify_from_isr();
}

#[executor::interrupt]
fn MachineTimer(_tf: &mut TrapFrame) {
    futures::timer::handle_timer_isr();
}

#[executor::interrupt]
fn MachineExternal(_tf: &mut TrapFrame) {
    platform::dispatch_external();
}
