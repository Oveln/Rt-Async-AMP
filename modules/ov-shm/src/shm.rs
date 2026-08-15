//! 共享内存区域驱动（compatible `ov,rt-async-amp`）。
//!
//! probe 从 DT 节点 `reg` 读取共享内存基址与大小，存入 `AtomicUsize`。
//! 运行期经 [`base`] / [`size`] 取用——取代旧 `amp.toml` → `amp_gen.rs`
//! → `SHMBASE` 编译期常量的地址来源。
//!
//! 节点由各板 DTS 手写（K3：`its/rt-async-k3.dts`；QEMU virt：对应 dts），
//! AP 侧 StarryOS rt_shm 与 rt-async 侧本驱动匹配同一个 compatible
//! `ov,rt-async-amp`，地址/大小两侧对齐（K3 对齐 amp.toml K3_SHMBASE/SIZE）。

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

/// 将共享内存写入对 AP 可见。
///
/// AMP 无跨核一致性，RP 写共享内存后必须确保 store 到达物理内存（K3 为 RCPU SRAM），
/// 再发通知让 AP 读取。CVA6 的 store 可能滞留在 store/write buffer 或
/// write-back dcache 中。`fence iorw,iorw` 是全内存屏障（对普通内存与 MMIO
/// 均排序，与 `fence rw,rw` 对普通内存的排序能力等价），排空 store buffer
/// 到目标介质。
///
/// 注：若 RP dcache 是 write-back 且 dirty 行未 evict，本 fence 不足以
/// 触发 cache 回写（CVA6 无 Zicbom）。但至少保证 store buffer 排空。
pub fn flush() {
    // SAFETY: fence iorw,iorw 是内存屏障指令，无内存副作用，仅保证后续
    // IO 操作在之前的 store 之后对总线可见。
    unsafe { core::arch::asm!("fence iorw, iorw"); }
}
