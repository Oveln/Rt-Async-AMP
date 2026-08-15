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

/// qemu-plic / qemu-aia 共享窗大小（reg 真源：its/rt-async-shm.dtsi）
pub const QEMU_SHM_SIZE: usize = 0x19000;
/// K3 RCPU SRAM 共享窗大小（reg 真源：its/rt-async-k3.dts + tgoskits AP dts）
pub const K3_SHM_SIZE: usize = 0x19000;
