# 设计：rt-async 在 K3 RT24 rcpu1 上的最小化 minimal

- **日期**：2026-07-03
- **目标**：让 rt-async 编译出的 ELF 能在 K3 SoC 的 RT24 rcpu1 实时核上运行，最小化验证：R_UART0 串口输出 `hello from rt-async`，且不拖累 AP 启动（SPL 握手成功、不卡 ~6s）。
- **依据**：`esos/bsp/spacemit/platform/rt24/os1_rcpu/baremetal/main.c`（已验证的裸机初始化序列）+ `周报三十`（根因总结）+ `esos` 源码（`pxa_uart.h` / `ccu-spacemit-k3.c`，常量交叉验证）。

---

## 1. 现状摸清（已确认的硬件 / 软件事实）

### 1.1 K3 rcpu1 加载链与地址

- `esos_rt24.its` 的 `rcpu1-fw` 节点：`load = <0x1 0x804000>` = `entry = 0x100804000`。即 **rcpu1 固件链接基址 = `0x100804000`**，可用区域 3M（与 esos `baremetal.ld` `LENGTH = 3M` 一致）。
- `build/stage/rt24_os1_rcpu_rtasync.elf`（既有产物）entry = `0x100804000`，与上述吻合。
- 加载链：SPL 从 `esos.itb` 加载 → `k3-rproc.c k3_rproc_start()` 解复位 + 唤醒 → AP 死等 `CORE0_BOOT_ENTRY_LO(0xc088007c)` 非 0（最多 ~6s）。**rcpu1 必须回写 CORE0 寄存器**（交叉规则：rcpu1 写 CORE0，rcpu0 写 CORE1）。

### 1.2 rt-async 启动 / 架构

- 入口：`__start`(asm，`.init` 段) → 设 gp/sp、可选 clear_bss → `__start_rust`(写 mtvec) → `j __rust_main`。
- `__rust_main` 由 `#[executor::main]` 宏（`executor-macro/src/lib.rs`）生成，依次：
  1. `platform::init(log_level)` — **目前只设 logger，无 MMIO**；
  2. `Spawner::new()` + `.init()`；
  3. 用户 `main(spawner)`；
  4. `platform::start()` — 调 `TimerChipImpl::enable_timer_irq()` + `enable_msi/mei` + 开全局中断；
  5. `loop { platform::idle() }`（WFI）。
- 因此 **`main` 之前不会有任何 K3 无效地址的 MMIO 访问**；钩子挂在 `platform::init()` 即在用户 `main` 之前、logger 设置之后，安全。

### 1.3 依赖方向（决定钩子机制）

- `platform` crate **依赖** `riscv64-rt`（`platform/Cargo.toml` 的 `riscv64` feature）。
- 故 `riscv64-rt`（arch）**不能反向依赖 platform** → Rust 不允许环。arch crate 因此不能实现 platform 定义的 trait。
- chip crate（`chip-k3-rt24`）**依赖 platform**，无环，可正常用 `extern_trait` 注册。

### 1.4 Chip / TimerChip 注册机制

- `platform/src/lib.rs` 用 `#[extern_trait(pub ChipImpl)] trait Chip`、`#[extern_trait(pub TimerChipImpl)] trait TimerChip`。
- 下游 chip crate `#[extern_trait] impl Chip for Xxx` 即注册到全局 `ChipImpl` / `TimerChipImpl`，运行时 `ChipImpl::put_str(...)` 分发。
- `platform::start()` 必然调 `TimerChipImpl::enable_timer_irq()`，故 K3 chip **必须同时实现 `Chip` 与 `TimerChip`**（TimerChip 可 stub）。

### 1.5 初始化序列（main.c 7 步，已被 esos 源码逐项验证）

| 步 | 操作 | 地址 / 值 | 来源 |
|----|------|-----------|------|
| 1 | SPL 握手回写 | `write32(0xc088007c, 1)`（rcpu1 写 CORE0，**最先**） | 周报 §2.1 |
| 2 | 上游 ruart_14 DDN gate | `0xc088003c \|= bit31`（保留 num=0x6a1/den=0x64） | `ccu-spacemit-k3.c:422` ruart_14_tbl |
| 3 | UART0 末端 gate | `0xc0881f00 = 0x3`（gate=0x3、mux=ruart_14、div=/1） | `ccu-spacemit-k3.c:442` ruart0_clk |
| 4 | pinmux | `0xd401e1e8`/`0xd401e1ec` ← `0xD044`（GPIO_122/123，MUX_MODE4） | dtb ruart0_3_cfg |
| 5 | 波特率/帧格式 | LCR=0x80→DLL=8,DLH=0→LCR=0x03(8N1)→FCR=0x07 | `pxa_uart.h` |
| 6 | **UUE 单元使能**⭐ | IER=0x40、MCR=0x08（PXA 专属，最易漏） | `pxa_uart.h:35,52` |
| 7 | put_str | 轮询 LSR bit5(0x20)，写 THR（`0xc0881000`） | main.c |

---

## 2. 方案决策（已与用户确认）

1. **TimerChip 用 stub（方案 A）**：minimal 无定时器任务，rtimer 寄存器映射留给后续。stub 的 `enable_timer_irq()` 为空操作，不产生中断。
2. **platform 提供两个 init 钩子**，合进 `platform::init()`：
   - **arch 钩子**：在 `riscv64-rt`（arch）定义 `pub fn arch_init()`，platform 内 `arch::arch_init()` 直接调用（platform 已 `pub use riscv64_rt as arch`，依赖方向 platform→arch，**无环**）。
   - **chip 钩子**：用 **`.weak` 弱符号机制**（而非 `extern_trait`、也不走 link.x `PROVIDE` 别名）。符号名 `_board_init`：arch crate 用 `global_asm!` 的 `.weak _board_init` 定义一个**原生弱符号**（空函数体）；chip crate 用 `#[unsafe(no_mangle)] pub extern "C" fn _board_init()` 提供强定义覆盖。理由见下"为什么不用 extern_trait / PROVIDE"。**K3 的握手+时钟+pinmux+UUE 全部放这里**。
3. **调用时机**：两个钩子都合进 `platform::init()`（已被宏调用，无需 app bin 显式调用，也不改宏）。顺序：logger → `arch::arch_init()` → `_board_init()`。
4. **构建组织**：新建独立 K3 app crate `apps/rt-async-k3` + chip crate `modules/chip-k3-rt24`，各自 memory.x（基址 `0x100804000`）；与 QEMU app 完全隔离。
5. **目标 triple**：`riscv64imac-unknown-none-elf`（复用 toolchain；RT24 CVA6 实为 RV64GC，但 minimal 无 FPU 代码，imac 可正确运行）。

### 为什么 chip 钩子用 `.weak` 弱符号而非 extern_trait

`extern_trait`（0.4.3）的派发是**强符号**：proxy 方法经 `#[link_name]` extern 引用实现侧的 `#[export_name]` 符号（见 `extern-trait-impl/src/decl/mod.rs:74-78` 的 `emit_method`）。这意味着**只要 platform 调了 `BoardInitImpl::board_init()`，最终链接的每个二进制都必须注册一个实现，否则链接报"undefined symbol"**。但本项目里：
- QEMU app（`rt-async-app`）只注册 `Chip`/`TimerChip`，不注册任何 board init；
- std-chip（非 riscv64）同理。

它们都会因强符号未定义而链接失败。

**弱符号 `.weak` 是唯一不引入依赖环、又不强制所有 bin 注册的机制**（platform 不依赖 chip crate，无法直接调其函数）。实现方式上，`.weak _board_init` 直接在 arch crate 用 `global_asm!` 定义一个**原生弱符号**（比 `PROVIDE(_board_init = _default_board_init)` + 多一个 `_default_board_init` 别名更直接——少一个符号、不动 link.x）。

链接行为已实测验证（riscv64-elf-ld）：
- 链入 chip crate 的强 `#[no_mangle] _board_init` 时 → `nm` 显示 `T _board_init`（强），调用解析到 chip 实现；
- 不链入时 → `nm` 显示 `W _board_init`（弱），仍链接成功，调用解析到 arch 的空实现。

arch 钩子因 platform→arch 是真实依赖，直接 `arch::arch_init()` 调用即可，连弱符号都不需要。

> 一句话：arch 钩子 = 直接函数调用（有真实依赖）；chip 钩子 = `.weak` 原生弱符号（platform 不依赖 chip crate；arch 提供弱空定义，chip 提供强覆盖）。

---

## 3. 架构设计

### 3.1 目录结构（新增/改动）

```
rt-async-amp/                              (根 workspace)
├── Cargo.toml                             【改】members 加 chip-k3-rt24、rt-async-k3
├── amp.toml                               【改】加 K3 地址段常量
├── rt-async/modules/platform/
│   ├── src/lib.rs                         【改】init() 内调两钩子（extern _board_init + arch::arch_init）
│   └── archs/riscv64-rt/src/lib.rs        【改】加 pub fn arch_init() + .weak _board_init 空定义
├── modules/chip-k3-rt24/                  【新建】K3 芯片实现 crate
│   ├── Cargo.toml
│   ├── build.rs                           生成 memory.x + amp_gen.rs
│   ├── memory.x                           RAM: ORIGIN=0x100804000, LENGTH=3M
│   └── src/
│       ├── lib.rs                         K3Rt24; impl Chip/TimerChip(stub) + #[no_mangle] _board_init 强覆盖
│       ├── uart.rs                        PXA-UART 寄存器常量 + init/putc
│       └── clock.rs                       握手 + ruart_14 + uart0 gate + pinmux 常量与函数
└── apps/rt-async-k3/                      【新建】独立 K3 app crate
    ├── Cargo.toml                         依赖 executor + platform + chip-k3-rt24
    ├── build.rs                           链接 riscv64-rt/link.x + 生成 memory.x
    └── src/bin/minimal.rs                 #[executor::main]; main 里只 put_str
```

### 3.2 platform 改动（核心）

**`rt-async/modules/platform/src/lib.rs`** — 在 `init()` 内调两个钩子：

```rust
extern "C" {
    fn _board_init();   // 弱符号：arch 提供 .weak 空定义，chip crate 可用强 #[no_mangle] 覆盖
}

pub fn init(max_level: log::LevelFilter) {
    let _ = LOGGER.init(max_level);

    #[cfg(feature = "riscv64")]
    arch::arch_init();          // arch 钩子：直接函数调用（platform→arch 真实依赖）

    #[cfg(feature = "riscv64")]
    unsafe { _board_init(); }   // chip 钩子：弱符号，K3 在此做 握手+时钟+pinmux+UUE；其他平台为空
}
```

> `#[cfg(feature="riscv64")]` 守卫使 std-chip（非 riscv64）路径完全不受影响。

**`rt-async/modules/platform/archs/riscv64-rt/src/lib.rs`** — 新增 arch 钩子 + chip 弱符号空定义：

```rust
/// arch 级早期初始化钩子。默认空实现；arch crate 可按需扩展。
/// （mtvec 已在 __start_rust 中设置，故此处不重复。）
pub fn arch_init() {}

// chip 板级初始化钩子：原生弱符号（空函数体）。
// platform 不依赖任何 chip crate，故无法直接调用其函数；改用弱符号，
// chip crate（如 chip-k3-rt24）用 #[no_mangle] extern "C" fn _board_init() 强定义覆盖。
// 不覆盖时（QEMU/std-chip）调用落到此空实现，无副作用。
core::arch::global_asm!(
    ".section .text",
    ".weak _board_init",
    "_board_init:",
    "ret",
);
```

> 不改 link.x、不引入 `_default_board_init` 别名：`.weak _board_init` 直接定义原生弱符号，少一个符号、机制最直接。链接行为见 §2 实测。arch 钩子用直接函数调用（有真实依赖）；chip 钩子用弱符号（platform 不依赖 chip crate，弱符号是唯一不引入环且不强制所有 bin 注册的机制）。二者风格不同是有意为之，对应各自的依赖拓扑。

### 3.3 chip-k3-rt24（核心移植）

**`src/clock.rs`** — 常量 + `enable_clock_chain()`（步骤 1-4）：

```rust
// 步骤1：SPL 握手回写（rcpu1 写 CORE0，必须最先）
pub const RCPU_CORE0_BOOT_ENTRY_LO: usize = 0xc088_007c;

// 步骤2：上游 ruart_14 DDN gate（0xc088003c bit31）
pub const RUART_14_CLK_CTRL: usize = 0xc088_003c;
pub const RUART_14_GATE_BIT: u32 = 1 << 31;

// 步骤3：UART0 末端 gate（gate=0x3、mux=ruart_14、div=/1）
pub const UART0_CLK_RST: usize = 0xc088_1f00;

// 步骤4：pinmux（pinctrl-single,pins，每 pin 一寄存器）
pub const PINCTRL_BASE: usize = 0xd401_e000;
pub const UART0_TX_PIN: usize = 122;   // 寄存器 = PINCTRL_BASE + 122*4 = 0xd401e1e8
pub const UART0_RX_PIN: usize = 123;   // 0xd401e1ec
pub const UART0_PIN_VAL: u32 = 0xD044; // MUX_MODE4 | EDGE_NONE | PULL_UP | PAD_DS8

#[inline(always)]
pub fn write32(addr: usize, val: u32) { /* core::ptr::write_volatile */ }
#[inline(always)]
pub fn read32(addr: usize) -> u32 { /* core::ptr::read_volatile */ }

/// 握手 + 时钟链 + pinmux（步骤 1-4）。board_init 第一步调用。
pub fn early_init() {
    write32(RCPU_CORE0_BOOT_ENTRY_LO, 1);          // 1. 握手（最先，解锁 AP）
    let v = read32(RUART_14_CLK_CTRL) | RUART_14_GATE_BIT;
    write32(RUART_14_CLK_CTRL, v);                  // 2. 上游 ruart_14 gate
    write32(UART0_CLK_RST, 0x3);                    // 3. UART0 末端 gate
    write32(PINCTRL_BASE + UART0_TX_PIN * 4, UART0_PIN_VAL); // 4. pinmux TX
    write32(PINCTRL_BASE + UART0_RX_PIN * 4, UART0_PIN_VAL); // 4. pinmux RX
}
```

**`src/uart.rs`** — PXA-UART 驱动（步骤 5-7）：

```rust
pub const UART0_BASE: usize = 0xc088_1000;
const THR: usize = 0x000; const IER: usize = 0x004; const FCR: usize = 0x008;
const LCR: usize = 0x00C; const MCR: usize = 0x010; const LSR: usize = 0x014;
const DLL: usize = 0x000; const DLH: usize = 0x004;
const DIVISOR: u32 = 8;   // 14.48MHz / (16 * 115200) ≈ 8
const UART_IER_UUE: u32 = 0x40;   // ⭐ PXA Unit Enable
const UART_MCR_OUT2: u32 = 0x08;

/// 波特率/FIFO/帧格式 + UUE（步骤 5-6）。board_init 第二步调用。
pub fn init() { /* LCR=0x80→DLL/DLH→LCR=0x03→FCR=0x07→IER=0x40→MCR=0x08 */ }

/// 轮询 LSR bit5，写 THR（步骤 7）。Chip::put_str 用。
pub fn putc(c: u8) { /* while !(read32(LSR) & 0x20) {}; write32(THR, c) */ }
```

**`src/lib.rs`** — Chip/TimerChip（extern_trait）+ board_init（弱符号覆盖）：

```rust
pub struct K3Rt24;

/// chip 钩子：覆盖 link.x 弱符号 `_board_init`。K3 全部硬件初始化在此。
/// 由 platform::init() 经弱符号调用（早于用户 main）。
#[unsafe(no_mangle)]
pub extern "C" fn _board_init() {
    clock::early_init();   // 步骤 1-4（含握手回写，最先 → 解锁 AP）
    uart::init();          // 步骤 5-6（波特率/8N1/FCR + UUE⭐）
}

#[extern_trait]
impl Chip for K3Rt24 {
    fn shutdown() -> ! { loop {} }
    fn put_str(s: &str) {
        for &b in s.as_bytes() {
            if b == b'\n' { uart::putc(b'\r'); }
            uart::putc(b);
        }
    }
    unsafe fn pend() {}
    unsafe fn clear_pend() {}
}

#[extern_trait]
impl TimerChip for K3Rt24 {   // stub（方案 A）
    fn freq_hz() -> u32 { 0 }
    fn now_ticks() -> u64 { 0 }
    fn set_deadline(_: u64) {}
    unsafe fn enable_timer_irq() {}   // 空操作，不产生中断
}
```

### 3.4 app crate `rt-async-k3`

**`src/bin/minimal.rs`**：

```rust
#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]

use core::pin::Pin;
use executor::priority::Priority;
use executor::spawner::Spawner;
use platform::{Chip, ChipImpl};

#[executor::main]
fn main(_spawner: Pin<&'static Spawner<1>>) {
    // board_init 已在 platform::init() 内由钩子完成（握手+时钟+pinmux+UUE）
    ChipImpl::put_str("hello from rt-async\n");
}

#[executor::interrupt]
fn MachineSoft(_tf: &mut platform::arch::TrapFrame) {}
```

> `Spawner<1>` 仅为满足宏的 `Pin<&Spawner<N>>` 签名要求；不 spawn 任何任务。`MachineSoft` interrupt 占位由宏要求（宏总是生成 `MachineSoft`，若用户定义则改写为 `__Inner_MachineSoft`）。

**`build.rs`**：复用 `apps/rt-async-app/build.rs` 模式，生成 memory.x（基址从 amp.toml 读 `RT24RCPU1BASE`/`RT24RCPU1SIZE`），链接 `-Tmemory.x -Tlink.x`（link.x 来自 riscv64-rt）。

**`Cargo.toml`**：依赖 `executor`、`platform`（features=["riscv64"]）、`chip-k3-rt24`、`log`。`[profile.release] panic="abort"`。

### 3.5 amp.toml 新增

```toml
# ── K3 RT24 rcpu1 ───────────────────────────────────────────────────────────
RT24RCPU1BASE = "0x100804000"
RT24RCPU1SIZE = "0x300000"     # 3M
```

### 3.6 根 workspace `Cargo.toml`

`members` 增加 `"modules/chip-k3-rt24"` 和 `"apps/rt-async-k3"`。

---

## 4. 执行时序（最终）

```
SPL 加载 ELF@0x100804000 → k3_rproc 解复位 → 跳 entry
  __start(asm): 设 gp/sp, clear_bss, __start_rust(写 mtvec) → j __rust_main
  __rust_main(宏生成):
    platform::init(level):
        LOGGER.init(level)              // logger
        arch::arch_init()               // arch 钩子（直接调用，空，扩展点）
        _board_init():                  // chip 钩子（弱符号） ⭐
            clock::early_init()         //   1.握手回写(解锁AP) 2.ruart_14 3.uart0 gate 4.pinmux
            uart::init()                //   5.波特率/8N1/FCR 6.UUE(0x40)+MCR(0x08)
    Spawner::init()
    main(spawner): ChipImpl::put_str("hello from rt-async\n")  // 7.轮询 LSR 写 THR
    platform::start(): TimerChip::enable_timer_irq()(空) + 开中断
    loop { WFI }
```

---

## 5. 成功标准

1. `cargo build -p rt-async-k3 --bin minimal --target riscv64imac-unknown-none-elf --release` 成功，产物 ELF 的 entry = `0x100804000`（`readelf -h` 验证）。
2. ELF 打包进 `esos_rt24.its` 的 `rcpu1-fw`（替换 `rt24_os1_rcpu.elf.lzo`）→ 刷板。
3. **R_UART0 串口看到 `hello from rt-async`**。
4. **SPL 不再卡顿**（U-Boot banner 在正常时间内出现）——证明握手成功、AP 未在 `k3_rproc_start` 超时。
5. QEMU app（`rt-async-app`）仍正常构建运行（钩子改动不破坏现有路径）。

---

## 6. 不做（YAGNI，留给后续）

- xtask 自动构建 / 打包 itb（本次手动 objcopy + lzo + mkimage 验证）。
- `Chip::get_char`、PLIC、rtimer 真实实现、IPC（ov-rpc/intercom）。
- arch_init 的可覆盖弱符号机制（当前空实现即够）。
- 切 `riscv64gc` triple（待需要硬件浮点时）。
- K3 专属 panic / trap 处理（复用 riscv64-rt 默认）。

---

## 7. 风险与缓解

| 风险 | 缓解 |
|------|------|
| extern_trait 强符号导致 QEMU/std-chip 链接失败 | **已在设计层规避**：chip 钩子不用 extern_trait，改用 `.weak` 原生弱符号，未覆盖时为 arch 的空实现。QEMU/std-chip 不受影响（已验证 extern_trait-impl 0.4.3 派发是强符号 `#[link_name]`/`#[export_name]`，故弃用该方案）。 |
| 钩子改动破坏 QEMU/std-chip 路径 | 所有钩子调用均 `#[cfg(feature="riscv64")]` 守卫；std-chip 不开 riscv64 feature。`.weak _board_init` 默认空函数体保证不覆盖时无副作用。 |
| 弱符号 `_board_init` 被 chip 强定义覆盖时符号冲突 | `.weak` 为弱定义、`#[no_mangle] extern "C" fn` 为强定义，链接器选强定义，无冲突。**已用 riscv64-elf-ld 实测**：链入强定义时 `nm` 显示 `T`（强）并解析到 chip 实现；不链入时显示 `W`（弱）仍链接成功。实现阶段再在真实产物上用 `nm` 复核。 |
| RT24 实际 RV64GC，imac triple 缺 FPU 指令 | minimal 无 FPU 代码；release `opt-level="s"` 也不引入硬件浮点。仅当链接器报 float ABI 相关错误才切 gc。 |
| 握手在 `platform::init()` 内，rt-async 启动栈在 init 前已跑（清 bss/设 mtvec） | 这些是纯寄存器/内存操作，无 K3 无效 MMIO 访问；6s 握手超时足够覆盖 rt-async 冷启动。已在 §1.2 分析。 |
