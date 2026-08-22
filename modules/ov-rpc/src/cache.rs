//! 共享窗 U 态按行缓存维护（feature `user-cbo`，riscv64/Zicbom）。
//!
//! X100 上 PMA 判定使共享 SRAM 映射实际 cacheable（PTE PBMT 不生效），
//! AP 用户态对窗口的写会滞留缓存、对 RP 不可见；此前的发布点是内核 ioctl
//! 的整窗 clean+invalidate（0x19000 = 1600 行/次），而每轮请求-响应真正
//! 触碰的内存只有：一条消息槽（4 行）+ 环索引行（1 行，read/write 同在
//! 64B 行内）。本模块把同步点缩成按行 cbo 操作，由发送/接收路径精确维护：
//!
//! ```text
//! 发送（ch0）：refresh 索引行 → try_send → publish(槽+索引) →
//!             refresh BUSY 行 → 读 BUSY 决定门铃（D2 路径零 syscall）
//! 接收（ch1）：AWAIT → refresh(索引+待读槽) → try_recv → publish 索引行
//! ```
//!
//! 前置条件：内核已置 `senvcfg`（CBCFE=1、CBIE=01，见 tgoskits somehal
//! `enable_user_cbo`）。CBIE=01 下 U 态 `cbo.inval` 按 flush 语义执行
//! （写回+作废），对含己方脏写的行也安全。未使能或无 Zicbom 的平台执行
//! cbo 触发 SIGILL——本 feature 只对 K3（riscv64 + 内核 zicbom）启用。
//!
//! 与 ov-channels 0.2.0 布局的耦合以编译期断言对账（见 [`RB_OFF`]），
//! 版本漂移即编译失败。已知的残余竞态（read/write 同行、AP 端 flush 写回
//! 陈旧 read 索引可回卷 RP 消费进度）仅并发流水线场景可达，单请求在途
//! 协议不可达。**A4（2026-08-22 二代定案）**：X100 cbo.flush 对在途
//! store 存在静默丢失，且**同核视角不可检测**——回读由 L1/L2 服务
//! 恒见新值，一代"发布后回读校验"缓解已被板上证伪（fresh_scan
//! D=100µs 走 flush+clean-inval 组合仍丢，itb f6fc7682 轮）。本模块
//! 回归单遍发布；丢失的检测与恢复移交时间视角——在途超时后幂等
//! 重发布（安全性论证见 [`publish_send`]，载体为 bench 看门狗；W2
//! 轮询产品化后内建于轮询回路的软期限）。

#[cfg(all(feature = "user-cbo", not(target_arch = "riscv64")))]
compile_error!("ov-rpc feature \"user-cbo\" 仅支持 riscv64 目标（Zicbom cbo 指令）");

/// cache block 大小（`riscv,cbom-block-size`，K3 X100 = 64；内核 someboot
/// `ZICBOM_BLOCK_SIZE` 同值）。
pub const CACHE_LINE: usize = 64;

/// `RingBuffer` 在 `Channel` 内的偏移：magic/version 头（4B）后按
/// `align(256)` 对齐到 0x100。read 索引 @rb+0、write @rb+8——**同一
/// cache line**，publish/refresh 一并覆盖。
pub const RB_OFF: usize = 0x100;

/// 消息槽在 `Channel` 内的起始偏移 = RingBuffer(0x100) + RingBuffer 内
/// buffer 字段偏移。
///
/// **2026-08-20 布局 bug 修正**：`Message` 为 `align(256)`，RingBuffer 的
/// buffer 字段对齐垫到 +0x100（真相源 `ov_channels::RB_SLOTS_OFF`）——
/// 本常量此前按 buffer@+0x10 误算为 0x110，导致 [`refresh_slot`] 刷新
/// 区错位 0xF0（槽区前 240B 未刷、多刷下一槽首行）。期间 AP 侧能正常
/// 读到响应，是因为内核 AWAIT 就绪检查前的 invalidate（含 ch1 全槽行）
/// 一直在兜底——"按行精确刷新"的优化贡献实际虚标。
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

#[cfg(all(target_arch = "riscv64", feature = "user-cbo"))]
#[inline]
fn line_range(base: usize, len: usize) -> core::ops::Range<usize> {
    (base & !(CACHE_LINE - 1))..base.saturating_add(len)
}

/// 把 `[base, base+len)` 的脏行清到 SRAM 并作废驻留副本（for-device
/// 发布点）。fence → cbo.flush 逐行 → fence，与内核 `DCacheOp::CleanInvalidate`
/// 同序：先序 fence 保证普通写先于 CBO 生效，后序 fence 保证门铃/索引读
/// 后于 CBO。
#[cfg(all(target_arch = "riscv64", feature = "user-cbo"))]
pub fn publish(base: usize, len: usize) {
    let range = line_range(base, len);
    unsafe {
        core::arch::asm!("fence rw, rw", options(nostack, preserves_flags));
        let mut a = range.start;
        while a < range.end {
            // cbo.flush（.insn i 15, 2, x0, rs1, 2）
            core::arch::asm!(".insn i 15, 2, x0, {addr}, 2", addr = in(reg) a, options(nostack));
            a += CACHE_LINE;
        }
        core::arch::asm!("fence rw, rw", options(nostack, preserves_flags));
    }
}

/// 作废 `[base, base+len)` 的驻留行（for-cpu 刷新点，读到 SRAM 真值）。
/// CBIE=01 下按 flush 语义执行，含脏写的行不丢数据。
#[cfg(all(target_arch = "riscv64", feature = "user-cbo"))]
pub fn refresh(base: usize, len: usize) {
    let range = line_range(base, len);
    unsafe {
        let mut a = range.start;
        while a < range.end {
            // cbo.inval（.insn i 15, 2, x0, rs1, 0）——senvcfg.CBIE=01 时
            // 硬件按 clean+invalidate 执行
            core::arch::asm!(".insn i 15, 2, x0, {addr}, 0", addr = in(reg) a, options(nostack));
            a += CACHE_LINE;
        }
        core::arch::asm!("fence rw, rw", options(nostack, preserves_flags));
    }
}

// ── 通道布局感知的协议助手（发送/接收路径的精确维护集）─────────────────
//
// 以下 `ch` 均为 ov-channels `Channel` 基地址（如
// `shm.channel_unchecked(id) as *const Channel as usize`）。

/// 读 `Channel` 的环索引（read, write）——经 [`refresh`] 过的行读到 SRAM 真值。
#[cfg(all(target_arch = "riscv64", feature = "user-cbo"))]
pub fn ring_indices(ch: usize) -> (usize, usize) {
    unsafe {
        let rb = (ch + RB_OFF) as *const core::sync::atomic::AtomicUsize;
        ((*rb).load(core::sync::atomic::Ordering::Acquire),
         (*rb.add(1)).load(core::sync::atomic::Ordering::Acquire))
    }
}

/// 发送前刷新：作废索引行。RP 推进的 read 索引决定满判定，陈旧副本会让
/// 容量计算偏小 → 假 `Full` 重试活锁。
#[cfg(all(target_arch = "riscv64", feature = "user-cbo"))]
pub fn refresh_before_send(ch: usize) {
    refresh(ch + RB_OFF, 2 * core::mem::size_of::<usize>());
}

/// 发送后发布：把刚写的消息槽（4 行）+ 索引行（1 行）清到 SRAM。
/// `slot` 为发送时的 write 索引（0..CHANNEL_CAPACITY）。
///
/// 顺序 = **槽数据先于索引**（SPSC 发布序：RP 见 write 推进前槽内容
/// 必须已落 SRAM；此前索引在前的写法靠时序侥幸）。
///
/// **A4 重发布幂等性论证**（在途超时后重发本函数的安全性依据，
/// 单请求在途协议）：重发时索引行只有两种状态——干净（上次发布
/// 已落地 ⇒ 重发 flush 无脏可清，为 no-op）或脏（上次发布丢失 ⇒
/// write 推进从未达 SRAM ⇒ RP 不可能已消费 ⇒ 行内 read 字段仍与
/// RP 一致）。后者重发不存在"把陈旧 read 写回、回卷 RP 消费进度"
/// 的窗口（该残余竞态仅并发流水线可达）；槽数据行同理。随重发
/// 多发的门铃只造成 RP 一次空轮询，无重复执行（环形队 try_recv
/// 单消费者 exactly-once）。
#[cfg(all(target_arch = "riscv64", feature = "user-cbo"))]
pub fn publish_send(ch: usize, slot: usize) {
    publish(slot_addr(ch, slot), core::mem::size_of::<ov_channels::Message>());
    publish(ch + RB_OFF, 2 * core::mem::size_of::<usize>());
}

/// 读 BUSY 前刷新窗口头行：陈旧 BUSY=1（RP 已 clear 并入睡）会误跳过
/// 门铃 → 丢失唤醒。`shm` 为 `SharedMemory` 基地址。
#[cfg(all(target_arch = "riscv64", feature = "user-cbo"))]
pub fn refresh_busy(shm: usize) {
    refresh(shm, CACHE_LINE);
}

/// 接收前刷新：作废索引行并返回当前 (read, write)。调用方据此对
/// `[read, write)` 内的槽位逐个 [`refresh_slot`] 后才能 try_recv
/// （try_recv 的内部读不会自查新鲜度）。
#[cfg(all(target_arch = "riscv64", feature = "user-cbo"))]
pub fn refresh_before_recv(ch: usize) -> (usize, usize) {
    refresh(ch + RB_OFF, 2 * core::mem::size_of::<usize>());
    ring_indices(ch)
}

/// 刷新单个消息槽（对端写的响应数据，驻留副本可能是旧轮内容）。
#[cfg(all(target_arch = "riscv64", feature = "user-cbo"))]
pub fn refresh_slot(ch: usize, slot: usize) {
    refresh(slot_addr(ch, slot), core::mem::size_of::<ov_channels::Message>());
}

/// 接收后发布：read 索引推进后调用，消费进度对 RP 可见（RP 回包的满判定
/// 读它；滞留缓存会导致 RP 侧幻影 pending / 假 Full）。此处的 A4 丢失
/// 后果与发送侧不同：仅回收进度延迟（128 槽单请求在途协议下假 Full
/// 不可达），下一次成功发布自愈，无挂死类风险，不做恢复。
#[cfg(all(target_arch = "riscv64", feature = "user-cbo"))]
pub fn publish_recv(ch: usize) {
    publish(ch + RB_OFF, 2 * core::mem::size_of::<usize>());
}

#[cfg(all(target_arch = "riscv64", feature = "user-cbo"))]
#[inline]
fn slot_addr(ch: usize, slot: usize) -> usize {
    ch + SLOTS_OFF + slot * core::mem::size_of::<ov_channels::Message>()
}
