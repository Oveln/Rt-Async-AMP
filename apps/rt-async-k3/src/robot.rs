//! 机器人控制（AKA-00 底盘 + 机械臂）——RP 侧协议栈。
//!
//! 协议在 RT24（AP 用户态只发语义 RPC，见 intercom 的方法表）；本模块实现
//! 两个协议，**双通道**：底盘走 R_UART0（与 console log 共线，容错依据见
//! [`PORT_CHASSIS`] 注释），机械臂走 **AP 域 UART5 TX 轮询**（`chip_k3_rt24::
//! ap_uart`，TX = pad83 mode4 → 40pin pin3 独立线——RT24 对 AP 域 UART
//! 中断不可达，ZP10S 纯写无应答，LSR 轮询 TX 即完整通道，时钟/引脚定案见
//! 该驱动模块文档）：
//!
//! - **底盘**（`src/base_control/tt_pid`，ESP32-C3 PID 电机控制器）：帧
//!   `AA 55 <cmd> <len> <payload> <xor>`，115200-8N1。运动指令 fire-and-forget
//!   （SET_SPEEDS/BRAKE/STOP），遥测请求-响应（GET_RPM/GET_ENCODER）。
//! - **机械臂**（`src/arm_control/zl/zp10s`，众灵舵机控制器）：ASCII 纯写
//!   `#<id>P<pulse>T1000!`，115200-8N1，无应答。
//!
//! R_UART0 共线纪律：协议运行期尽量少打 log（log 文本会被 ESP32 当噪声
//! 丢弃，但占用 TX 线时间）；ZP10S 线独立后 console 文本不再到达舵机。
//!
//! ## 任务模型（robot_ctrl bin）
//!
//! | 任务 | 优先级 | 节拍 | 职责 |
//! |------|--------|------|------|
//! | [`task_chassis`] | P1（高于 RPC 服务 P2） | 10ms | 应用 setpoint / 处理 INIT；每 10 拍（100ms）做一轮遥测 transact 刷新快照 |
//! | [`task_arm`] | P1 | 10ms | 消费命令队列；GRAB/RELEASE 多步序列（sleep 时序对齐 AKA-00） |
//!
//! P1 + 定时器抢占保证：即便 RPC 服务任务在 `process_elastic` 弹性自旋窗口
//! （最长 ~2s）内不让出，机器任务的 timer ISR 唤醒也会立即抢占执行——
//! 异步完成的响应延迟收敛到节拍级（≤10ms），不受自旋窗口影响。
//!
//! ## 请求完成的两种路径
//!
//! - **同步快照**（`CHASSIS_GET`）：服务 handler 直读原子快照返回（数据
//!   陈旧度 ≤100ms，控制环足够）。
//! - **异步完成**（`CHASSIS_INIT`/`ARM_GRAB`/`ARM_RELEASE`，acall）：handler
//!   把 rid 投队返回；任务完成动作后 [`respond`] 构造 `Message::response`
//!   经 CH1 + 门铃补发，AP 侧 `recv_for(rid)` 闭环。
//!
//! ## 并发约束
//!
//! 单 hart。R_UART0 的 TX 由两写者混用（console / chassis 任务，单核串行
//! 无字节撕裂，ESP32 按 AA 55 帧头重同步），RX 只由 chassis 任务消费；
//! ZP10S 通道单写者（task_arm → ap_uart 阻塞轮询，帧级 1.3ms）。RPC handler
//! 与任务之间的交接全部经原子量/SPSC 环（Release/Acquire 发布序，防 P1
//! 抢占 P2 造成的撕裂）。`UART_WRITE/READ`（raw 诊断）与任务并发访问
//! R_UART0 仅限 bring-up 阶段（CHASSIS_INIT 之前 / 空闲时），协议运行期
//! 不混用。

// 原子类型一律用 portable-atomic：K3 专属 target（atomic-cas:false）下 core
// 原生 RMW（swap/fetch_add）被 cfg 掉，经 critical-section 回退；其余 target
// 别名 core 原生，零开销（与 intercom 插桩计数器同一约定，见 Cargo.toml）。
use portable_atomic::{
    AtomicBool, AtomicI32, AtomicU16, AtomicU32, AtomicU64, AtomicU8, AtomicUsize,
};
use core::cell::UnsafeCell;
use core::sync::atomic::Ordering;

use chip_k3_rt24::ap_uart;
use chip_k3_rt24::pxa_uart::{self, PxaUart};
use fugit::ExtU64;
use ov_channels::Message;

/// 底盘串口 slot = 0（R_UART0，与 console 共口）。
///
/// 共口依据（2026-08-27 载板原理图 k3-com260_kit_v02 定案）：com260 40 针
/// 排针上唯一完整可用的 R.UART 就是 R_UART0 @ GPIO_122/123 → 40pin
/// pin29(TX)/pin32(RX)（网络名 GPIO01/GPIO07，经 1.8V→3.3V 电平转换）。
/// 底盘（ESP32 二进制帧）+ console log 共线（机械臂已分离到 AP 域 UART5
/// 独立线，见模块头）：
/// - ESP32 按 `AA 55` 帧头重同步，log 文本 / 任何 ASCII 不构成有效帧头；
/// - RX 方向（pin32）只有 ESP32 遥测驱动，单向干净。
/// 备选双口（M.2 槽空时经插座脚引出）：R.UART4（TX=GPIO_80→PCIeA_WAKEn
/// 多槽共线、RX=GPIO_81→PCIeA_CLKREQn@M.2 A 槽；m2，GPIO4 bank 因驱动
/// M.2 3.3V 信号原生 3.3V 免转换）——启用时在 DTS 加回对应 serial 节点
/// 并改本常量。（R.UART2 证伪：GPIO_58 网络 USB0_VBUS_DET 不在排针、
/// GPIO_57→M.2 E-key BT_EN。）
pub const PORT_CHASSIS: usize = 0;

// ============================================================================
// tt_pid 底盘协议（帧层）
// ============================================================================

const FRAME_H1: u8 = 0xAA;
const FRAME_H2: u8 = 0x55;

const CMD_INIT: u8 = 0x01;
const CMD_CONFIG: u8 = 0x02;
const CMD_STOP: u8 = 0x11;
const CMD_BRAKE: u8 = 0x12;
const CMD_SET_SPEEDS: u8 = 0x13;
const CMD_GET_RPM: u8 = 0x20;
const CMD_GET_ENCODER: u8 = 0x22;

const RSP_ACK: u8 = 0x80;
const RSP_RPM_DATA: u8 = 0x90;

/// 构建一帧（帧头+cmd+len+payload+xor 校验），返回总长。out 须 ≥ 5+len。
fn build_frame(cmd: u8, payload: &[u8], out: &mut [u8]) -> usize {
    let mut chk = cmd ^ (payload.len() as u8);
    for &b in payload {
        chk ^= b;
    }
    out[0] = FRAME_H1;
    out[1] = FRAME_H2;
    out[2] = cmd;
    out[3] = payload.len() as u8;
    out[4..4 + payload.len()].copy_from_slice(payload);
    out[4 + payload.len()] = chk;
    5 + payload.len()
}

/// 接收缓冲上限（应答最长 = GET_ENCODER 的 8B payload + 帧头 5B = 13B）。
const RX_BUF: usize = 32;

/// 从 UART 非阻塞搬全部待读字节进 buf（满了即停），返回写入后的长度。
fn drain_uart(uart: &PxaUart, buf: &mut [u8; RX_BUF], mut len: usize) -> usize {
    while len < buf.len() {
        match uart.read_raw() {
            Some(b) => {
                buf[len] = b;
                len += 1;
            }
            None => break,
        }
    }
    len
}

/// 从 buf 头部解析一帧完整合法帧；成功返回 (cmd, payload 拷贝, payload 长度)
/// 并消费该帧，失败返回 None（数据不足/校验错时按需丢弃字节继续找头）。
fn parse_frame(buf: &mut [u8; RX_BUF], len: &mut usize) -> Option<(u8, [u8; 12], usize)> {
    loop {
        // 找帧头（丢脏字节）。
        while *len >= 1 && !(buf[0] == FRAME_H1 && (*len < 2 || buf[1] == FRAME_H2)) {
            buf.copy_within(1..*len, 0);
            *len -= 1;
        }
        if *len < 5 {
            return None;
        }
        let flen = 5 + buf[3] as usize;
        if flen > RX_BUF || flen > 5 + 12 {
            // 长度字段超出协议合理范围：脏数据，丢头重找。
            buf.copy_within(1..*len, 0);
            *len -= 1;
            continue;
        }
        if *len < flen {
            return None; // 帧未收全
        }
        let mut chk = buf[2] ^ buf[3];
        for i in 4..flen - 1 {
            chk ^= buf[i];
        }
        if chk != buf[flen - 1] {
            // 校验失败：丢一个字节继续找（可能是帧内出现的伪帧头）。
            buf.copy_within(1..*len, 0);
            *len -= 1;
            continue;
        }
        let cmd = buf[2];
        let plen = flen - 5;
        let mut payload = [0u8; 12];
        payload[..plen].copy_from_slice(&buf[4..4 + plen]);
        buf.copy_within(flen..*len, 0);
        *len -= flen;
        return Some((cmd, payload, plen));
    }
}

/// mtime 毫秒（24MHz tick → ms，u64 防跨钟差为负）。
fn now_ms() -> u64 {
    use platform::Timer as _;
    chip_k3_rt24::clint_k3::TIMER.now() / 24_000
}

/// 发一帧并限时等一帧合法应答（先清残留输入，对齐 Python `_send_cmd`
/// 的 `reset_input_buffer()` 语义）。调用者须独占该 UART。
fn transact(uart: &PxaUart, cmd: u8, payload: &[u8], timeout_ms: u64) -> Option<(u8, [u8; 12], usize)> {
    let mut txb = [0u8; 16];
    let n = build_frame(cmd, payload, &mut txb);
    while uart.read_raw().is_some() {} // 清残留
    uart.write_raw(&txb[..n]);
    let mut rx = [0u8; RX_BUF];
    let mut rxlen = 0usize;
    let deadline = now_ms() + timeout_ms;
    while now_ms() < deadline {
        rxlen = drain_uart(uart, &mut rx, rxlen);
        if let Some(frame) = parse_frame(&mut rx, &mut rxlen) {
            return Some(frame);
        }
    }
    None
}

// ============================================================================
// zp10s 机械臂协议（纯写 ASCII）
// ============================================================================

/// 角度 → 脉宽：500 + angle/270*2000，限幅 [500, 2500]（对齐 AKA-00）。
fn angle_to_pulse(angle: u16) -> u32 {
    (500 + (angle as u32 * 2000) / 270).clamp(500, 2500)
}

/// 生成 `#<servo>P<pulse>T1000!` 指令帧，返回字节切片（借用 out）。
fn angle_cmd(servo: u8, angle: u16, out: &mut [u8; 16]) -> &[u8] {
    struct W<'a> {
        buf: &'a mut [u8],
        len: usize,
    }
    impl core::fmt::Write for W<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let b = s.as_bytes();
            if self.len + b.len() > self.buf.len() {
                return Err(core::fmt::Error);
            }
            self.buf[self.len..self.len + b.len()].copy_from_slice(b);
            self.len += b.len();
            Ok(())
        }
    }
    let mut w = W { buf: out, len: 0 };
    use core::fmt::Write as _;
    let _ = write!(w, "#{:03}P{:04}T1000!", servo, angle_to_pulse(angle));
    let used = w.len;
    &out[..used]
}

/// 力矩指令：#255PULK（释放）/ #255PULR（恢复）。
const TORQUE_RELEASE: &[u8] = b"#255PULK";
const TORQUE_RESTORE: &[u8] = b"#255PULR";

// ── ZP10S 位姿常量（AKA-00 运行配置 arm_angles.json，同其 angle_config.py
//    内置默认一致；2026-09-04 板测校准）─────────────────────────────────
// servo0/servo1 = 臂关节，servo2 = 夹爪；夹爪大角度 = 张开、小角度 = 闭合。
// 注意：不要用仓库里的 arm_angles_default.json（过时样本，夹爪极性相反——
// 板测 grab 变成"收爪下探、到底张开"，2026-09-05 对照上游运行值修正）。
const POSE_GRAB_S0: u16 = 245;
const POSE_GRAB_S1: u16 = 180;
const POSE_LIFT_S0: u16 = 200;
const POSE_LIFT_S1: u16 = 180;
const GRIPPER_OPEN: u16 = 150;
const GRIPPER_CLOSE: u16 = 90;

// ============================================================================
// chassis 状态（RPC handler 同步读 / 任务写，全原子量）
// ============================================================================

/// INIT/CONFIG 是否已获 ACK。
static CH_INITED: AtomicBool = AtomicBool::new(false);
static CH_RPM_L: AtomicI32 = AtomicI32::new(0);
static CH_RPM_R: AtomicI32 = AtomicI32::new(0);
static CH_ENC_M1: AtomicI32 = AtomicI32::new(0);
static CH_ENC_M2: AtomicI32 = AtomicI32::new(0);
/// 遥测/查询失败累计（诊断）。
static CH_ERR: AtomicU32 = AtomicU32::new(0);
/// 最近一次成功遥测的时刻（ms；0 = 从未）。
static CH_LAST_MS: AtomicU64 = AtomicU64::new(0);

/// 待应用的 setpoint（handler 写 / 任务读后清 dirty）。i16 语义同协议 ±100。
static CH_SET_L: AtomicI32 = AtomicI32::new(0);
static CH_SET_R: AtomicI32 = AtomicI32::new(0);
static CH_SET_DIRTY: AtomicBool = AtomicBool::new(false);
/// 停车请求：0=无，1=滑行 STOP，2=刹车 BRAKE（优先于 setpoint）。
static CH_STOP_REQ: AtomicU8 = AtomicU8::new(0);

/// 待处理的 INIT 请求（单槽；忙时 handler 直接拒绝）。
static CH_INIT_RID: AtomicU64 = AtomicU64::new(0);
static CH_INIT_PPR: AtomicU16 = AtomicU16::new(0);
static CH_INIT_PWM: AtomicU16 = AtomicU16::new(0);
static CH_INIT_PENDING: AtomicBool = AtomicBool::new(false);

/// 大端 i16/i32 解包。
fn be_i16(b: &[u8]) -> i16 {
    i16::from_be_bytes([b[0], b[1]])
}
fn be_i32(b: &[u8]) -> i32 {
    i32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// chassis 任务：10ms 节拍；INIT 处理 + setpoint 应用 + 100ms 遥测。
#[executor::task]
pub async fn task_chassis() {
    let Some(uart) = pxa_uart::port(PORT_CHASSIS) else {
        log::warn!("[robot] chassis port (slot {}) not probed, task idle", PORT_CHASSIS);
        return;
    };
    let mut tick: u32 = 0;
    loop {
        // ① 待处理 INIT：INIT→CONFIG 两段 transact，ACK 后异步补响应。
        if CH_INIT_PENDING.swap(false, Ordering::AcqRel) {
            let rid = CH_INIT_RID.load(Ordering::Relaxed);
            let ppr = CH_INIT_PPR.load(Ordering::Relaxed);
            let pwm = CH_INIT_PWM.load(Ordering::Relaxed);
            // CONFIG payload = struct.pack(">HH", ppr, pwm_freq)。
            let mut cfg = [0u8; 4];
            cfg[0..2].copy_from_slice(&ppr.to_be_bytes());
            cfg[2..4].copy_from_slice(&pwm.to_be_bytes());
            let ok = matches!(transact(uart, CMD_INIT, &[], 200), Some((RSP_ACK, _, _)))
                && matches!(transact(uart, CMD_CONFIG, &cfg, 200), Some((RSP_ACK, _, _)));
            CH_INITED.store(ok, Ordering::Release);
            respond(rid, ok as u32);
        }

        // ② 停车请求（优先）/ setpoint 变化 → fire-and-forget 帧。
        match CH_STOP_REQ.swap(0, Ordering::AcqRel) {
            2 => send_frame(uart, CMD_BRAKE, &[2]),
            1 => send_frame(uart, CMD_STOP, &[2]),
            _ => {
                if CH_SET_DIRTY.swap(false, Ordering::AcqRel) {
                    let l = CH_SET_L.load(Ordering::Relaxed).clamp(-100, 100) as i16;
                    let r = CH_SET_R.load(Ordering::Relaxed).clamp(-100, 100) as i16;
                    let mut pl = [0u8; 4];
                    pl[0..2].copy_from_slice(&l.to_be_bytes());
                    pl[2..4].copy_from_slice(&r.to_be_bytes());
                    send_frame(uart, CMD_SET_SPEEDS, &pl);
                }
            }
        }

        // ③ 遥测：每 10 拍（100ms）一轮 GET_ENCODER + GET_RPM。
        tick = tick.wrapping_add(1);
        if tick % 10 == 0 && CH_INITED.load(Ordering::Acquire) {
            telemetry(uart);
        }

        futures::timer::after(10.millis()).await;
    }
}

/// 发一帧 fire-and-forget（不等应答）。
fn send_frame(uart: &PxaUart, cmd: u8, payload: &[u8]) {
    let mut txb = [0u8; 16];
    let n = build_frame(cmd, payload, &mut txb);
    uart.write_raw(&txb[..n]);
}

/// 一轮遥测：编码器（>ii M1/M2）+ 转速（两帧 0x90，mid0=右 mid1=左）。
fn telemetry(uart: &PxaUart) {
    if let Some((_, pl, n)) = transact(uart, CMD_GET_ENCODER, &[], 100) {
        if n >= 8 {
            CH_ENC_M1.store(be_i32(&pl[0..4]), Ordering::Release);
            CH_ENC_M2.store(be_i32(&pl[4..8]), Ordering::Release);
        }
    } else {
        CH_ERR.fetch_add(1, Ordering::Relaxed);
    }
    if let Some((RSP_RPM_DATA, pl, n)) = transact(uart, CMD_GET_RPM, &[2], 100) {
        if n >= 3 {
            let rpm = be_i16(&pl[1..3]) as i32;
            match pl[0] {
                0 => CH_RPM_R.store(rpm, Ordering::Release),
                1 => CH_RPM_L.store(rpm, Ordering::Release),
                _ => {}
            }
        }
    } else {
        CH_ERR.fetch_add(1, Ordering::Relaxed);
    }
    // GET_RPM mid=2 回两帧：第二帧（另一侧）再收一拍。
    if let Some((RSP_RPM_DATA, pl, n)) = recv_one(uart, 50) {
        if n >= 3 {
            let rpm = be_i16(&pl[1..3]) as i32;
            match pl[0] {
                0 => CH_RPM_R.store(rpm, Ordering::Release),
                1 => CH_RPM_L.store(rpm, Ordering::Release),
                _ => {}
            }
        }
    }
    CH_LAST_MS.store(now_ms(), Ordering::Release);
}

/// 不发帧，限时收一帧合法应答（GET_RPM 第二帧用）。
fn recv_one(uart: &PxaUart, timeout_ms: u64) -> Option<(u8, [u8; 12], usize)> {
    let mut rx = [0u8; RX_BUF];
    let mut rxlen = 0usize;
    let deadline = now_ms() + timeout_ms;
    while now_ms() < deadline {
        rxlen = drain_uart(uart, &mut rx, rxlen);
        if let Some(frame) = parse_frame(&mut rx, &mut rxlen) {
            return Some(frame);
        }
    }
    None
}

// ============================================================================
// arm 命令队列 + 任务
// ============================================================================

/// 机械臂命令（RPC handler 生产 / arm 任务消费）。
pub enum ArmCmd {
    /// 单舵机角度（0-270°）。
    Angle { servo: u8, angle: u16 },
    /// 力矩释放（true）/ 恢复（false）。
    Torque { release: bool },
    /// 完整抓取序列（完成后按 rid 异步补响应 0）。
    Grab { rid: u64 },
    /// 张开夹爪（完成后按 rid 异步补响应 0）。
    Release { rid: u64 },
}

/// SPSC 命令环（单生产者 = RPC 服务任务，单消费者 = arm 任务）。
///
/// SAFETY（Sync）：仅单 hart 系统（K3 RT24 唯一 rcpu1 hart）上使用；即便
/// P1 任务抢占 P2 生产者，index 的 Release/Acquire 发布序保证槽数据可见，
/// UnsafeCell 槽只在 head/tail 推进后由对端访问（生产者独占未发布槽、
/// 消费者独占已发布槽，永不重叠）。
struct ArmRing {
    slots: UnsafeCell<[Option<ArmCmd>; 8]>,
    head: AtomicUsize, // 消费者
    tail: AtomicUsize, // 生产者
    dropped: AtomicU32,
}

// SAFETY：见结构体文档——单 hart + SPSC 索引发布序保证无数据竞争。
unsafe impl Sync for ArmRing {}

static ARM_RING: ArmRing = ArmRing {
    slots: UnsafeCell::new([const { None }; 8]),
    head: AtomicUsize::new(0),
    tail: AtomicUsize::new(0),
    dropped: AtomicU32::new(0),
};

impl ArmRing {
    /// 入队（满则丢弃并计数，返回是否成功）。
    fn push(&self, cmd: ArmCmd) -> bool {
        let t = self.tail.load(Ordering::Relaxed);
        let h = self.head.load(Ordering::Acquire);
        if t.wrapping_sub(h) >= 8 {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        // SAFETY：单生产者，tail 尚未发布，该槽独占。
        unsafe { (*self.slots.get())[t % 8] = Some(cmd) };
        self.tail.store(t.wrapping_add(1), Ordering::Release);
        true
    }

    /// 出队（空返回 None）。
    fn pop(&self) -> Option<ArmCmd> {
        let h = self.head.load(Ordering::Relaxed);
        let t = self.tail.load(Ordering::Acquire);
        if h == t {
            return None;
        }
        // SAFETY：单消费者，tail 已推进（Acquire），槽数据已发布且独占。
        let cmd = unsafe { (*self.slots.get())[h % 8].take() };
        self.head.store(h.wrapping_add(1), Ordering::Release);
        cmd
    }

    /// 已丢命令计数（诊断）。
    fn dropped(&self) -> u32 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// 臂指令经 AP UART5 阻塞发出（send 末尾等 LSR.TEMT，整帧移出线才返回，
/// ~1.3ms@115200——调用方后续 sleep 自帧尾起算）。单生产者：仅本任务调用。
fn arm_send(bytes: &[u8]) {
    let n = ap_uart::send(bytes);
    if n != bytes.len() {
        log::warn!("[robot] ap-uart send short: {}/{}", n, bytes.len());
    }
}

/// arm 任务：10ms 节拍消费命令；多步序列内 async sleep 对齐 AKA-00 时序。
#[executor::task]
pub async fn task_arm() {
    loop {
        while let Some(cmd) = ARM_RING.pop() {
            match cmd {
                ArmCmd::Angle { servo, angle } => {
                    let mut b = [0u8; 16];
                    arm_send(angle_cmd(servo, angle, &mut b));
                }
                ArmCmd::Torque { release } => {
                    arm_send(if release { TORQUE_RELEASE } else { TORQUE_RESTORE });
                }
                ArmCmd::Grab { rid } => {
                    // 张开夹爪 → 0.5s → 夹取位姿 → 1s → 闭合 → 2s → 抬起。
                    let mut b = [0u8; 16];
                    arm_send(angle_cmd(2, GRIPPER_OPEN, &mut b));
                    futures::timer::after(500.millis()).await;
                    arm_send(angle_cmd(0, POSE_GRAB_S0, &mut b));
                    arm_send(angle_cmd(1, POSE_GRAB_S1, &mut b));
                    futures::timer::after(1000.millis()).await;
                    arm_send(angle_cmd(2, GRIPPER_CLOSE, &mut b));
                    futures::timer::after(2000.millis()).await;
                    arm_send(angle_cmd(0, POSE_LIFT_S0, &mut b));
                    arm_send(angle_cmd(1, POSE_LIFT_S1, &mut b));
                    respond(rid, 0);
                }
                ArmCmd::Release { rid } => {
                    let mut b = [0u8; 16];
                    arm_send(angle_cmd(2, GRIPPER_OPEN, &mut b));
                    futures::timer::after(500.millis()).await;
                    respond(rid, 0);
                }
            }
        }
        futures::timer::after(10.millis()).await;
    }
}

// ============================================================================
// RPC handler 侧入口（intercom.rs 调用）
// ============================================================================

/// `CHASSIS_SET_SPEED`：写 setpoint（i16 ±100 语义），任务 10ms 内应用。
pub fn chassis_set_speed(left: i16, right: i16) {
    CH_SET_L.store(left as i32, Ordering::Relaxed);
    CH_SET_R.store(right as i32, Ordering::Relaxed);
    CH_SET_DIRTY.store(true, Ordering::Release);
}

/// `CHASSIS_STOP`：brake=2 刹车 / 1 滑行（任务下拍应用，优先于 setpoint）。
pub fn chassis_stop(brake: u8) {
    CH_STOP_REQ.store(brake, Ordering::Release);
}

/// `CHASSIS_GET` 快照（nonce 回显 + 遥测 + 状态）。
pub fn chassis_get(nonce: u32) -> (u32, u32, i32, i32, i32, i32, u32, u64) {
    (
        nonce,
        CH_INITED.load(Ordering::Acquire) as u32,
        CH_RPM_L.load(Ordering::Acquire),
        CH_RPM_R.load(Ordering::Acquire),
        CH_ENC_M1.load(Ordering::Acquire),
        CH_ENC_M2.load(Ordering::Acquire),
        CH_ERR.load(Ordering::Relaxed),
        CH_LAST_MS.load(Ordering::Acquire),
    )
}

/// `CHASSIS_INIT`（acall）：登记请求，任务完成 INIT+CONFIG 后补响应。
/// 忙（已有未完成 INIT）时立即回 0xFFFFFFFF 表示拒绝。
pub fn chassis_init(rid: u64, ppr: u16, pwm_freq: u16) {
    if CH_INIT_PENDING.load(Ordering::Acquire) {
        respond(rid, u32::MAX);
        return;
    }
    CH_INIT_RID.store(rid, Ordering::Relaxed);
    CH_INIT_PPR.store(ppr, Ordering::Relaxed);
    CH_INIT_PWM.store(pwm_freq, Ordering::Relaxed);
    CH_INIT_PENDING.store(true, Ordering::Release);
}

/// `ARM_SET_ANGLE`：入队（0-270°，超界在协议层由脉宽限幅兜底）。
pub fn arm_set_angle(servo: u8, angle: u16) {
    ARM_RING.push(ArmCmd::Angle { servo, angle });
}

/// `ARM_TORQUE`：入队力矩释放（true）/恢复（false）。
pub fn arm_torque(release: u8) {
    ARM_RING.push(ArmCmd::Torque { release: release != 0 });
}

/// `ARM_GRAB`（acall）：入队抓取序列，完成后按 rid 补响应。
pub fn arm_grab(rid: u64, _nonce: u32) {
    if !ARM_RING.push(ArmCmd::Grab { rid }) {
        respond(rid, u32::MAX); // 队满拒绝
    }
}

/// `ARM_RELEASE`（acall）：入队张开夹爪，完成后按 rid 补响应。
pub fn arm_release(rid: u64, _nonce: u32) {
    if !ARM_RING.push(ArmCmd::Release { rid }) {
        respond(rid, u32::MAX);
    }
}

/// 机械臂通道诊断（命令环丢弃计数；AP UART 轮询发送无队列，不丢字节）。
pub fn arm_dropped() -> u32 {
    ARM_RING.dropped()
}

/// 异步完成响应：构造 Response 经 CH1 + 门铃补发（AP recv_for(rid) 闭环）。
///
/// # Preconditions
/// 仅在共享窗就绪后可达（请求本身经 RPC 到达，隐含 init 完成）。
pub fn respond(rid: u64, result: u32) {
    match Message::response(rid, &result) {
        Ok(msg) => crate::intercom::send_message(msg),
        Err(e) => log::warn!("[robot] response serialize failed: {:?}", e),
    }
}
