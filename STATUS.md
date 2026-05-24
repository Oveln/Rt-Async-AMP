# rt-async-amp 项目状态与后续计划

## 项目概述

RISC-V AMP (非对称多处理) 系统：hart 0 运行 rt-async (M-mode RTOS)，hart 1 运行 StarryOS (通过 OpenSBI 进入 S-mode)。目标平台 QEMU virt riscv64。

## 当前已完成

### 1. 项目骨架 ✓

```
rt-async-amp/
├── Cargo.toml              # workspace: ipc, chip-qemu-virt-rt, rt-async-app
├── Makefile                # make rt-async / opensbi / starryos / run
├── .gitignore              # /target, /build, /opensbi, *.bin, *.elf
├── .cargo/config.toml      # target = riscv64imac-unknown-none-elf
├── rust-toolchain.toml     # nightly-2026-04-25
├── memory.x                # ORIGIN = 0x80800000, LENGTH = 32M
├── run.sh                  # 独立启动脚本
├── apps/rt-async-app/      # 双任务 demo (task_a 500ms + task_b 700ms)
├── modules/
│   ├── ipc/                # 共享内存 IPC (SharedControl, RingBuffer, Message)
│   └── chip-qemu-virt-rt/  # QEMU virt 芯片 (UART0, CLINT, send_ipi_to_linux)
├── rt-async/               # git submodule (rt-async RTOS)
├── StarryOS/               # git submodule (StarryOS Linux)
└── opensbi/                # 克隆的 OpenSBI (非 submodule, 在 .gitignore 中)
```

### 2. OpenSBI hart 路由 ✓

文件: `opensbi/firmware/fw_base.S`（2 处 patch）

```asm
_start:
    /* hart 0: mret 到 0x80800000 (rt-async, M-mode) */
    csrr t0, mhartid
    bnez t0, .Lnormal_boot
    li t1, 0x80800000
    csrw mepc, t1
    li t1, ((0b11 << 11) | (1 << 7))   # MPP=M, MPIE=1
    csrw mstatus, t1
    mret
.Lnormal_boot:
    /* hart 1+: 跳过 fw_boot_hart 检查，直接抢 lottery 成为 boot hart */
    j _try_lottery
```

文件: `opensbi/Makefile`（3 处 patch）
- `OPENSBI_LD_PIE := n` — 禁用 PIE（bare-metal ld 不支持）
- 注释掉 `$(error ...)` PIE 检查
- 清空 CFLAGS/ASFLAGS/ELFFLAGS 中的 PIE flags

编译命令: `make opensbi`（内部: `make PLATFORM=generic CROSS_COMPILE=riscv64-elf- FW_TEXT_START=0x80000000`）

### 3. 双核 QEMU 验证 ✓

```
hart 0 (rt-async):  task_a tick #0,1,2... (500ms) + task_b tock #0,1,2... (700ms)
hart 1 (OpenSBI):   OpenSBI v1.8 banner, Boot HART ID: 1, Platform HART Count: 2
```

启动命令: `make rt-async && make opensbi && make run`

### 4. 内存布局

```
0x80000000  OpenSBI fw_dynamic.bin (-bios 加载)
0x80200000  StarryOS (-device loader, 预留)
0x80800000  rt-async (-device loader)
0x88000000  共享内存 IPC (SHMEM_BASE)
```

### 5. 未提交

项目还没有任何 git commit（main 分支为空）。所有改动都在工作区。

## 待完成工作

### TODO 1: 提交当前代码

- `git add` 所有项目文件（不含 opensbi/）
- `.gitmodules` 和 submodules (rt-async, StarryOS) 需要正确提交
- 约定式提交，无 AI 信息
- 建议: 一个初始 commit `init: rt-async-amp 项目骨架 + OpenSBI hart 路由`

### TODO 2: UART 双串口

**当前状态**: rt-async 和 OpenSBI 共用 UART0（`-nographic` 下 UART0 直连 stdio）。`chip-qemu-virt-rt/src/lib.rs` 中 `UART_BASE = 0x1000_0000`（UART0）。

**需要做的**:
1. `UART_BASE` 改为 `0x1000_0100`（UART1）
2. QEMU 启动参数改为: `-serial null -serial stdio`（UART0=null, UART1=stdio）
   - 但实测 `QEMU virt` 的 UART1 在 `-nographic` 下无法直接映射到 stdio
   - 可能需要自定义设备树或用 `-chardev` 方式
3. **暂时不改也行**: 等 StarryOS 集成时再处理，StarryOS 用 UART0，rt-async 用 UART1，通过不同串口分离输出

### TODO 3: StarryOS 编译集成

**当前状态**: `make starryos` target 存在但未验证。

**需要做的**:
1. 确认 StarryOS submodule 的 QEMU virt riscv64 构建方式
2. StarryOS 入口地址设为 `0x80200000`
3. StarryOS 使用 UART0 输出
4. 验证 `make starryos` 能正确生成 `build/starryos.bin`
5. 双核测试: `make run` 同时加载 rt-async + StarryOS

**参考**: StarryOS 的构建系统在 `StarryOS/Makefile` 和 `StarryOS/scripts/` 中

### TODO 4: IPC 集成

**当前状态**: `modules/ipc/` 结构体定义完整，但两侧均未接入。

**rt-async 侧** (apps/rt-async-app/):
- `MachineSoft` ISR 中 `PEND_MARKER == false` 分支调用 `ipc::SharedControl::instance()` 轮询 `linux_to_rtos` 队列
- 发送消息: 写 `rtos_to_linux` 队列 → `chip_qemu_virt_rt::send_ipi_to_linux()`（写 MSIP1）
- 需要在 `main()` 中调用 `unsafe { ipc::SharedControl::init() }`

**StarryOS 侧**:
- 通过 SBI ecall 触发 IPI（OpenSBI 写 MSIP0 → rt-async 收到 MachineSoft 中断）
- SSI 中断处理中轮询 `rtos_to_linux` 队列
- 共享内存物理地址 `0x88000000`，StarryOS 需映射访问

**IPI 流向**:
```
StarryOS → SBI ecall → OpenSBI 写 MSIP0 → rt-async MachineSoft 中断
rt-async → 写 MSIP1 → OpenSBI 设 MIP.SSIP → StarryOS SSI 中断
```

### TODO 5: Makefile 完善

- `opensbi`: 已完成（包含克隆后的编译流程）
- `starryos`: 需要适配 StarryOS 实际构建方式
- `run`: 已完成基础版，等 StarryOS 集成后验证双加载
- 考虑添加 `make test` 单核测试 target（`ORIGIN=0x80000000`，`-smp 1`，无 OpenSBI）

## 关键技术决策

### OpenSBI 编译 (macOS + bare-metal toolchain)

- 交叉编译器: `riscv64-elf-gcc` (Homebrew)
- 禁用 PIE: bare-metal `ld` 不支持 `-pie`，patch 了 OpenSBI Makefile 跳过 PIE 检查
- `FW_TEXT_START=0x80000000`: 让链接地址匹配 QEMU `-bios` 加载地址，否则 `lla` 指令产生错误地址

### hart 路由设计

- hart 0: 在 `_start` 最开头 mret 跳走，不参与 OpenSBI 初始化
- hart 1+: 通过 `j _try_lottery` 跳过 `fw_boot_hart` 检查，强制走 lottery
- hart 1 成为 boot hart，执行完整初始化（包括为所有 hart 分配 scratch space）
- hart 2+ (如果有) 拿不到 lottery → 等 `_boot_status` → secondary hart 流程
- 对 N 核 (N≥2) 通用，不需要修改

### 参考文档

- IPC 设计参考: `/Users/oveln/projects/embassy_preempt_VisionFive2/embassy_preempt/blogs/docs/多系统间通讯.md`
- rt-async 架构: `/Users/oveln/projects/rt-async/`
