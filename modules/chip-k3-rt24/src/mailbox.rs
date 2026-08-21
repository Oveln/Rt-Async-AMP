//! K3 RT24 Mailbox 驱动（纯信令门铃模式 + async await）。
//!
//! K3 SoC 的 Mailbox 硬件提供 4 通道 × 深度 8 的 FIFO + 中断能力，
//! 用于核间信令。本驱动仅使用门铃语义：写 FIFO 触发对端中断，不承载
//! 业务数据（数据走共享内存 ov-rpc）。
//!
//! ## 多实例设计
//!
//! 每个硬件 mailbox 单元是一个 [`MboxK3`] 实例，状态（基址/IRQ/用户
//! 映射/通知锁存器）全部存在结构体自身。probe 从 DT 读 reg/irq 填入
//! 对应实例。ISR 经 IRQ→实例查找表（[`INSTANCES`]）取回实例，无硬编码
//! 分发。加新 mailbox 只需 `pub static MBXn: MboxK3 = MboxK3::new();` +
//! 加入 `K3_DRIVERS`，无需改动方法体。
//!
//! ## async await
//!
//! 每个实例内嵌一个 [`IrqLatch`]。接收方 `mbx.recv().await` 即可异步
//! 等待中断；发送方 `mbx.signal(ch)` 触发对端中断。IrqLatch 复用
//! platform 层的通用"关中断→注册 waker→重检→开中断"竞态修复模式。
//!
//! ## 寄存器层
//!
//! 用 tock-registers 的 `register_structs!` + `register_bitfields!` 定义
//! 完整寄存器映射（含嵌套中断寄存器组数组 + 自动填充），所有 MMIO 访问
//! 经 tock-registers 的 volatile 方法（`.get()`/`.set()`/`.read()`/
//! `.is_set()`），不手写 `read_volatile`/`write_volatile`。与 tgoskits
//! 子模块的 PLIC / GIC 等驱动一致。
//!
//! ## PLIC 时序
//!
//! probe 只做纯硬件初始化（清 FIFO、设阈值、存基址）。中断注册 + PLIC
//! 使能推迟到 [`setup_interrupts`]，由 `Board::late_init()` 调用——
//! 此时 PLIC 已 probe（DFS 先序保证）。

use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use fdt_parser::Node;
use platform::device::{Driver, Mailbox};
use platform::irq::IrqLatch;
use platform::Slot;
use tock_registers::interfaces::{Readable, Writeable};
use tock_registers::registers::{ReadOnly, ReadWrite, WriteOnly};
use tock_registers::{register_bitfields, register_structs};

// ── 硬件常量 ──────────────────────────────────────────────────────

/// 每个 mailbox 实例的通道数。
const NUM_CHANNELS: usize = 4;

// ── 位掩码 ────────────────────────────────────────────────────────

/// NEW_MSG 中断位掩码（通道 m 对应 bit 2m）。
const fn new_msg_mask(ch: usize) -> u32 {
    1u32 << (ch * 2)
}

// ── 寄存器定义（tock-registers）──────────────────────────────────

register_bitfields![u32,
    FifoStatus [
        IS_FULL  OFFSET(0) NUMBITS(1) [],
        IS_EMPTY OFFSET(1) NUMBITS(1) [],
    ],
    MsgStatus [
        /// 当前 FIFO 中的消息数（位 [3:0]）。
        MSG_COUNT OFFSET(0) NUMBITS(4) [],
    ],
];

register_structs! {
    /// Mailbox 中断寄存器组（每 user 一组，偏移 0x100 + 0x10×u）。
    pub MboxIrqRegs {
        (0x00 => irq_status:     ReadWrite<u32>), // IRQSTATUS_RAW — 读为 pending，写 1 置位（调试）
        (0x04 => irq_status_clr: ReadWrite<u32>), // IRQSTATUS_CLR — 写 1 清除对应 pending 位；可读（esos 用读-改-写）
        (0x08 => irq_en_set:     ReadWrite<u32>), // IRQENABLE_SET — 写 1 使能，读回确认
        (0x0c => irq_en_clr:     WriteOnly<u32>), // IRQENABLE_CLR — 写 1 禁能
        (0x10 => @END),
    }
}

register_structs! {
    /// K3 Mailbox 完整寄存器映射（每实例 0x400 字节）。
    pub MboxRegs {
        (0x000 => mbox_version:  ReadOnly<u32>),
        (0x004 => _reserved0),
        (0x010 => mbox_sysconfig: ReadWrite<u32>),
        (0x014 => _reserved1),
        (0x040 => mbox_msg:      [ReadWrite<u32>; 4]),
        (0x050 => _reserved2),
        (0x080 => fifo_status:   [ReadOnly<u32, FifoStatus::Register>; 4]),
        (0x090 => _reserved3),
        (0x0c0 => msg_status:    [ReadOnly<u32, MsgStatus::Register>; 4]),
        (0x0d0 => _reserved4),
        (0x100 => mbox_irq:      [MboxIrqRegs; 2]),
        (0x120 => _reserved5),
        (0x180 => mbox_thresh:   [ReadWrite<u32>; 8]), // 2 users × 4 regs
        (0x1a0 => _reserved6),
        (0x400 => @END),
    }
}

// ── IRQ→实例查找表 ────────────────────────────────────────────────

/// IRQ 号 → mailbox 实例的查找表。
///
/// ISR 签名 `unsafe fn(u32)` 无上下文参数，故 ISR 经 IRQ 号在此表反查
/// 实例指针。probe 时以实例的 IRQ 号索引写入。容量与 platform 的
/// MAX_IRQ 对齐。
static INSTANCES: [AtomicUsize; platform::irq::MAX_IRQ] =
    [const { AtomicUsize::new(0) }; platform::irq::MAX_IRQ];

/// 注册实例到 IRQ 查找表。
fn register_instance(irq: u32, mbox: &'static MboxK3) {
    INSTANCES[irq as usize].store(mbox as *const _ as usize, Ordering::Release);
}

/// ISR 经 IRQ 号取回实例指针。
fn instance_for_irq(irq: u32) -> Option<&'static MboxK3> {
    // Relaxed 足够：注册发生在 setup_interrupts（开中断前、同 hart 串行），
    // ISR 读取时写入早已按程序序完成；本表仅本核读写，无跨核同步需求。
    // （Acquire 降级是热路径优化：K3 实测原子读经 Atomics Wrapper 序列化
    // ~2.2µs/笔，是 ddrain/唤醒链的固定成本。）
    let ptr = INSTANCES[irq as usize].load(Ordering::Relaxed);
    if ptr == 0 {
        None
    } else {
        // SAFETY: ptr 来自 register_instance 存入的 &'static MboxK3。
        Some(unsafe { &*(ptr as *const MboxK3) })
    }
}

// ── Mailbox 实例 ──────────────────────────────────────────────────

/// K3 Mailbox 实例。状态全部存在结构体自身。
///
/// 每个硬件 mailbox 单元对应一个 `static MBXn: MboxK3`。实例之间完全
/// 对称——无硬编码基址/IRQ/方向分发。
///
/// 字段访问序约定：`base`/`irq`/`user_local`/`user_remote` 均由 probe
/// （boot DFS、开中断前、同 hart 串行）写入一次后只读，运行期读取统一用
/// `Relaxed`——同 hart 程序序已保证可见，Acquire 在 K3 上是 ~2.2µs/笔的
/// Atomics Wrapper 序列化开销（热路径实测归因见 2026-08-17 延迟战役）。
pub struct MboxK3 {
    base: AtomicUsize,
    irq: AtomicU32,
    user_local: AtomicU8,
    user_remote: AtomicU8,
    latch: IrqLatch,
}

impl MboxK3 {
    /// 创建未初始化的实例（base=0 表示尚未 probe）。
    pub const fn new() -> Self {
        Self {
            base: AtomicUsize::new(0),
            irq: AtomicU32::new(0),
            user_local: AtomicU8::new(0),
            user_remote: AtomicU8::new(1),
            latch: IrqLatch::new(),
        }
    }

    /// 返回寄存器引用。probe 前调用为 panic（调用方应确保已 probe）。
    fn regs(&self) -> &MboxRegs {
        // Relaxed 足够：base 仅由本 hart 在 probe（boot DFS、开中断前）写入
        // 一次，此后只读；同 hart 程序序保证可见，无跨核同步需求。热路径
        // 必须避免 Acquire——2026-08-17 板上实测原子读经 Atomics Wrapper
        // 序列化 ~2.2µs/笔（与地址无关），ISR 排空舞步与 signal 门铃路径
        // 的历史成本主要来自此处及 user_local/user_remote 的 Acquire 读。
        let addr = self.base.load(Ordering::Relaxed);
        assert!(addr != 0, "mailbox: not probed");
        // SAFETY: addr 来自 probe 写入的 DT reg，指向已验证的 MMIO 区域。
        // 单 hart 串行访问，无别名引用（tock-registers 内部用 volatile）。
        unsafe { &*(addr as *const MboxRegs) }
    }

    /// 排空指定通道 FIFO（probe 初始化用，避免遗留数据触发虚假中断）。
    fn drain_channel(regs: &MboxRegs, ch: usize) {
        // 先读 mbox_msg 再判 is_empty——对齐 esos spacemit_chan_startup 的
        // while(1){readl;check;break} 模式，保证至少一次硬件 acknowledge。
        loop {
            let _ = regs.mbox_msg[ch].get();
            if regs.fifo_status[ch].is_set(FifoStatus::IS_EMPTY) {
                break;
            }
        }
    }

    // ── 状态/调试方法 ──

    /// 读 FIFOSTATUS 的 is_empty 位。
    pub fn fifo_is_empty(&self, channel: u8) -> bool {
        self.regs().fifo_status[channel as usize].is_set(FifoStatus::IS_EMPTY)
    }

    /// 读 FIFOSTATUS 的 is_full 位。
    pub fn fifo_is_full(&self, channel: u8) -> bool {
        self.regs().fifo_status[channel as usize].is_set(FifoStatus::IS_FULL)
    }

    /// 读 MSGSTATUS 的 num_msg 位（位[3:0]）。
    pub fn msg_count(&self, channel: u8) -> u32 {
        self.regs().msg_status[channel as usize].read(MsgStatus::MSG_COUNT)
    }

    /// 读 IRQENABLE_SET 中指定通道的 NEW_MSG 位是否置位。
    pub fn irq_enabled(&self, channel: u8) -> bool {
        let user_local = self.user_local.load(Ordering::Relaxed) as usize;
        let en = self.regs().mbox_irq[user_local].irq_en_set.get();
        en & new_msg_mask(channel as usize) != 0
    }

    /// 读 IRQSTATUS_RAW 中指定通道的 NEW_MSG pending 位。
    pub fn irq_pending_raw(&self, channel: u8) -> bool {
        let user_local = self.user_local.load(Ordering::Relaxed) as usize;
        let raw = self.regs().mbox_irq[user_local].irq_status.get();
        raw & new_msg_mask(channel as usize) != 0
    }

    /// 写 IRQSTATUS_CLR 清除指定通道的 NEW_MSG pending 位。
    pub fn clear_irq_pending(&self, channel: u8) {
        let user_local = self.user_local.load(Ordering::Relaxed) as usize;
        self.regs().mbox_irq[user_local].irq_status_clr.set(new_msg_mask(channel as usize));
    }

    /// 手动写 IRQSTATUS_RAW（W1S）置位本地 NEW_MSG pending，触发硬件中断。
    ///
    /// 已废弃——勿用！NEW_MSG pending 是"FIFO 非空"电平信号，esos 清除它的
    /// 唯一途径是读 FIFO 直到空（num_msg==0）。W1S 写 RAW 是手册 §16.6.4.6
    /// 的 debug 注入：置位 pending 但 FIFO 为空，ISR 读不出东西、写 CLR 也
    /// 清不掉该幽灵 pending → IRQ 线恒 assert → PLIC 中断风暴（实测 isr_count
    /// 1→9+ 持续增长）。本地自测请改用写 FIFO（触发对端）配合对端回写，或
    /// 直接等对端真实通知。
    pub fn trigger_local_irq(&self, channel: u8) {
        let user_local = self.user_local.load(Ordering::Relaxed) as usize;
        self.regs().mbox_irq[user_local].irq_status.set(new_msg_mask(channel as usize));
    }

    // ── async 接收 ──

    /// 异步等待一次中断到达。
    ///
    /// 每完成一次表示 ISR 处理了一个中断。可循环 `mbx.recv().await` 收割。
    /// 内部经 [`IrqLatch`] 的关中断→注册 waker→重检→开中断临界区，
    /// 消除注册/触发竞态。
    ///
    /// 仅 riscv64 可用——IrqLatch 依赖 arch 关/开中断原语。
    #[cfg(feature = "riscv64")]
    pub fn recv(&self) -> platform::IrqFuture<'_> {
        platform::IrqFuture::new(&self.latch)
    }
}

// ── 静态实例 ──────────────────────────────────────────────────────

/// Mailbox 实例池。Driver::probe 从中取第一个空闲实例（base==0）绑定。
/// DFS 先序保证 DT 中靠前的节点先被 probe，先绑定到 MBX_POOL[0]。
static MBX_POOL: [MboxK3; 2] = [MboxK3::new(), MboxK3::new()];

/// MBX3：绑定 DT 中第一个 mailbox 节点，即**物理 mailbox4**（0xCAC91000，
/// IRQ 69）。变量名沿用历史 "MBX3"（= DT 第 1 个 mailbox 节点），与物理
/// mailbox 编号无关——DTS 只保留 mailbox4 一个节点（mailbox3 归 esos(rcpu0)
/// 占用，rcpu0 DTB status "okay"），故 MBX_POOL[0] 绑定的是物理 mailbox4。
pub static MBX3: &MboxK3 = &MBX_POOL[0];
/// MBX4：已弃用占位（MBX_POOL[1]）。DTS 仅一个 mailbox 节点（物理 mailbox4，
/// 绑定到 MBX3），本实例无节点可 probe（base=0），保留仅为池容量占位。
/// 注意：esos(rcpu0) 占用的是物理 **mailbox3**（非 mailbox4）；物理 mailbox4
/// 在 esos 侧 DTB status "disabled"、空闲可用——见 its/rt-async-k3.dts 注释。
pub static MBX4: &MboxK3 = &MBX_POOL[1];

/// Mailbox driver 单例——实现 Driver trait，probe 时向实例池分配。
pub static DRIVER: MboxDriver = MboxDriver;

/// Driver wrapper：Driver trait 的载体（MboxK3 本身不 impl Driver，
/// 避免 boot() 对每个节点调 N 次 probe 的竞争）。
pub struct MboxDriver;

// ── Slot（供 app 经 registry 取用）────────────────────────────────

/// MBX3 Slot（AP/starryos → rcpu1 方向；绑定物理 mailbox4，见 [`MBX3`]）。
pub static MBX3_SLOT: Slot<&'static dyn Mailbox> = Slot::new();
/// MBX4 Slot——已弃用（见 [`MBX4`] 说明：无节点可 probe，占位保留）。
pub static MBX4_SLOT: Slot<&'static dyn Mailbox> = Slot::new();

// ── Driver 实现 ────────────────────────────────────────────────────

impl Driver for MboxDriver {
    fn compatible(&self) -> &'static [&'static str] {
        &["spacemit,k3-mailbox"]
    }

    fn probe(&self, node: &Node<'_>) {
        // boot() 对每个 DT 节点遍历所有 compatible 匹配的 driver 并逐一 probe。
        // 本 driver 是单例，每个 mailbox 节点只会调一次 probe。probe 从实例池
        // 取第一个空闲实例（base==0）绑定——DFS 先序保证 DT 靠前的节点绑定
        // MBX_POOL[0]，靠后的绑定 MBX_POOL[1]。
        let mbox = MBX_POOL
            .iter()
            .find(|m| m.base.load(Ordering::Acquire) == 0)
            .expect("mailbox: no free instance in pool");

        let reg = node
            .reg()
            .and_then(|mut r| r.next())
            .expect("mailbox: missing reg");
        let base = reg.address as usize;

        // 从 DT 读 rcpu-communicate 属性，确定 USER_LOCAL/USER_REMOTE。
        // 寄存器组编号以 esos 官方 rcpu1 驱动为准
        // （bsp/spacemit/drivers/mailbox/k3_mailbox.h + platform/rt24/os1_rcpu 的
        // k3.dtsi：mailbox4 节点无 rcpu-communicate 属性 → rcpu_communicate=false
        // → USER0_MBOX_OFFSET=0 / USER1_MBOX_OFFSET=1）：
        //   - 本地（rcpu1）收中断用 mbox_irq[1]（esos ISR 读 USER1_MBOX_OFFSET）
        //   - 发送时 enable 对端（AP）的 mbox_irq[0]（esos send_data 写 USER0）
        // 本驱动保持与官方一致的 (local=1, remote=0)。
        // 注意：rcpu-communicate=true 时 esos 宏会翻转（USER0=1/USER1=0），与
        // AP 侧 rt_shm 的硬编码（starryos=USER0=数组0, rcpu1=USER1=数组1）错位
        // → 必须保证 DTS 节点无 rcpu-communicate 属性（rcpu_comm=false）。
        let rcpu_comm = node.find_property("rcpu-communicate").is_some();
        let (local, remote) = if rcpu_comm { (0, 1) } else { (1, 0) };
        mbox.user_local.store(local, Ordering::Release);
        mbox.user_remote.store(remote, Ordering::Release);

        // 从 DT 读中断号。
        let irq = node
            .find_property("interrupts")
            .expect("mailbox: missing interrupts")
            .u32();

        // 存储实例状态。
        mbox.base.store(base, Ordering::Release);
        mbox.irq.store(irq, Ordering::Release);

        // ── 硬件初始化（此时 PLIC 尚未就绪，仅操作 mailbox 自身寄存器）──

        // SAFETY: base 来自 DT reg，指向已验证的 MMIO 区域。
        let regs = unsafe { &*(base as *const MboxRegs) };

        // 清空全部 4 个通道 FIFO（避免遗留数据触发虚假中断）。
        for ch in 0..NUM_CHANNELS {
            MboxK3::drain_channel(regs, ch);
        }

        // 清除 IRQSTATUS pending（esos `spacemit_chan_shutdown` 同款）。
        // 否则上次运行遗留的 pending 会在 setup_interrupts 使能中断后
        // 立即触发一次"幽灵通知"，recv().await 误判为 AP 消息。
        regs.mbox_irq[local as usize].irq_status_clr.set(0xff);

        // 显式设置中断阈值 = 0（每条消息都触发中断，不依赖复位默认值）。
        // 每 user 占 4 个寄存器（0x180 + user×0x10），取第 0 个。
        regs.mbox_thresh[local as usize * 4].set(0);

        // 注意：ISR 注册 + PLIC 使能不在 probe 中执行——
        // 推迟到 setup_interrupts()（由 Board::late_init() 调用）。

        log::info!(
            "k3 mailbox @ {:#x}: irq={}, rcpu-comm={}, user_local={}",
            base,
            irq,
            rcpu_comm,
            local
        );
    }
}

// ── Mailbox trait 实现 ─────────────────────────────────────────────

impl Mailbox for MboxK3 {
    fn signal(&self, channel: u8) {
        let ch = channel as usize;
        let user_remote = self.user_remote.load(Ordering::Relaxed) as usize;
        let regs = self.regs();

        // 先使能对端 NEW_MSG 中断（确保对端能收到），再写 FIFO。
        regs.mbox_irq[user_remote].irq_en_set.set(new_msg_mask(ch));
        regs.mbox_msg[ch].set(1);
    }

    fn ack(&self, channel: u8) -> u32 {
        self.regs().mbox_msg[channel as usize].get()
    }

    fn irq(&self) -> u32 {
        self.irq.load(Ordering::Relaxed)
    }

    fn enable_new_msg_irq(&self, channel: u8) {
        let user_local = self.user_local.load(Ordering::Relaxed) as usize;
        self.regs().mbox_irq[user_local]
            .irq_en_set
            .set(new_msg_mask(channel as usize));
    }

    fn disable_new_msg_irq(&self, channel: u8) {
        let user_local = self.user_local.load(Ordering::Relaxed) as usize;
        self.regs().mbox_irq[user_local]
            .irq_en_clr
            .set(new_msg_mask(channel as usize));
    }
}

// ── PeerNotifier 实现（跨核通知后端，注册进 ov-shm 的 NOTIFIER slot）──

impl ov_shm::notifier::PeerNotifier for MboxK3 {
    fn notify(&self) {
        // 契约要求：notify 返回前须保证共享内存数据对 AP 可见。
        core::sync::atomic::fence(Ordering::Release);
        // 写 mailbox4（MBX3 绑定的物理实例，0xCAC91000）FIFO 通道 1 →
        // 触发 AP/StarryOS 的 NEW_MSG 中断。通道约定与 AP 侧 notifier 节点
        // 对称（tx-channel=0 / rx-channel=1）：
        //   AP → RP 走通道 0（本驱动 enable_new_msg_irq(0) 接收），
        //   RP → AP 走通道 1（此处 signal(1) 触发 AP 的 rx-channel=1）。
        // 注意：K3 mailbox 写 mbox_msg[ch] 会同时置位**双方 user** 的 ch NEW_MSG。
        // 若发通道与本地使能的接收通道相同会自激——故收发必须分通道。
        self.signal(1);
    }
}

// ── 中断设置（Board::late_init 调用）──────────────────────────────

/// 注册 mailbox ISR 并在 PLIC 使能中断。
///
/// 必须在 PLIC probe 之后调用（INTC slot 已就绪）。
/// 由 `Board::late_init()` 调用。
///
/// 对每个已 probe 的实例：注册通用 ISR + 使能 PLIC + 使能通道 0 的
/// NEW_MSG 中断。ISR 经 IRQ 查找表取回实例，清 FIFO + 清 pending +
/// 通知 latch。
pub fn setup_interrupts() {
    let intctl = platform::driver::intctl();
    for mbox in MBX_POOL.iter() {
        if mbox.base.load(Ordering::Acquire) == 0 {
            continue; // 未 probe
        }
        let irq = mbox.irq();
        register_instance(irq, mbox);
        platform::irq::register_irq(irq, mbox_isr);
        // PLIC 要求 priority > threshold 才能转发中断。probe 已设 threshold=0，
        // 故 priority 至少为 1。不设 priority 时默认为 0 → 中断被 PLIC 丢弃。
        intctl.set_priority(irq, 1);
        intctl.enable_irq(irq);
        mbox.enable_new_msg_irq(0);
        // 清掉 PLIC 层残留 pending：上一轮运行未 complete 的中断会在本函数
        // 返回后（开中断）立即触发 ISR，把 latch 置位——task 启动后第一次
        // recv() 会误消费一条"幽灵通知"（此时共享内存还是残留数据）。
        // 此处开中断前 claim+complete 一次，彻底清空。
        let claimed = intctl.claim();
        if claimed != 0 {
            intctl.complete(claimed);
        }
        log::info!("mailbox @ irq {}: interrupts enabled", irq);
    }

    // 注册跨核通知后端：MBX3（= 物理 mailbox4，0xCAC91000/IRQ 69）是
    // AP(rt_shm notifier) ↔ rcpu1 方向的信令通道。mailbox3 归 esos(rcpu0)
    // 的 rproc 专用（rcpu0 DTB status "okay"），本 DTS 未保留其节点；
    // 物理 mailbox4 在 esos 侧 disabled、空闲可用。仅当已 probe
    // （base 非零）才注册。MBX3 本身是 `&'static MboxK3`（MBX_POOL 的引用）。
    if MBX3.base.load(Ordering::Acquire) != 0 {
        ov_shm::notifier::NOTIFIER.set(MBX3);
        log::info!("ov-shm notifier: MBX3 registered as PeerNotifier");
    }
}

// ── ISR ────────────────────────────────────────────────────────────

/// 最近一次 mailbox ISR 入口的 AON_TIMER1 时间戳（tick，2MHz，0 = 尚无中断）。
///
/// 延迟插桩用：intercom 读取该值拆分 D1（中断唤醒）路径的
/// `t_sched − t_isr`（IRQ 进入 → IPC 任务恢复执行）分段。仅在 ISR 入口
/// 单点写入，无锁；多次中断只保留最后一次（单请求在途的测量场景下精确）。
pub static LAST_IRQ_TS: AtomicU64 = AtomicU64::new(0);

/// 最近一次 mailbox ISR 排空舞步完成点的 AON_TIMER1 时间戳（tick，0 = 尚无中断）。
///
/// 延迟插桩用：把 D1 的 `t_sched − t_isr`（板上实测 69.8µs，2026-08-17）
/// 拆成「ISR 内 mailbox MMIO 排空舞步」（t_drain − t_isr）与「trap 返回 +
/// 执行器派发 + 任务恢复」（t_sched − t_drain）两段，定位大头去向。
/// 与 [`LAST_IRQ_TS`] 同款：仅在 ISR 内单点写入，无锁；单请求在途的
/// 测量场景下精确。stamp 点在 latch.notify() 之前（舞步刚结束）。
pub static LAST_ISR_DONE_TS: AtomicU64 = AtomicU64::new(0);

/// 通用 Mailbox ISR。
///
/// 经 IRQ 号查 [`INSTANCES`] 表取回实例。外层 while 循环处理 ISR 执行
/// 期间可能到达的新消息（竞态修复）。每次迭代重新读 pending，直到无
/// 待处理 NEW_MSG，最后通知 latch 唤醒 await 方。
///
/// # Safety
/// 中断上下文调用，关中断执行，不可阻塞。
unsafe fn mbox_isr(irq: u32) {
    // 入口时间戳（延迟插桩 D1 分段基准）。timer_k3::now() 是固定地址
    // MMIO 读（AON_TIMER1 @2MHz，本地域热 4ns/笔），无 probe/slot 依赖，
    // ISR 上下文安全——与 T_SCHED/stamp 链同钟（dd 分段恒等式前提）。
    LAST_IRQ_TS.store(crate::timer_k3::now(), Ordering::Relaxed);
    let mbox = match instance_for_irq(irq) {
        Some(m) => m,
        None => return,
    };
    let regs = mbox.regs();
    let user_local = mbox.user_local.load(Ordering::Relaxed) as usize;

    loop {
        let raw = regs.mbox_irq[user_local].irq_status.get();
        let enabled = regs.mbox_irq[user_local].irq_en_set.get();
        let pending = raw & enabled;

        if pending == 0 {
            break;
        }

        for ch in 0..NUM_CHANNELS {
            if pending & new_msg_mask(ch) != 0 {
                // 排空 FIFO——必须先读 mbox_msg 再判 num_msg（对齐 esos 驱动
                // spacemit_mbox_irq 的 while(1){readl; check;break} 模式）。
                // K3 mailbox 的 NEW_MSG 是电平信号：只要曾有待读消息，level
                // 就拉高并 latch。硬件要求至少一次 mbox_msg 读访问作为
                // acknowledge 才能让 irq_status_clr 的写永久生效。
                // 若先判 num_msg==0 再跳过读（旧 bug），当 FIFO 已被对端或
                // 上轮 ISR 排空时，缺少 acknowledge → 写 CLR 只临时清 latch →
                // 硬件因 level 仍高立即重新置位 → 中断风暴（raw=0x1 cnt=0）。
                loop {
                    let _ = regs.mbox_msg[ch].get();
                    if mbox.msg_count(ch as u8) == 0 {
                        break;
                    }
                }
                // 清除中断 pending：RMW（读-改-写）保留其他位，对齐 esos
                // `j = readl(clr); j |= mask; writel(clr, j)`。
                let cur = regs.mbox_irq[user_local].irq_status_clr.get();
                regs.mbox_irq[user_local]
                    .irq_status_clr
                    .set(cur | new_msg_mask(ch));
            }
        }
    }

    // 排空舞步完成点（延迟插桩：t_drain，见 LAST_ISR_DONE_TS 文档）。
    LAST_ISR_DONE_TS.store(crate::timer_k3::now(), Ordering::Relaxed);

    // 唤醒等待中断的 async task（如有）。
    mbox.latch.notify();
    // PLIC complete 由 dispatch_external 框架自动调用
}
