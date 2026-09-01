use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::IntoRawFd;

use ov_channels::{ChannelId, Message, MsgType, SharedMemory};

#[allow(dead_code)]
const RT_SHM_IOC_NOTIFY: libc::c_ulong = rtshm_abi::IOC_NOTIFY as libc::c_ulong;
const RT_SHM_IOC_AWAIT: libc::c_ulong = rtshm_abi::IOC_AWAIT as libc::c_ulong;
const RT_SHM_IOC_CLR_PENDING: libc::c_ulong = rtshm_abi::IOC_CLR_PENDING as libc::c_ulong;

// K3 共享窗大小（真源 its/rt-async-k3.dts + tgoskits AP dts，值 0x19000）。
// 必须 ≥ ov-channels SharedMemory::<3> footprint 0x18700——ch1.magic 在
// +0x8300、ch2.magic 在 +0x10500，mmap 小于全窗时用户态访问会越界。
const SHM_SIZE: usize = rtshm_abi::K3_SHM_SIZE;

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

    fn shm(&self) -> &SharedMemory<3> {
        unsafe { &*(self.ptr as *const SharedMemory<3>) }
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
    let count = std::env::args()
        .nth(1)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(3);

    println!("[test_ipc] opening /dev/rt_shm...");
    let rt = RtShm::open().expect("failed to open /dev/rt_shm");
    rt.clear_pending().expect("CLR_PENDING failed");
    println!("[test_ipc] opened fd={}", rt.fd);

    let shm = rt.shm();
    let timeout = std::time::Duration::from_secs(5);
    let start = std::time::Instant::now();
    let mut ticks: u32 = 0;
    while !shm.is_valid() {
        if start.elapsed() > timeout {
            panic!(
                "shared memory not initialized after {:?} -- is rt-async running?",
                timeout
            );
        }
        // （原"每 100ms 经 NOTIFY 内核 CBO 同步点刷新视图"已随 PMA 非缓存
        // 窗口撤除——裸轮询即 SRAM 真值；这里保留门铃节奏仅为让 RP 至多
        // 空醒一轮的自愈路径，无缓存语义。）
        if ticks % 10 == 0 {
            let _ = rt.notify();
        }
        ticks += 1;
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    println!("[test_ipc] shm valid");

    let ch0 = ChannelId::new(0);
    let ch1 = ChannelId::new(1);

    // 清理上一轮运行残留：异常退出（Ctrl-C/断言失败）会在 ch1 留下未消费
    // 回包，不清会让本轮第一条 recv 拿到旧消息、回显断言失败。
    {
        let rx = shm.receiver(ch1).unwrap();
        let mut stale = 0;
        while let Some(m) = rx.try_recv() {
            stale += 1;
            println!("[test_ipc] drained stale ch1 msg: type={:?}", m.ty());
        }
        if stale > 0 {
            println!("[test_ipc] drained {stale} stale message(s) before start");
        }
    }

    for i in 0..count {
        println!("\n=== round {} ===", i + 1);

        let tx = shm.sender(ch0).unwrap();
        let rx = shm.receiver(ch1).unwrap();

        let msg = Message::notification(100 + i as u32);
        println!("[test_ipc] sending notification id={} via ch0...", 100 + i);
        tx.try_send(&msg).expect("ch0 send failed");

        println!("[test_ipc] ioctl NOTIFY (IPI to rt-async)...");
        rt.notify().expect("NOTIFY failed");

        println!("[test_ipc] ioctl AWAIT (mailbox IRQ + 窗口同步)...");
        // mailbox IRQ 已实证送达（内核 count 打印）；AWAIT 返回 = 内核经 NC
        // 别名确认 ch1 真有数据，且返回时已 clean+invalidate 窗口——随后的
        // 读必然取到 SRAM 真值（此前轮询读空是用户态映射实际 cacheable 吃了
        // 陈旧行，不是 RP 没回包）。
        rt.await_ipi().expect("AWAIT failed");
        println!("[test_ipc] AWAIT returned");

        // 回显断言：AWAIT 返回后首条应为本轮通知的回显（id 匹配）。若 ch1
        // 先排入了更早轮次的滞留消息（drain 由 ADD 轮负责），这里报出来。
        match rx.try_recv() {
            Some(resp) => match resp.ty() {
                Some(MsgType::Notification) => {
                    if let Some(id) = resp.as_notification() {
                        assert_eq!(id, 100 + i as u32, "notification echo id mismatch");
                        println!("[test_ipc] received notification from ch1: id={id} ✓");
                    }
                }
                Some(other) => panic!(
                    "expected notification echo, got {other:?} (rid={:?})",
                    resp.request_id()
                ),
                None => panic!("msg type empty"),
            },
            None => panic!("ch1 empty after AWAIT（has_pending 判定与读不一致）"),
        }

        let a = i as i32 * 10;
        let b = i as i32 * 7 + 3;
        let rid = 2000u64 + i as u64;
        // method_id 必须带 NOTIFY_FLAG（bit 63，ov-rpc 协议约定）：带标志的
        // 请求服务端回包后会发 IPI 门铃（HandledKind::Notify → on_notify）；
        // 不带则是 Quiet 回包——数据进 ch1 但不发门铃，AWAIT 只能靠回包
        // 恰好赶在首次就绪检查前落好（poll1-luck），赶不上即永久挂死。
        // （板上实锤：缺标志时 AWAIT 时过时挂；RP 侧 ADD 正常处理、数据
        // 正常落 ch1，AP 侧 mailbox IRQ 却不增——门铃根本没发。）
        let method = 2u64 | ov_rpc::NOTIFY_FLAG; // 2 = RtAsyncRpc::ADD
        let req = Message::request(rid, method, &(a, b)).expect("request serialize failed");
        println!("[test_ipc] sending ADD request({}, {}) via ch0...", a, b);
        tx.try_send(&req).expect("ch0 send failed");

        println!("[test_ipc] ioctl NOTIFY...");
        rt.notify().expect("NOTIFY failed");

        // AWAIT 返回 = 回包已落 SRAM 且窗口已同步；排空 ch1 中先于本请求
        // 的杂散消息（上轮残留/通知回显），直到取到本 rid 的 Response。
        println!("[test_ipc] ioctl AWAIT...");
        rt.await_ipi().expect("AWAIT failed");
        let resp = loop {
            match rx.try_recv() {
                Some(r) if r.ty() == Some(MsgType::Response) => break r,
                Some(other) => {
                    println!("[test_ipc] drain 杂散消息 type={:?}", other.ty());
                }
                None => {
                    // 杂散消息排空后本 rid 回包尚未到达：经内核 AWAIT 等下一个
                    // 门铃（返回自带全窗 invalidate）——用户态映射实际
                    // cacheable，裸轮询 try_recv 只会反复命中陈旧缓存行。
                    // 本请求带 NOTIFY_FLAG，RP 回包后必发门铃。
                    rt.await_ipi().expect("AWAIT failed");
                }
            }
        };
        assert_eq!(resp.ty(), Some(MsgType::Response), "ADD: expected Response, got {:?}", resp.ty());
        let (resp_rid, result) = resp.as_response::<i32>().expect("ADD: malformed response");
        assert_eq!(resp_rid, rid, "ADD: wrong request ID");
        assert_eq!(result, a.wrapping_add(b), "ADD: wrong result");
        println!("[test_ipc] ADD response OK: rid={} result={}", resp_rid, result);
    }

    println!("\n[test_ipc] done");
}
