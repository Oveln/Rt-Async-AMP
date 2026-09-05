//! robot —— rtsh 的原生 Python 扩展（PyO3）。
//!
//! 把 rtsh 库层（`user-apps/rtsh/src/lib.rs`：/dev/rt_shm 共享窗 + ov-rpc
//! 服务发现 + 机器人语义方法）直接绑定成 Python 类，不经子进程 / 管道
//! 协议。rtsh 是唯一用户态程序（REPL / 单发），本扩展是它的 Python 库
//! 形态——两者同一套实现，方法名经服务发现解析 mid。
//!
//! ```python
//! from robot import Robot, RobotError
//! r = Robot()                 # 打开 /dev/rt_shm + 服务发现
//! r.init()                    # 底盘 INIT+CONFIG（对齐 AKA-00 TtPidChassis）
//! r.set_speed(30, 30)         # 双轮速度 ±100
//! r.brake()
//! print(r.get_encoder())      # (M1, M2)
//! print(r.get_speeds())       # (left, right)
//! r.set_angle(2, 120)         # 单舵机 0-270°
//! r.grab(); r.release()       # 抓取全序列 / 张爪（阻塞至完成）
//! ```
//!
//! 接口名对齐 AKA-00 的 MotorPairProtocol / ServoProtocol（同此前的
//! robot.py 管道封装；本扩展与 robot.py 的 import 名相同，同目录共存时
//! `.so` 优先加载——两者 API 兼容，迁移无感）。
//!
//! ## 阻塞与 GIL
//!
//! 所有等响应的调用（call/acall）在等待期间**释放 GIL**（`Python::detach`，
//! 即旧版 `allow_threads`），`grab()` 阻塞约 4.5s 也不会卡住解释器其它
//! 线程。超时经 ppoll 实现（见 rtsh lib 文档），全程不使用信号，与
//! `signal` 模块无冲突。
//!
//! 内层经 [`std::sync::Mutex`] 串行化：rt_shm 驱动的 IPC waker 为单槽、
//! RPC rid 匹配无锁表，同一实例并发调用本就不被协议允许——这里用锁把
//! "单实例一次一个调用者"表达为线程安全语义（后来的线程阻塞等锁，等
//! 锁期间同样不持 GIL）。
//!
//! ## 错误
//!
//! Rust 侧 `Err` 一律抛 [`RobotError`]（message 为原错误串）。
//!
//! ## 并发约束
//!
//! 需要真正并发（如一边运动一边查遥测）就开两个实例——但两个实例等
//! 响应仍共用驱动的单槽 waker，同时只有一个线程能处在等待态，第二个
//! 等待者会覆盖第一个的 waker（驱动文档明示的边界）。常规用法（单线程
//! 编排 / 异步任务排队）不受影响。

use std::sync::Mutex;

use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::PyDict;

pyo3::create_exception!(robot, RobotError, PyException, "rt-async-amp 机器人控制错误");

/// rtsh::Shell 的 Python 绑定（构造即打开 /dev/rt_shm 并做服务发现）。
///
/// 内层 Mutex 见模块文档「阻塞与 GIL」：协议纪律的串行化 + 等待期释放
/// GIL（锁在 detach 闭包内获取，等待锁也不占 GIL）。
#[pyclass]
struct Robot {
    sh: Mutex<rtsh::Shell>,
}

/// Err(String) → RobotError。
fn to_py(e: String) -> PyErr {
    RobotError::new_err(e)
}

impl Robot {
    /// 在释放 GIL 的闭包内取锁并执行 f（闭包 Send：仅捕获 &Mutex）。
    fn with_inner<T>(
        &self,
        py: Python<'_>,
        f: impl FnOnce(&mut rtsh::Shell) -> Result<T, String> + Send,
    ) -> PyResult<T>
    where
        T: Send,
    {
        py.detach(|| {
            let mut g = self.sh.lock().unwrap_or_else(|p| p.into_inner());
            f(&mut g)
        })
        .map_err(to_py)
    }
}

#[pymethods]
impl Robot {
    /// 打开 /dev/rt_shm + 服务发现（RP 固件未起 / 无服务发现时抛 RobotError）。
    /// 两步都可能阻塞（就绪轮询 / 发现响应），统一在释放 GIL 后执行。
    #[new]
    fn new(py: Python<'_>) -> PyResult<Self> {
        let sh = py
            .detach(|| {
                let mut sh = rtsh::Shell::open()?;
                sh.discover().map(|()| sh)
            })
            .map_err(to_py)?;
        Ok(Self { sh: Mutex::new(sh) })
    }

    /// `UART_STATUS`：dict(ports, chassis_inited, chassis_err, arm_dropped)。
    fn status(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let s = self.with_inner(py, |sh| sh.robot_status())?;
        let d = PyDict::new(py);
        d.set_item("ports", s.ports)?;
        d.set_item("chassis_inited", s.chassis_inited)?;
        d.set_item("chassis_err", s.chassis_err)?;
        d.set_item("arm_dropped", s.arm_dropped)?;
        Ok(d.into_any().unbind())
    }

    /// 底盘 INIT+CONFIG，等 ACK；返回固件结果码（0=成功）。
    #[pyo3(signature = (ppr = 4680, pwm = 20000))]
    fn init(&mut self, py: Python<'_>, ppr: u16, pwm: u16) -> PyResult<u32> {
        self.with_inner(py, |sh| sh.robot_init(ppr, pwm))
    }

    /// 双轮速度 ±100（send，立即返回）。
    fn set_speed(&mut self, left: i16, right: i16) -> PyResult<()> {
        let mut g = self.sh.lock().unwrap_or_else(|p| p.into_inner());
        g.robot_set_speed(left, right).map_err(to_py)
    }

    /// 滑行停车（send）。
    fn stop(&mut self) -> PyResult<()> {
        let mut g = self.sh.lock().unwrap_or_else(|p| p.into_inner());
        g.robot_stop(false).map_err(to_py)
    }

    /// 刹车（send）。
    fn brake(&mut self) -> PyResult<()> {
        let mut g = self.sh.lock().unwrap_or_else(|p| p.into_inner());
        g.robot_stop(true).map_err(to_py)
    }

    /// 遥测快照：dict(inited, rpm_left, rpm_right, enc_m1, enc_m2, err, last_ms)。
    fn get(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let t = self.with_inner(py, |sh| sh.robot_get())?;
        let d = PyDict::new(py);
        d.set_item("inited", t.inited)?;
        d.set_item("rpm_left", t.rpm_left)?;
        d.set_item("rpm_right", t.rpm_right)?;
        d.set_item("enc_m1", t.enc_m1)?;
        d.set_item("enc_m2", t.enc_m2)?;
        d.set_item("err", t.err)?;
        d.set_item("last_ms", t.last_ms)?;
        Ok(d.into_any().unbind())
    }

    /// 编码器累计脉冲 (M1, M2)（对齐 AKA-00 MotorPairProtocol.get_encoder）。
    fn get_encoder(&mut self, py: Python<'_>) -> PyResult<(i32, i32)> {
        let t = self.with_inner(py, |sh| sh.robot_get())?;
        Ok((t.enc_m1, t.enc_m2))
    }

    /// 实时转速 (left, right)（对齐 AKA-00 MotorPairProtocol.get_speeds）。
    fn get_speeds(&mut self, py: Python<'_>) -> PyResult<(i32, i32)> {
        let t = self.with_inner(py, |sh| sh.robot_get())?;
        Ok((t.rpm_left, t.rpm_right))
    }

    /// 单舵机角度（servo 0/1=关节，2=夹爪；0-270°；send）。
    fn set_angle(&mut self, servo: u8, angle: u16) -> PyResult<()> {
        let mut g = self.sh.lock().unwrap_or_else(|p| p.into_inner());
        g.robot_set_angle(servo, angle).map_err(to_py)
    }

    /// 力矩释放（可手动掰姿态）/ 恢复。
    #[pyo3(signature = (release = true))]
    fn torque(&mut self, release: bool) -> PyResult<()> {
        let mut g = self.sh.lock().unwrap_or_else(|p| p.into_inner());
        g.robot_torque(release).map_err(to_py)
    }

    /// 完整抓取序列（阻塞约 4.5s 至完成）；返回固件结果码。
    fn grab(&mut self, py: Python<'_>) -> PyResult<u32> {
        self.with_inner(py, |sh| sh.robot_grab())
    }

    /// 张开夹爪（阻塞约 0.5s）；返回固件结果码。
    fn release(&mut self, py: Python<'_>) -> PyResult<u32> {
        self.with_inner(py, |sh| sh.robot_release())
    }

    /// raw UART 写（bring-up 诊断）：data 为 bytes（≤32）。
    fn uwrite(&mut self, py: Python<'_>, port: u8, data: &[u8]) -> PyResult<u32> {
        self.with_inner(py, |sh| sh.robot_uwrite(port, data))
    }

    /// raw UART 读（清空当前 RX）：返回 bytes。
    #[pyo3(signature = (port, max = 32))]
    fn uread(&mut self, py: Python<'_>, port: u8, max: u8) -> PyResult<Vec<u8>> {
        self.with_inner(py, |sh| sh.robot_uread(port, max))
    }
}

/// 模块入口（部署文件名 robot.so / robot.cpython-314-riscv64-linux-gnu.so）。
#[pymodule]
fn robot(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Robot>()?;
    m.add("RobotError", py.get_type::<RobotError>())?;
    Ok(())
}
