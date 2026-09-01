//! robot-ctl —— AP 用户态机器人控制入口（AKA-00 底盘 + 机械臂）。
//!
//! 配对 RP 固件：`k3-robot-ctrl`（apps/rt-async-k3 robot_ctrl bin）。协议在
//! RP 侧（robot 模块），本程序只发语义 RPC（ov-rpc 经 /dev/rt_shm 共享窗 +
//! mailbox IPI，见 user-test-rpc 的链路说明）。
//!
//! ## 两种运行模式
//!
//! 1. **单发 CLI**：`robot-ctl <op> [args..]`，结果 JSON 打 stdout（shell
//!    调试 / 排针实验）。例：`robot-ctl status`、`robot-ctl drive 30 30`。
//! 2. **常驻 serve**：`robot-ctl serve`，stdin/stdout JSON 行协议——Python
//!    等经 Popen 管道调用（见同目录 robot.py）。进程常驻，/dev/rt_shm
//!    mmap 与就绪等待只做一次，重复调用零启动开销。
//!
//! ## JSON 行协议（扁平对象，值为字符串或整数，无嵌套）
//!
//! ```text
//! 请求：{"op":"set_speed","left":30,"right":30}
//! 响应：{"ok":true,"op":"set_speed"}
//! 错误：{"ok":false,"op":"set_speed","error":"..."}
//! ```
//!
//! ## op 清单
//!
//! | op | 参数 | 类型 | 说明 |
//! |----|------|------|------|
//! | status | — | call | 端口 probe 掩码 / 底盘状态 / 臂队列丢弃数 |
//! | init | [ppr pwm] | acall | 底盘 INIT+CONFIG（默认 4680/20000），等 ACK |
//! | drive | L R | send | 双轮速度 ±100 |
//! | stop / brake | — | send | 滑行停 / 刹车 |
//! | get | — | call | 遥测快照（RPM/编码器，陈旧度 ≤100ms） |
//! | arm | S A | send | 舵机 S 角度 A（0-270°） |
//! | torque | [0/1] | send | 力矩恢复/释放（默认释放） |
//! | grab | — | acall | 抓取全序列（~4.5s，完成才返回） |
//! | release | — | acall | 张开夹爪（~0.5s） |
//! | uwrite | PORT HEX | call | raw 写（bring-up 排针实验） |
//! | uread | PORT [MAX] | call | raw 读（清空当前 RX） |

use std::io::{self, BufRead, Write};
use std::os::unix::io::IntoRawFd;

use ov_channels::{ChannelId, SharedMemory};
use ov_rpc::define_service_client;

#[allow(dead_code)]
const RT_SHM_IOC_NOTIFY: libc::c_ulong = rtshm_abi::IOC_NOTIFY as libc::c_ulong;
#[allow(dead_code)]
const RT_SHM_IOC_AWAIT: libc::c_ulong = rtshm_abi::IOC_AWAIT as libc::c_ulong;
#[allow(dead_code)]
const RT_SHM_IOC_CLR_PENDING: libc::c_ulong = rtshm_abi::IOC_CLR_PENDING as libc::c_ulong;
const SHM_SIZE: usize = rtshm_abi::K3_SHM_SIZE;

// 与 RP 固件 intercom.rs 的 method id 镜像（acall 在客户端按 call 声明）。
define_service_client! {
    RtAsyncRpc {
        UART_WRITE:  8 => call uart_write(port: u8, len: u8, data: [u8; 32]) -> u32;
        UART_READ:   9 => call uart_read(port: u8, max: u8) -> (u32, [u8; 32]);
        UART_STATUS: 10 => call uart_status(nonce: u32) -> (u32, u32, u32, u32, u32);
        CHASSIS_SET_SPEED: 11 => send chassis_set_speed(left: i16, right: i16);
        CHASSIS_STOP:     12 => send chassis_stop(brake: u8);
        CHASSIS_GET:      13 => call chassis_get(nonce: u32) -> (u32, u32, i32, i32, i32, i32, u32, u64);
        CHASSIS_INIT:     14 => call chassis_init(ppr: u16, pwm_freq: u16) -> u32;
        ARM_SET_ANGLE: 15 => send arm_set_angle(servo: u8, angle: u16);
        ARM_TORQUE:    16 => send arm_torque(release: u8);
        ARM_GRAB:      17 => call arm_grab(nonce: u32) -> u32;
        ARM_RELEASE:   18 => call arm_release(nonce: u32) -> u32;
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

/// /dev/rt_shm 封装（open + mmap + 三个 ioctl + 就绪等待/残留排空）。
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

    /// 阻塞等待 RP 门铃（mailbox IRQ 唤醒；SIGALRM 可打断为 EINTR）。
    fn await_ipi(&self) -> io::Result<()> {
        do_ioctl(self.fd, RT_SHM_IOC_AWAIT, 0)?;
        Ok(())
    }

    /// 轮询共享窗 magic 就绪（5s 超时；RP 固件未起/未 init 时 false）。
    fn wait_valid(&self) -> bool {
        let shm = unsafe { SharedMemory::<3>::at(self.shm_addr()) };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if shm.is_valid() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
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

extern "C" fn on_sigalrm(_sig: libc::c_int) {}

/// 机器人客户端：RPC 调用 + 超时看门狗（SIGALRM 打断阻塞的 AWAIT）。
struct Robot {
    rt: RtShm,
    client: RtAsyncRpc,
}

impl Robot {
    fn open() -> Result<Self, String> {
        unsafe {
            libc::signal(libc::SIGALRM, on_sigalrm as libc::sighandler_t);
        }
        let rt = RtShm::open().map_err(|e| format!("open /dev/rt_shm: {e}"))?;
        if !rt.wait_valid() {
            return Err("shared window not ready (RP firmware down?)".into());
        }
        rt.drain_ch1();
        let client = RtAsyncRpc::new(rt.shm_addr());
        Ok(Self { rt, client })
    }

    fn notify(&self) -> io::Result<()> {
        self.rt.notify()
    }

    /// 阻塞等响应并按 rid 取回（SIGALRM 超时 → Err）。
    fn wait_reply<T: serde::de::DeserializeOwned>(
        &mut self,
        rid: ov_rpc::RequestId,
        timeout_s: u32,
    ) -> Result<T, String> {
        unsafe { libc::alarm(timeout_s) };
        let out = (|| -> Result<T, String> {
            let mut spins: u32 = 0;
            loop {
                self.rt.await_ipi().map_err(|e| format!("await: {e}"))?;
                self.client.poll_responses();
                match self
                    .client
                    .recv_for::<T>(rid)
                    .map_err(|e| format!("recv: {e:?}"))?
                {
                    Some(v) => return Ok(v),
                    None => {
                        // 空唤醒（他源门铃/杂散）：有限次重等后放弃。
                        spins += 1;
                        if spins > 64 {
                            return Err("no response (spins exhausted)".into());
                        }
                    }
                }
            }
        })();
        unsafe { libc::alarm(0) };
        out
    }
}

// ============================================================================
// JSON 输出（本协议为扁平小对象，直接拼串；字符串值均为安全字符集）
// ============================================================================

fn ok_json(op: &str, fields: &str) -> String {
    if fields.is_empty() {
        format!("{{\"ok\":true,\"op\":\"{op}\"}}")
    } else {
        format!("{{\"ok\":true,\"op\":\"{op}\",{fields}}}")
    }
}

fn err_json(op: &str, msg: &str) -> String {
    format!("{{\"ok\":false,\"op\":\"{op}\",\"error\":\"{msg}\"}}")
}

// ============================================================================
// 扁平 JSON 解析（输入协议专用：值 = 字符串或整数，无嵌套/转义）
// ============================================================================

#[derive(Clone, Debug)]
enum Val {
    N(i64),
    S(String),
}

struct Req {
    op: String,
    kv: Vec<(String, Val)>,
}

impl Req {
    fn num(&self, key: &str) -> Option<i64> {
        self.kv.iter().find(|(k, _)| k == key).and_then(|(_, v)| match v {
            Val::N(n) => Some(*n),
            _ => None,
        })
    }

    #[allow(dead_code)]
    fn str(&self, key: &str) -> Option<&str> {
        self.kv.iter().find(|(k, _)| k == key).and_then(|(_, v)| match v {
            Val::S(s) => Some(s.as_str()),
            _ => None,
        })
    }

    fn num_or(&self, key: &str, default: i64) -> i64 {
        self.num(key).unwrap_or(default)
    }
}

fn parse_flat(line: &str) -> Option<Req> {
    let b = line.as_bytes();
    let mut i = 0;
    let mut op: Option<String> = None;
    let mut kv = Vec::new();

    let read_str = |i: &mut usize| -> Option<String> {
        if *i >= b.len() || b[*i] != b'"' {
            return None;
        }
        let start = *i + 1;
        let mut j = start;
        while j < b.len() && b[j] != b'"' {
            j += 1;
        }
        if j >= b.len() {
            return None;
        }
        let s = line[start..j].to_string();
        *i = j + 1;
        Some(s)
    };

    while i < b.len() && b[i] != b'{' {
        i += 1;
    }
    i += 1;
    loop {
        while i < b.len() && matches!(b[i], b' ' | b',' | b'\t' | b'\n' | b'\r') {
            i += 1;
        }
        if i >= b.len() || b[i] == b'}' {
            break;
        }
        let key = read_str(&mut i)?;
        while i < b.len() && b[i] != b':' {
            i += 1;
        }
        i += 1;
        while i < b.len() && b[i] == b' ' {
            i += 1;
        }
        let val = if i < b.len() && b[i] == b'"' {
            Val::S(read_str(&mut i)?)
        } else {
            let start = i;
            let mut j = i;
            if j < b.len() && b[j] == b'-' {
                j += 1;
            }
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j == start {
                return None;
            }
            i = j;
            Val::N(line[start..j].parse().ok()?)
        };
        if key == "op" {
            if let Val::S(s) = val {
                op = Some(s);
            }
        } else {
            kv.push((key, val));
        }
    }
    Some(Req { op: op?, kv })
}

/// 十六进制串 → 字节（"AA5501" → [0xAA,0x55,0x01]）。
fn parse_hex(s: &str) -> Option<Vec<u8>> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn hex_str(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

// ============================================================================
// op 分发（CLI 与 serve 共用）
// ============================================================================

fn exec(robot: &mut Robot, req: &Req) -> String {
    let op = req.op.as_str();
    let notify = || robot.notify().expect("NOTIFY failed");
    match op {
        "status" => {
            let rid = robot.client.uart_status(1, notify);
            match rid {
                Err(e) => err_json(op, &format!("send: {e:?}")),
                Ok(rid) => match robot.wait_reply::<(u32, u32, u32, u32, u32)>(rid, 5) {
                    Ok(v) => ok_json(
                        op,
                        &format!(
                            "\"nonce\":{},\"ports\":{},\"chassis_inited\":{},\"chassis_err\":{},\"arm_dropped\":{}",
                            v.0, v.1, v.2, v.3, v.4
                        ),
                    ),
                    Err(e) => err_json(op, &e),
                },
            }
        }
        "init" => {
            let ppr = req.num_or("ppr", 4680) as u16;
            let pwm = req.num_or("pwm", 20000) as u16;
            let rid = robot.client.chassis_init(ppr, pwm, notify);
            match rid {
                Err(e) => err_json(op, &format!("send: {e:?}")),
                Ok(rid) => match robot.wait_reply::<u32>(rid, 5) {
                    Ok(r) if r == u32::MAX => err_json(op, "busy (init already pending)"),
                    Ok(r) => ok_json(op, &format!("\"result\":{r}")),
                    Err(e) => err_json(op, &e),
                },
            }
        }
        "drive" | "set_speed" => {
            let l = req.num_or("left", 0) as i16;
            let r = req.num_or("right", 0) as i16;
            match robot.client.chassis_set_speed(l, r, notify) {
                Ok(()) => ok_json(op, &format!("\"left\":{l},\"right\":{r}")),
                Err(e) => err_json(op, &format!("send: {e:?}")),
            }
        }
        "stop" => match robot.client.chassis_stop(1, notify) {
            Ok(()) => ok_json(op, ""),
            Err(e) => err_json(op, &format!("send: {e:?}")),
        },
        "brake" => match robot.client.chassis_stop(2, notify) {
            Ok(()) => ok_json(op, ""),
            Err(e) => err_json(op, &format!("send: {e:?}")),
        },
        "get" => {
            let rid = robot.client.chassis_get(1, notify);
            match rid {
                Err(e) => err_json(op, &format!("send: {e:?}")),
                Ok(rid) => match robot.wait_reply::<(u32, u32, i32, i32, i32, i32, u32, u64)>(rid, 5) {
                    Ok(v) => ok_json(
                        op,
                        &format!(
                            "\"inited\":{},\"rpm_left\":{},\"rpm_right\":{},\"enc_m1\":{},\"enc_m2\":{},\"err\":{},\"last_ms\":{}",
                            v.1, v.2, v.3, v.4, v.5, v.6, v.7
                        ),
                    ),
                    Err(e) => err_json(op, &e),
                },
            }
        }
        "arm" | "set_angle" => {
            let s = req.num_or("servo", 0) as u8;
            let a = req.num_or("angle", 150) as u16;
            match robot.client.arm_set_angle(s, a, notify) {
                Ok(()) => ok_json(op, &format!("\"servo\":{s},\"angle\":{a}")),
                Err(e) => err_json(op, &format!("send: {e:?}")),
            }
        }
        "torque" => {
            let rel = req.num_or("release", 1) as u8;
            match robot.client.arm_torque(rel, notify) {
                Ok(()) => ok_json(op, &format!("\"release\":{rel}")),
                Err(e) => err_json(op, &format!("send: {e:?}")),
            }
        }
        "grab" => {
            let rid = robot.client.arm_grab(1, notify);
            match rid {
                Err(e) => err_json(op, &format!("send: {e:?}")),
                // 全序列 ~4.5s（500+1000+2000ms sleep + 帧时间）。
                Ok(rid) => match robot.wait_reply::<u32>(rid, 15) {
                    Ok(r) if r == u32::MAX => err_json(op, "arm queue full"),
                    Ok(r) => ok_json(op, &format!("\"result\":{r}")),
                    Err(e) => err_json(op, &e),
                },
            }
        }
        "release" => {
            let rid = robot.client.arm_release(1, notify);
            match rid {
                Err(e) => err_json(op, &format!("send: {e:?}")),
                Ok(rid) => match robot.wait_reply::<u32>(rid, 5) {
                    Ok(r) if r == u32::MAX => err_json(op, "arm queue full"),
                    Ok(r) => ok_json(op, &format!("\"result\":{r}")),
                    Err(e) => err_json(op, &e),
                },
            }
        }
        "uwrite" => {
            let port = req.num_or("port", 1) as u8;
            let Some(bytes) = req.str("hex").and_then(parse_hex) else {
                return err_json(op, "missing/invalid hex");
            };
            if bytes.len() > 32 {
                return err_json(op, "hex too long (max 32 bytes)");
            }
            let mut data = [0u8; 32];
            data[..bytes.len()].copy_from_slice(&bytes);
            let rid = robot.client.uart_write(port, bytes.len() as u8, data, notify);
            match rid {
                Err(e) => err_json(op, &format!("send: {e:?}")),
                Ok(rid) => match robot.wait_reply::<u32>(rid, 5) {
                    Ok(n) if n == u32::MAX => err_json(op, "port not probed"),
                    Ok(n) => ok_json(op, &format!("\"written\":{n}")),
                    Err(e) => err_json(op, &e),
                },
            }
        }
        "uread" => {
            let port = req.num_or("port", 1) as u8;
            let max = req.num_or("max", 32) as u8;
            let rid = robot.client.uart_read(port, max, notify);
            match rid {
                Err(e) => err_json(op, &format!("send: {e:?}")),
                Ok(rid) => match robot.wait_reply::<(u32, [u8; 32])>(rid, 5) {
                    Ok((n, _)) if n == u32::MAX => err_json(op, "port not probed"),
                    Ok((n, data)) => {
                        let n = n as usize;
                        ok_json(op, &format!("\"n\":{n},\"hex\":\"{}\"", hex_str(&data[..n.min(32)])))
                    }
                    Err(e) => err_json(op, &e),
                },
            }
        }
        _ => err_json(op, "unknown op"),
    }
}

// ============================================================================
// CLI 参数 → Req（位置参数按 op 各自约定）
// ============================================================================

fn cli_to_req(args: &[String]) -> Result<Req, String> {
    let op = args.first().ok_or("missing op")?.clone();
    let n = |i: usize| -> Result<i64, String> {
        args.get(i)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("bad arg #{}", i))
    };
    let mut kv = Vec::new();
    match op.as_str() {
        "status" | "get" | "grab" | "release" | "stop" | "brake" => {}
        "init" => {
            kv.push(("ppr".into(), Val::N(args.get(1).map(|s| s.parse().unwrap_or(4680)).unwrap_or(4680))));
            kv.push(("pwm".into(), Val::N(args.get(2).map(|s| s.parse().unwrap_or(20000)).unwrap_or(20000))));
        }
        "drive" | "set_speed" => {
            kv.push(("left".into(), Val::N(n(1)?)));
            kv.push(("right".into(), Val::N(n(2)?)));
        }
        "arm" | "set_angle" => {
            kv.push(("servo".into(), Val::N(n(1)?)));
            kv.push(("angle".into(), Val::N(n(2)?)));
        }
        "torque" => {
            kv.push(("release".into(), Val::N(args.get(1).map(|s| s.parse().unwrap_or(1)).unwrap_or(1))));
        }
        "uwrite" => {
            kv.push(("port".into(), Val::N(n(1)?)));
            kv.push(("hex".into(), Val::S(args.get(2).ok_or("missing hex")?.clone())));
        }
        "uread" => {
            kv.push(("port".into(), Val::N(n(1)?)));
            if let Some(m) = args.get(2) {
                kv.push(("max".into(), Val::N(m.parse().map_err(|_| "bad max")?)));
            }
        }
        "serve" => return Err("serve".into()),
        _ => return Err(format!("unknown op {op}")),
    }
    Ok(Req { op, kv })
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    if argv.first().map(String::as_str) == Some("serve") {
        let robot = match Robot::open() {
            Ok(r) => r,
            Err(e) => {
                let mut out = io::stdout().lock();
                let _ = writeln!(out, "{{\"ok\":false,\"op\":\"open\",\"error\":\"{e}\"}}");
                let _ = out.flush();
                std::process::exit(1);
            }
        };
        let mut robot = robot;
        let stdin = io::stdin();
        let mut out = io::stdout().lock();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let s = if line.trim().is_empty() {
                String::new()
            } else {
                match parse_flat(&line) {
                    Some(req) => exec(&mut robot, &req),
                    None => err_json("parse", "bad json line"),
                }
            };
            if writeln!(out, "{s}").and_then(|_| out.flush()).is_err() {
                break; // 对端关闭
            }
        }
        return;
    }

    let usage = "usage: robot-ctl <serve | op [args...]>\n\
                 ops: status | init [ppr pwm] | drive L R | stop | brake | get |\n\
                      arm SERVO ANGLE | torque [0/1] | grab | release |\n\
                      uwrite PORT HEX | uread PORT [MAX]";

    let req = match cli_to_req(&argv) {
        Ok(r) => r,
        Err(e) if e == "serve" => {
            println!("{usage}");
            return;
        }
        Err(e) => {
            eprintln!("error: {e}\n{usage}");
            std::process::exit(2);
        }
    };
    let mut robot = match Robot::open() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    println!("{}", exec(&mut robot, &req));
}
