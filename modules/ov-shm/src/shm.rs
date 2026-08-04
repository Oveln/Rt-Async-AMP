//! 共享内存区域驱动（compatible `ov,rt-async-amp`）。
//!
//! probe 从 DT 节点 `reg` 读取共享内存基址与大小，存入 `AtomicUsize`。
//! 运行期经 [`base`] / [`size`] 取用——取代旧 `amp.toml` → `amp_gen.rs`
//! → `SHMBASE` 编译期常量的地址来源。
//!
//! 节点与 notifier 子节点统一由 `its/rt-async-shm.dtsi` 生成（单一真相源，
//! AP 侧 StarryOS rt_shm 与 rt-async 侧本驱动匹配同一个 compatible
//! `ov,rt-async-amp`，两侧 DTS 经宏参数化引用同一 dtsi）。

use core::sync::atomic::{AtomicUsize, Ordering};

use fdt_parser::Node;
use platform::device::Driver;

/// probe 写入的 SHM 基址。0 表示尚未 probe。
static BASE: AtomicUsize = AtomicUsize::new(0);
/// probe 写入的 SHM 大小。
static SIZE: AtomicUsize = AtomicUsize::new(0);

/// 共享内存区域单例（零大小）。
pub struct ShmDriver;

/// 全局单例，供板级 driver 列表注册。
pub static INSTANCE: ShmDriver = ShmDriver;

impl Driver for ShmDriver {
    fn compatible(&self) -> &'static [&'static str] {
        &["ov,rt-async-amp"]
    }

    fn probe(&self, node: &Node<'_>) {
        let reg = node
            .reg()
            .expect("ov-shm: missing reg property")
            .next()
            .expect("ov-shm: empty reg");
        let base = reg.address as usize;
        let size = reg.size.expect("ov-shm: #size-cells = 0, missing size") as usize;
        assert!(base != 0, "ov-shm: zero base address");
        BASE.store(base, Ordering::Release);
        SIZE.store(size, Ordering::Release);
        log::info!("[ov-shm] probed: base={base:#x}, size={size:#x}");
    }
}

/// 返回共享内存基址。probe 前调用为 panic。
pub fn base() -> usize {
    let base = BASE.load(Ordering::Acquire);
    assert!(base != 0, "ov-shm: shm driver not probed");
    base
}

/// 返回共享内存大小。probe 前调用为 panic。
pub fn size() -> usize {
    SIZE.load(Ordering::Acquire)
}
