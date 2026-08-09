//! 双核 AMP 通信模块
//!
//! 基于 ov-rpc 框架，在 ov_channels 共享内存通道之上提供类型安全的 RPC。
//!
//! 共享内存布局:
//! - Channel 0: StarryOS -> rt-async (请求/通知)
//! - Channel 1: rt-async -> StarryOS (响应/通知)
//!
//! 弹性忙等: 处理完消息后，服务端会在一段弹性时间内忙等，
//! 期间设置 BUSY 标志，客户端据此跳过不必要的 IPI。
//!
//! 共享内存基址与跨核通知设备均来自设备树（`ov-shm` crate probe），
//! 不再从 amp.toml 编译期常量获取。
//!
//! K3 平台：共享内存 0xc0800000（`ov,rt-async-amp` 节点，68KB），
//! 跨核通知经 mailbox4 硬件中断（IRQ 69）替代 QEMU 的 MachineSoft IPI。

use core::sync::atomic::{AtomicUsize, Ordering};

use ov_channels::{ChannelId, Message, MsgType, SharedMemory};
use ov_rpc::{define_service, RpcServer};

// ============================================================================
// RPC 服务定义
// ============================================================================

define_service! {
    /// rt-async 侧的 RPC 服务
    RtAsyncRpc {
        ECHO:  0 => call echo(val: u32) -> u32;
        ADD:   1 => call add(a: i32, b: i32) -> i32;
        DELAY: 2 => send delay(us: u32);
    }
}

impl RtAsyncRpc {
    fn echo(val: u32) -> u32 {
        val
    }
    fn add(a: i32, b: i32) -> i32 {
        a.wrapping_add(b)
    }
    /// 精确延时（busy-wait）：在 process_all 中顺序执行，
    /// 保证前后 RPC 指令之间的时序精度。
    fn delay(us: u32) {
        let freq = platform::timer().freq_hz() as u64;
        let target = platform::timer().now() + (us as u64) * freq / 1_000_000;
        while platform::timer().now() < target {
            core::hint::spin_loop();
        }
    }
}

// ============================================================================
// RPC Server 实例
// ============================================================================

/// `ov_shm::shm::base()` 在 `init()` 时落地。`RpcServer::new` 是 `const fn`
/// 且仅存基址，故运行期每次调用构造即可（无共享内存访问）。
/// `init()` 必须先于其他 `intercom` 函数调用，否则读到未初始化的共享内存。
static SHM_BASE: AtomicUsize = AtomicUsize::new(0);

/// 弹性忙等自旋上限。
///
/// 每次无消息后自旋此次数；若期间收到新消息则重新处理。
/// 在 K3 RT24 (614.4 MHz) 下，100000 次自旋约 0.2–0.5 ms，
/// 足以覆盖连续 RPC 调用的间隔（K3 主频为 QEMU virt ~10 MHz 的约 60 倍，
/// 若联调发现频繁 sleep/wake 抖动，可适当调大该值）。
const ELASTIC_SPIN_LIMIT: u32 = 100000;

// ============================================================================
// 公共 API
// ============================================================================

/// 初始化共享内存（由 rt-async 启动时调用一次）
pub fn init() {
    let base = ov_shm::shm::base();
    unsafe {
        let shm = SharedMemory::<3>::at(base);
        shm.init();
    }
    // 先 init 再发布基址：`MachineSoft` ISR 可能在本函数执行期间触发
    // （`ipc_wait::notify_from_isr` → `has_pending`），若基址先行发布
    // 而共享内存尚未初始化，ISR 会读到未初始化的 ring buffer 索引。
    SHM_BASE.store(base, Ordering::Release);
    log::info!("[InterCom] initialized at {:#x}", base);
}

/// 检查是否有待处理消息
///
/// # Preconditions
///
/// `init()` must have been called before this function, otherwise this will
/// read from uninitialized shared memory.
///
/// 注意：`MachineSoft` ISR（`ipc_wait::notify_from_isr`）会早于 `task_ipc`
/// 首次 poll 调用本函数（spawn 时 `pend()` 已写 MSIP，`intercom::init()` 尚未
/// 执行）。此时 `SHM_BASE` 仍为 0，直接构造 `RpcServer::new(0)` 会解引用
/// 地址 0（LoadFault）。故基址为 0 时安全返回 false（无待处理消息）。
pub fn has_pending() -> bool {
    let base = SHM_BASE.load(Ordering::Acquire);
    if base == 0 {
        return false;
    }
    RpcServer::new(base).has_pending()
}

/// 向对端核心发送通知 IPI（经 ov-shm 的 notifier 设备，DT 配置后端）。
///
/// 对端 IPI handler 唤醒任何阻塞在 `AWAIT` 的任务，该任务随后直接检查
/// CH1 的 ring buffer 判断消息是否可用——ring buffer 是唯一真相源，
/// 无需中间计数器。
#[inline]
fn send_notify_ipi() {
    ov_shm::notifier::notifier().notify();
}

/// 弹性忙等处理：处理所有消息并在弹性窗口内自旋等待更多请求。
///
/// 工作流程:
/// 1. 设置 BUSY 标志
/// 2. 循环处理所有待处理消息，每个 Notify 响应立即发 IPI
/// 3. 无消息时弹性自旋等待 `ELASTIC_SPIN_LIMIT` 次
/// 4. 自旋期间若收到新消息，重新处理
/// 5. 弹性窗口过期后，清除 BUSY 并做最终竞争检查
///
/// # IPI 策略
///
/// 每个 Notify 响应写入 CH1 后立即调用 `send_notify_ipi()`：直接写
/// MSIP0 触发 Linux 侧中断。Linux IPI handler 仅唤醒阻塞任务，由
/// `await_ipi` 直接读取 CH1 ring buffer 判断是否有消息——无需中间计数器，
/// 彻底消除了计数与实际消息数不匹配的死锁风险。
///
/// 返回已处理的消息数量。
///
/// # Preconditions
///
/// `init()` must have been called before this function, otherwise this will
/// access uninitialized shared memory.
pub fn process_elastic() -> usize {
    let shm = unsafe { SharedMemory::<3>::at(ov_shm::shm::base()) };

    // 1. 标记忙等
    shm.set_busy();

    let mut total_count = 0;

    loop {
        // 2. 处理所有待处理消息，每个 Notify 立即回 IPI
        let n = server().process_all::<RtAsyncRpc, _, _>(
            |msg| handle_non_rpc(msg),
            || send_notify_ipi(),
        );
        total_count += n;

        if n > 0 {
            // 有工作完成，立即检查更多（不经自旋）
            continue;
        }

        // 3. 无消息，弹性自旋
        let mut spun = 0u32;
        while spun < ELASTIC_SPIN_LIMIT {
            if server().has_pending() || server().has_urgent() {
                break;
            }
            spun += 1;
            core::hint::spin_loop();
        }

        if spun < ELASTIC_SPIN_LIMIT {
            // 自旋期间收到新消息，重新处理
            continue;
        }

        // 4. 弹性窗口过期，准备睡眠
        break;
    }

    // 5. 清除 BUSY 标志（Release 语义）
    shm.clear_busy();

    // 6. 全内存屏障 + 最终竞争检查
    //    防止客户端写请求与清除 BUSY 之间的竞争：
    //    如果客户端在 clear_busy() 之后才读到 BUSY=0，则客户端会发 IPI；
    //    如果客户端在 clear_busy() 之前读了 BUSY=1（跳过 IPI），
    //    则此处的 fence 保证我们能看到客户端的请求。
    core::sync::atomic::fence(Ordering::SeqCst);
    
    if server().has_pending() || server().has_urgent() {
        // 竞争窗口内收到请求，重新处理。
        // 不再设置 BUSY：服务端即将睡眠，客户端看到 BUSY=0 后会发 IPI 唤醒。
        let n = server().process_all::<RtAsyncRpc, _, _>(
            |msg| handle_non_rpc(msg),
            || send_notify_ipi(),
        );
        total_count += n;
    }
    log::info!("[InterCom] elastic processing complete, total {} messages handled", total_count);
    total_count
}

fn handle_non_rpc(msg: Message) {
    match msg.ty() {
        Some(MsgType::Notification) => {
            if let Some(id) = msg.as_notification() {
                log::info!("[InterCom] notification: {}", id);
                send_notification(id);
            }
        }
        Some(MsgType::Data) => {
            if let Some(data) = msg.as_data() {
                log::info!("[InterCom] data: {} bytes", data.len());
            }
        }
        _ => {}
    }
}

/// 向 StarryOS 发送消息
///
/// # Preconditions
///
/// `init()` must have been called before this function, otherwise this will
/// access uninitialized shared memory.
pub fn send_message(msg: Message) {
    unsafe {
        let shm = SharedMemory::<3>::at(ov_shm::shm::base());
        match shm.sender(ChannelId::new(1)) {
            Ok(tx) => {
                if let Err(e) = tx.try_send(&msg) {
                    log::warn!("[InterCom] send failed: {:?}", e);
                } else {
                    send_notify_ipi();
                }
            }
            Err(e) => {
                log::warn!("[InterCom] sender acquisition failed: {:?}", e);
            }
        }
    }
}

/// 向 StarryOS 发送通知
pub fn send_notification(id: u32) {
    let msg = Message::notification(id);
    send_message(msg);
}

/// 获取 RPC Server（基于当前 SHM 基址构造）
///
/// # Preconditions
///
/// `init()` must have been called before using the returned server to
/// process messages, otherwise shared memory will be uninitialized.
pub fn server() -> RpcServer {
    RpcServer::new(SHM_BASE.load(Ordering::Acquire))
}
