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
/// ioctl：仅 flush 共享窗（clean+invalidate，不发门铃）。
///
/// BUSY=1 跳过 NOTIFY 的发送方（ov-rpc 省 IPI 优化）用它发布缓存滞留的
/// 写到 SRAM——用户态映射实际 cacheable（X100 PBMT 不生效），NOTIFY 的
/// flush 是唯一发布点，跳过门铃时必须补这个纯 flush，否则请求对 RP
/// 不可见、read 索引不推进（幻影 pending）。
/// 值须与 tgoskits rt_shm.rs 内核侧保持同值（双仓对齐义务）。
pub const IOC_FLUSH: u32 = 0x735_005;

/// ioctl：读内核延迟插桩时间戳（诊断，K3 专用）。
///
/// arg = 用户态 `*mut u64` 数组（2 项），返回
/// `[上次 NOTIFY 门铃 MMIO 写前一瞬的内核单调 ns,
///   上次 K3 mailbox IRQ handler 入口的内核单调 ns]`。
/// 供 bench `dd` 场景与 RP 侧 mtime 戳（PING 回传）交叉分解门铃投递 /
/// 回程耗时。值须与 tgoskits rt_shm.rs 内核侧保持同值（双仓对齐义务）。
pub const IOC_RD_KTS: u32 = 0x735_006;

/// `IOC_NOTIFY` / `IOC_AWAIT` 的 arg 标志位：调用方已按行完成缓存维护
/// （ov-rpc `user-cbo` feature，cbo.flush/cbo.inval 精确到消息槽+索引行），
/// 内核跳过整窗 clean+invalidate——NOTIFY 只发门铃（前插一道 fence），
/// AWAIT 不做返回前 flush、就绪检查的作废也缩至 ch1 magic+索引两行。
/// 为 0（默认）保持整窗同步点语义，老程序不受影响。
pub const ARG_USER_CBO: usize = 1;

/// qemu-plic / qemu-aia 共享窗大小（reg 真源：its/rt-async-shm.dtsi）
pub const QEMU_SHM_SIZE: usize = 0x19000;
/// K3 RCPU SRAM 共享窗大小（reg 真源：its/rt-async-k3.dts + tgoskits AP dts）
pub const K3_SHM_SIZE: usize = 0x19000;
