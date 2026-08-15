# rt-async-amp —— 异构多核实时双内核 AMP 系统

在异构多核平台上构建 **StarryOS（通用）+ rt-async（实时）双内核** 系统：通用大核跑 Linux 兼容内核处理 AI 推理与复杂计算，实时小核跑自研 Rust async RTOS 处理电机/传感器等微秒级实时任务，两核通过共享内存 + 核间中断（IPI）低延迟协作，互不干扰。

- **文档网站**：<https://rtos.oveln.icu>　·　**开发周报**：<https://rtos.oveln.icu/周报-Oveln/>
- **技术报告**：<https://rtos.oveln.icu/技术报告/>　·　**项目计划**：<https://rtos.oveln.icu/项目计划/>
- **rt-async 内核**：<https://github.com/Oveln/rt-async>　·　**StarryOS**：<https://github.com/Oveln/StarryOS>

> 技术调研、开发计划与里程碑、测试记录、问题复盘等详细内容在上述文档网站（blogs）中维护，本 README 只保留简介、架构与使用方法。

---

## 简介

机器人、具身智能等新一代智能装备同时需要两类截然不同的计算能力：Linux 生态的通用算力（AI 推理、网络、文件系统）与微秒级确定性实时（电机控制、传感器采样）。单一 Linux（即使 PREEMPT_RT）难以同时满足，本项目的答案是**异构双内核 AMP**：

- **StarryOS**（大核，S 态）：Linux 系统调用兼容内核，承接通用负载；
- **rt-async**（小核，M 态）：自研 Rust async RTOS——无动态内存、async/await 任务模型、优先级抢占 + 时间片调度，承接实时负载；
- 两核**只经共享内存通道（ov-channels）+ IPI 门铃**协作，实时侧不受通用侧抖动影响。

三个运行环境同构管理（`envs/<name>.toml`，详见使用方法）：

| 环境 | 硬件 | 定位 |
|---|---|---|
| `qemu-plic` | QEMU virt 双核（PLIC） | 日常快速冒烟，秒级迭代 |
| `qemu-aia` | QEMU virt 双核（APLIC+IMSIC） | K3 AP 侧中断架构的仿真对应物（当前仅 AP 侧可跑） |
| `k3-com260` | 进迭时空 K3 COM260（X100 + RT24 rcpu1） | 真板：实时硬件操控落地 |

## 架构

两个硬件线程（hart）各自运行不同的软件栈，**仅通过共享内存交换数据、通过 IPI 互相通知**：

- **Hart 0** 运行完整操作系统 **StarryOS**（Linux 系统调用、文件系统、用户进程）；
- **Hart 1** 运行裸金属异步执行器 **rt-async**，具有确定性实时调度能力，**无操作系统**。

![系统架构总览](docs/assets/arch-overview.svg)

QEMU virt 环境双核 AMP，OpenSBI 完成异构分流：

```
QEMU virt (-smp 2 -m 256M)
├─ hart 0: OpenSBI → sret S-mode → StarryOS @ 0x80200000  (UART0 @ 0x10000000)
└─ hart 1: OpenSBI → mret M-mode → rt-async @ 0x82800000  (UART1 @ 0x10002000)

共享内存 IPC @ 0x88000000 (ov-channels SharedMemory<3>, 100KB)
```

- **OpenSBI 补丁**（`patches/opensbi-amp.patch`）：hart 1 直接 mret 到 rt-async（M-mode）、默认下一地址指向 StarryOS、禁 PIE、IPI 转发（直写 MSIP → SSIP）、允许 S-mode 写 CLINT MSIP；
- **QEMU 补丁**（`patches/qemu-uart1.patch`）：在 `0x10002000` 增加第二路 NS16550A UART（IRQ 12）供 rt-async 独立输出。

K3 真板环境：AP 侧 X100 大核跑 StarryOS（APLIC+IMSIC 中断架构，经 FIT uimg 启动），RP 侧 RT24 rcpu1（CVA6）跑 rt-async（SPL 从 esos.itb 拉起，RCPU SRAM 共享窗 + mailbox4 门铃）。

📐 **完整架构文档**（executor 流程、任务状态机、RPC 模式、shm 结构、内存布局、模块依赖等 7 节交互式图表）见 [`docs/architecture.html`](docs/architecture.html)。

## 使用方法

本项目使用 [`cargo xtask`](xtask/) 作为构建编排器（取代 Makefile），所有克隆/打补丁、构建、运行、安装、清理均通过 xtask 子命令完成，子命令一览用 `cargo xtask --help` 查看。

### 环境模型

**环境（environment）是一等公民**：`envs/<name>.toml` 声明该环境的 QEMU 机器参数、StarryOS 板级配置（tgoskits 内路径）、AP 侧 DTB 来源与 K3 打包 bin。产物按环境隔离在 `build/<env>/` 下，互不干扰。

| 环境 | 构建命令 | 运行/刷写 | `build/<env>/` 产物 |
|---|---|---|---|
| `qemu-plic` | `cargo xtask build qemu-plic` | `cargo xtask run`（默认环境） | `fw_dynamic.bin` / `starryos.bin` / `rt-async*.bin` / `ap.dtb` / `rt-async.dtb` |
| `qemu-aia` | `cargo xtask build qemu-aia` | `cargo xtask run --env qemu-aia`（**仅 AP 侧完整**，见下） | 同上（`ap.dtb` 由 dumpdtb+overlay 自动生成） |
| `k3-com260` | `cargo xtask build k3-com260` | RP：`./scripts/flash/k3-flash.sh`；AP：手动 fastboot+bootm（见下） | `esos.itb`（RP 侧）/ `starryos.uimg`（AP 侧）/ `rt-async-k3-*.elf` |

> user-apps 与环境无关，产物留在 `build/` 顶层。`amp.toml` 只管地址布局与上游 repo pin；机器参数等环境属性在 `envs/`。

### 前置依赖

- **Rust 工具链**：`rustup`（安装见 [rustup.rs](https://rustup.rs)）+ `rustup target add riscv64imac-unknown-none-elf`
- `riscv64-elf-gcc`（Homebrew：`brew install riscv64-elf-gcc`；Ubuntu：`gcc-riscv64-unknown-elf`）
- **Musl 工具链**：交叉编译 `user-apps` 与 StarryOS 的 C 依赖（lwprintf）所需，安装见下
- Ninja / Meson（构建定制 QEMU：`brew install ninja meson` / `sudo apt install ninja-build meson`）、Python 3
- `cc` + `dtc`（设备树编译链；`sudo apt install device-tree-compiler`）
- 仅 K3 刷写需要：`mkimage` / `lzop` / `fastboot` / pyserial（`pip3 install pyserial`）

#### 安装 Musl 工具链

参照 [StarryOS](https://github.com/Starry-OS/StarryOS) 的做法，使用预编译包（而非手动 `musl cross-make`）：

1. 从 [setup-musl releases](https://github.com/arceos-org/setup-musl/releases/tag/prebuilt) 下载 `riscv64-linux-musl-cross`；
2. 解压到某路径，例如 `/opt/riscv64-linux-musl-cross`；
3. 加入 `PATH`：

   ```bash
   export PATH=/opt/riscv64-linux-musl-cross/bin:$PATH
   # 也可 export RISCV64_MUSL_CROSS=/opt/riscv64-linux-musl-cross（xtask 会自动前置其 bin）
   ```

### 首次搭建（qemu-plic 全链路）

```bash
git submodule update --init --recursive   # 1. 初始化子模块（rt-async / tgoskits）
cargo xtask setup                          # 2. 克隆 + 打补丁 OpenSBI 与 QEMU（amp.toml pin 版本）
cargo xtask qemu                           # 3. 源码构建定制 QEMU（含 rt-async 专用 UART1）
cargo xtask build qemu-plic                # 4. 环境聚合构建（OpenSBI + StarryOS + RP bins + user-apps）
cd tgoskits && cargo xtask starry rootfs --arch riscv64 && cd ..   # 5. 准备 rootfs（tgoskits 正统流程）
cargo xtask install --all                # 6. 将 user-apps 安装进 rootfs
cargo xtask run                            # 7. 启动双核 AMP
```

### 日常开发循环（QEMU 环境）

启动后两路串口的观察方式：

- **UART0 → 本终端 stdin/stdout**：OpenSBI 启动横幅 → StarryOS 启动日志 → `root@starry:/root #` 交互 shell。
- **UART1（rt-async 侧）→ Unix socket** `/tmp/rt-async-uart.sock`，两种接法：
  - `cargo xtask run`（前台）：另开终端 `socat - UNIX-CONNECT:/tmp/rt-async-uart.sock`。注意 QEMU 是 socket 服务端，**连接之前的启动期输出会丢**；
  - `cargo xtask run --tmux`：tmux 左右分屏（左 QEMU、右 UART1），socat 先监听、QEMU 作客户端连接，**不丢启动期输出**（推荐）。

想持续留档 UART1 日志给 `cargo xtask log` 跟踪，把 socat 输出 tee 到约定文件：

```bash
socat - UNIX-CONNECT:/tmp/rt-async-uart.sock | tee build/rt-async-uart.log
```

**正常冒烟标志**：UART0 上 OpenSBI 横幅 + StarryOS shell + `rt_shm: device initialized, phys base 0x88000000`；UART1 上 `[heartbeat] tick #N` 每 500ms 一条（demo bin）。

换 rt-async 固件（run 用 cargo 短名，不带平台前缀）：

```bash
cargo xtask run --bin console              # demo / console / console_interrupt
```

### qemu-aia：AIA 仿真环境（当前仅 AP 侧）

机器参数 `virt,aia=aplic-imsic`，StarryOS 侧 somehal 运行时按 FDT 探测，自动走 IMSIC+APLIC 路径（`OpenSBI: Platform IPI Device : aia-imsic` 即为生效标志）；AP 侧 DTB 由本环境机器 dumpdtb 导出基线、`fdtoverlay` 叠加共享窗节点（`its/rt-async-ap.overlay.dts`），中断拓扑自动正确，无需手写 imsic 节点。

**当前边界（事实状态）**：AIA 是 StarryOS 侧的支持，**rt-async 暂不支持 AIA**（RP 侧只有 PLIC 驱动，而该机器无 PLIC，RP 侧会静默挂死）。因此 `run --env qemu-aia` 目前用于 **AP 侧 AIA 行为验证**（MSI 投递、stopei EOI 等先在仿真确认再到 K3 真板）；等 rt-async 补 APLIC/IMSIC 驱动后启用完整双端。详见 `envs/qemu-aia.toml` 注释。

### K3 真板（k3-com260）

**构建（xtask 职责到产物为止）**：

```bash
cargo xtask build k3-com260
# 交付两个产物：
#   build/k3-com260/esos.itb       RP 侧：rcpu0 官方 esos + rcpu1 rt-async（默认 k3-sched-demo）
#   build/k3-com260/starryos.uimg  AP 侧：StarryOS FIT（kernel + dtb）
```

**刷 RP 固件（一键，串口 fastboot）**：

```bash
./scripts/flash/k3-flash.sh                     # 构建 + 打包 itb + 刷写，板子最终停在 U-Boot 提示符
K3_TARGET=k3-ipc-demo ./scripts/flash/k3-flash.sh   # 换其他 rcpu1 bin
./scripts/flash/k3-flash.sh --no-build          # 复用已打包的 itb 重刷
```

**刷/启 AP 内核（手动三步，同一串口会话）**：

```bash
# U-Boot 侧：进入 fastboot，FIT 上传到 0x180000000（与 kernel/fdt 加载地址错开）
fastboot -l 0x180000000 -s 0x04000000 usb 0
# Host 侧：上传 starryos.uimg
fastboot stage build/k3-com260/starryos.uimg
# U-Boot 侧：Ctrl-C 退回提示符后启动（bootargs 已固化在设备树，无需 setenv）
bootm 0x180000000
```

串口设备名在 `scripts/flash/flash.conf` 配置（Linux 默认 `/dev/ttyUSB0`=主 UART、`/dev/ttyUSB1`=R_UART0，115200）。

> K3 的 rcpu0 跑固定复用的官方 esos，rcpu1 跑本仓库构建的 rt-async；两者由 SPL 从同一 itb 的不同节点加载（无 DTB handoff，DTB 内嵌进 ELF）。AP 与 RP 是两套独立镜像。详见 [`scripts/flash/README.md`](scripts/flash/README.md)。

### 用户态应用（user-apps）开发循环

user-apps 是 StarryOS 侧的 musl 静态二进制，两个环境的上板方式不同：

```bash
cargo xtask build user-test-rpc      # 单独构建（环境聚合构建也会带上）
# qemu-plic：注入 rootfs 后在 StarryOS shell 里运行
cargo xtask install --all            # 全部注入（或 install build/user-test-rpc --dst /user-test-rpc）
cargo xtask run                      # shell 里执行 /user-test-rpc
# k3-com260：板上无编译环境/网络时经串口传 base64 还原
bash scripts/serial_send.sh build/user-test-ipc /tmp/user-test-ipc   # 输出脚本粘到板子串口
```

### 常用子命令速查

```bash
# 环境聚合构建（一个环境一条命令，产物落 build/<env>/）
cargo xtask build qemu-plic  #   OpenSBI + StarryOS + 全部 qemu bins + user-apps
cargo xtask build qemu-aia   #   同上（AIA 机器；AP DTB 由 dumpdtb+overlay 自动生成）
cargo xtask build k3-com260  #   全部 k3 bins + esos.itb + starryos.uimg
# 构建单个目标：组件（opensbi / starryos / user-test-*）或 rt-async bin
cargo xtask build <target>   #   rt-async bin 用 <平台>-<bin> 命名（落平台默认环境目录）：
                             #     qemu-demo / qemu-console / qemu-console-interrupt → flat bin（QEMU loader）
                             #     k3-sched-demo → ELF（esos 脚本整合进 itb）
cargo xtask build qemu       # 构建全部 QEMU rt-async bin
cargo xtask build k3         # 构建全部 K3 rt-async bin
cargo xtask run --env qemu-aia # 启动指定环境的 QEMU AMP（--bin demo 换 RP bin，run 用短名）
                              #   注：qemu-aia 目前仅 AP 侧可跑（见上文）
cargo xtask log              # 彩色前缀跟踪 rt-async 的 UART1 日志（tee 到 build/rt-async-uart.log）
cargo xtask install --all    # 将 user-apps 安装进 StarryOS rootfs
cargo xtask qemu             # 从源码构建带 UART1 的定制 QEMU
cargo xtask clean --dist     # 清理构建产物（--dist 连带删除 opensbi/ 与 qemu/）
cargo xtask completions fish # 生成 shell 补全脚本（bash/zsh/fish/...）
```

### 约定说明

- `amp.toml` 是地址常量与上游 repo pin 的单一真相源：patch 文件中的 `{VAR}` 模板变量在 `cargo xtask setup` 时由其顶层取值替换；板级 crate 的 `build.rs` 亦由它生成 `amp_gen.rs` 常量。机器参数等**环境属性**在 `envs/*.toml`。
- 修改 `.dts`/`.dtsi` 后无需手动编译：`run` 时按 mtime 增量编译 DTB（`cc -E → dtc` 链，与 K3 `build.rs` 一致）。
- 仓库结构与开发规范（分支工作流、提交约定、双仓流程）见 [`AGENTS.md`](AGENTS.md)。
