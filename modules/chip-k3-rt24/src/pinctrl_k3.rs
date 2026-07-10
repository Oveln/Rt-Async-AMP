//! K3 RT24 pinctrl-single 驱动。
//!
//! 实现 `pinctrl-single` 绑定的最小子集：解析外设节点的 `pinctrl-0` 属性
//!（phandle → `_cfg` 子节点 → `pinctrl-single,pins` 的 (offset, value) 对），
//! 逐对写入 pinmux 寄存器。
//!
//! `pinctrl-single,pins` 编码：`K3_PADCONF(pinid, conf)` 经 cc 预处理 +
//! dtc 求值后展开为两个 u32 cell `[offset=pinid*4, value=MUX|EDGE|PULL|DS]`。
//! 故该属性是平坦 u32 数组，两两一组。
//!
//! ## 调用时机
//!
//! [`platform::driver::boot`] 遍历 DT 时，对每个节点在 driver probe **之前**
//! 调用 [`PinController::apply`]。pinctrl controller 节点（`compatible =
//! "pinctrl-single"`）先于外设节点被 probe（DFS 先序），故外设 probe 时
//! 引脚已配置完毕。无 `pinctrl-0` 的节点 apply 为 no-op。

use core::sync::atomic::{AtomicUsize, Ordering};

use fdt_parser::Node;
use platform::device::{Driver, PinController};
use tock_registers::interfaces::Writeable;
use tock_registers::registers::ReadWrite;
use tock_registers::register_structs;

register_structs! {
    /// 单个 pinmux 配置寄存器（每 pin 4 字节，散落在 base + pin*4）。
    pub PinReg {
        (0x00 => conf: ReadWrite<u32>),
        (0x04 => @END),
    }
}

/// probe 写入的 pinctrl 寄存器基址（从 DT `reg` 属性读取）。
static BASE: AtomicUsize = AtomicUsize::new(0);

/// pinctrl-single 控制器单例（零大小）。
pub struct PinctrlK3;

/// 全局单例，供 probe 注册进 [`platform::driver::PINCTRL`]。
pub static PINCTRL: PinctrlK3 = PinctrlK3;

impl Driver for PinctrlK3 {
    fn compatible(&self) -> &'static [&'static str] {
        &["pinctrl-single"]
    }

    fn probe(&self, node: &Node<'_>) {
        let base = node
            .reg()
            .and_then(|mut r| r.next())
            .expect("k3 pinctrl: missing reg property")
            .address as usize;
        BASE.store(base, Ordering::Release);
        platform::driver::PINCTRL.set(&PINCTRL);
        log::info!("k3 pinctrl: base {:#x}", base);
    }
}

impl PinController for PinctrlK3 {
    fn apply(&self, node: &Node<'_>) {
        // 1. 读 pinctrl-0 phandle；无则 no-op。
        let Some(phandle_prop) = node.find_property("pinctrl-0") else {
            return;
        };
        let phandle = phandle_prop.u32().into();

        // 2. phandle → _cfg 节点。
        let Some(cfg) = node.fdt().get_node_by_phandle(phandle) else {
            log::warn!("k3 pinctrl: {} has dangling pinctrl-0", node.name());
            return;
        };

        // 3. 读 pinctrl-single,pins → 平坦 u32 数组 [offset, value, ...]。
        let Some(pins_prop) = cfg.find_property("pinctrl-single,pins") else {
            log::warn!("k3 pinctrl: {} has no pinctrl-single,pins", cfg.name());
            return;
        };

        let base = BASE.load(Ordering::Acquire);

        // 4. 两两一组写入 pinmux 寄存器。
        let mut vals = pins_prop.u32_list();
        while let (Some(offset), Some(value)) = (vals.next(), vals.next()) {
            let addr = base + offset as usize;
            // SAFETY: addr = pinctrl base + pin offset，来自 DT reg + 编译期
            // 求值的 pinctrl-single,pins 元组。指向有效 MMIO 区，单 hart 串行。
            let reg: &PinReg = unsafe { &*(addr as *const PinReg) };
            reg.conf.set(value);
        }
    }
}
