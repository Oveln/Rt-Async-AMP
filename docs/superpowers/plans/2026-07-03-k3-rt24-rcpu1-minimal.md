# K3 RT24 rcpu1 Minimal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 rt-async 编译出的 ELF 能在 K3 SoC 的 RT24 rcpu1 实时核上运行，R_UART0 输出 `hello from rt-async`，且不拖累 AP 启动（SPL 握手成功）。

**Architecture:** 在 `rt-async` 的 platform 层加两个 init 钩子（arch 直接函数调用 + chip 用 `.weak` 弱符号），合进 `platform::init()`。新建 `modules/chip-k3-rt24`（实现 `Chip`/`TimerChip`(stub) + `_board_init` 强覆盖，内含 main.c 验证过的 7 步硬件初始化）和独立 app crate `apps/rt-async-k3`（基址 `0x100804000`，复用 `riscv64-rt` 的 link.x）。

**Tech Stack:** Rust nightly（`riscv64imac-unknown-none-elf`），`extern_trait` 0.4.3（Chip 注册），RISC-V 裸机（`riscv` crate），link.x 弱符号（`.weak`），K3 RT24 PXA-UART + CCU 时钟链。

**参考：** 设计文档 `docs/superpowers/specs/2026-07-03-k3-rt24-rcpu1-minimal-design.md`；初始化序列源自 `esos/.../baremetal/main.c`（已验证）。

**测试约定（裸机固件，无宿主单测框架）：** 每个任务的"测试"= `cargo build` 编译通过 + 链接产物检查（`readelf` 看 entry、`nm` 看符号绑定）。最终验收在硬件（用户刷板后串口看到 `hello from rt-async` + SPL 不卡顿）。TDD 在此体现为"先写最小能编译的骨架 → 逐步填充 → 每步编译验证"。

---

## 文件结构

新建 / 改动文件清单（每个文件单一职责）：

| 文件 | 动作 | 职责 |
|------|------|------|
| `amp.toml` | 改 | 加 K3 地址段常量 `RT24RCPU1BASE`/`RT24RCPU1SIZE` |
| `Cargo.toml`（根 workspace） | 改 | members 加 `modules/chip-k3-rt24`、`apps/rt-async-k3` |
| `rt-async/modules/platform/src/lib.rs` | 改 | `init()` 内调两钩子 |
| `rt-async/modules/platform/archs/riscv64-rt/src/lib.rs` | 改 | 加 `arch_init()` + `.weak _board_init` 空定义 |
| `modules/chip-k3-rt24/Cargo.toml` | 新建 | chip crate 清单 |
| `modules/chip-k3-rt24/build.rs` | 新建 | 生成 memory.x + amp_gen.rs |
| `modules/chip-k3-rt24/src/lib.rs` | 新建 | `K3Rt24`；impl Chip/TimerChip(stub) + `_board_init` 强覆盖 |
| `modules/chip-k3-rt24/src/clock.rs` | 新建 | 握手 + ruart_14 + uart0 gate + pinmux 常量与函数 |
| `modules/chip-k3-rt24/src/uart.rs` | 新建 | PXA-UART 寄存器常量 + init/putc |
| `apps/rt-async-k3/Cargo.toml` | 新建 | app crate 清单 |
| `apps/rt-async-k3/.cargo/config.toml` | 新建 | target + build-std |
| `apps/rt-async-k3/build.rs` | 新建 | 链接 link.x + 生成 memory.x |
| `apps/rt-async-k3/src/bin/minimal.rs` | 新建 | `#[executor::main]`，main 里 put_str |

---

## Task 1: amp.toml + 根 workspace 加 K3 配置

**Files:**
- Modify: `amp.toml`（在 Peripherals 段后追加）
- Modify: `Cargo.toml`（根 workspace members）

- [ ] **Step 1: 在 `amp.toml` 末尾追加 K3 RT24 rcpu1 地址段**

在 `amp.toml` 的 `[qemu_src]` 段**之前**（即 SBI constants / QEMU configuration 之间）插入：

```toml
# ── K3 RT24 rcpu1 ───────────────────────────────────────────────────────────
RT24RCPU1BASE = "0x100804000"
RT24RCPU1SIZE = "0x300000"     # 3M，与 esos baremetal.ld 一致
```

- [ ] **Step 2: 根 `Cargo.toml` 的 members 增加两个 crate**

把 `[workspace] members` 改为（追加最后两项）：

```toml
[workspace]
members = [
    "modules/chip-qemu-virt-rt",
    "modules/ov-rpc",
    "apps/rt-async-app",
    "user-apps/user-test-ipc",
    "user-apps/user-test-rpc",
    "user-apps/user-test-sched",
    "modules/chip-k3-rt24",
    "apps/rt-async-k3",
    "xtask",
]
resolver = "3"
```

- [ ] **Step 3: 验证 amp.toml 仍可被 xtask 解析（不会因格式错误 break 现有构建）**

Run: `cargo build -p xtask`
Expected: 编译通过（无 toml 解析错误）。xtask 不在 build 时读 amp.toml，但若格式错，chip/app 的 build.rs 后续会炸——此步先确认 toml 合法。

- [ ] **Step 4: Commit**

```bash
git add amp.toml Cargo.toml
git commit -m "config: 加 K3 RT24 rcpu1 地址段与 workspace 成员占位"
```

---

## Task 2: platform 加两个 init 钩子（arch + chip 弱符号）

**Files:**
- Modify: `rt-async/modules/platform/archs/riscv64-rt/src/lib.rs`（末尾追加 arch_init + global_asm）
- Modify: `rt-async/modules/platform/src/lib.rs`（改 `init()`）

- [ ] **Step 1: 在 `riscv64-rt/src/lib.rs` 末尾追加 arch 钩子 + chip 弱符号空定义**

在文件末尾（`_default_setup_interrupts` 函数之后）追加：

```rust

// ── init 钩子（供 platform::init() 调用）──────────────────────────────────

/// arch 级早期初始化钩子。默认空实现；arch crate 可按需扩展。
/// （mtvec 已在 `__start_rust` 中设置，故此处不重复。）
pub fn arch_init() {}

/// chip 板级初始化钩子：原生弱符号（空函数体）。
///
/// platform 不依赖任何 chip crate，故无法直接调用其函数；改用弱符号——
/// chip crate（如 chip-k3-rt24）用 `#[no_mangle] extern "C" fn _board_init()`
/// 强定义覆盖。不覆盖时（QEMU/std-chip）调用落到此空实现，无副作用。
///
/// 链接行为已实测：强定义存在时 `nm` 显示 `T`（强）并解析到 chip 实现；
/// 不存在时显示 `W`（弱）仍链接成功。
core::arch::global_asm!(
    ".section .text",
    ".weak _board_init",
    "_board_init:",
    "ret",
);
```

- [ ] **Step 2: 改 `platform/src/lib.rs` 的 `init()`，调用两钩子**

把现有的：

```rust
pub fn init(max_level: log::LevelFilter) {
    let _ = LOGGER.init(max_level);
}
```

改为：

```rust
extern "C" {
    fn _board_init(); // 弱符号：arch 提供 .weak 空定义，chip crate 用强 #[no_mangle] 覆盖
}

pub fn init(max_level: log::LevelFilter) {
    let _ = LOGGER.init(max_level);

    #[cfg(feature = "riscv64")]
    arch::arch_init(); // arch 钩子：直接函数调用（platform→arch 真实依赖）

    #[cfg(feature = "riscv64")]
    unsafe {
        _board_init()
    }; // chip 钩子：弱符号，K3 在此做 握手+时钟+pinmux+UUE；其他平台为空
}
```

- [ ] **Step 3: 编译验证 platform crate（riscv64 feature）**

Run: `cd rt-async && cargo build -p platform --features riscv64 --target riscv64imac-unknown-none-elf`
Expected: 编译通过。

> 若报 `can't find crate for 'core'`，需 `build-std`。rt-async workspace 的 target 配置见 `apps` 的 `.cargo/config`。可改用：`cargo build -p platform --features riscv64`（让默认 target 生效）或直接进 Step 4 用真实 app 验证。

- [ ] **Step 4: 关键验证——QEMU app 仍能编译（弱符号默认空实现不破坏现有路径）**

Run: `cd apps/rt-async-app && cargo build --target riscv64imac-unknown-none-elf --release --bin demo`
Expected: 编译通过。
（这证明：QEMU app 不提供 `_board_init` 强定义时，弱符号空实现被链接，`platform::init()` 中的 `unsafe { _board_init() }` 调用落到空 ret，无副作用。）

- [ ] **Step 5: 验证弱符号确实存在且为 W 绑定（在 QEMU app 产物上）**

Run: `riscv64-elf-nm target/riscv64imac-unknown-none-elf/release/demo | grep _board_init`
Expected: 一行，第二列为 `W`（weak），如 `... W _board_init`。
（若为 `T` 则说明意外被强定义覆盖，需排查；若无输出则弱符号未被链接，需排查 global_asm。）

- [ ] **Step 6: Commit**

```bash
git add rt-async/modules/platform/archs/riscv64-rt/src/lib.rs rt-async/modules/platform/src/lib.rs
git commit -m "feat(platform): init() 加 arch + chip(.weak _board_init) 两个 init 钩子"
```

---

## Task 3: chip-k3-rt24 crate 骨架（Cargo.toml + build.rs + 空 lib.rs）

**Files:**
- Create: `modules/chip-k3-rt24/Cargo.toml`
- Create: `modules/chip-k3-rt24/build.rs`
- Create: `modules/chip-k3-rt24/src/lib.rs`（先放空骨架，下个任务填充）

- [ ] **Step 1: 创建 `modules/chip-k3-rt24/Cargo.toml`**

```toml
[package]
name = "chip-k3-rt24"
version = "0.1.0"
edition = "2024"
publish = false

[dependencies]
platform = { path = "../../rt-async/modules/platform" }
extern-trait = "0.4.3"
riscv = "0.16.0"

[build-dependencies]
xtask = { path = "../../xtask" }
```

- [ ] **Step 2: 创建 `modules/chip-k3-rt24/build.rs`**

```rust
use std::path::Path;

fn main() {
    let ws = xtask::config::workspace_dir_from_manifest();
    let config = xtask::config::load_amp_toml(&ws);
    let out_dir_str = std::env::var("OUT_DIR").unwrap();
    let out_dir = Path::new(&out_dir_str);
    xtask::config::generate_amp_rs(&config, out_dir);
    println!("cargo:rerun-if-changed={}/amp.toml", ws.display());
}
```

- [ ] **Step 3: 创建 `modules/chip-k3-rt24/src/lib.rs`（最小骨架）**

```rust
//! # K3 RT24 rcpu1 芯片实现
//!
//! 为进迭时空 K3 SoC 的 RT24 实时小核（rcpu1，CVA6/RV64GC）提供
//! [`Chip`] / [`TimerChip`] 实现与板级初始化。
//!
//! 初始化序列移植自 esos 的 `os1_rcpu/baremetal/main.c`（已验证）。

#![no_std]
```

- [ ] **Step 4: 编译验证 crate 能被 workspace 识别且编译通过**

Run: `cargo build -p chip-k3-rt24 --target riscv64imac-unknown-none-elf`
Expected: 编译通过（空 crate）。

- [ ] **Step 5: Commit**

```bash
git add modules/chip-k3-rt24
git commit -m "feat(chip-k3-rt24): crate 骨架（Cargo.toml/build.rs/lib.rs）"
```

---

## Task 4: clock.rs — 握手 + 时钟链 + pinmux（步骤 1-4）

**Files:**
- Create: `modules/chip-k3-rt24/src/clock.rs`
- Modify: `modules/chip-k3-rt24/src/lib.rs`（pub mod clock）

- [ ] **Step 1: 创建 `modules/chip-k3-rt24/src/clock.rs`**

常量与函数均直接对应 `main.c`，注释标明来源行。

```rust
//! K3 RT24 rcpu1 时钟链 + pinmux + SPL 握手常量与初始化。
//!
//! 移植自 esos `os1_rcpu/baremetal/main.c`（已验证），对应
//! 设计文档 §1.5 的步骤 1-4。

// 步骤1：SPL 启动握手。k3_rproc_start() 唤醒 rcpu1 后死等
// CORE0_BOOT_ENTRY_LO 非 0（~6s）。rcpu1 必须回写 *CORE0* 寄存器
// （交叉规则：rcpu0 写 CORE1，rcpu1 写 CORE0）。必须最先做，否则 AP 卡 6s。
// 见 drivers/remoteproc/k3-rproc.c k3_rproc_start() case 1。
pub const RCPU_CORE0_BOOT_ENTRY_LO: usize = 0xc088_007c;

// 步骤2：RCPU_UART_NM_CLK_14M_CTRL（0xc0880000+0x3C，BASE_TYPE_RCPU reg 3）。
// 上游 DDN 分频器，产生 ruart_14(~14.48MHz)——UART0 末端 mux 的输入 0。
//   bit[31]    gate (1=使能分频器)
//   bit[30:16] den  (0x64，来自 ruart_14_tbl)
//   bit[15:0]  num  (0x6a1，来自 ruart_14_tbl)
// 不置 bit31 则 ruart_14 被关，UART0 即便自身 gate 开了也无时钟。
// 见 ccu-spacemit-k3.c:422 ruart_14_tbl / ruart_14。
pub const RUART_14_CLK_CTRL: usize = 0xc088_003c;
pub const RUART_14_GATE_BIT: u32 = 1u32 << 31;

// 步骤3：RCPU1_UART0_CLK_RST（0xc0881f00，CCU reg-block index 4 offset 0）。
//   bit[1:0]  gate (0x3=使能)
//   bit[5:4]  mux  (0=ruart_14 ~14.48MHz)
//   bit[18:8] div  (0=/1)
// 见 ccu-spacemit-k3.c:442 ruart0_clk。
pub const UART0_CLK_RST: usize = 0xc088_1f00;
pub const UART0_CLK_RST_ENABLE: u32 = 0x0000_0003;

// 步骤4：pinmux。ruart0_3_cfg 用 "pinctrl-single,pins"（offset/value 对，
// 每 pin 一个寄存器），故 GPIO_n 寄存器 = PINCTRL_BASE + n*4。
//   GPIO_122 (0x1e8) -> UART0_TX,  GPIO_123 (0x1ec) -> UART0_RX
// 值 = MUX_MODE4 | EDGE_NONE | PULL_UP | PAD_DS8 = 0xD044（per ruart0_3_cfg）。
pub const PINCTRL_BASE: usize = 0xd401_e000;
pub const UART0_TX_PIN: usize = 122;
pub const UART0_RX_PIN: usize = 123;
pub const UART0_PIN_VAL: u32 = 0xD044;

#[inline(always)]
pub(crate) fn write32(addr: usize, val: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, val) };
}

#[inline(always)]
pub(crate) fn read32(addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

/// 握手回写 + 上游 ruart_14 gate + UART0 末端 gate + pinmux（步骤 1-4）。
///
/// `_board_init()` 第一步调用。握手必须最先（解锁 AP 的 6s 轮询）。
pub fn early_init() {
    // 1. SPL 握手回写（最先，解锁 AP）
    write32(RCPU_CORE0_BOOT_ENTRY_LO, 1);

    // 2. 使能上游 ruart_14 DDN gate（保留 num/den，只置 bit31）
    let v = read32(RUART_14_CLK_CTRL) | RUART_14_GATE_BIT;
    write32(RUART_14_CLK_CTRL, v);

    // 3. 使能 UART0 末端 gate（gate=0x3、mux=ruart_14、div=/1）
    write32(UART0_CLK_RST, UART0_CLK_RST_ENABLE);

    // 4. pinmux：GPIO_122=TX, GPIO_123=RX
    write32(PINCTRL_BASE + UART0_TX_PIN * 4, UART0_PIN_VAL);
    write32(PINCTRL_BASE + UART0_RX_PIN * 4, UART0_PIN_VAL);
}
```

- [ ] **Step 2: 在 `lib.rs` 暴露 clock 模块**

把 `lib.rs` 末尾追加：

```rust

pub mod clock;
```

- [ ] **Step 3: 编译验证**

Run: `cargo build -p chip-k3-rt24 --target riscv64imac-unknown-none-elf`
Expected: 编译通过。

- [ ] **Step 4: Commit**

```bash
git add modules/chip-k3-rt24/src/clock.rs modules/chip-k3-rt24/src/lib.rs
git commit -m "feat(chip-k3-rt24): clock.rs 握手+ruart_14+uart0 gate+pinmux（步骤1-4）"
```

---

## Task 5: uart.rs — PXA-UART 波特率/FIFO/UUE + putc（步骤 5-7）

**Files:**
- Create: `modules/chip-k3-rt24/src/uart.rs`
- Modify: `modules/chip-k3-rt24/src/lib.rs`（pub mod uart）

- [ ] **Step 1: 创建 `modules/chip-k3-rt24/src/uart.rs`**

```rust
//! K3 RT24 rcpu1 UART0 驱动（PXA 派生 UART，`spacemit,pxa-uart0`）。
//!
//! 移植自 esos `os1_rcpu/baremetal/main.c` + `pxa_uart.h`/`pxa_uart_initialize()`，
//! 对应设计文档 §1.5 的步骤 5-7。

pub const UART0_BASE: usize = 0xc088_1000;

// NS16550 兼容寄存器偏移
const THR: usize = 0x000; // 发送保持
const IER: usize = 0x004; // 中断使能（DLAB=0 时）；DLH（DLAB=1 时）
const FCR: usize = 0x008; // FIFO 控制
const LCR: usize = 0x00C; // 线路控制
const MCR: usize = 0x010; // modem 控制
const LSR: usize = 0x014; // 线路状态
const DLL: usize = 0x000; // 除数低（DLAB=1）
const DLH: usize = 0x004; // 除数高（DLAB=1）

// PXA-uart 专属使能位——不置 UUE，整个 UART 单元 disabled，THR 写入不出波形。
// 见 esos pxa_uart.h:35,52 与 pxa_uart_initialize()。
const UART_IER_UUE: u32 = 0x40; // UART Unit Enable
const UART_MCR_OUT2: u32 = 0x08;

const LCR_DLAB: u32 = 0x80; // 设波特率时置
const LCR_8N1: u32 = 0x03; // 8 数据位、1 停止位、无校验
const FCR_ENABLE_CLEAR: u32 = 0x07; // 使能 FIFO + 清 RX/TX

const LSR_THR_EMPTY: u32 = 0x20; // THR 空（可写）

// 14.48MHz / (16 * 115200) ≈ 8
const DIVISOR: u32 = 8;

#[inline(always)]
fn write32(addr: usize, val: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, val) };
}

#[inline(always)]
fn read32(addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

/// 配置波特率/FIFO/帧格式 + UUE 单元使能（步骤 5-6）。
///
/// `_board_init()` 第二步调用（在 `clock::early_init()` 之后）。
/// 步骤6（UUE）是 PXA-uart 最易漏的关键位。
pub fn init() {
    // 5. 波特率：设 DLAB → DLL/DLH → 清 DLAB 设 8N1 → FCR
    write32(UART0_BASE + LCR, LCR_DLAB);
    write32(UART0_BASE + DLL, DIVISOR & 0xFF);
    write32(UART0_BASE + DLH, (DIVISOR >> 8) & 0xFF);
    write32(UART0_BASE + LCR, LCR_8N1);
    write32(UART0_BASE + FCR, FCR_ENABLE_CLEAR);

    // 6. UUE 单元使能（PXA 专属，⭐ 最易漏）
    write32(UART0_BASE + IER, UART_IER_UUE);
    write32(UART0_BASE + MCR, UART_MCR_OUT2);
}

/// 轮询 LSR bit5（THR 空）后写 THR（步骤 7）。
pub fn putc(c: u8) {
    while read32(UART0_BASE + LSR) & LSR_THR_EMPTY == 0 {}
    write32(UART0_BASE + THR, c as u32);
}
```

- [ ] **Step 2: 在 `lib.rs` 暴露 uart 模块**

把 `lib.rs` 末尾（`pub mod clock;` 之后）追加：

```rust
pub mod uart;
```

- [ ] **Step 3: 编译验证**

Run: `cargo build -p chip-k3-rt24 --target riscv64imac-unknown-none-elf`
Expected: 编译通过。

- [ ] **Step 4: Commit**

```bash
git add modules/chip-k3-rt24/src/uart.rs modules/chip-k3-rt24/src/lib.rs
git commit -m "feat(chip-k3-rt24): uart.rs 波特率/FIFO/UUE+putc（步骤5-7）"
```

---

## Task 6: lib.rs — Chip/TimerChip(stub) 实现 + _board_init 强覆盖

**Files:**
- Modify: `modules/chip-k3-rt24/src/lib.rs`

- [ ] **Step 1: 在 `lib.rs` 末尾追加 Chip/TimerChip 实现 + `_board_init` 强覆盖**

把 `lib.rs` 末尾（`pub mod uart;` 之后）追加：

```rust

use extern_trait::extern_trait;
use platform::{Chip, TimerChip};

/// K3 RT24 rcpu1 芯片类型（零大小，仅作 trait impl 载体）。
pub struct K3Rt24;

/// chip 钩子：覆盖 arch 的 `.weak _board_init` 弱符号。
/// K3 全部硬件初始化在此：握手+时钟链+pinmux（步骤1-4）+ 波特率/UUE（步骤5-6）。
/// 由 `platform::init()` 经弱符号调用（早于用户 main）。
#[unsafe(no_mangle)]
pub extern "C" fn _board_init() {
    clock::early_init(); // 步骤 1-4（含握手回写，最先 → 解锁 AP）
    uart::init(); // 步骤 5-6（波特率/8N1/FCR + UUE⭐）
}

#[extern_trait]
impl Chip for K3Rt24 {
    fn shutdown() -> ! {
        loop {}
    }

    fn put_str(s: &str) {
        for &b in s.as_bytes() {
            if b == b'\n' {
                uart::putc(b'\r'); // 串口需 \r\n
            }
            uart::putc(b);
        }
    }

    unsafe fn pend() {}

    unsafe fn clear_pend() {}
}

/// TimerChip stub（方案 A）：minimal 无定时器任务，rtimer 留后续。
/// `enable_timer_irq()` 为空操作，不产生中断。
#[extern_trait]
impl TimerChip for K3Rt24 {
    fn freq_hz() -> u32 {
        0
    }

    fn now_ticks() -> u64 {
        0
    }

    fn set_deadline(_tick: u64) {}

    unsafe fn enable_timer_irq() {}
}
```

- [ ] **Step 2: 编译验证（含 extern_trait 注册）**

Run: `cargo build -p chip-k3-rt24 --target riscv64imac-unknown-none-elf`
Expected: 编译通过。

- [ ] **Step 3: Commit**

```bash
git add modules/chip-k3-rt24/src/lib.rs
git commit -m "feat(chip-k3-rt24): impl Chip/TimerChip(stub) + _board_init 强覆盖"
```

---

## Task 7: apps/rt-async-k3 — 独立 K3 app crate 骨架

**Files:**
- Create: `apps/rt-async-k3/Cargo.toml`
- Create: `apps/rt-async-k3/.cargo/config.toml`
- Create: `apps/rt-async-k3/build.rs`
- Create: `apps/rt-async-k3/src/bin/minimal.rs`

- [ ] **Step 1: 创建 `apps/rt-async-k3/Cargo.toml`**

```toml
[package]
name = "rt-async-k3"
version = "0.1.0"
edition = "2024"
publish = false

[dependencies]
executor = { path = "../../rt-async/modules/executor" }
platform = { path = "../../rt-async/modules/platform" }
chip-k3-rt24 = { path = "../../modules/chip-k3-rt24" }
log = { version = "0.4", default-features = false }

[build-dependencies]
xtask = { path = "../../xtask" }

[features]
default = ["riscv64"]
riscv64 = ["platform/riscv64"]

[[bin]]
name = "minimal"

[profile.dev]
panic = "abort"

[profile.release]
panic = "abort"
```

- [ ] **Step 2: 创建 `apps/rt-async-k3/.cargo/config.toml`**

```toml
[build]
target = "riscv64imac-unknown-none-elf"

[unstable]
build-std = ["core"]
```

- [ ] **Step 3: 创建 `apps/rt-async-k3/build.rs`**

仿 `apps/rt-async-app/build.rs`，链接 riscv64-rt 的 link.x + 从 amp.toml 生成 memory.x（基址用 K3 的 `RT24RCPU1BASE`/`RT24RCPU1SIZE`）。

```rust
use std::path::Path;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = std::env::var("OUT_DIR").unwrap();

    println!("cargo:rustc-link-search={}", out_dir);
    let rt_dir = format!("{}/../../rt-async/modules/platform/archs/riscv64-rt", manifest_dir);
    println!("cargo:rustc-link-search={}", rt_dir);
    println!("cargo:rustc-link-arg=-Tmemory.x");
    println!("cargo:rustc-link-arg=-Tlink.x");
    println!("cargo:rerun-if-changed=build.rs");

    let ws = xtask::config::workspace_dir_from_manifest();
    println!("cargo:rerun-if-changed={}/amp.toml", ws.display());

    let amp = xtask::config::load_amp_toml(&ws);

    let rtasync_base = amp.get("RT24RCPU1BASE").expect("missing RT24RCPU1BASE");
    let rtasync_size =
        xtask::config::parse_size(amp.get("RT24RCPU1SIZE").expect("missing RT24RCPU1SIZE"));

    let memory_x = format!(
        "ENTRY(__start);\n\nMEMORY\n{{\n    RAM : ORIGIN = {rtasync_base}, LENGTH = 0x{rtasync_size:x}\n}}\n\n_max_hart_id = 0;\n_hart_stack_size = 4096;\n"
    );
    std::fs::write(Path::new(&out_dir).join("memory.x"), memory_x).unwrap();
}
```

- [ ] **Step 4: 创建 `apps/rt-async-k3/src/bin/minimal.rs`**

```rust
//! K3 RT24 rcpu1 最小化验证：R_UART0 输出 `hello from rt-async`。
//!
//! 板级初始化（握手+时钟+pinmux+UUE）已在 `platform::init()` 内由
//! `chip-k3-rt24` 的 `_board_init` 强覆盖完成（早于本 main）。

#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

use core::pin::Pin;

use executor::priority::Priority;
use executor::spawner::Spawner;
use platform::{Chip, ChipImpl};

#[executor::main]
fn main(_spawner: Pin<&'static Spawner<1>>) {
    // board_init 已在 platform::init() 内由钩子完成
    ChipImpl::put_str("hello from rt-async\n");
}

#[executor::interrupt]
fn MachineSoft(_tf: &mut platform::arch::TrapFrame) {}
```

- [ ] **Step 5: 编译验证 app 能链接出 ELF**

Run: `cd apps/rt-async-k3 && cargo build --release --bin minimal`
Expected: 编译链接通过，产出 `target/riscv64imac-unknown-none-elf/release/minimal`。

> 若报 undefined symbol 之类链接错误，常见原因：link.x 未找到（检查 build.rs 的 link-search 路径）、`__start` 未定义（确认 riscv64-rt 被 platform 依赖引入）。

- [ ] **Step 6: Commit**

```bash
git add apps/rt-async-k3
git commit -m "feat(rt-async-k3): 独立 K3 app crate + minimal bin（基址 0x100804000）"
```

---

## Task 8: 验证 —— ELF entry / 弱符号覆盖 / QEMU 未回归

**Files:** （无改动，仅检查）

- [ ] **Step 1: 验证 K3 ELF 的 entry = 0x100804000**

Run: `riscv64-elf-readelf -h target/riscv64imac-unknown-none-elf/release/minimal | grep "Entry point"`
Expected: `Entry point address: 0x100804000`

- [ ] **Step 2: 验证 `_board_init` 在 K3 产物中为强定义（T），解析到 chip 实现**

Run: `riscv64-elf-nm target/riscv64imac-unknown-none-elf/release/minimal | grep _board_init`
Expected: 一行，第二列为 `T`（强），如 `00000000100804... T _board_init`。
（注意：release profile 有 `strip = true`（根 Cargo.toml），可能剥离符号。若 nm 无输出，临时用 debug 构建验证：`cargo build --bin minimal` 后 `nm .../debug/minimal | grep _board_init`，应见 `T`。）

- [ ] **Step 3: 验证 K3 Chip/TimerChip 已注册（extern_trait）**

Run: `riscv64-elf-nm target/riscv64imac-unknown-none-elf/release/minimal | grep -iE "k3rt24|board_init|put_str" | head`
Expected: 至少见 `_board_init`（T）。extern_trait 的派发符号名是哈希化的，不必逐个核对；重点是链接成功 + `_board_init` 强定义存在。

- [ ] **Step 4: 回归验证——QEMU app（demo）仍正常编译链接 + `_board_init` 仍为弱（W）**

Run: `cd apps/rt-async-app && cargo build --release --bin demo`
Expected: 编译链接通过。
Run: `riscv64-elf-nm target/riscv64imac-unknown-none-elf/release/demo | grep _board_init`
Expected: 第二列为 `W`（weak，未覆盖，落 arch 空实现）。
（若 demo 产物被 strip 无符号，用 debug 构建 `cargo build --bin demo` 后再 nm。）

- [ ] **Step 5: 若上述全过，Commit（记录验证通过点，便于回溯）**

```bash
git commit --allow-empty -m "verify: K3 ELF entry=0x100804000 + _board_init 强覆盖；QEMU 未回归（_board_init 仍 W）"
```

---

## Task 9: 产出可刷板的 .bin（objcopy）+ 文档化刷板步骤

**Files:**
- Create: `scripts/k3-build-minimal.sh`（可选，便利脚本）

- [ ] **Step 1: objcopy 产出 flat binary**

Run:
```bash
mkdir -p build
riscv64-elf-objcopy -O binary target/riscv64imac-unknown-none-elf/release/minimal build/rt24_os1_rcpu_rtasync_k3.bin
```
Expected: 产出 `build/rt24_os1_rcpu_rtasync_k3.bin`，大小为 ELF 实际代码/数据段之和（几 KB）。

- [ ] **Step 2: （可选）创建便利脚本 `scripts/k3-build-minimal.sh`**

```bash
#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
(cd apps/rt-async-k3 && cargo build --release --bin minimal)
mkdir -p build
riscv64-elf-objcopy -O binary \
    target/riscv64imac-unknown-none-elf/release/minimal \
    build/rt24_os1_rcpu_rtasync_k3.bin
echo "rt-async-k3 minimal → build/rt24_os1_rcpu_rtasync_k3.bin"
```
然后 `chmod +x scripts/k3-build-minimal.sh`。

- [ ] **Step 3: 文档化刷板步骤（在 spec 的"成功标准"或周报里记录）**

刷板流程（用户在 esos 仓库侧操作）：
1. 把 `build/rt24_os1_rcpu_rtasync_k3.bin`（或对应 ELF）lzo 压缩后替换 esos 的 `output/esos/rt24_os1_rcpu.elf.lzo`；
2. 在 esos 仓库 `./build.sh itb` 重打 `esos.itb`（rcpu1-fw 节点 load/entry 已是 `0x100804000`，无需改 its）；
3. 刷板启动，观察 R_UART0 串口。

- [ ] **Step 4: Commit**

```bash
git add scripts/k3-build-minimal.sh
git commit -m "build: k3-build-minimal.sh 便利脚本 + 文档化刷板步骤"
```

---

## 验收（硬件侧，用户执行）

- [ ] **H1: 刷板后 R_UART0 串口看到 `hello from rt-async`**（核心目标）。
- [ ] **H2: SPL 不再卡顿**（U-Boot banner 在正常时间内出现）——证明握手成功、AP 未在 `k3_rproc_start` 超时。
- [ ] **H3（可选面包屑）**: 若串口无输出，用 U-Boot `md.l 0xc088007c`（握手应=1）、`md.l 0xc0881f00`（应=0x3）、`md.l 0xc088003c`（bit31 应置）读回，判断 `_board_init` 跑到哪一步（参照周报三十 §2.2 方法）。

---

## Self-Review

**1. Spec coverage（对照设计文档逐项）：**
- §2 方案A（TimerChip stub）→ Task 6 ✅
- §2 arch 钩子（`arch::arch_init()`）→ Task 2 Step 1 ✅
- §2 chip 钩子（`.weak _board_init` + 强覆盖）→ Task 2 Step 1（弱定义）+ Task 6 Step 1（强覆盖）✅
- §2 调用时机（合进 `platform::init()`）→ Task 2 Step 2 ✅
- §2 amp.toml + workspace → Task 1 ✅
- §3.1 目录结构全部文件 → 文件结构表 + 各 Task ✅
- §3.3 clock.rs（步骤1-4）→ Task 4 ✅
- §3.3 uart.rs（步骤5-7）→ Task 5 ✅
- §3.3 lib.rs（Chip/TimerChip + _board_init）→ Task 6 ✅
- §3.4 app crate + minimal.rs → Task 7 ✅
- §5 成功标准（ELF entry、弱符号、QEMU 未回归）→ Task 8 ✅
- §5 刷板 → Task 9 + 验收 H1/H2 ✅

**2. Placeholder scan:** 无 TBD/TODO；每个代码步骤含完整代码；命令含 expected output。✅

**3. Type consistency:** `K3Rt24`（Task 6）一致；`clock::early_init()`/`uart::init()`/`uart::putc()`（Task 4/5/6）签名一致；`_board_init`（Task 2 弱定义、Task 6 强覆盖）符号名一致；`Spawner<1>`（Task 7）与宏要求的 `Pin<&Spawner<N>>` 一致。✅
