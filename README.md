# rt-async-amp

QEMU RISC-V virt 双核 AMP：rt-async (hart 0, M-mode) + StarryOS (hart 1, S-mode via OpenSBI)

## 架构

```
QEMU virt (-smp 2 -m 256M)
├─ hart 0: OpenSBI → mret M-mode → rt-async @ 0x80800000 (UART1)
└─ hart 1: OpenSBI → mret S-mode → StarryOS @ 0x80200000 (UART0)

共享内存 IPC @ 0x88000000 (64KB)
```

## 快速开始

```bash
# 1. 初始化 submodule
git submodule update --init --recursive

# 2. 编译 rt-async app
make rt-async

# 3. 编译 OpenSBI (需要 riscv64 交叉编译工具链)
make opensbi

# 4. 编译 StarryOS
make starryos

# 5. 启动 QEMU
make run
# 或
./run.sh
```

## 目录结构

```
rt-async/          ← rt-async RTOS (submodule)
StarryOS/          ← StarryOS 内核 (submodule)
opensbi/           ← OpenSBI (带 hart 路由 patch, submodule)
modules/ipc/       ← 共享内存 IPC 协议定义
modules/chip-qemu-virt-rt/ ← UART1 芯片实现
apps/rt-async-app/ ← rt-async 侧应用
```

## OpenSBI hart 路由

OpenSBI 需要打 patch，在 `fw_base.S` 的启动路径中根据 `mhartid` 分发：

```asm
# hart 0 → rt-async (M-mode)
csrr t0, mhartid
bnez t0, .Lnormal_boot
la   t1, 0x80800000
csrw mepc, t1
li   t1, (0b11 << 11) | (1 << 7)  # MPP=M, MPIE=1
csrw mstatus, t1
mret
.Lnormal_boot:
# hart 1+: 正常 OpenSBI 流程
```

## IPC 通信

通过 CLINT MSIP 寄存器触发跨核中断：

- **StarryOS → rt-async**: SBI ecall → OpenSBI 写 MSIP0 → rt-async MachineSoft 中断
- **rt-async → StarryOS**: 写 MSIP1 → OpenSBI 收到 MSI → 设置 MIP.SSIP → StarryOS SSI 中断

共享内存使用无锁 SPSC 环形缓冲区，定义在 `modules/ipc/`。
