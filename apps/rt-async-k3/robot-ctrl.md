# robot_ctrl —— K3 机器人控制固件

`robot_ctrl` bin 跑在 K3 RT24 实时小核（rcpu1，CVA6/RV64GC）上，通过两条
UART 通道驱动 AKA-00 桌面机器人：底盘（ESP32-C3 PID 电机控制器，tt_pid
二进制帧协议）与机械臂（众灵 ZP10S 串口总线舵机，ASCII 协议）。AP 侧
（StarryOS 用户态）经 ov-rpc 语义方法远程控制，不接触任何串口细节。

代码位置：

| 文件 | 内容 |
|------|------|
| `src/robot.rs` | 两个串口协议栈 + 命令队列 + P1 协议任务 + RPC handler 入口 |
| `src/bin/robot_ctrl.rs` | 固件装配：四个任务的优先级与 spawn |
| `src/intercom.rs` | RPC 方法表（含机器人语义方法，mid 8-18） |

AP 侧配套：`user-apps/rtsh`（唯一用户态程序，REPL / `rtsh <cmd>` 单发，
机器人命令与通用 RPC 同壳）与 `user-apps/robot-py`（rtsh 库层的原生
Python 扩展，`import robot` 直接调用）。

## 1. 通道拓扑与接线

| 通道 | 控制器 | 40pin 引脚 | 电平 | 协议 | 波特率 |
|------|--------|-----------|------|------|--------|
| 底盘（slot 0） | R_UART0（PXA 派生，IRQ 17） | pin29=TX / pin32=RX（pad GPIO_122/123，MUX_MODE4） | 1.8V pad → BTRD04A02 → 3.3V | tt_pid 二进制帧 | 115200-8N1 |
| 机械臂 | AP 域 UART5，TX-only LSR 轮询 | pin3=TX（pad GPIO_83 mode4，网络 I2C3_SDA，直连无电平转换） | 3.3V 原生 | ZP10S ASCII 纯写 | 115200-8N1 |

要点：

- **底盘与 console 共口**。R_UART0 是 com260 40pin 上唯一完整布线的
  R.UART（2026-08-27 原理图 k3-com260_kit_v02 实查），console log 与
  ESP32 帧共线可行：ESP32 按 `AA 55` 帧头重同步，ASCII 文本不构成有效
  帧头；RX 方向只有 ESP32 遥测驱动。共线纪律：协议运行期少打 log（文本
  会被 ESP32 丢弃，但占用 TX 线时间）。备选方案：M.2 槽空时引出
  R.UART4（TX=GPIO_80 / RX=GPIO_81），见 `robot.rs` 的 `PORT_CHASSIS` 注释。
- **机械臂走 AP 域 UART5 的原因**：RT24 对 AP 域 UART"总线可达、中断
  不可达"（中断只进 AP 侧 APLIC）；ZP10S 只收不发，TX 用 LSR 轮询即是
  完整通道。AP 引导链会把 pad83 重 mux 成 I2C3，驱动在每次发送前自查
  并重写（`pad83_heal`，详见 `chip_k3_rt24::ap_uart` 模块文档）。工作时钟
  由 probe 读 APBC/sel 寄存器自适应，不写死。
- **pin29/32 注意事项**：与 M.2 KEY-E 槽 SDIO 线共网，槽内不得插
  WiFi/BT 卡；3.3V 侧无强上拉，无对端驱动时线路浮空（真机场景不受影响，
  裸测需 USB 串口先插 TX 或外接 10kΩ 上拉到 pin1）。

## 2. 任务模型

```text
P1  task_chassis ── R_UART0(slot0，与 console 共口) ── 40pin pin29(TX)/pin32(RX)
P1  task_arm     ── AP 域 UART5 TX 轮询（pad83 mode4）── 40pin pin3
P2  task_ipc     ── 共享窗 + mailbox4（intercom：RPC 分发/异步完成）
P3  watchdog     ── magic 自愈（同 ipc_demo）
```

| 任务 | 优先级 | 节拍 | 职责 |
|------|--------|------|------|
| `task_chassis` | P1 | 10ms | 应用 setpoint / 处理 INIT（transact 等 ACK）；每 10 拍（100ms）一轮 GET_ENCODER + GET_RPM 遥测刷新快照 |
| `task_arm` | P1 | 10ms | 消费 `ArmCmd` 命令环；GRAB/RELEASE 多步序列内 async sleep 对齐 AKA-00 时序 |
| `task_ipc` | P2 | 弹性 | `process_elastic` 忙等处理 RPC，窗口过期后 `MBX3.recv()` 等 mailbox4 中断（IRQ 69）唤醒 |
| `magic_watchdog` | P3 | — | magic 字自愈，同 ipc_demo |

P1 高于 RPC 服务 P2 + 定时器抢占的意义：即便 P2 处在弹性自旋窗口（最长
约 2s）内不让出，timer ISR 唤醒的 P1 任务也会立即抢占执行——acall 异步
完成的响应延迟收敛到节拍级（≤10ms）。

并发约束（单 hart）：命令交接全部经原子量 / SPSC 环（Release/Acquire
发布序，防 P1 抢占 P2 造成的撕裂）；ZP10S 通道单写者；`UART_WRITE/READ`
（raw 诊断）与协议任务共用 R_UART0 仅限 bring-up 阶段，协议运行期不混用。

## 3. RPC 方法表（intercom.rs）

mid 0 为 INIT 服务发现，AP 侧按名解析，以下 id 仅为线上值：

| mid | 方法 | 类别 | 参数 → 返回 | 说明 |
|-----|------|------|------------|------|
| 8 | `UART_WRITE` | call | port, len, data[32] → 实发字节数 | raw 诊断写（bring-up 用） |
| 9 | `UART_READ` | call | port, max → (n, data[32]) | raw 诊断读 |
| 10 | `UART_STATUS` | call | nonce → (nonce, ports 掩码, chassis_inited, chassis_err, arm_dropped) | 状态查询 |
| 11 | `CHASSIS_SET_SPEED` | send | left, right（i16，语义 ±100） | fire-and-forget，10ms 内下发 |
| 12 | `CHASSIS_STOP` | send | brake：1=滑行 STOP，2=刹车 BRAKE | 优先于 setpoint |
| 13 | `CHASSIS_GET` | call | nonce → (nonce, inited, rpm_l, rpm_r, enc_m1, enc_m2, err, last_ms) | 同步读 100ms 内的遥测快照 |
| 14 | `CHASSIS_INIT` | acall | ppr, pwm_freq（默认 4680/20000）→ 0=成功 | INIT+CONFIG 两帧 ACK 后补响应；忙时立即回 `0xFFFFFFFF` |
| 15 | `ARM_SET_ANGLE` | send | servo(0/1=关节，2=夹爪), angle(0-270°) | 单舵机转角，发完即回 |
| 16 | `ARM_TORQUE` | send | release：1=释放（`#255PULK`），0=恢复（`#255PULR`） | 释放后可手动掰姿态 |
| 17 | `ARM_GRAB` | acall | nonce → 0=完成 | 完整抓取序列，约 4.5s；队满立即回 `0xFFFFFFFF` |
| 18 | `ARM_RELEASE` | acall | nonce → 0=完成 | 张开夹爪，约 0.5s |

类别语义：`send` 无响应；`call` 同步等响应；`acall` 请求立即入队返回，
P1 任务完成动作后经 CH1 + 门铃补发响应，AP 侧按同 rid 无感闭环
（`recv_for(rid)`）。`0xFFFFFFFF` 统一表示忙/队满拒绝。

## 4. 底盘协议（tt_pid，ESP32-C3）

帧格式：`AA 55 <cmd> <len> <payload…> <xor>`，xor 为 cmd 起逐字节异或。
运动指令 fire-and-forget，遥测为请求-响应（transact 前先清残留输入，
对齐 Python `reset_input_buffer()` 语义）：

| cmd | 方向 | payload | 应答 |
|-----|------|---------|------|
| 0x01 INIT | → | 无 | 0x80 ACK |
| 0x02 CONFIG | → | `>HH`：PPR(4680)、PWM 频率(20000) | 0x80 ACK |
| 0x11 STOP / 0x12 BRAKE | → | `02` | 无（fire-and-forget） |
| 0x13 SET_SPEEDS | → | `>hh`：左右轮 ±100 | 无 |
| 0x20 GET_RPM | → | mid（0=右 1=左 2=双） | 0x90 `(mid, >h rpm)`；mid=2 回两帧 |
| 0x22 GET_ENCODER | → | 无 | `>ii`：M1、M2 |

遥测失败计数进 `chassis_err`，最近成功时刻为 `last_ms`（ms，mtime 毫秒）。

## 5. 机械臂协议（ZP10S，ASCII 纯写）

- 角度指令：`#<id>P<pulse>T1000!`，`pulse = 500 + angle/270×2000` 限幅
  [500, 2500]，与 AKA-00 上游换算一致；T1000 = 1s 内到位。
- 力矩：`#255PULK`（释放）/ `#255PULR`（恢复），广播 id 255。
- 通道无应答（纯写），诊断靠命令环丢弃计数 `arm_dropped`（应恒为 0）。

抓取序列（`ARM_GRAB`，总长约 4.5s）：

```text
夹爪张开(150°) → 0.5s → 双关节到抓取位姿(245°/180°) → 1s
→ 夹爪闭合(90°) → 2s → 双关节到抬起位姿(200°/180°) → 补响应
```

`ARM_RELEASE`：夹爪张开 → 0.5s → 补响应。

## 6. 位姿常量与标定

位姿/夹爪角度是 `src/robot.rs` 中的编译期常量（无运行时配置文件）：

```rust
const POSE_GRAB_S0: u16 = 245;   // 抓取位姿，servo0
const POSE_GRAB_S1: u16 = 180;   // 抓取位姿，servo1
const POSE_LIFT_S0: u16 = 200;   // 抬起位姿，servo0
const POSE_LIFT_S1: u16 = 180;   // 抬起位姿，servo1
const GRIPPER_OPEN: u16 = 150;   // 夹爪张开
const GRIPPER_CLOSE: u16 = 90;   // 夹爪闭合
```

来源与教训：数值取自 AKA-00 仓库的**运行配置** `arm_angles.json`（与其
`angle_config.py` 内置默认一致）。2026-09-05 板测发现 grab 表现为"收爪
下探、到底反而张开、空手抬起"，根源是此前误抄了同仓库的
`arm_angles_default.json`——那是过时样本，夹爪极性相反（44/150）。经验：
**以车上实际加载的 `arm_angles.json` 为准，不以 `_default` 文件为准**；
夹爪方向为大角度=张开。

再标定流程：换机械臂或改装后，用 `arm S A` 逐
舵机试出合适角度，改上表常量后 `cargo xtask build k3-robot-ctrl` 重刷
固件。ZP10S 支持 0-270°，超界由脉宽限幅截断。

## 7. AP 侧使用速查

rtsh 单发与 REPL 命令同名（示例用单发）：

```text
status            # ports 掩码 / chassis_inited / arm_dropped（应恒为 0）
init [PPR PWM]    # 底盘 INIT+CONFIG，等 ACK（acall，约百毫秒级）
drive L R         # 双轮速度 ±100（send）
stop | brake      # 滑行 / 刹车
get               # 遥测快照：inited/rpm/编码器/err/last_ms
arm S A           # 单舵机角度，如 arm 0 150 归中、arm 2 150 张爪
torque [0/1]      # 力矩恢复/释放（默认释放；摆位后记得 torque 0 锁回）
grab | release    # 抓取全序列（阻塞约 4.5s）/ 张爪（约 0.5s）
uwrite/uread      # raw UART 读写（仅 bring-up）
```

演示流程（机械臂）：

```text
status        # 确认 RPC 通
torque 1      # 手动摆起始姿态（可选）
torque 0
arm 0 150 && arm 1 180   # 归位
grab          # 张爪→下探→合爪→抬起，完成才返回
release       # 放下
```

Python 原生库：robot-py 扩展（`import robot`）提供同一套方法，返回
类型化结果（dict / 元组）、错误抛 `RobotError`，详见 `user-apps/robot-py/README.md`。

## 8. 构建与刷写

```bash
cargo xtask build k3-robot-ctrl   # 产物 build/k3-com260/rt-async-k3-robot-ctrl.elf
ELF_SRC=build/k3-com260/rt-async-k3-robot-ctrl.elf bash scripts/flash/k3-pack-itb.sh
# U-Boot: fastboot -l $loadaddr -s 0x100000 usb 0   ← Host: fastboot stage esos.itb
#         Ctrl-C 后 mtd erase esos && mtd write esos $loadaddr && reset
```

依赖：opensbi.itb 须为 PMA 非缓存窗口固件（共享窗协议前提，见 README）；
AP 侧 StarryOS 起 `/dev/rt_shm` 后，固件 task_ipc 才退出 `wait_ready`。

## 9. 已知约束

- 单 hart 固件，`UART_WRITE/READ` 与协议任务不可并发使用同一串口
  （bring-up 期专用）。
- 机械臂通道为阻塞轮询发送（整帧约 1.3ms），命令环 8 槽，压入过快会
  丢弃并计入 `arm_dropped`；`grab`/`release` 队满直接拒绝。
- 底盘遥测周期 100ms，`CHASSIS_GET` 返回的是快照（控制环够用，不适合
  做硬实时反馈）。
- pin29/32 与 M.2 KEY-E 共网：槽内插卡会驱动 SDIO 线，与底盘通信冲突。
