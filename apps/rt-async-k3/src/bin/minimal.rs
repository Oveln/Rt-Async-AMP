//! K3 RT24 rcpu1 最小化验证：R_UART0 输出 `hello from rt-async`。
//!
//! 板级初始化（握手+时钟+pinmux+UUE）已在 `platform::init()` 内由
//! `ChipImpl::board_init()` → `chip-k3-rt24` 的 `Chip::board_init()` 完成
//! （早于本 main）。

#![no_std]
#![no_main]

// ── 强制链接 chip crate ──────────────────────────────────────────────────
// chip-k3-rt24 的对外符号有两类，均以 `export_name`/`#[extern_trait]` 形式
// 导出，不被本 bin 的常规代码路径引用，故 rustc 默认不会把 chip rlib 交给
// 链接器（`--gc-sections` 会剔除）：
//   1. `#[extern_trait]` 的 ChipImpl/TimerChipImpl —— platform 经 link_name 引用；
//   2. `Chip::board_init()` —— platform::init() 经 ChipImpl 调用。
// 因此需要一个对 chip crate 公开符号的真实引用来把整个 rlib 拉入链接集。
// K3Rt24 是 chip crate 的公开零大小类型，保留其实例即可构成强引用锚点。
use chip_k3_rt24::K3Rt24;
#[used]
static _FORCE_LINK_CHIP_K3_RT24: K3Rt24 = K3Rt24;

use core::pin::Pin;

use executor::spawner::Spawner;
use platform::{Chip, ChipImpl};

#[executor::main]
fn main(_spawner: Pin<&'static Spawner<1>>) {
    // board_init 已在 platform::init() 内由钩子完成
    ChipImpl::put_str("hello from rt-async\n");
}

#[executor::interrupt]
fn MachineSoft(_tf: &mut platform::arch::TrapFrame) {}
