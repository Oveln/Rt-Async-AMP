use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::IntoRawFd;

use ov_channels::ChannelId;
use ov_rpc::{define_service, RpcClient};

const RT_SHM_IOC_NOTIFY: libc::c_ulong = 0x7350_01;
const RT_SHM_IOC_AWAIT: libc::c_ulong = 0x7350_02;
const SHM_SIZE: usize = 67_072;

define_service! {
    RtAsyncRpc {
        ECHO: 0 => fn echo(val: u32) -> u32;
        ADD: 1 => fn add(a: i32, b: i32) -> i32;
    }
}

impl RtAsyncRpc {
    fn echo(val: u32) -> u32 {
        val
    }
    fn add(a: i32, b: i32) -> i32 {
        a.wrapping_add(b)
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
    let count = std::env::args()
        .nth(1)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(3);

    println!("[test_rpc] opening /dev/rt_shm...");
    let rt = RtShm::open().expect("failed to open /dev/rt_shm");

    let client = RpcClient::new(rt.shm_addr(), ChannelId::new(0), ChannelId::new(1));

    for i in 0..count {
        println!("\n=== round {} ===", i + 1);

        let val = 42 + i as u32;
        print!("[test_rpc] ECHO({}) ... ", val);
        let rid = client
            .call_async(RtAsyncRpc::ECHO, &val)
            .expect("ECHO send failed");
        rt.notify().expect("NOTIFY failed");
        rt.await_ipi().expect("AWAIT failed");
        let result: u32 = client.wait_response(rid).expect("ECHO no response");
        assert_eq!(result, val);
        println!("= {} OK", result);

        let a = i as i32 * 10;
        let b = i as i32 * 7 + 3;
        let expected = a.wrapping_add(b);
        print!("[test_rpc] ADD({}, {}) ... ", a, b);
        let rid = client
            .call_async(RtAsyncRpc::ADD, &(a, b))
            .expect("ADD send failed");
        rt.notify().expect("NOTIFY failed");
        rt.await_ipi().expect("AWAIT failed");
        let result: i32 = client.wait_response(rid).expect("ADD no response");
        assert_eq!(result, expected);
        println!("= {} OK", result);
    }

    println!("\n[test_rpc] all {} rounds passed", count);
}
