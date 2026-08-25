//! 共享窗布局常量与环索引访问器（AP 用户态）。
//!
//! **历史**：本模块曾承载 U 态 Zicbom 按行缓存维护（feature `user-cbo`：
//! 发送/接收路径的 cbo.flush/cbo.inval 精确到消息槽+索引行，替代内核
//! ioctl 整窗 clean+invalidate）。2026-08-26 PMA 定案后已全部撤除——
//! 共享窗经 OpenSBI 固件（opensbi-k3 `feat/pma-audio-io`）把覆盖窗口的
//! PMA entry 翻为 IO，AP 侧读写物理直达 SRAM，任何缓存维护都是纯开销
//! （实测每条 cbo ~160ns）。同日撤除的还有内核 rt_shm 四个缓存同步点、
//! somehal 的 U 态 cbo 放行（senvcfg）。
//!
//! 现存内容仅两样：
//! * ov-channels 0.2.0 布局常量（`RB_OFF`/`SLOTS_OFF`）+ 编译期对账断言
//!   （版本漂移即编译失败）；
//! * [`ring_indices`]——环索引的 Acquire 读助手（bench 场景读索引用）。
//!   非缓存窗口下读恒 SRAM 真值，无任何伴随维护。

/// cache block 大小（`riscv,cbom-block-size`，K3 X100 = 64；cbo 时代的
/// 遗留常量，保留供布局对账参考）。
pub const CACHE_LINE: usize = 64;

/// `RingBuffer` 在 `Channel` 内的偏移：magic/version 头（4B）后按
/// `align(256)` 对齐到 0x100。read 索引 @rb+0、write @rb+8。
pub const RB_OFF: usize = 0x100;

/// 消息槽在 `Channel` 内的起始偏移 = RingBuffer(0x100) + RingBuffer 内
/// buffer 字段偏移（对齐 256，真相源 `ov_channels::RB_SLOTS_OFF`）。
pub const SLOTS_OFF: usize = RB_OFF + ov_channels::RB_SLOTS_OFF;

// ov-channels 0.2.0 布局编译期对账：Message=256B；RingBuffer =
// {read,write} 头 + buffer（对齐 256，偏移 RB_SLOTS_OFF）+ 128×256，
// repr(align(256)) 尺寸取整到 0x8100；Channel = 4B 头 + 0x100 偏移的
// RingBuffer = 0x8200。crate 升级若改变布局，这里立即编译失败，逼着
// 重新核对本模块的偏移常量。
const _: () = {
    use core::mem::size_of;
    assert!(size_of::<ov_channels::Message>() == 256);
    const RB_SIZE: usize =
        (ov_channels::RB_SLOTS_OFF + ov_channels::CHANNEL_CAPACITY * 256 + 0xFF) & !0xFF;
    assert!(size_of::<ov_channels::Channel>() == RB_OFF + RB_SIZE);
};

/// 读 `Channel` 的环索引 (read, write)。非缓存窗口直读 SRAM 真值。
///
/// `ch` 为 ov-channels `Channel` 基地址（如
/// `shm.channel_unchecked(id) as *const Channel as usize`）。
pub fn ring_indices(ch: usize) -> (usize, usize) {
    unsafe {
        let rb = (ch + RB_OFF) as *const core::sync::atomic::AtomicUsize;
        ((*rb).load(core::sync::atomic::Ordering::Acquire),
         (*rb.add(1)).load(core::sync::atomic::Ordering::Acquire))
    }
}
