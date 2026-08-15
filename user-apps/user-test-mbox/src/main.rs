//! AP 端 mailbox 中断链路自测（无需 rt-async 对端）。
//!
//! 打开 `/dev/rt_shm` 并 ioctl `TEST_MBOX`：内核侧向 mailbox 本地 user
//! 软件注入一条 new_msg，验证 mailbox → APLIC → IMSIC → CPU → IRQ handler
//! 全链路贯通。ioctl 返回 handler 触发次数。
//!
//! 用途：在真板上确认 StarryOS 侧 mailbox4 驱动可用（不依赖 rt-async
//! 是否已运行），是双系统通信联调的前置验证。

use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::IntoRawFd;

// rtshm-abi 与 StarryOS kernel 的 RT_SHM_IOC_TEST_MBOX 保持一致。
const RT_SHM_IOC_TEST_MBOX: libc::c_ulong = rtshm_abi::IOC_TEST_MBOX as libc::c_ulong;

fn do_ioctl(fd: libc::c_int, cmd: libc::c_ulong, arg: libc::c_ulong) -> io::Result<libc::c_int> {
    let ret = unsafe { libc::ioctl(fd, cmd as _, arg) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

fn main() {
    let rounds = std::env::args()
        .nth(1)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1);

    println!("[test_mbox] opening /dev/rt_shm...");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/rt_shm")
        .expect("failed to open /dev/rt_shm");
    let fd = file.into_raw_fd();
    println!("[test_mbox] opened fd={}", fd);

    for i in 0..rounds {
        println!("\n=== round {} ===", i + 1);
        match do_ioctl(fd, RT_SHM_IOC_TEST_MBOX, 0) {
            Ok(n) => println!(
                "[test_mbox] TEST_MBOX OK: IRQ handler fired {} time(s) (mailbox→APLIC→CPU→handler 链路贯通)",
                n
            ),
            Err(e) => {
                eprintln!(
                    "[test_mbox] TEST_MBOX FAILED: {} (中断未触发——检查 mailbox/APLIC 中断链路)",
                    e
                );
                std::process::exit(1);
            }
        }
    }

    unsafe { libc::close(fd) };
    println!("\n[test_mbox] done");
}
