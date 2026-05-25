use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::IntoRawFd;

use ov_channal::{ChannelId, Message, MsgType, SharedMemory};

const RT_SHM_IOC_NOTIFY: libc::c_ulong = 0x7350_01;
const RT_SHM_IOC_AWAIT: libc::c_ulong = 0x7350_02;

const SHM_SIZE: usize = 67_072;

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

    fn shm(&self) -> &SharedMemory {
        unsafe { &*(self.ptr as *const SharedMemory) }
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

    println!("[test_ipc] opening /dev/rt_shm...");
    let rt = RtShm::open().expect("failed to open /dev/rt_shm");
    println!("[test_ipc] opened fd={}", rt.fd);

    let shm = rt.shm();
    assert!(shm.is_valid(), "shared memory invalid");
    println!("[test_ipc] shm valid");

    let ch0 = ChannelId::new(0);
    let ch1 = ChannelId::new(1);

    for i in 0..count {
        println!("\n=== round {} ===", i + 1);

        let tx = shm.sender(ch0).unwrap();
        let rx = shm.receiver(ch1).unwrap();

        let msg = Message::notification(100 + i as u32);
        println!("[test_ipc] sending notification id={} via ch0...", 100 + i);
        tx.try_send(&msg).expect("ch0 send failed");

        println!("[test_ipc] ioctl NOTIFY (IPI to rt-async)...");
        rt.notify().expect("NOTIFY failed");

        std::thread::sleep(std::time::Duration::from_millis(100));

        println!("[test_ipc] ioctl AWAIT (wait for rt-async reply)...");
        rt.await_ipi().expect("AWAIT failed");
        println!("[test_ipc] AWAIT returned (got IPI from rt-async)");

        if let Some(resp) = rx.try_recv() {
            match resp.ty() {
                Some(MsgType::Notification) => {
                    if let Some(id) = resp.as_notification() {
                        println!("[test_ipc] received notification from ch1: id={}", id);
                    }
                }
                Some(MsgType::Response) => {
                    let rid = resp.request_id().unwrap();
                    if let Some((_, result)) = resp.as_response::<i32>() {
                        println!("[test_ipc] received response: rid={} result={}", rid, result);
                    }
                }
                _ => {
                    println!("[test_ipc] received msg type={:?} from ch1", resp.ty());
                }
            }
        } else {
            println!("[test_ipc] ch1 empty (no reply from rt-async)");
        }

        let a = i as i32 * 10;
        let b = i as i32 * 7 + 3;
        let rid = 2000u64 + i as u64;
        let req = Message::request(rid, 1, &(a, b)).expect("request serialize failed");
        println!("[test_ipc] sending ADD request({}, {}) via ch0...", a, b);
        tx.try_send(&req).expect("ch0 send failed");

        println!("[test_ipc] ioctl NOTIFY...");
        rt.notify().expect("NOTIFY failed");

        std::thread::sleep(std::time::Duration::from_millis(100));

        println!("[test_ipc] ioctl AWAIT...");
        rt.await_ipi().expect("AWAIT failed");

        if let Some(resp) = rx.try_recv() {
            if resp.ty() == Some(MsgType::Response) {
                if let Some((rid, result)) = resp.as_response::<i32>() {
                    println!("[test_ipc] received response: rid={} result={}", rid, result);
                }
            } else {
                println!("[test_ipc] received msg type={:?} from ch1", resp.ty());
            }
        } else {
            println!("[test_ipc] ch1 empty (no reply)");
        }
    }

    println!("\n[test_ipc] done");
}
