# robot-py —— rtsh 库层的原生 Python 扩展

`import robot` 直接控制机器人，不经子进程 / 管道协议。实现为 PyO3 扩展
（cdylib），绑定 `user-apps/rtsh` 的库层（`rtsh::Shell`：/dev/rt_shm 共享窗
+ ov-rpc 服务发现 + 机器人语义方法）——与 rtsh（唯一用户态程序）同一套
实现，方法名经服务发现解析 mid，固件侧重编号无需重编本扩展。

配对固件：RP 侧 `k3-robot-ctrl`（协议、任务模型、RPC 方法表见
`apps/rt-async-k3/robot-ctrl.md`）。

## 构建

目标 ABI = **板上 rootfs 的 Python 3.14（Yocto 构建，riscv64 glibc）**——
与 user-apps 其它 crate 的 musl 静态线不同，本 crate 是 glibc 动态库例外。
Python 头文件取自 `~/riscv-yocto`（rootfs 构建环境）的 python3 recipe
sysroot；环境不在默认路径时用 `ROBOT_PY_CROSS_LIB_DIR` 指定。

```bash
cargo xtask build robot-py
# → build/robot.cpython-314-riscv64-linux-gnu.so
```

直接 cargo（xtask 内部即此命令）：

```bash
PYO3_CROSS=1 PYO3_CROSS_PYTHON_VERSION=3.14 \
PYO3_CROSS_LIB_DIR=~/riscv-yocto/build/tmp/work/riscv64imafdc-poky-linux/python3/3.14.2/sysroot-destdir/usr \
CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_GNU_LINKER=riscv64-linux-gnu-gcc \
  cargo build --release -p robot-py --target riscv64gc-unknown-linux-gnu
```

host 上 `cargo check -p robot-py` 可做纯类型检查（按 host Python 解析
PyO3 配置，无需交叉环境）。

## 部署

```bash
# Host 起服务（build/ 目录）：
python3 -m http.server -d build 8000
# 板上（StarryOS shell）：wget 到 sys.path 内目录（如 /tmp）：
wget http://<host-ip>:8000/robot.cpython-314-riscv64-linux-gnu.so -O /tmp/robot.cpython-314-riscv64-linux-gnu.so
python3 - <<'PY'
import sys; sys.path.insert(0, "/tmp")
from robot import Robot
r = Robot()
PY
```

## API

接口名对齐 AKA-00 的 `MotorPairProtocol` / `ServoProtocol`（此前 robot.py
管道封装的同名方法；与 robot.py 共存时 `.so` 优先加载，API 兼容）。

| 方法 | 签名 | 返回 | 说明 |
|------|------|------|------|
| `Robot()` | — | — | 打开 /dev/rt_shm + 服务发现；失败抛 `RobotError` |
| `status` | — | dict | ports / chassis_inited / chassis_err / arm_dropped |
| `init` | ppr=4680, pwm=20000 | int | 底盘 INIT+CONFIG 等 ACK（acall）；0=成功 |
| `set_speed` | left, right（±100） | — | 双轮速度（send） |
| `stop` / `brake` | — | — | 滑行停 / 刹车（send） |
| `get` | — | dict | inited / rpm_left / rpm_right / enc_m1 / enc_m2 / err / last_ms |
| `get_encoder` | — | (M1, M2) | 编码器累计脉冲 |
| `get_speeds` | — | (left, right) | 实时 RPM |
| `set_angle` | servo, angle（0-270°） | — | 单舵机（0/1=关节，2=夹爪；send） |
| `torque` | release=True | — | 力矩释放 / 恢复 |
| `grab` | — | int | 抓取全序列，阻塞约 4.5s 至完成（acall） |
| `release` | — | int | 张开夹爪，阻塞约 0.5s（acall） |
| `uwrite` | port, data: bytes（≤32） | int | raw UART 写（bring-up 诊断） |
| `uread` | port, max=32 | bytes | raw UART 读 |

所有 Rust 侧错误（超时、队满、端口未 probe、固件未提供方法……）统一抛
`robot.RobotError`，message 为原始错误串。

```python
from robot import Robot, RobotError

r = Robot()
r.init()                      # 底盘上电
r.set_speed(30, 30); ...; r.brake()
r.set_angle(0, 150); r.set_angle(1, 180)     # 臂归位
r.grab(); r.release()         # 抓取演示（阻塞至动作完成）
print(r.get_speeds(), r.get_encoder())
```

## 语义细节

- **GIL**：等响应的调用（call/acall）等待期间释放 GIL，`grab()` 阻塞
  4.5s 也不会卡住解释器其它线程。超时经 ppoll 实现（rt_shm 驱动
  Pollable + 内核侧超时），**全程不用信号**，与 `signal` 模块无冲突。
- **并发**：单实例一次只允许一个线程调用（协议约束，内部 Mutex 串行
  化——后来的线程阻塞等锁，等锁期间不占 GIL）。需要并发就开两个实例，
  但驱动的 IPC waker 为单槽，同时只能有一个线程处在等待态；常规单线程
  编排不受影响。
- **超时**：普通调用 3s / 慢命令（init、release）10s / grab 15s，与
  rtsh 一致（`rtsh::T_CALL_MS` 等常量）。
- **acall 语义**：init/grab/release 的请求立即入队、动作完成后才补响应，
  Python 侧表现为方法阻塞至动作结束。

## 目录

```
robot-py/
├── Cargo.toml   # lib name = robot（PyInit_robot），cdylib
├── README.md
└── src/lib.rs   # #[pyclass] Robot（Mutex<rtsh::Shell>）+ RobotError
```
