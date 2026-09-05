//! rtsh 库层 —— /dev/rt_shm 客户端 + ov-rpc Shell（服务发现 / 按名调用）
//! + 机器人语义方法。
//!
//! 本 crate 既是唯一用户态程序 rtsh（`src/main.rs` REPL / 单发）的实现，
//! 也作为库供 `user-apps/robot-py`（PyO3 原生 Python 扩展）绑定——机器人
//! 控制的用户态入口收敛为「rtsh 一个程序 + robot 一个 Python 库」。
//!
//! ## 等待与超时
//!
//! RPC 等响应的阻塞等待经 **ppoll(/dev/rt_shm, timeout)** 实现：rt_shm 驱动
//! 实现 `Pollable`（ch1 有数据时 level-triggered POLLIN，内核侧 register 后
//! 复查防丢唤醒），ppoll 自带毫秒级超时——全程无信号参与（此前版本用
//! SIGALRM 打断 AWAIT ioctl，与 Python signal 模块冲突，已撤除）。
//!
//! ## 并发约束
//!
//! 单进程内一次只有一个线程使用 [`Shell`]（rt_shm 驱动的 IPC waker 为单
//! 槽，且 rid 匹配无锁表）。`Shell: Send` 允许跨线程移动，非 `Sync`。

use std::cell::RefCell;
use std::io;
use std::os::unix::io::IntoRawFd;
use std::time::{Duration, Instant};

use ov_channels::{ChannelId, SharedMemory};

const RT_SHM_IOC_NOTIFY: libc::c_ulong = rtshm_abi::IOC_NOTIFY as libc::c_ulong;
#[allow(dead_code)]
const RT_SHM_IOC_CLR_PENDING: libc::c_ulong = rtshm_abi::IOC_CLR_PENDING as libc::c_ulong;
const SHM_SIZE: usize = rtshm_abi::K3_SHM_SIZE;

/// 普通 call 响应超时（毫秒）。
pub const T_CALL_MS: u64 = 3_000;
/// 慢命令超时（毫秒）：底盘 INIT（等 ACK）/ membench（部分 op 忙等 >1s）。
pub const T_SLOW_MS: u64 = 10_000;
/// 抓取全序列超时（毫秒，~4.5s 动作 + 帧时间）。
pub const T_GRAB_MS: u64 = 15_000;

// 方法名与 RP 固件 intercom.rs define_service! 的常量名对齐（ECHO/ADD/
// …/ARM_RELEASE）；mid 运行时经服务发现按名字解析（见 Shell::discover），
// 编号无需镜像。acall 在客户端按 call 发起（响应由完成方补发）。

// ============================================================================
// /dev/rt_shm 封装（open + mmap + ioctl + ppoll 等待 + 就绪等待/排空）
// ============================================================================

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
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/rt_shm")?;
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

    #[allow(dead_code)]
    fn clear_pending(&self) -> io::Result<()> {
        do_ioctl(self.fd, RT_SHM_IOC_CLR_PENDING, 0)?;
        Ok(())
    }

    /// ppoll 等待 ch1 门铃数据（POLLIN），带超时；EINTR 视为空唤醒返回，
    /// 由调用方按剩余 deadline 重等。超时到点不报错（返回 Ok，revents 为
    /// 空），调用方以 deadline 判超时。
    fn wait_inet(&self, timeout: Duration) -> io::Result<()> {
        let ts = libc::timespec {
            tv_sec: timeout.as_secs() as _,
            tv_nsec: timeout.subsec_nanos() as libc::c_long,
        };
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: pfd/ts 为本栈对象；ppoll 不修改 sigmask（null）。
        let r = unsafe { libc::ppoll(&mut pfd, 1, &ts, std::ptr::null()) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(());
            }
            return Err(e);
        }
        Ok(())
    }

    /// 轮询共享窗 magic 就绪（5s 超时；RP 固件未起/未 init 时 false）。
    fn wait_valid(&self) -> bool {
        let shm = unsafe { SharedMemory::<3>::at(self.shm_addr()) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if shm.is_valid() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// 排空 ch1 残留响应（上一次异常退出的在途响应）。
    fn drain_ch1(&self) {
        let shm = unsafe { SharedMemory::<3>::at(self.shm_addr()) };
        if let Ok(rx) = shm.receiver(ChannelId::new(1)) {
            while rx.try_recv().is_some() {}
        }
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

// ============================================================================
// shell 客户端（服务发现 + 按名 call/send + ppoll 超时等待）
// ============================================================================

/// 发现表条目：(mid, flags, 名称)——flags 见 ov_rpc::descriptor。
pub struct Discovered {
    pub mid: u64,
    pub flags: u8,
    pub name: String,
}

/// rt-async shell 客户端：INIT 服务发现一次，命令按方法名解析 mid。
pub struct Shell {
    rt: RtShm,
    client: ov_rpc::RpcClient,
    /// 服务发现结果（open 后 INIT 一次；`Shell::discover` 可刷新）。
    pub table: Vec<Discovered>,
}

// SAFETY: Shell 仅持有 fd 与 mmap 地址（无线程亲和状态、无 GIL/线程本地
// 依赖），允许跨线程移动；不支持多线程同时使用（驱动 IPC waker 单槽，
// 见模块文档并发约束），故不实现 Sync。
unsafe impl Send for Shell {}

/// 发现表中的机器人方法名（与 RP 固件 intercom.rs 对齐；mid 由发现解析）。
pub mod names {
    pub const UART_STATUS: &str = "UART_STATUS";
    pub const CHASSIS_SET_SPEED: &str = "CHASSIS_SET_SPEED";
    pub const CHASSIS_STOP: &str = "CHASSIS_STOP";
    pub const CHASSIS_GET: &str = "CHASSIS_GET";
    pub const CHASSIS_INIT: &str = "CHASSIS_INIT";
    pub const ARM_SET_ANGLE: &str = "ARM_SET_ANGLE";
    pub const ARM_TORQUE: &str = "ARM_TORQUE";
    pub const ARM_GRAB: &str = "ARM_GRAB";
    pub const ARM_RELEASE: &str = "ARM_RELEASE";
    pub const UART_WRITE: &str = "UART_WRITE";
    pub const UART_READ: &str = "UART_READ";
}

impl Shell {
    /// 打开 /dev/rt_shm 并等共享窗就绪（RP 固件未起/未 init 时 Err）。
    /// 调用 [`Shell::discover`] 后方法表才可用。
    pub fn open() -> Result<Self, String> {
        let rt = RtShm::open().map_err(|e| format!("open /dev/rt_shm: {e}"))?;
        if !rt.wait_valid() {
            return Err("shared window not ready (RP firmware down?)".into());
        }
        rt.drain_ch1();
        let client = ov_rpc::RpcClient::new(rt.shm_addr());
        Ok(Self { rt, client, table: Vec::new() })
    }

    /// 服务发现：INIT（method 0）→ 描述符解析 → 刷新方法表。
    ///
    /// 旧固件（无 INIT 拦截）下收到 poison 响应（描述符解不开）或版本门
    /// 直接拒绝发送——均为明确报错而非挂死。
    pub fn discover(&mut self) -> Result<(), String> {
        let rid = self
            .client
            .discover(|| {
                let _ = self.rt.notify();
            })
            .map_err(|e| format!("INIT 发送失败: {e:?}"))?;

        let raw = self.wait_raw(rid, T_CALL_MS, "INIT 响应")?;
        let d = ov_rpc::descriptor::parse(&raw)
            .ok_or_else(|| "描述符解析失败（旧固件无服务发现？）".to_string())?;
        self.table = d
            .methods()
            .map(|m| Discovered { mid: m.mid, flags: m.flags, name: m.name.to_string() })
            .collect();
        Ok(())
    }

    /// 方法表渲染（启动横幅与 `services` 命令共用）。
    pub fn services_text(&self) -> String {
        let mut lines = vec![format!(
            "可用 RPC 方法：{} 个（mid 0 = INIT 服务发现）",
            self.table.len()
        )];
        lines.push("  mid  形态    名称".into());
        for m in &self.table {
            let kind = ov_rpc::descriptor::MethodDesc { mid: m.mid, flags: m.flags, name: &m.name }
                .kind_name();
            lines.push(format!("  {:>3}  {:<6}  {}", m.mid, kind, m.name));
        }
        lines.join("\n")
    }

    /// 按方法名解析 mid（发现表查找；未注册 → Err 快速失败）。
    pub fn mid(&self, name: &str) -> Result<u64, String> {
        self.table
            .iter()
            .find(|m| m.name == name)
            .map(|m| m.mid)
            .ok_or_else(|| format!("固件未提供方法 {name}（services 查看方法表）"))
    }

    /// 双向调用（call：服务端回 IPI）。NOTIFY 失败记入返回 Err。
    pub fn call<A: serde::Serialize>(
        &mut self,
        name: &str,
        args: &A,
    ) -> Result<ov_rpc::RequestId, String> {
        let mid = self.mid(name)?;
        let nerr = RefCell::new(None);
        let r = self
            .client
            .call(mid, args, || {
                if let Err(e) = self.rt.notify() {
                    if nerr.borrow().is_none() { *nerr.borrow_mut() = Some(format!("notify: {e}")); }
                }
            })
            .map_err(|e| format!("send: {e:?}"));
        if let Some(e) = nerr.into_inner() {
            return Err(e);
        }
        r
    }

    /// 单向下发（send：不期待响应）。NOTIFY 失败记入返回 Err。
    pub fn send<A: serde::Serialize>(&mut self, name: &str, args: &A) -> Result<(), String> {
        let mid = self.mid(name)?;
        let nerr = RefCell::new(None);
        let r = self
            .client
            .send(mid, args, || {
                if let Err(e) = self.rt.notify() {
                    if nerr.borrow().is_none() { *nerr.borrow_mut() = Some(format!("notify: {e}")); }
                }
            })
            .map_err(|e| format!("send: {e:?}"));
        if let Some(e) = nerr.into_inner() {
            return Err(e);
        }
        r
    }

    /// 阻塞等响应并按 rid 取回（ppoll 超时；空唤醒有限次重等）。
    pub fn wait_reply<T: serde::de::DeserializeOwned>(
        &mut self,
        rid: ov_rpc::RequestId,
        timeout_ms: u64,
    ) -> Result<T, String> {
        let raw = self.wait_raw(rid, timeout_ms, "响应")?;
        postcard::from_bytes(&raw).map_err(|e| format!("recv: {e:?}"))
    }

    /// 等待原始响应字节（discover 的描述符等非 postcard 载荷用）。
    fn wait_raw(
        &mut self,
        rid: ov_rpc::RequestId,
        timeout_ms: u64,
        what: &str,
    ) -> Result<Vec<u8>, String> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut spins: u32 = 0;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(format!("等{what}超时（{timeout_ms}ms）"));
            };
            self.rt.wait_inet(remaining).map_err(|e| format!("ppoll: {e}"))?;
            self.client.poll_responses();
            match self.client.recv_raw_for(rid) {
                Some(bytes) => return Ok(bytes.to_vec()),
                None => {
                    // 空唤醒（他源门铃/杂散）：有限次重等后放弃。
                    spins += 1;
                    if spins > 64 {
                        return Err("no response (spins exhausted)".into());
                    }
                }
            }
        }
    }

    // ── 机器人语义方法（REPL 与 robot-py 共用；方法名经发现表解析）──

    /// `UART_STATUS`：端口 probe 掩码 / 底盘状态 / 臂队列丢弃数。
    pub fn robot_status(&mut self) -> Result<Status, String> {
        let rid = self.call(names::UART_STATUS, &(1u32,))?;
        let v: (u32, u32, u32, u32, u32) = self.wait_reply(rid, T_CALL_MS)?;
        Ok(Status {
            ports: v.1,
            chassis_inited: v.2 != 0,
            chassis_err: v.3,
            arm_dropped: v.4,
        })
    }

    /// `CHASSIS_INIT`（acall）：底盘 INIT+CONFIG，等 ACK。忙时 Err。
    pub fn robot_init(&mut self, ppr: u16, pwm_freq: u16) -> Result<u32, String> {
        let rid = self.call(names::CHASSIS_INIT, &(ppr, pwm_freq))?;
        let r: u32 = self.wait_reply(rid, T_SLOW_MS)?;
        if r == u32::MAX {
            return Err("busy（已有 init 在途）".into());
        }
        Ok(r)
    }

    /// `CHASSIS_SET_SPEED`：双轮速度 ±100（send；越界截到 ±100）。
    pub fn robot_set_speed(&mut self, left: i16, right: i16) -> Result<(), String> {
        self.send(names::CHASSIS_SET_SPEED, &(left.clamp(-100, 100), right.clamp(-100, 100)))
    }

    /// `CHASSIS_STOP`：brake=true 刹车 / false 滑行。
    pub fn robot_stop(&mut self, brake: bool) -> Result<(), String> {
        self.send(names::CHASSIS_STOP, &(if brake { 2u8 } else { 1u8 },))
    }

    /// `CHASSIS_GET`：遥测快照（陈旧度 ≤100ms）。
    pub fn robot_get(&mut self) -> Result<Telemetry, String> {
        let rid = self.call(names::CHASSIS_GET, &(1u32,))?;
        let v: (u32, u32, i32, i32, i32, i32, u32, u64) = self.wait_reply(rid, T_CALL_MS)?;
        Ok(Telemetry {
            inited: v.1 != 0,
            rpm_left: v.2,
            rpm_right: v.3,
            enc_m1: v.4,
            enc_m2: v.5,
            err: v.6,
            last_ms: v.7,
        })
    }

    /// `ARM_SET_ANGLE`：单舵机角度（send）。
    pub fn robot_set_angle(&mut self, servo: u8, angle: u16) -> Result<(), String> {
        self.send(names::ARM_SET_ANGLE, &(servo, angle))
    }

    /// `ARM_TORQUE`：力矩释放（true）/ 恢复（false）。
    pub fn robot_torque(&mut self, release: bool) -> Result<(), String> {
        self.send(names::ARM_TORQUE, &(release as u8,))
    }

    /// `ARM_GRAB`（acall）：完整抓取序列（约 4.5s，完成才返回）。队满 Err。
    pub fn robot_grab(&mut self) -> Result<u32, String> {
        let rid = self.call(names::ARM_GRAB, &(1u32,))?;
        let r: u32 = self.wait_reply(rid, T_GRAB_MS)?;
        if r == u32::MAX {
            return Err("arm queue full".into());
        }
        Ok(r)
    }

    /// `ARM_RELEASE`（acall）：张开夹爪（约 0.5s）。
    pub fn robot_release(&mut self) -> Result<u32, String> {
        let rid = self.call(names::ARM_RELEASE, &(1u32,))?;
        let r: u32 = self.wait_reply(rid, T_SLOW_MS)?;
        if r == u32::MAX {
            return Err("arm queue full".into());
        }
        Ok(r)
    }

    /// `UART_WRITE`：raw 写（bring-up 诊断；data ≤32 字节）。端口未 probe Err。
    pub fn robot_uwrite(&mut self, port: u8, data: &[u8]) -> Result<u32, String> {
        if data.len() > 32 {
            return Err("hex 过长（≤32 字节）".into());
        }
        let mut buf = [0u8; 32];
        buf[..data.len()].copy_from_slice(data);
        let rid = self.call(names::UART_WRITE, &(port, data.len() as u8, buf))?;
        let n: u32 = self.wait_reply(rid, T_CALL_MS)?;
        if n == u32::MAX {
            return Err("port not probed".into());
        }
        Ok(n)
    }

    /// `UART_READ`：raw 读（清空当前 RX）。成功返回读到的字节。
    pub fn robot_uread(&mut self, port: u8, max: u8) -> Result<Vec<u8>, String> {
        let rid = self.call(names::UART_READ, &(port, max))?;
        let (n, data): (u32, [u8; 32]) = self.wait_reply(rid, T_CALL_MS)?;
        if n == u32::MAX {
            return Err("port not probed".into());
        }
        Ok(data[..(n as usize).min(32)].to_vec())
    }
}

/// `UART_STATUS` 结果（端口 probe 掩码 / 底盘状态 / 臂队列丢弃数）。
#[derive(Clone, Copy, Debug)]
pub struct Status {
    pub ports: u32,
    pub chassis_inited: bool,
    pub chassis_err: u32,
    pub arm_dropped: u32,
}

/// `CHASSIS_GET` 遥测快照（陈旧度 ≤100ms）。
#[derive(Clone, Copy, Debug)]
pub struct Telemetry {
    pub inited: bool,
    pub rpm_left: i32,
    pub rpm_right: i32,
    pub enc_m1: i32,
    pub enc_m2: i32,
    pub err: u32,
    pub last_ms: u64,
}
