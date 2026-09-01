//! RPC 服务端

use core::fmt;
use core::sync::atomic::Ordering;

use ov_channels::{ChannelId, Message, MsgType, PAYLOAD_SIZE, SharedMemory};

use crate::{MethodId, strip_flags, is_one_way, wants_notify};

/// 响应发送失败计数（响应通道 try_send 失败——ring 满时响应被静默丢弃）。
///
/// 插桩用：K3 延迟/压力测试经 intercom 的 STATS 方法读取，检测"响应丢失"
/// （客户端视角即 seq 缺口）。无锁原子计数，中断/任务上下文均可递增。
/// portable-atomic：K3 专属 target（atomic-cas:false）下 core RMW 被
/// cfg 掉，经 CS 回退；标准 target 上别名 core 原生，行为不变。
pub static RESP_SEND_FAILS: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);

/// 通道布局约定。
pub mod channel {
    use ov_channels::ChannelId;
    pub const REQ: ChannelId = ChannelId::new(0);
    pub const RESP: ChannelId = ChannelId::new(1);
    pub const URGENT: ChannelId = ChannelId::new(2);
}

/// handler 处理错误。
///
/// 由 [`define_service!`](crate::define_service) 宏产生。两种情况下服务端
/// 都会向双向调用回 poison 响应（Response kind + 原 rid + 不可解码载荷），
/// 客户端得到
/// `RecvError::DeserializeFailed` 而非挂死等超时。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcError {
    /// 方法已匹配但请求参数反序列化失败。
    DeserializeFailed,
    /// 响应序列化超过单条消息上限（见 [`RESPONSE_DATA_MAX`]）——服务定义
    /// 的返回类型过大，属配置错误。
    ResponseTooLarge,
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeserializeFailed => write!(f, "request deserialization failed"),
            Self::ResponseTooLarge => write!(f, "response exceeds message size limit"),
        }
    }
}

/// 响应结果区上限（字节）：rid(8B) 之外的 postcard 结果载荷。
///
/// 总预算 = ov-channels 字节流单消息上限
/// ([`MAX_STREAM_LEN`](ov_channels::stream::MAX_STREAM_LEN) = 2036B) − rid 8B。
pub const RESPONSE_DATA_MAX: usize = ov_channels::stream::MAX_STREAM_LEN - 8;

/// RPC 响应暂存缓冲（服务端侧，`rid(8B LE) ++ postcard(T)` 线格式）。
///
/// **零成本写路径**（K3 板 2026-09-01 回归教训）：no_std 禁动态分配，
/// 大响应暂存只能内联定长——若用普通 `[u8; 2036]` 数组，构造时语言
/// 语义强制全量清零（每条响应 memset ~2KB），且按值返回经 `Reply`
/// 枚举跨函数搬移两次 ~2KB；在无缓存 SRAM（写单价为读的 5 倍）上
/// 合计 ~170µs/条。故本类型：
/// - 内部 `core::mem::MaybeUninit` + `len` 追踪——**写多少初始化多少**，
///   `empty()` 是零成本构造（仅栈预留，无 memset）；
/// - 由调用方持有（`process_channel` 栈上 / acall 完成方局部），
///   `handle` 经**出参**写入、返回无字段的 [`Reply`]（寄存器返回），
///   消灭按值搬移；
/// - `send_response` 对 ≤255B 载荷走 [`ov_channels::Sender::try_send_raw`]
///   前缀直写（响应字节在环槽里恰好写一次，尾部陈旧无害——postcard
///   自定界，接收端解码自停，见 `ov_channels` 环层 try_send_raw 的
///   线格式容差说明）。
///
/// 大响应（255B < len ≤ 2036B）走 [`ov_channels::stream`] 帧多块原子发布。
#[derive(Clone)]
pub struct Response {
    buf: core::mem::MaybeUninit<[u8; ov_channels::stream::MAX_STREAM_LEN]>,
    len: usize,
}

impl Response {
    /// 空缓冲（零成本：不触碰内存，仅 len 置零）。
    #[inline]
    pub const fn empty() -> Self {
        Self {
            buf: core::mem::MaybeUninit::uninit(),
            len: 0,
        }
    }

    /// 序列化写入：`rid ++ postcard(result)`。
    ///
    /// 只写实际使用的字节；postcard 结果超预算（> [`RESPONSE_DATA_MAX`]）
    /// 返回 [`RpcError::ResponseTooLarge`] 且本缓冲保持空（len 不更新）。
    pub fn write<T: serde::Serialize>(&mut self, rid: u64, result: &T) -> Result<(), RpcError> {
        // SAFETY（MaybeUninit 不变量）：[..8] 由本行写 rid 立即初始化；
        // [8..] 交给 postcard 作目标写多少初始化多少；len 只在成功后
        // 更新为 8+used——`bytes()` 只暴露 [..len]，未初始化字节永不被读。
        // from_raw_parts_mut 只对目标区建 &mut [u8]（u8 无无效位模式），
        // 不经 &mut [u8; 2036] 引用整块未初始化内存。
        let dst: &mut [u8] = unsafe {
            core::slice::from_raw_parts_mut(
                self.buf.as_mut_ptr() as *mut u8,
                ov_channels::stream::MAX_STREAM_LEN,
            )
        };
        dst[..8].copy_from_slice(&rid.to_le_bytes());
        match postcard::to_slice(result, &mut dst[8..]) {
            Ok(used) => {
                self.len = 8 + used.len();
                Ok(())
            }
            // 本路径 postcard 只会因目标缓冲不足失败（SerializeBufferOverflow）
            Err(_) => Err(RpcError::ResponseTooLarge),
        }
    }

    /// 原始字节写入（**不**经 postcard）——已知自有线格式的载荷（如
    /// INIT 服务发现描述符）。超预算同 [`Self::write`]。
    pub fn write_raw(&mut self, rid: u64, bytes: &[u8]) -> Result<(), RpcError> {
        if bytes.len() > RESPONSE_DATA_MAX {
            return Err(RpcError::ResponseTooLarge);
        }
        // SAFETY：同 write——[..8+bytes.len()] 本函数内全部写入；
        // from_raw_parts_mut 不对未初始化区建数组引用。
        let dst: &mut [u8] = unsafe {
            core::slice::from_raw_parts_mut(
                self.buf.as_mut_ptr() as *mut u8,
                ov_channels::stream::MAX_STREAM_LEN,
            )
        };
        dst[..8].copy_from_slice(&rid.to_le_bytes());
        dst[8..8 + bytes.len()].copy_from_slice(bytes);
        self.len = 8 + bytes.len();
        Ok(())
    }

    /// 完整载荷视图：`rid ++ postcard(T)`（即已初始化的 `[..len]`）。
    pub fn bytes(&self) -> &[u8] {
        // SAFETY：write/write_raw 的不变量——[..len] 已初始化；
        // from_raw_parts 只对 [..len] 建切片引用，未初始化区不触碰。
        unsafe { core::slice::from_raw_parts(self.buf.as_ptr() as *const u8, self.len) }
    }

    /// 本响应对应的请求 ID（缓冲为空时返回 `None`）。
    pub fn rid(&self) -> Option<u64> {
        let b = self.bytes();
        (b.len() >= 8).then(|| u64::from_le_bytes(b[..8].try_into().unwrap()))
    }

    /// 载荷长度（含 8B rid）。
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// 恒为 `false`（写入即含 rid，最小 8B）；为 clippy len/is_empty 配对保留。
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 不 dump 载荷：MaybeUninit 内只有 [..len] 可读，Debug 面板用
        // len/rid 足够定位。
        f.debug_struct("Response")
            .field("len", &self.len)
            .field("rid", &self.rid())
            .finish()
    }
}

/// [`RpcHandler::handle`] 的返回值（**无字段**——寄存器返回，零搬移）。
///
/// 三态区分是 poison 响应语义的前提：服务端对"方法未注册"要回 poison
/// （防客户端挂死），但**不能**把 acall 的"稍后补发"误判成未注册——
/// 否则客户端会在等待异步完成时先收到 poison。响应载荷经出参 `out`
/// 写入（见 [`Response`] 文档），本枚举只表达流程分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// 无即时响应：one-way 完成（客户端不期待响应）或方法未注册（服务端
    /// 按请求的 one_way 位区分二者——未注册的双向调用回 poison）。
    Silent,
    /// 已受理（acall）：响应由完成方稍后经 CH1 + 自行门铃补发。服务端
    /// 不发任何即时响应，流程上等价 Quiet。
    Deferred,
    /// 响应已写入 `out`（单块/多块由 [`send_response`] 按长度决定）。
    Written,
}

/// 按长度选择线格式发送一条响应。
///
/// - `len ≤ 255` → [`Sender::try_send_raw`](ov_channels::Sender::try_send_raw)
///   前缀直写（kind + 实际字节进环槽，尾部陈旧——postcard 自定界，见
///   `ov_channels::RingBuffer::try_send_raw` 的线格式容差说明）；
/// - 否则 → 字节流帧多块原子发布（>255B 恒 ≥2 块，客户端按
///   "Single=裸格式 / Multi=流帧"的约定无损区分）。
///
/// 发送失败（环满/通道无效）原样上抛，由调用方计数（如
/// [`RESP_SEND_FAILS`]）或降级处理。
pub fn send_response(
    tx: &ov_channels::Sender<'_>,
    resp: &Response,
) -> Result<(), ov_channels::SendError> {
    let kind = MsgType::Response as u8;
    if resp.len <= PAYLOAD_SIZE {
        tx.try_send_raw(kind, resp.bytes())
    } else {
        tx.try_send_stream(kind, resp.bytes())
    }
}

/// 错误指示响应（poison）：Response kind + 原请求 rid + 不可解码载荷。
///
/// 载荷在 rid 之后填 `0xFF`——连续 `0xFF` 是非法 postcard varint，任何
/// `T` 的 `from_bytes` 必然失败，客户端按 rid 命中后得到
/// `RecvError::DeserializeFailed`。三个发送点：方法未注册 / 参数反序列化
/// 失败 / 响应超长，把"客户端挂死等超时"变成"立即且可归因的失败"
/// （版本错配下调用漂移 op 的快失败面）。
fn poison_response(rid: u64) -> Message {
    let mut p = [0u8; PAYLOAD_SIZE];
    p[..8].copy_from_slice(&rid.to_le_bytes());
    p[8..].fill(0xFF);
    Message::raw(MsgType::Response as u8, p)
}

/// RPC 请求处理 trait。
///
/// 推荐使用 [`define_service!`](crate::define_service) 宏自动生成实现。
pub trait RpcHandler {
    /// 处理一个 RPC 请求。
    ///
    /// `method` 已去除协议 flag，是实际的 method ID。响应经**出参**
    /// `out` 写入（零搬移，见 [`Response`] 文档）：
    /// - `Ok(Reply::Written)` — `out` 已填好，服务端写回响应通道
    /// - `Ok(Reply::Silent)` — 方法未注册，或单向调用已完成（无响应）
    /// - `Ok(Reply::Deferred)` — acall 已受理，响应稍后由完成方补发
    /// - `Err(RpcError)` — 参数反序列化失败或响应超长（服务端回 poison）
    fn handle(method: MethodId, msg: Message, out: &mut Response) -> Result<Reply, RpcError>;

    /// 服务描述符字节（[`descriptor`](crate::descriptor) 紧凑格式），
    /// 由 [`define_service!`](crate::define_service) const 生成。
    ///
    /// method 0（[`crate::METHOD_INIT`]）的 INIT 请求在进 `handle` 之前
    /// 就被服务端用本描述符应答。默认实现是**合法的空描述符**
    /// （proto + desc_len=1 + count=0，手写 impl 可不覆盖——INIT 会
    /// 得到 0 方法的表，调用方据此识别"服务端不提供发现"）。
    fn descriptor() -> &'static [u8] {
        &[crate::descriptor::PROTOCOL_VERSION, 1, 0]
    }
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
// NotRpc(Message) 内联 256B 为有意设计（no_std 无 alloc）；每消息至多
// 构造一次、随回调即弃。
#[allow(clippy::large_enum_variant)]
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
        let shm = self.shm();
        let Ok(rx) = shm.receiver(ch) else {
            return ProcessResult::NoMessage;
        };
        let msg = rx.try_recv();
        let Some(msg) = msg else {
            return ProcessResult::NoMessage;
        };

        let Some(raw_method) = msg.method_id() else {
            return ProcessResult::NotRpc(msg);
        };

        // Request kind 且 payload ≥ 16B（method_id 已验）⇒ rid 必在；
        // unwrap_or(0) 仅防御畸形消息。rid 须在 handle 消费 msg 前取出
        // （poison 响应要按它寻址）。
        let rid = msg.request_id().unwrap_or(0);

        let one_way = is_one_way(raw_method);
        let notify = wants_notify(raw_method);
        let method = strip_flags(raw_method);

        // 服务发现：method 0 保留（METHOD_INIT）——描述符经大响应通路
        // 回发，不进 handler。旧固件（无拦截、未重编号）下 INIT 落到
        // handler 的未知方法 → poison → 客户端快失败识别"无服务发现"。
        let mut resp = Response::empty();
        if method == crate::METHOD_INIT {
            match resp.write_raw(rid, H::descriptor()) {
                Ok(()) => {
                    if !one_way
                        && let Ok(tx) = shm.sender(self.resp_ch)
                        && send_response(&tx, &resp).is_err()
                    {
                        RESP_SEND_FAILS.fetch_add(1, Ordering::Relaxed);
                    }
                    return ProcessResult::Handled(if notify {
                        HandledKind::Notify
                    } else {
                        HandledKind::Quiet
                    });
                }
                // 描述符超预算（服务表配置错误）：按响应超长走 poison
                Err(_) => {
                    if !one_way
                        && let Ok(tx) = shm.sender(self.resp_ch)
                        && tx.try_send(&poison_response(rid)).is_err()
                    {
                        RESP_SEND_FAILS.fetch_add(1, Ordering::Relaxed);
                    }
                    return ProcessResult::Unhandled(method);
                }
            }
        }

        match H::handle(method, msg, &mut resp) {
            Ok(Reply::Written) => {}
            Ok(Reply::Deferred) => {
                // acall 已受理：无即时响应，完成方自行补发 + 门铃。
                // 等价 Quiet（计已处理，不回 IPI）——绝不能走 poison。
                return ProcessResult::Handled(HandledKind::Quiet);
            }
            Ok(Reply::Silent) => {
                if one_way {
                    return ProcessResult::Handled(HandledKind::OneWay);
                }
                // 方法未注册（版本错配/漂移 op）：poison 响应防客户端挂死
                if let Ok(tx) = shm.sender(self.resp_ch)
                    && tx.try_send(&poison_response(rid)).is_err()
                {
                    RESP_SEND_FAILS.fetch_add(1, Ordering::Relaxed);
                }
                return ProcessResult::Unhandled(method);
            }
            Err(e) => {
                // 参数反序列化失败或响应超长
                #[cfg(feature = "logging")]
                log::warn!("[RpcServer] {} for method {}", e, method);
                #[cfg(not(feature = "logging"))]
                let _ = &e;
                if !one_way
                    && let Ok(tx) = shm.sender(self.resp_ch)
                    && tx.try_send(&poison_response(rid)).is_err()
                {
                    RESP_SEND_FAILS.fetch_add(1, Ordering::Relaxed);
                    #[cfg(feature = "logging")]
                    log::warn!("[RpcServer] failed to send error response for method {}", method);
                }
                return ProcessResult::Unhandled(method);
            }
        }

        if !one_way {
            if let Ok(tx) = shm.sender(self.resp_ch) {
                if send_response(&tx, &resp).is_err() {
                    RESP_SEND_FAILS.fetch_add(1, Ordering::Relaxed);
                    #[cfg(feature = "logging")]
                    log::warn!("[RpcServer] failed to send response for method {}", method);
                }
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
