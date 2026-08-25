//! `/dev/rt_shm` 用户态 ABI：ioctl 号与共享窗尺寸。
//!
//! 主仓 user-apps（ipc/mbox/rpc/sched）共用本 crate，避免各自硬编码。
//! 内核侧真值在 `tgoskits/os/StarryOS/kernel/src/pseudofs/dev/rt_shm.rs`，
//! 双仓保持同值（对齐义务记于 AGENTS.md）；长期归宿是并入 ov-channels
//! 随版本发布、双端同源。
//!
//! 窗口尺寸仅用于用户态 mmap 长度；地址布局真源是设备树：
//! qemu 侧 `its/rt-async-shm.dtsi`，K3 侧 `its/rt-async-k3.dts` 与
//! tgoskits 的 AP dts（内核均从 DT probe，不读本 crate）。

/// ioctl：通知对端（RP→AP 方向的门铃语义）
pub const IOC_NOTIFY: u32 = 0x735_001;
/// ioctl：等待对端通知（阻塞）
pub const IOC_AWAIT: u32 = 0x735_002;
/// ioctl：清除 pending 标志
pub const IOC_CLR_PENDING: u32 = 0x735_003;
/// ioctl：软件注入 mailbox new_msg（自测 APLIC→handler 全链路）
pub const IOC_TEST_MBOX: u32 = 0x735_004;
/// （已撤除）原 0x735_005 = 仅 flush 共享窗不发门铃——PMA 非缓存窗口
/// （opensbi-k3 固件翻 entry）后无缓存可维护，ioctl 与内核侧实现一并
/// 删除。号位保留注释防止误复用；内核侧同款注记见 tgoskits rt_shm.rs。

/// ioctl：读内核延迟插桩时间戳（诊断，K3 专用）。
///
/// arg = 用户态 `*mut u64` 数组（2 项），返回
/// `[上次 NOTIFY 门铃 MMIO 写前一瞬的内核单调 ns,
///   上次 K3 mailbox IRQ handler 入口的内核单调 ns]`。
/// 供 bench `dd` 场景与 RP 侧 mtime 戳（PING 回传）交叉分解门铃投递 /
/// 回程耗时。值须与 tgoskits rt_shm.rs 内核侧保持同值（双仓对齐义务）。
pub const IOC_RD_KTS: u32 = 0x735_006;

/// （已撤除）原 NOTIFY/AWAIT 的 arg 标志位（=1，调用方已完成 U 态按行
/// 缓存维护、内核跳过整窗同步点）——随 PMA 非缓存窗口一并删除，两个
/// ioctl 的 arg 现被忽略（传 0）。

/// qemu-plic / qemu-aia 共享窗大小（reg 真源：its/rt-async-shm.dtsi）
pub const QEMU_SHM_SIZE: usize = 0x19000;
/// K3 RCPU SRAM 共享窗大小（reg 真源：its/rt-async-k3.dts + tgoskits AP dts）
pub const K3_SHM_SIZE: usize = 0x19000;
