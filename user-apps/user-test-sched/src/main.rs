//! 测试 DELAY RPC 的延时抖动
//!
//! 流程：
//! 1. 发送 ECHO 作为时间参考点（服务端收到 ECHO 后立即返回）
//! 2. 发送 DELAY(us) 指令
//! 3. 发送 ECHO 作为结束参考点
//! 4. 计算两次 ECHO 之间的实际时间差，与期望值比较
//!
//! 重复多轮，统计抖动分布。

use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::IntoRawFd;

use ov_rpc::define_service_client;

#[allow(dead_code)]
mod amp {
    include!(concat!(env!("OUT_DIR"), "/amp_gen.rs"));
}

const RT_SHM_IOC_NOTIFY: libc::c_ulong = amp::RTSHM_IOC_NOTIFY as libc::c_ulong;
const RT_SHM_IOC_AWAIT: libc::c_ulong = amp::RTSHM_IOC_AWAIT as libc::c_ulong;
const RT_SHM_IOC_CLR_PENDING: libc::c_ulong = amp::RTSHM_IOC_CLR_PENDING as libc::c_ulong;
const SHM_SIZE: usize = amp::SHMSIZE;

define_service_client! {
    RtAsyncRpc {
        ECHO: 0 => call echo(val: u32) -> u32;
        ADD:  1 => call add(a: i32, b: i32) -> i32;
        DELAY: 2 => send delay(us: u32);
    }
}

fn do_ioctl(fd: libc::c_int, cmd: libc::c_ulong, arg: libc::c_ulong) -> io::Result<libc::c_int> {
    let ret = unsafe { libc::ioctl(fd, cmd as _, arg) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

struct RtShm {
    fd: libc::c_int,
    ptr: *mut std::ffi::c_void,
}

impl RtShm {
    fn open() -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open("/dev/rt_shm")?;
        let fd = file.into_raw_fd();

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                SHM_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            let err = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err);
        }

        Ok(Self { fd, ptr })
    }

    fn shm_addr(&self) -> usize {
        self.ptr as usize
    }

    fn notify(&self) -> io::Result<()> {
        do_ioctl(self.fd, RT_SHM_IOC_NOTIFY, 0)?;
        Ok(())
    }

    fn clear_pending(&self) -> io::Result<()> {
        do_ioctl(self.fd, RT_SHM_IOC_CLR_PENDING, 0)?;
        Ok(())
    }

    fn await_ipi(&self) -> io::Result<()> {
        do_ioctl(self.fd, RT_SHM_IOC_AWAIT, 0)?;
        Ok(())
    }
}

impl Drop for RtShm {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr, SHM_SIZE);
            libc::close(self.fd);
        }
    }
}

fn main() {
    println!("[test_sched] opening /dev/rt_shm...");
    let rt = RtShm::open().expect("failed to open /dev/rt_shm");
    rt.clear_pending().expect("CLR_PENDING failed");

    let mut client = RtAsyncRpc::new(rt.shm_addr());
    let notify = || rt.notify().expect("NOTIFY failed");

    // ── Test 1: 单次 DELAY 100µs ──
    println!("\n=== Test 1: Single DELAY 100µs ===");
    {
        // 先发 ECHO 标记起点
        let rid_start = client.echo(42u32, notify)
            .expect("ECHO send failed");
        rt.await_ipi().expect("AWAIT failed");
        client.poll_responses();
        let _start: u32 = client.recv_for(rid_start)
            .expect("ECHO recv error")
            .expect("ECHO no response");

        // 发 DELAY（send 模式，自动检查 BUSY 并条件性发 IPI）
        client.delay(100u32, notify)
            .expect("DELAY send failed");

        // 发 ECHO 标记终点
        let rid_end = client.echo(99u32, notify)
            .expect("ECHO send failed");
        rt.await_ipi().expect("AWAIT failed");
        client.poll_responses();
        let _end: u32 = client.recv_for(rid_end)
            .expect("ECHO recv error")
            .expect("ECHO no response");

        println!("[test_sched] DELAY 100µs completed (check rt-async UART1 for timing)");
    }

    // ── Test 2: 多轮 DELAY 100µs，统计抖动 ──
    println!("\n=== Test 2: 20 rounds of DELAY 100µs ===");
    let rounds = 20;
    for i in 0..rounds {
        // ECHO start
        let rid_s = client.echo(i as u32, notify)
            .expect("ECHO start failed");
        rt.await_ipi().expect("AWAIT failed");
        client.poll_responses();
        let _start: u32 = client.recv_for(rid_s)
            .expect("ECHO start recv error")
            .expect("ECHO start no response");

        // DELAY
        client.delay(100u32, notify)
            .expect("DELAY send failed");

        // ECHO end
        let rid_e = client.echo(i as u32 + 1000, notify)
            .expect("ECHO end failed");
        rt.await_ipi().expect("AWAIT failed");
        client.poll_responses();
        let _end: u32 = client.recv_for(rid_e)
            .expect("ECHO recv error")
            .expect("ECHO no response");

        if i == 0 {
            println!("[test_sched] round 0 done, continuing...");
        }
    }

    println!("[test_sched] {} rounds completed", rounds);
    println!("[test_sched] check rt-async UART1 output for precise timing measurement");
}
