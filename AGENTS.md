# AGENTS.md

本文件为 AI 编程助手（ZCode 等）在 **rt-async-amp** 仓库工作时提供指引。

`rt-async-amp` 是异构多核双内核 AMP 系统：通用大核跑 Linux 兼容内核，实时小核跑
自研 Rust async RTOS（rt-async），两核经共享内存 + IPI 协作。目标平台为 **进迭时空
K3 SoC 的 RT24 实时小核（rcpu1，CVA6/RV64GC）**，同时维护 QEMU virt 仿真验证路径。

---

## 1. 仓库结构

```
rt-async-amp/                  ← 主仓（本仓），集成分支 master
├── rt-async/                  ← 子模块（rt-async 内核），集成分支 main
│   └── modules/{executor,executor-macro,futures,platform,timer}
├── modules/                   ← 板级 crate（依赖 rt-async 的 platform 契约层）
│   ├── chip-k3-rt24/          ←   K3 RT24 rcpu1 板级驱动（pinctrl/uart/clint/plic/...）
│   ├── chip-qemu-virt-rt/     ←   QEMU virt 仿真板级驱动
│   └── ov-rpc/                ←   跨核 RPC
├── apps/
│   ├── rt-async-k3/           ←   K3 固件（sched_demo 等，构建产物 → build/*.elf）
│   ├── rt-async-app/          ←   QEMU virt 固件
│   └── rt-async-k3-app/       ←   K3 用户态应用
├── its/                       ←   设备树源（.dts）+ 宏定义（k3-pinctrl.h / k3-clock.h）
├── xtask/                     ←   构建工具链（cargo xtask build/run/flash/...）
├── amp.toml                   ←   地址布局 + 构建参数 single source of truth
├── patches/ opensbi/ qemu/    ←   上游依赖与补丁
├── tgoskits/                  ←   通用内核子模块（StarryOS 衍生，AGENTS.md 自带）
└── build/                     ←   构建产物（.elf/.bin/.dtb，不入 git）
```

- **子模块 `rt-async`**：内核 + 平台抽象，独立 workspace，集成分支 `main`。
- **子模块 `tgoskits`**：通用内核，自带 `AGENTS.md`，本仓一般不直接改。
- **主仓**：板级驱动 + AMP 集成 + 设备树 + 构建工具链，集成分支 `master`。

---

## 2. 构建与测试

**工具链**：`nightly-2026-04-25`（见 `rust-toolchain.toml`），目标
`riscv64imac-unknown-none-elf`。构建产物输出到 `build/`。

### 推荐：xtask（处理 target、产物拷贝、地址布局等）

```bash
cargo xtask build k3-sched-demo    # 构建 K3 sched_demo → build/rt-async-k3-sched-demo.elf
cargo xtask build qemu-demo        # 构建 QEMU demo
cargo xtask run    k3-sched-demo   # 构建 + 运行（QEMU 仿真）
cargo xtask flash  k3-sched-demo   # 构建 + 刷写到 K3 真板
```

### 直接 cargo（需手动指定 target）

```bash
cargo build --release -p rt-async-k3 --target riscv64imac-unknown-none-elf
cargo build --release -p rt-async-app --target riscv64imac-unknown-none-elf
```

> **子模块构建陷阱**：主仓和子仓 `rt-async` 各有一份 `.cargo/config.toml`，两者
> `runner` 字段类型不同（string vs array）。**从主仓根目录构建时 cargo 只读主仓
> config**（正常）；但如果 `cd rt-async` 后构建会触发合并冲突。始终从主仓根用
> `cargo xtask` 或从对应 workspace 根目录执行命令。

---

## 3. 设备树（DTS）编译链

K3 的设备树源 `its/rt-async-k3.dts` 用官方 esos 宏写法（`K3_PADCONF` /
`MUX_MODE4` 等，定义在 `its/k3-pinctrl.h`）+ 时钟 ID 宏（`K3_CLK_RUART0`
等，定义在 `its/k3-clock.h`）。编译链与 esos 一致：

```
.dts  ──cc -E──▶  .pp.dts  ──dtc──▶  .dtb
       （展开              （求值算术
        #include/#define）  生成 dtb）
```

- **cc 做宏展开**：`cc -E -P -nostdinc -undef -x assembler-with-cpp -I its/`。cc 在
  所有开发机（macOS=clang / Linux=gcc）上都有，不引入额外依赖。
- **dtc 做算术求值**：dtc 原生支持 `< (a*b) (c|d) >` cell 表达式。
- 实测 `K3_PADCONF(GPIO_122, (MUX_MODE4|EDGE_NONE|PULL_UP|PAD_DS8))` →
  `<0x1e8 0xd044>`（offset=122*4, value=MUX|EDGE|PULL|DS）。
- 编译在 `modules/chip-k3-rt24/build.rs` 中完成，产物 `.dtb` 经
  `include_bytes!(env!("K3_DTB_PATH"))` 内嵌进固件。`.dtb` 不入 git。

改了 `.dts` 或 `k3-pinctrl.h` 后，验证：

```bash
cc -E -P -nostdinc -undef -x assembler-with-cpp -I its/ its/rt-async-k3.dts > /tmp/k3.pp.dts
dtc -I dts -O dtb -o /tmp/k3.dtb /tmp/k3.pp.dts    # 应无错
dtc -I dtb -O dts /tmp/k3.dtb | grep "pinctrl-single,pins"   # 反编译核对值
```

---

## 4. K3 RT24 板级驱动模型

板级 crate `modules/chip-k3-rt24/` 实现 `platform::Board` + 一组 K3 专属 driver，经
设备树 probe 实例化。driver model 契约层在子模块 `rt-async/modules/platform/`。

### 驱动注册流程

1. 板级 crate 在 `lib.rs` 的 `K3_DRIVERS: &[&dyn Driver]` 中列出所有可被 DT 探测的
   driver（顺序重要——见下"DFS 先序约束"）。
2. `Board::init()` → `handshake::spl_handshake()`（SPL 握手，解锁 AP 6s 轮询）→
   `init_dtb`（内嵌 DTB）→ `DRIVERS.set(K3_DRIVERS)` + `boot()`。
3. `boot()` DFS 先序遍历设备树，对每个节点：先 `try_pinctrl().apply(node)` 应用
   `pinctrl-0`，再 `try_clock().enable_for(node)` 使能 `clocks` 属性指向的功能时钟，
   最后按 compatible 匹配 driver 调 `probe()`——故外设 probe 前引脚和时钟都已就绪。

### DFS 先序约束（重要）

`boot()` 按设备树文档顺序 DFS 先序遍历。**依赖其他 driver 先 probe 的 driver，必须
保证被依赖者在 DTS 和 `K3_DRIVERS` 列表中都排在前面**：

- **pinctrl controller** 必须排在 serial 之前（serial probe 前 PINCTRL slot 要就绪）。
  故 DTS 中 `pinctrl@d401e000` 节点排在 `serial@c0881000` 前；`K3_DRIVERS` 中
  `&pinctrl_k3::PINCTRL` 是首项。
- **CCU 时钟控制器** 必须排在所有需要时钟的外设之前（外设 probe 前 CLOCK slot 要
  就绪，`enable_for` 要能在 probe 前配好末端 gate）。故 DTS 中 `ccu@c0880000` 节点
  排在 serial 等外设前；`K3_DRIVERS` 中 `&clock::CCU` 排在 pinctrl 之后、外设之前。

### 添加新 K3 驱动（模板，参考 `pinctrl_k3.rs`）

1. 新建 `modules/chip-k3-rt24/src/<name>.rs`：定义零大小单例 + `static` MMIO 基址
   `AtomicUsize`，impl `Driver`（`compatible()` + `probe()`）+ 功能 trait。
   （功能 trait 示例：`pinctrl_k3.rs` 的 `PinController`、`clock.rs` 的 `ClockProvider`。）
2. `lib.rs`：`pub mod <name>;` + 在 `K3_DRIVERS` 加入 `&<name>::INSTANCE`。
3. `its/rt-async-k3.dts`：加设备节点（compatible / reg / interrupt），如需 pinmux
   则配 `pinctrl-0` + `_cfg` 子节点；如需工作时钟则配 `clocks = <&ccu K3_CLK_xxx>`
   （ID 宏在 `k3-clock.h`，并在 `clock.rs` 的 `CLK_TABLE` 加对应表项——CCU driver
   会在 probe 前自动使能时钟）。
4. 如引入新功能 trait（如 `I2cBus`），先在子模块 `rt-async` 的 `device.rs` 定义，
   `driver.rs` 加 Slot + 访问器——这会同时改两个仓库，走"双仓开发流程"。

### K3 关键地址（详见 `amp.toml`）

| 外设 | 基址 | 说明 |
|------|------|------|
| SysTimer | `0xe4000000` | mtime(+0xbff8 全局)/mtimecmp(+0x4000+hart<<27)/MSIP(+0x0+hart<<27) |
| PLIC | `0xe0000000` | 自定义布局，stride hart<<27 |
| R_UART0 | `0xc0881000` | PXA 派生，IRQ 17 |
| pinctrl | `0xd401e000` | pinmux 寄存器，每 pin 4 字节 |
| ri2c0/1/2 | `0xc0886000/6100/6200` | K3 I2C 控制器（14 寄存器） |
| CCU 时钟域 | `0xc088_003c` | RCPU_SYSCTRL：ruart_14 上游 DDN gate（bit31）|
| | `0xc088_1f00` | RCPU_UARTCTRL：UART0~5 末端 CLK_RST（gate/mux/div/reset）|
| | `0xc088_6f00` | RCPU_I2CCTRL：I2C0~2 末端 CLK_RST |
| | `0xc088_5f00` | RCPU_SPICTRL：SSP0~1 末端 CLK_RST |

> 注：CCU 时钟域基址是时钟/复位控制寄存器，与外设 MMIO 基址（如
> `ri2c0 0xc0886000`）是不同概念——前者控制后者的工作时钟，后者是外设自身
> 的功能寄存器。详见 `clock.rs` + 手册 17.4.2。RT24 小核不碰 PLL（AP 电源域，
> SPL 已配好），只消费已分频的固定频率时钟源。

---

## 5. Git 工作流

### 分支与集成分支

- **主仓集成分支：`master`**；**子模块 `rt-async` 集成分支：`main`**。
- feature 分支命名：`feat/<topic>`（如 `feat/pinctrl-k3`、`feat/driver-registry-refactor`）。
  修复用 `fix/<topic>`，重构用 `refactor/<topic>`，但历史以 `feat/` 为主。
- feature 分支从对应集成分支切出，完成后用 **`--no-ff`** 合并回去（保留特性分支
  拓扑，便于追溯）。

### 单仓开发（只改一个仓库）

```
master(main) ──●──●──●
                \       \
                 ●──●──● feat/<topic>      # 开发
                          ↘ --no-ff merge ──●  # merge commit 回集成分支
```

1. `git checkout -b feat/<topic>`（从集成分支）。
2. 开发 + 提交（一个逻辑改动一个 commit）。
3. 审阅 diff + 验证编译后，`git checkout master && git merge --no-ff feat/<topic>`。

### 双仓开发（同时改 rt-async 子模块 + 主仓板级）

当改动跨子模块（platform 框架）和主仓（板级 driver/DTS）时，在**两个仓库各建同名
`feat/<topic>` 分支**，协同开发：

**阶段 A —— 开发期：**
1. 子模块 `rt-async`：`git checkout -b feat/<topic>`（从 `main`），开发框架改动并提交。
2. 主仓：`git checkout -b feat/<topic>`（从 `master`），开发板级部分。期间把子模块
   指针指向子模块分支的 commit：
   ```
   cd rt-async && git checkout feat/<topic>   # 子模块切到 feature 分支
   cd .. && git add rt-async && git commit -m "submodule(rt-async): bump ..."
   ```
   这样主仓 feature 分支的每个 commit 都指向子模块 feature 分支的具体 commit。

**阶段 B —— 合并期（顺序关键，先子后主）：**
1. **先合并子模块**：`cd rt-async && git checkout main && git merge --no-ff feat/<topic>`。
2. 主仓 feature 分支**对齐子模块 merge commit**：子模块切到 `main`，主仓 bump 指针
   并提交：
   ```
   cd rt-async && git checkout main          # 子模块现在指向 main 的 merge commit
   cd .. && git add rt-async && git commit -m "submodule(rt-async): bump 指针到 main 最新 merge commit（对齐 no-ff merge 后的 main HEAD）"
   ```
3. **再合并主仓**：`git checkout master && git merge --no-ff feat/<topic>`。

这样保证：主仓 `master` 的每个 commit 指向的子模块 commit 都在子模块 `main` 上，
checkout `master` 时子模块不会处于 detached 或 feature 分支状态。

### 子模块操作要点

- 合并后子模块应停在集成分支（`main`）而非 feature 分支：`cd rt-async && git checkout main`。
- bump 子模块指针的 commit message 用统一前缀 `submodule(rt-async): bump ...`。
- **不要手动改子模块的 `.cargo/config.toml`**（见 §2 构建陷阱）。

---

## 6. Commit 约定

- 约定式提交（Conventional Commits）：`<type>(<scope>): <描述>`。
  - type：`feat` / `fix` / `docs` / `refactor` / `test` / `chore` / `build`
  - scope：crate 或子系统（`k3` / `chip-k3-rt24` / `platform` / `qemu` / `build` /
    `submodule` / `docs`…）
  - 描述**用中文**。
- **代码注释和 commit message 用中文；类型/变量/API 标识符用英文。**
- 一个逻辑改动一个 commit；跨子模块的指针 bump 单独成 commit。
- 多步重构（如 driver-registry）可用 `Step N` 前缀分阶段提交。

历史范例：
```
feat(k3): pinctrl-single driver + DTS 宏编译链 + ELF 构建时间戳
feat(platform): 新增 PinController trait + boot() 自动应用 pinctrl-0
build(qemu-virt): 编译期从 .dts 用 dtc 生成 .dtb
submodule(rt-async): bump 指针到 main 最新 merge commit（对齐 no-ff merge 后的 main HEAD）
```

---

## 7. 提交前流程

每次用户要求提交时，按以下顺序执行：

1. **审阅 diff**：对本次 diff 审阅——变更是否与用户意图一致、是否有遗漏/多余文件、
   commit message 是否准确反映变更。
2. **验证编译**：
   - 改了板级 crate / app → `cargo build --release -p rt-async-k3 --target riscv64imac-unknown-none-elf`（或对应 crate）。
   - 改了 DTS → 走 §3 的 cc -E + dtc 验证 + 反编译核对值。
   - 改了子模块 platform → 确保主仓 + 子仓都能编译。
3. **检查文档是否需要同步更新**（跳过周报和技术报告）：检查 `README.md`、
   `AGENTS.md` 等是否因本次变更需要修改，将需要更新的文档清单反馈给用户。
4. 审阅通过后再提交。

---

## 8. 约定

- Edition 2024，resolver 3。全部 `#![no_std]`，**禁止动态内存分配**。
- 所有共享状态用 `static` + `AtomicUsize` / `Slot<T>` 承载（无 alloc）。
- 共享状态读写必须在 `critical_section::with()` 中；手动 `Sync` impl 须注释安全性依据；
  每个 `unsafe` 块上方注释说明为何安全。
- `log::info!()`/`log::error!()` 输出到 console UART，阻塞写，**中断上下文勿高频打印**。
- 构建产物（`build/*.elf`、`*.bin`、`*.dtb`）不入 git，由 build.rs / xtask 派生。
- **过程中产生的设计文档、计划文档不入 git**（如 `docs/superpowers/`），除非用户明确要求。
- `amp.toml` 是地址布局 + 构建参数的 single source of truth，driver 硬编码地址时与
  之保持一致并加注释。

---

## 9. 常见任务

### 添加新 K3 外设驱动

见 §4"添加新 K3 驱动"。若需要新功能 trait（子模块改动），走 §5 双仓开发流程。

### 添加新构建目标（app/bin）

1. 在 `apps/<app>/src/bin/` 加 bin 文件，`Cargo.toml` 加 `[[bin]]`。
2. `xtask/src/build.rs` 的 `TARGETS` 数组加一项（`name` / `target_name` / `out` /
   `app_dir` / `package`）。
3. `cargo xtask build <target_name>` 验证。

### 刷写 K3 真板

```bash
cargo xtask flash k3-sched-demo    # 构建 + 通过 scripts/flash 刷写
```

详见 `scripts/flash`。
