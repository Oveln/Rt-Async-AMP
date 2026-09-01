//! rtsh —— rt-async 交互式 shell（AP 用户态，StarryOS）。
//!
//! 把 /dev/rt_shm 当设备打开（open + mmap + NOTIFY/AWAIT ioctl 门铃），在
//! 共享窗上经 ov-rpc 与 RP 固件交互式对话——一个"rt-async 功能 shell"。
//! 链路与 robot-ctl 相同（见 user-test-rpc 的链路说明）。
//!
//! 配对固件与命令面：
//! - K3 `k3-robot-ctrl`：全部命令可用（通用 + 机器人语义 + probe 测量面）；
//! - QEMU `rt-async-app`：仅 echo / add / delay（固件只注册这三个方法，
//!   其余命令按名字解析失败、立即报错）。
//!
//! 方法 id 不做编译期镜像：启动时 INIT 服务发现一次（打印方法表），命令
//! 按方法名从描述符解析 mid——固件侧重编号无需重编 rtsh。参数/返回类型
//! 仍编译在各命令调用点（描述符不携带类型信息，新增/改签名需同步）。
//!
//! 用法：
//! - 交互 REPL：`rtsh`，提示符 `rtsh> `；空行重复上一条，quit / Ctrl-D 退出。
//!   tty 规范模式自带行编辑（退格 / Ctrl-U / Ctrl-W），无历史翻阅。
//! - 单发：`rtsh <cmd> [args..]`，执行一条即退出（脚本 / 快捷调用）。

use std::io::{self, Write};
use std::os::unix::io::IntoRawFd;

use ov_channels::{ChannelId, SharedMemory};

#[allow(dead_code)]
const RT_SHM_IOC_NOTIFY: libc::c_ulong = rtshm_abi::IOC_NOTIFY as libc::c_ulong;
#[allow(dead_code)]
const RT_SHM_IOC_AWAIT: libc::c_ulong = rtshm_abi::IOC_AWAIT as libc::c_ulong;
#[allow(dead_code)]
const RT_SHM_IOC_CLR_PENDING: libc::c_ulong = rtshm_abi::IOC_CLR_PENDING as libc::c_ulong;
const SHM_SIZE: usize = rtshm_abi::K3_SHM_SIZE;

/// 普通 call 响应超时（秒）。
const T_CALL: u32 = 3;
/// 慢命令超时（秒）：底盘 INIT（等 ACK）/ membench（部分 op 忙等 >1s）。
const T_SLOW: u32 = 10;
/// 抓取全序列超时（秒，~4.5s 动作 + 帧时间）。
const T_GRAB: u32 = 15;

// 方法名与 RP 固件 intercom.rs define_service! 的常量名对齐（ECHO/ADD/
// …/ARM_RELEASE）；mid 运行时经服务发现按名字解析（见 Shell::discover），
// 编号无需镜像。acall 在客户端按 call 发起（响应由完成方补发）。

// ============================================================================
// /dev/rt_shm 封装（与 robot-ctl 同构：open + mmap + ioctl + 就绪等待/排空）
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

// ============================================================================
// shell 客户端（RPC 调用 + SIGALRM 超时看门狗，同 robot-ctl）
// ============================================================================

extern "C" fn on_sigalrm(_sig: libc::c_int) {}

/// 发现表条目：(mid, flags, 名称)——flags 见 ov_rpc::descriptor。
struct Discovered {
    mid: u64,
    flags: u8,
    name: String,
}

struct Shell {
    rt: RtShm,
    client: ov_rpc::RpcClient,
    /// 服务发现结果（启动时 INIT 一次；`services` 命令可刷新）。
    table: Vec<Discovered>,
}

impl Shell {
    fn open() -> Result<Self, String> {
        unsafe {
            libc::signal(libc::SIGALRM, on_sigalrm as *const () as libc::sighandler_t);
        }
        let rt = RtShm::open().map_err(|e| format!("open /dev/rt_shm: {e}"))?;
        if !rt.wait_valid() {
            return Err("shared window not ready (RP firmware down?)".into());
        }
        rt.drain_ch1();
        let client = ov_rpc::RpcClient::new(rt.shm_addr());
        Ok(Self { rt, client, table: Vec::new() })
    }

    fn notify(&self) -> io::Result<()> {
        self.rt.notify()
    }

    /// 服务发现：INIT（method 0）→ 描述符解析 → 刷新方法表。
    ///
    /// 旧固件（无 INIT 拦截）下收到 poison 响应（描述符解不开）或版本门
    /// 直接拒绝发送——均为明确报错而非挂死。
    fn discover(&mut self) -> Result<(), String> {
        let rid = self
            .client
            .discover(|| self.rt.notify().map_err(|e| e.to_string()).unwrap_or(()))
            .map_err(|e| format!("INIT 发送失败: {e:?}"))?;

        unsafe { libc::alarm(T_CALL) };
        let raw = (|| -> Result<Vec<u8>, String> {
            let mut spins: u32 = 0;
            loop {
                self.rt.await_ipi().map_err(|e| format!("await: {e}"))?;
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
        })();
        unsafe { libc::alarm(0) };
        let raw = raw?;

        let d = ov_rpc::descriptor::parse(&raw)
            .ok_or_else(|| "描述符解析失败（旧固件无服务发现？）".to_string())?;
        self.table = d
            .methods()
            .map(|m| Discovered { mid: m.mid, flags: m.flags, name: m.name.to_string() })
            .collect();
        Ok(())
    }

    /// 方法表渲染（启动横幅与 `services` 命令共用）。
    fn services_text(&self) -> String {
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
    fn mid(&self, name: &str) -> Result<u64, String> {
        self.table
            .iter()
            .find(|m| m.name == name)
            .map(|m| m.mid)
            .ok_or_else(|| format!("固件未提供方法 {name}（services 查看方法表）"))
    }

    /// 双向调用（call：服务端回 IPI）。
    fn call<A: serde::Serialize>(&mut self, name: &str, args: &A) -> Result<ov_rpc::RequestId, String> {
        let mid = self.mid(name)?;
        self.client
            .call(mid, args, || self.notify().expect("NOTIFY failed"))
            .map_err(|e| format!("send: {e:?}"))
    }

    /// 单向下发（send：不期待响应）。
    fn send<A: serde::Serialize>(&mut self, name: &str, args: &A) -> Result<(), String> {
        let mid = self.mid(name)?;
        self.client
            .send(mid, args, || self.notify().expect("NOTIFY failed"))
            .map_err(|e| format!("send: {e:?}"))
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
// 参数/格式化小工具
// ============================================================================

/// 必选数值参数（`#i` 位置，解析失败/缺失 → Err）。
fn need<T: std::str::FromStr>(toks: &[&str], i: usize, what: &str) -> Result<T, String> {
    toks.get(i)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("缺少/无法解析参数 #{i}（{what}）"))
}

/// 可选数值参数（缺失取默认值；给出但解析失败 → Err）。
fn opt<T: std::str::FromStr>(toks: &[&str], i: usize, def: T) -> Result<T, String> {
    match toks.get(i) {
        None | Some(&"") => Ok(def),
        Some(&s) => s.parse().map_err(|_| format!("无法解析参数 #{i}（{s}）")),
    }
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

/// 十六进制地址解析（"0xc088c04c" / "c088c04c"）。
fn parse_addr(s: &str) -> Option<u32> {
    let s = s.trim_start_matches("0x").trim_start_matches("0X");
    u32::from_str_radix(s, 16).ok()
}

/// AP 侧单调钟（RTT 计时；musl vdso，开销 ~百 ns，远小于 IPC RTT）。
fn now_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

// ============================================================================
// STATS 计数器（镜像 intercom::stat_idx，双端对齐义务，同 user-test-bench）
// ============================================================================

const STAT_NAMES: [&str; 20] = [
    "msgs", "d1", "d2", "d3", "d4", "redundant_irq", "resp_fail", "heals",
    "win_last_ns", "win_min_ns", "win_max_ns", "windows",
    "svc_last_ns", "svc_min_ns", "svc_max_ns", "t_now", "freq_hz",
    "lit_viol", "lit_rounds", "lit_state",
];

/// 计数器值格式化：ns 型附 µs 换算；min 初值 u64::MAX = 尚无样本。
fn fmt_stat(idx: u32, v: u64) -> String {
    match (idx, v) {
        (9 | 13, u64::MAX) => "N/A（尚无样本）".into(),
        (8 | 9 | 10 | 12 | 13 | 14, v) => format!("{v}（{:.1} µs）", v as f64 / 1e3),
        (_, v) => v.to_string(),
    }
}

fn stat_query(sh: &mut Shell, idx: u32) -> Result<u64, String> {
    let rid = sh.call("STATS", &(idx,))?;
    sh.wait_reply::<u64>(rid, T_CALL)
}

// ============================================================================
// 命令实现
// ============================================================================

/// `ping [N]`：N 次 RTT 往返（默认 1，封顶 10000）。单次打印 RP 侧分段
/// （isr→sched / sched→seen，仅 D1 单请求在途时精确）；多次打印
/// min/avg/p50/p95/p99/max 与发现路径分布。
fn cmd_ping(sh: &mut Shell, toks: &[&str]) -> Result<String, String> {
    let n: usize = opt(toks, 1, 1usize)?.clamp(1, 10_000);

    // RP mtime 频率（Hz，stat 16）——分段 ticks→µs 换算；查不到则省略分段。
    let freq_mhz: Option<f64> = stat_query(sh, 16).ok().filter(|&v| v > 0).map(|v| v as f64 / 1e6);

    let mut rtts: Vec<f64> = Vec::with_capacity(n);
    let mut paths = [0usize; 5]; // 下标 1..=4 = D1..D4
    let mut single: Option<(u64, u8, u64, u64, u64, u64)> = None;

    for seq in 0..n as u64 {
        let t0 = now_ns();
        let rid = sh.call("PING", &(seq,))?;
        let r = sh.wait_reply::<(u64, u8, u64, u64, u64, u64)>(rid, T_CALL)?;
        let t1 = now_ns();
        rtts.push((t1 - t0) as f64 / 1e3);
        paths[(r.1 as usize).min(4)] += 1;
        single = Some((r.0, r.1, r.2, r.3, r.4, r.5));
    }

    if n == 1 {
        let (val, tag, t_isr, _t_drain, t_sched, t_seen) = single.unwrap();
        let mut s = format!(
            "ping: rtt={:.1} µs  path=D{}  echo={}",
            rtts[0], tag, val
        );
        if let Some(f) = freq_mhz {
            // 分段仅 D1（isr 唤醒）且单请求在途时精确（多消息在途 t_isr 会被
            // 后续中断覆盖，见 intercom.rs PING 文档）。
            let seg = |a: u64, b: u64| b.saturating_sub(a) as f64 / f;
            s.push_str(&format!(
                "\n      rp: isr→sched={:.1} µs  sched→seen={:.1} µs",
                seg(t_isr, t_sched),
                seg(t_sched, t_seen)
            ));
        }
        return Ok(s);
    }

    rtts.sort_by(|a, b| a.total_cmp(b));
    let avg = rtts.iter().sum::<f64>() / rtts.len() as f64;
    let pick = |p: f64| rtts[((rtts.len() - 1) as f64 * p).round() as usize];
    Ok(format!(
        "ping: n={n}  rtt µs: min={:.1} avg={:.1} p50={:.1} p95={:.1} p99={:.1} max={:.1}\n      path: d1={} d2={} d3={} d4={}",
        rtts[0],
        avg,
        pick(0.50),
        pick(0.95),
        pick(0.99),
        rtts[rtts.len() - 1],
        paths[1],
        paths[2],
        paths[3],
        paths[4],
    ))
}

/// `stat [IDX]`：无参 dump 0-16（probe 扩展列 17-23 单查）。
fn cmd_stat(sh: &mut Shell, toks: &[&str]) -> Result<String, String> {
    if let Some(a) = toks.get(1) {
        let idx: u32 = a
            .parse()
            .map_err(|_| format!("bad idx（0-19）: {a}"))?;
        if idx as usize >= STAT_NAMES.len() {
            return Err(format!("idx 越界（0-19）: {idx}"));
        }
        let v = stat_query(sh, idx)?;
        return Ok(format!("{idx:2} {} = {}", STAT_NAMES[idx as usize], fmt_stat(idx, v)));
    }
    let mut lines = Vec::new();
    for idx in 0..=16u32 {
        let v = stat_query(sh, idx)?;
        lines.push(format!("{idx:2} {:<14} {}", STAT_NAMES[idx as usize], fmt_stat(idx, v)));
    }
    lines.push("（17-19 为 probe 固件扩展列：`stat <idx>` 单查）".into());
    Ok(lines.join("\n"))
}

/// `services`：重新服务发现并列出方法表（启动时已自动执行一次）。
fn cmd_services(sh: &mut Shell, _toks: &[&str]) -> Result<String, String> {
    sh.discover()?;
    Ok(sh.services_text())
}

fn help_text() -> String {
    [
        "通用 RPC（K3 / QEMU 固件均有）",
        "  services               重新服务发现并列出方法表（启动时已自动一次）",
        "  echo N                 回显",
        "  add A B                整数加",
        "  delay US               RP 侧精确延时 µs（单向，无响应）",
        "  ping [N]               N 次 RTT（默认 1）：单次含 RP 分段；多次含分位数 + 路径分布",
        "  stat [IDX]             插桩计数器（无参 = 全部 0-16；17-23 仅 probe 固件）",
        "机器人（K3 k3-robot-ctrl）",
        "  status                 端口 probe 掩码 / 底盘状态 / 臂队列丢弃数",
        "  init [PPR PWM]         底盘 INIT+CONFIG 等 ACK（默认 4680/20000）",
        "  drive L R | stop | brake | get    双轮速度 ±100 / 滑行停 / 刹车 / 遥测快照",
        "  arm S A | torque [0/1] | grab | release    舵机角度 / 力矩 / 抓取 / 张开",
        "  uwrite PORT HEX | uread PORT [MAX]         raw UART 读写（bring-up）",
        "probe 测量面（仅 probe 固件，普通固件上未注册即报错）",
        "  membench OP [ARG]      RP 侧微基准（op 表见 intercom.rs membench_op）",
        "  peek ADDR              只读寄存器 1000 连读单价（PEEK_T；勿指向 FIFO 类寄存器）",
        "  litmus OP [ARG]        顺序性实验触发（结果经 stat 17-19 轮询）",
        "shell",
        "  help | quit            帮助 / 退出（空行重复上一条）",
    ]
    .join("\n")
}

// ============================================================================
// 命令分发（REPL 与单发共用）
// ============================================================================

fn exec(sh: &mut Shell, toks: &[&str]) -> Result<String, String> {
    let Some(cmd) = toks.first().copied() else {
        return Ok(String::new());
    };
    match cmd {
        "help" | "?" => Ok(help_text()),

        // ── 通用 ──
        "echo" => {
            let v: u32 = need(toks, 1, "N")?;
            let rid = sh.call("ECHO", &(v,))?;
            let r: u32 = sh.wait_reply(rid, T_CALL)?;
            Ok(format!("echo: {r}"))
        }
        "add" => {
            let a: i32 = need(toks, 1, "A")?;
            let b: i32 = need(toks, 2, "B")?;
            let rid = sh.call("ADD", &(a, b))?;
            let r: i32 = sh.wait_reply(rid, T_CALL)?;
            Ok(format!("add: {a} + {b} = {r}"))
        }
        "delay" => {
            let us: u32 = need(toks, 1, "US")?;
            sh.send("DELAY", &(us,))?;
            Ok(format!("delay: {us} µs 已下发（单向，无响应）"))
        }
        "ping" => cmd_ping(sh, toks),
        "stat" => cmd_stat(sh, toks),
        "services" | "disc" => cmd_services(sh, toks),

        // ── 机器人（K3 k3-robot-ctrl；输出语义同 robot-ctl）──
        "status" => {
            let rid = sh.call("UART_STATUS", &(1u32,))?;
            let v = sh.wait_reply::<(u32, u32, u32, u32, u32)>(rid, T_CALL)?;
            Ok(format!(
                "status: ports={:#04x} chassis_inited={} chassis_err={} arm_dropped={}",
                v.1, v.2, v.3, v.4
            ))
        }
        "init" => {
            let ppr: u16 = opt(toks, 1, 4680u16)?;
            let pwm: u16 = opt(toks, 2, 20000u16)?;
            let rid = sh.call("CHASSIS_INIT", &(ppr, pwm))?;
            let r: u32 = sh.wait_reply(rid, T_SLOW)?;
            if r == u32::MAX {
                Err("busy（已有 init 在途）".into())
            } else {
                Ok(format!("init: result={r}"))
            }
        }
        "drive" | "set_speed" => {
            let l: i16 = need(toks, 1, "L")?;
            let r: i16 = need(toks, 2, "R")?;
            sh.send("CHASSIS_SET_SPEED", &(l, r))?;
            Ok(format!("drive: left={l} right={r}"))
        }
        "stop" => {
            sh.send("CHASSIS_STOP", &(1u8,))?;
            Ok("stop: 已下发".into())
        }
        "brake" => {
            sh.send("CHASSIS_STOP", &(2u8,))?;
            Ok("brake: 已下发".into())
        }
        "get" => {
            let rid = sh.call("CHASSIS_GET", &(1u32,))?;
            let v = sh
                .wait_reply::<(u32, u32, i32, i32, i32, i32, u32, u64)>(rid, T_CALL)?;
            Ok(format!(
                "get: inited={} rpm_left={} rpm_right={} enc_m1={} enc_m2={} err={} last_ms={}",
                v.1, v.2, v.3, v.4, v.5, v.6, v.7
            ))
        }
        "arm" | "set_angle" => {
            let s: u8 = need(toks, 1, "SERVO")?;
            let a: u16 = need(toks, 2, "ANGLE")?;
            sh.send("ARM_SET_ANGLE", &(s, a))?;
            Ok(format!("arm: servo={s} angle={a}"))
        }
        "torque" => {
            let rel: u8 = opt(toks, 1, 1u8)?;
            sh.send("ARM_TORQUE", &(rel,))?;
            Ok(format!("torque: release={rel}"))
        }
        "grab" => {
            let rid = sh.call("ARM_GRAB", &(1u32,))?;
            let r: u32 = sh.wait_reply(rid, T_GRAB)?;
            if r == u32::MAX {
                Err("arm queue full".into())
            } else {
                Ok(format!("grab: result={r}"))
            }
        }
        "release" => {
            let rid = sh.call("ARM_RELEASE", &(1u32,))?;
            let r: u32 = sh.wait_reply(rid, T_SLOW)?;
            if r == u32::MAX {
                Err("arm queue full".into())
            } else {
                Ok(format!("release: result={r}"))
            }
        }
        "uwrite" => {
            let port: u8 = need(toks, 1, "PORT")?;
            let Some(bytes) = toks.get(2).and_then(|s| parse_hex(s)) else {
                return Err("缺少/无效 hex".into());
            };
            if bytes.len() > 32 {
                return Err("hex 过长（≤32 字节）".into());
            }
            let mut data = [0u8; 32];
            data[..bytes.len()].copy_from_slice(&bytes);
            let rid = sh.call("UART_WRITE", &(port, bytes.len() as u8, data))?;
            let n: u32 = sh.wait_reply(rid, T_CALL)?;
            if n == u32::MAX {
                Err("port not probed".into())
            } else {
                Ok(format!("uwrite: written={n}"))
            }
        }
        "uread" => {
            let port: u8 = need(toks, 1, "PORT")?;
            let max: u8 = opt(toks, 2, 32u8)?;
            let rid = sh.call("UART_READ", &(port, max))?;
            match sh.wait_reply::<(u32, [u8; 32])>(rid, T_CALL)? {
                (u32::MAX, _) => Err("port not probed".into()),
                (n, data) => {
                    let n = n as usize;
                    Ok(format!("uread: n={n} hex={}", hex_str(&data[..n.min(32)])))
                }
            }
        }

        // ── probe 测量面（仅 probe 固件实现；普通固件上服务端按未知
        //    method 丢弃，本侧等到看门狗超时报错）──
        "membench" => {
            let op: u32 = need(toks, 1, "OP")?;
            let arg: u32 = opt(toks, 2, 0u32)?;
            let rid = sh.call("MEMBENCH", &(op, arg))?;
            let (ns, ck) = sh.wait_reply::<(u64, u64)>(rid, T_SLOW)?;
            Ok(format!("membench: op={op} arg={arg} → ns={ns} ck=0x{ck:x}（{ck}）"))
        }
        "peek" => {
            let Some(addr) = toks.get(1).and_then(|s| parse_addr(s)) else {
                return Err("缺少/无效地址（十六进制，如 0xc088c04c）".into());
            };
            if addr & 3 != 0 {
                return Err("地址须 4 字节对齐（misaligned load 会 fault）".into());
            }
            // PEEK_T = op 16（1000 次连续只读，返回 (ns, 末次值)）。
            let rid = sh.call("MEMBENCH", &(16u32, addr))?;
            let (ns, v) = sh.wait_reply::<(u64, u64)>(rid, T_SLOW)?;
            Ok(format!(
                "peek: {addr:#010x} → {:#010x}（1000 连读 {ns} ns，均 {:.1} ns/笔）\n      仅限只读寄存器：读 FIFO/msg 类寄存器会消费数据",
                v as u32, ns as f64 / 1000.0
            ))
        }
        "litmus" => {
            let op: u32 = need(toks, 1, "OP")?;
            let arg: u32 = opt(toks, 2, 0u32)?;
            sh.send("LITMUS", &(op, arg))?;
            Ok("litmus: 已下发（结果经 stat 17-19 轮询）".into())
        }

        _ => Err(format!("未知命令 '{cmd}'（help 查看命令表）")),
    }
}

// ============================================================================
// 入口：REPL / 单发
// ============================================================================

fn repl(mut sh: Shell) {
    println!("rtsh: /dev/rt_shm 就绪（help 查看命令，quit / Ctrl-D 退出）");
    // 启动自动服务发现一次：打印可用方法表，命令按名字解析 mid。
    // 失败不退出（help/quit/services 仍可用）——根因会在错误里自述。
    match sh.discover() {
        Ok(()) => println!("{}", sh.services_text()),
        Err(e) => println!("warning: 服务发现失败：{e}"),
    }
    let stdin = io::stdin();
    let mut last: Vec<String> = Vec::new();
    loop {
        print!("rtsh> ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        match stdin.read_line(&mut line) {
            Ok(0) => {
                println!(); // Ctrl-D
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
        let line = line.trim();
        if line.is_empty() {
            if last.is_empty() {
                continue;
            }
            // 空行 = 重复上一条
        } else {
            last = line.split_whitespace().map(str::to_string).collect();
        }
        let toks: Vec<&str> = last.iter().map(String::as_str).collect();
        if matches!(toks[0], "quit" | "exit") {
            break;
        }
        match exec(&mut sh, &toks) {
            Ok(s) => {
                if !s.is_empty() {
                    println!("{s}");
                }
            }
            Err(e) => println!("error: {e}"),
        }
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    if argv.is_empty() {
        match Shell::open() {
            Ok(sh) => repl(sh),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let toks: Vec<&str> = argv.iter().map(String::as_str).collect();
    if matches!(toks[0], "help" | "--help" | "-h") {
        println!("{}", help_text());
        return;
    }

    let mut sh = match Shell::open() {
        Ok(sh) => sh,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    // 单发也先发现一次（方法名解析需要；失败仅告警，命令自身会报错）。
    if let Err(e) = sh.discover() {
        eprintln!("warning: 服务发现失败：{e}");
    }
    match exec(&mut sh, &toks) {
        Ok(s) => {
            if !s.is_empty() {
                println!("{s}");
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
