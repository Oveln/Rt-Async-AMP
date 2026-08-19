//! 共享内存区域驱动（compatible `ov,rt-async-amp`）。
//!
//! probe 从 DT 节点 `reg` 读取共享内存基址与大小，存入 `AtomicUsize`。
//! 运行期经 [`base`] / [`size`] 取用——取代旧 `amp.toml` → `amp_gen.rs`
//! → `SHMBASE` 编译期常量的地址来源。
//!
//! 节点定义：K3 手写在 `its/rt-async-k3.dts`；QEMU virt 两侧 DTS 经宏
//! 参数化引用单一真相源 `its/rt-async-shm.dtsi`。AP 侧 StarryOS rt_shm 与
//! rt-async 侧本驱动匹配同一个 compatible `ov,rt-async-amp`，地址/大小
//! 两侧对齐：K3 对齐 its/rt-async-k3.dts + tgoskits AP dts，QEMU 对齐
//! its/rt-async-shm.dtsi（amp.toml 不再持有这些值）。

use core::sync::atomic::{AtomicUsize, Ordering};

use fdt_parser::Node;
use platform::device::Driver;

/// probe 写入的 SHM 基址。0 表示尚未 probe。
// 哨兵 usize::MAX：probe 前的「未初始化」标记——基址本身可为 0
// （K3 RP 侧本地别名窗口，见 probe 注释），不能用 0 做哨兵。
static BASE: AtomicUsize = AtomicUsize::new(usize::MAX);
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
        // ②c：允许基址 0——K3 RP 侧 DT 故意用 RCPU 本地 SRAM 别名窗口
        // （0x0..0x80000 镜像主域 0xC080_0000）访问共享窗，冷读 7× 快于
        // 绕 M2F 桥的主域端口。probe 由 compatible 匹配驱动、节点 reg 显式
        // 存在，0 不是「解析失败」信号（未 probe 由 base() 的哨兵断言兜底）。
        BASE.store(base, Ordering::Release);
        SIZE.store(size, Ordering::Release);
        log::info!("[ov-shm] probed: base={base:#x}, size={size:#x}");
    }
}

/// 返回共享内存基址。probe 前调用为 panic。
///
/// 读取用 Relaxed：BASE 仅由本 hart 在 probe（boot DFS、开中断前）写入
/// 一次后只读，同 hart 程序序已保证可见。热路径（process_elastic 每唤醒
/// 周期、弹性自旋）必须避免 Acquire——K3 上原子读经 Atomics Wrapper
/// 序列化 ~2.2µs/笔（2026-08-17 延迟归因实测）。
pub fn base() -> usize {
    let base = BASE.load(Ordering::Relaxed);
    assert!(base != usize::MAX, "ov-shm: shm driver not probed");
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
