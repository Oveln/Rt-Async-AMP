#!/usr/bin/env python3
"""robot.py —— robot-ctl 的 Python 封装（对齐 AKA-00 的控制接口）。

用法（板上，需先部署 robot-ctl 并运行 k3-robot-ctrl 固件）：

    from robot import Robot
    r = Robot("/tmp/robot-ctl")
    r.init()                    # 底盘 INIT+CONFIG（AKA-00 TtPidChassis 构造语义）
    r.set_speed(30, 30)         # 双轮速度 ±100（MotorPairProtocol.set_speed）
    r.brake()
    print(r.get_encoder())      # 编码器累计脉冲 (M1, M2)
    print(r.get_speeds())       # 实时 RPM (left, right)
    r.set_angle(2, 120)         # 单舵机角度 0-270（ServoProtocol.set_angle）
    r.grab()                    # 抓取全序列（阻塞 ~4.5s）
    r.release()                 # 张开夹爪

接口名刻意对齐 AKA-00 的 MotorPairProtocol / ServoProtocol（sleep→stop 的
Python 关键字冲突除外）。所有方法返回响应 dict（含 ok 字段）。
"""

import json
import subprocess


class Robot:
    def __init__(self, bin_path: str = "./robot-ctl"):
        self.p = subprocess.Popen(
            [bin_path, "serve"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
            bufsize=1,
        )

    def _rpc(self, op: str, **kw) -> dict:
        self.p.stdin.write(json.dumps({"op": op, **kw}) + "\n")
        self.p.stdin.flush()
        line = self.p.stdout.readline()
        if not line:
            raise RuntimeError("robot-ctl serve exited")
        return json.loads(line)

    # ── 底盘（MotorPairProtocol）─────────────────────────────────────
    def init(self, ppr: int = 4680, pwm_freq: int = 20000) -> dict:
        """INIT + CONFIG（构造语义：失败时 result=0）。"""
        return self._rpc("init", ppr=ppr, pwm=pwm_freq)

    def set_speed(self, left: int, right: int) -> dict:
        return self._rpc("set_speed", left=left, right=right)

    def brake(self) -> dict:
        return self._rpc("brake")

    def stop(self) -> dict:
        """滑行停（AKA-00 sleep()）。"""
        return self._rpc("stop")

    def get_speeds(self) -> tuple:
        """实时 RPM (left, right)。"""
        r = self._rpc("get")
        return r.get("rpm_left", 0), r.get("rpm_right", 0)

    def get_encoder(self) -> tuple:
        """编码器累计脉冲 (M1, M2)。"""
        r = self._rpc("get")
        return r.get("enc_m1", 0), r.get("enc_m2", 0)

    def status(self) -> dict:
        return self._rpc("status")

    # ── 机械臂（ServoProtocol，ZP10S）────────────────────────────────
    def set_angle(self, servo_id: int, angle: int) -> dict:
        return self._rpc("set_angle", servo=servo_id, angle=angle)

    def release_torque(self) -> dict:
        return self._rpc("torque", release=1)

    def restoring_torque(self) -> dict:
        return self._rpc("torque", release=0)

    def grab(self) -> dict:
        """抓取全序列：张开→夹取位姿→闭合→抬起（阻塞 ~4.5s）。"""
        return self._rpc("grab")

    def release(self) -> dict:
        """张开夹爪（阻塞 ~0.5s）。"""
        return self._rpc("release")

    # ── raw UART 诊断（bring-up / 排针实验 E1/E2）────────────────────
    def uart_write(self, port: int, data: bytes) -> dict:
        return self._rpc("uwrite", port=port, hex=data.hex().upper())

    def uart_read(self, port: int, max: int = 32) -> bytes:
        r = self._rpc("uread", port=port, max=max)
        return bytes.fromhex(r.get("hex", ""))

    def close(self) -> None:
        self.p.terminate()
        self.p.wait()


if __name__ == "__main__":
    r = Robot()
    print(json.dumps(r.status(), ensure_ascii=False))
