//! RPC 服务端

use core::sync::atomic::Ordering;

use ov_channels::{ChannelId, Message, SharedMemory};

use crate::{MethodId, strip_flags, is_one_way, wants_notify};

/// 响应发送失败计数（响应通道 try_send 失败——ring 满时响应被静默丢弃）。
///
/// 插桩用：K3 延迟/压力测试经 intercom 的 STATS 方法读取，检测"响应丢失"
/// （客户端视角即 seq 缺口）。无锁原子计数，中断/任务上下文均可递增。
/// portable-atomic：K3 专属 target（atomic-cas:false）下 core RMW 被
/// cfg 掉，经 CS 回退；标准 target 上别名 core 原生，行为不变。
pub static RESP_SEND_FAILS: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);

// ── 延迟插桩（feature "stamps"）─────────────────────────────────────────
//
// 把 `process_channel` 内部分解为四段（dispatch 分解戳，dseen/svc 残余
// 归因用，2026-08-17 延迟战役）：入口 / try_recv 完成 / handler 完成 /
// 响应写入完成。时钟由 app 注入（ov-rpc 平台无关，不直接依赖任何定时器；
// K3 app 装配 clint mtime，见 intercom::init/wait_ready 的 set_clock）。
// 每戳 = fn 指针 Relaxed 载入（纯 ld）+ mtime MMIO 读 + Relaxed store，
// 实测 <0.5µs/条；feature 关闭时零开销。

/// 延迟插桩：dispatch 分解戳与 app 时钟钩子（feature `stamps`）。
#[cfg(feature = "stamps")]
pub mod stamp {
    use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    /// app 注入的时钟（返回任意单调 tick；0 = 未装配，now() 返回 0）。
    static CLOCK: AtomicUsize = AtomicUsize::new(0);

    /// 戳存储：[ch_enter, recv_done, handle_done, resp_done,
    /// idx_done, serde_done]（后两槽为 L0 归因细分戳，2026-08-20）。
    static T: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

    /// 装配时钟（app init 期调用一次；单核串行，Relaxed 足够）。
    pub fn set_clock(f: fn() -> u64) {
        CLOCK.store(f as usize, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn mark(i: usize) {
        let p = CLOCK.load(Ordering::Relaxed);
        if p == 0 {
            return;
        }
        // SAFETY: p 来自 set_clock 存入的合法 fn 指针。
        let f: fn() -> u64 = unsafe { core::mem::transmute(p) };
        T[i].store(f(), Ordering::Relaxed);
    }

    /// 读第 i 戳（STATS 转发用）。
    pub fn get(i: usize) -> u64 {
        if i < 6 {
            T[i].load(Ordering::Relaxed)
        } else {
            0
        }
    }
}

#[cfg(feature = "stamps")]
use stamp::mark;

#[cfg(not(feature = "stamps"))]
#[inline]
fn mark(_i: usize) {}

/// 戳索引（STATS 语义对齐用，双端镜像义务见 intercom::stat_idx）。
pub mod stamp_idx {
    /// process_channel 入口。
    pub const CH_ENTER: usize = 0;
    /// try_recv 完成（消息已取出）。
    pub const RECV_DONE: usize = 1;
    /// handler 完成（响应已构造）。
    pub const HANDLE_DONE: usize = 2;
    /// 响应写入完成。
    pub const RESP_DONE: usize = 3;
    /// try_recv 双索引 Acquire 完成（L0 细分：magic+read+write 三笔之后、
    /// 槽读之前）。drx 拆为 [CH_ENTER→IDX_DONE]（索引 Acquire）与
    /// [IDX_DONE→RECV_DONE]（槽读+Release）。
    pub const IDX_DONE: usize = 4;
    /// method_id/flags 剥离完成（L0 细分：dserde 拆为 [RECV_DONE→
    /// SERDE_DONE]（字段读）与 [SERDE_DONE→t_seen]（dispatch 宏+postcard
    /// 反序列化+进 handler））。
    pub const SERDE_DONE: usize = 5;
}

/// 通道布局约定。
pub mod channel {
    use ov_channels::ChannelId;
    pub const REQ: ChannelId = ChannelId::new(0);
    pub const RESP: ChannelId = ChannelId::new(1);
    pub const URGENT: ChannelId = ChannelId::new(2);
}

/// 反序列化失败时返回的错误指示符。
///
/// 由 [`define_service!`](crate::define_service) 宏在 payload 无法解码时产生。
/// 服务端据此发送错误响应，防止客户端在两方调用上永久阻塞。
pub struct DeserializeFailed;

/// RPC 请求处理 trait。
///
/// 推荐使用 [`define_service!`](crate::define_service) 宏自动生成实现。
pub trait RpcHandler {
    /// 处理一个 RPC 请求。
    ///
    /// `method` 已去除协议 flag，是实际的 method ID。
    /// - 返回 `Ok(Some(response))` — 序列化结果，写回响应通道
    /// - 返回 `Ok(None)` — 方法未知或单向调用已完成（无响应）
    /// - 返回 `Err(DeserializeFailed)` — method 已匹配但 payload 反序列化失败
    fn handle(method: MethodId, msg: Message) -> Result<Option<Message>, DeserializeFailed>;
}

/// 处理结果的附带信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandledKind {
    /// 双向调用，需要回 IPI (call 模式)
    Notify,
    /// 双向调用，不回 IPI (call_poll 模式)
    Quiet,
    /// 单向调用，无响应发送
    OneWay,
}

/// [`RpcServer::process_one`] / [`RpcServer::process_urgent`] 的返回结果。
#[derive(Debug)]
pub enum ProcessResult {
    /// Channel 中无待处理消息
    NoMessage,
    /// RPC 请求已处理
    Handled(HandledKind),
    /// RPC 请求已知但未被处理（方法未知或 handler 返回 None）
    Unhandled(MethodId),
    /// 非 RPC 消息，交由调用者处理
    NotRpc(Message),
}

/// RPC 服务端。
///
/// ```text
/// CH0 (req_ch):    Client ──▶ 本端  (普通请求)
/// CH1 (resp_ch):   本端  ──▶ Client (响应)
/// CH2 (urgent_ch): Client ──▶ 本端  (急停)
/// ```
pub struct RpcServer {
    shm_addr: usize,
    req_ch: ChannelId,
    resp_ch: ChannelId,
    urgent_ch: ChannelId,
}

impl RpcServer {
    /// 使用默认通道布局创建。
    pub const fn new(shm_addr: usize) -> Self {
        Self::with_channels(shm_addr, channel::REQ, channel::RESP, channel::URGENT)
    }

    /// 自定义通道创建。
    pub const fn with_channels(
        shm_addr: usize,
        req_ch: ChannelId,
        resp_ch: ChannelId,
        urgent_ch: ChannelId,
    ) -> Self {
        Self {
            shm_addr,
            req_ch,
            resp_ch,
            urgent_ch,
        }
    }

    #[inline]
    fn shm(&self) -> &'static SharedMemory<3> {
        unsafe { SharedMemory::<3>::at(self.shm_addr) }
    }

    fn process_channel<H: RpcHandler>(&self, ch: ChannelId) -> ProcessResult {
        mark(stamp_idx::CH_ENTER);
        let shm = self.shm();
        let Ok(rx) = shm.receiver(ch) else {
            return ProcessResult::NoMessage;
        };

        // stamps 构建：手写展开 Channel::try_recv（语义与 ov-channels
        // channel.rs/ring.rs 逐笔对齐），在双索引 Acquire 后插 IDX_DONE
        // 细分戳——L0 归因（2026-08-20）：drx 43.6µs 中 fence 理论仅
        // ~10µs，段内拆分定位其余归属。偏移与 ov-rpc cache.rs 编译期
        // 断言同源：magic@0 / read@0x100 / write@0x108 / slots@0x110。
        #[cfg(feature = "stamps")]
        let msg = {
            use core::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
            // SAFETY: ch 句柄来自 shm.receiver 的同一 channel_unchecked；
            // 各偏移落在 Channel 布局内（cache.rs 断言对账），原子重解释
            // 只读 magic/索引、Release 推进 read——与 ring.try_recv 同序。
            unsafe {
                let base = shm.channel_unchecked(ch) as *const ov_channels::Channel as usize;
                let magic = &*(base as *const AtomicU16);
                if magic.load(Ordering::Acquire) != ov_channels::MAGIC {
                    None
                } else {
                    let rb = (base + 0x100) as *const AtomicUsize;
                    let read = (*rb).load(Ordering::Acquire);
                    let _write = (*rb.add(1)).load(Ordering::Acquire);
                    mark(stamp_idx::IDX_DONE);
                    if read == _write {
                        None
                    } else {
                        let slot = (base + 0x110
                            + read * core::mem::size_of::<ov_channels::Message>())
                            as *const ov_channels::Message;
                        let m = slot.read_volatile();
                        (*rb).store(
                            (read + 1) % ov_channels::CHANNEL_CAPACITY,
                            Ordering::Release,
                        );
                        Some(m)
                    }
                }
            }
        };
        #[cfg(not(feature = "stamps"))]
        let msg = rx.try_recv();
        let Some(msg) = msg else {
            return ProcessResult::NoMessage;
        };
        mark(stamp_idx::RECV_DONE);

        let Some(raw_method) = msg.method_id() else {
            return ProcessResult::NotRpc(msg);
        };
        mark(stamp_idx::SERDE_DONE);

        let one_way = is_one_way(raw_method);
        let notify = wants_notify(raw_method);
        let method = strip_flags(raw_method);

        let resp = match H::handle(method, msg) {
            Ok(Some(resp)) => {
                mark(stamp_idx::HANDLE_DONE);
                resp
            }
            Ok(None) => {
                // One-way handlers return Ok(None) by design.
                // If this was a one-way call, report it as handled.
                // Otherwise the method ID was unknown.
                if one_way {
                    return ProcessResult::Handled(HandledKind::OneWay);
                }
                return ProcessResult::Unhandled(method);
            }
            Err(_) => {
                // Method matched but payload deserialization failed.
                #[cfg(feature = "logging")]
                log::warn!("[RpcServer] deserialization failed for method {}", method);
                if !one_way {
                    // Send an error response so the client doesn't hang forever.
                    if let Ok(tx) = shm.sender(self.resp_ch) {
                        if tx.try_send(&Message::notification(0)).is_err() {
                            RESP_SEND_FAILS.fetch_add(1, Ordering::Relaxed);
                            #[cfg(feature = "logging")]
                            log::warn!("[RpcServer] failed to send error response for method {}", method);
                        }
                    }
                }
                return ProcessResult::Unhandled(method);
            }
        };

        if !one_way {
            if let Ok(tx) = shm.sender(self.resp_ch) {
                if tx.try_send(&resp).is_err() {
                    RESP_SEND_FAILS.fetch_add(1, Ordering::Relaxed);
                    #[cfg(feature = "logging")]
                    log::warn!("[RpcServer] failed to send response for method {}", method);
                }
                mark(stamp_idx::RESP_DONE);
            } else {
                #[cfg(feature = "logging")]
                log::warn!("[RpcServer] failed to acquire response channel for method {}", method);
            }
        }

        ProcessResult::Handled(if one_way {
            HandledKind::OneWay
        } else if notify {
            HandledKind::Notify
        } else {
            HandledKind::Quiet
        })
    }

    /// 处理急停通道 (CH2) 的一条消息。
    pub fn process_urgent<H: RpcHandler>(&self) -> ProcessResult {
        self.process_channel::<H>(self.urgent_ch)
    }

    /// 处理普通通道 (CH0) 的一条消息。
    pub fn process_one<H: RpcHandler>(&self) -> ProcessResult {
        self.process_channel::<H>(self.req_ch)
    }

    /// 先处理所有急停，再处理所有普通消息。
    ///
    /// 非 RPC 消息通过 `on_other` 回调。
    /// 每处理完一个 Notify 模式的请求，立即调用 `on_notify` 回 IPI，
    /// 保证客户端延迟最小化。
    /// 返回已处理的消息数量。
    pub fn process_all<H: RpcHandler, F: FnMut(Message), N: FnMut()>(
        &self,
        mut on_other: F,
        mut on_notify: N,
    ) -> usize {
        let mut count = 0;

        loop {
            match self.process_urgent::<H>() {
                ProcessResult::NoMessage => break,
                ProcessResult::Handled(HandledKind::OneWay) => count += 1,
                ProcessResult::Handled(HandledKind::Notify) => {
                    count += 1;
                    on_notify();
                }
                ProcessResult::Handled(HandledKind::Quiet) => {
                    count += 1;
                }
                ProcessResult::Unhandled(_) => {}
                ProcessResult::NotRpc(msg) => on_other(msg),
            }
        }

        loop {
            match self.process_one::<H>() {
                ProcessResult::NoMessage => break,
                ProcessResult::Handled(HandledKind::OneWay) => count += 1,
                ProcessResult::Handled(HandledKind::Notify) => {
                    count += 1;
                    on_notify();
                }
                ProcessResult::Handled(HandledKind::Quiet) => {
                    count += 1;
                }
                ProcessResult::Unhandled(_) => {}
                ProcessResult::NotRpc(msg) => on_other(msg),
            }
        }

        count
    }

    /// 检查急停通道是否有待处理消息。
    pub fn has_urgent(&self) -> bool {
        let shm = self.shm();
        shm.receiver(self.urgent_ch)
            .is_ok_and(|rx| rx.has_pending())
    }

    /// 检查普通通道是否有待处理消息。
    pub fn has_pending(&self) -> bool {
        let shm = self.shm();
        shm.receiver(self.req_ch)
            .is_ok_and(|rx| rx.has_pending())
    }
}
