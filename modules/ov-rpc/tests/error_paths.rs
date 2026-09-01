//! 错误路径测试：poison 响应三个发送点 + acall Deferred 无 poison + FIFO 不楔死。
//!
//! v0.2.0 语义：服务端对双向调用在「方法未注册 / 参数反序列化失败 /
//! 响应超长」三种情况下回 poison 响应（Response kind + 原 rid + 0xFF
//! 填充的不可解码载荷），客户端按 rid 命中后得到
//! `RecvError::DeserializeFailed`——替代 v0.1.x 的挂死等超时
//! （notification(0) 形式的"错误响应"过不了客户端 request_id 检查，
//! 实际从未生效过）。

use ov_channels::{ChannelId, Message, MsgType, SharedMemory};
use ov_rpc::{
    define_service, HandledKind, ProcessResult, RecvError, RpcClient, RpcServer,
};

define_service! {
    pub TargetService {
        PARSE: 1 => call parse(v: u32) -> u32;
        BIG:   2 => call big(n: u32) -> Vec<u8>;
    }
}

impl TargetService {
    fn parse(v: u32) -> u32 { v }
    fn big(n: u32) -> Vec<u8> { vec![0xAB; n as usize] }
}

// acall 服务：验证 Deferred 不触发 poison（否则异步完成协议被拆——
// 客户端会在等待补发响应时先收到错误）。
define_service! {
    pub AsyncService {
        KICK: 1 => acall kick(nonce: u32);
    }
}

impl AsyncService {
    fn kick(_rid: u64, _nonce: u32) {}
}

struct Ctx {
    _shm: &'static SharedMemory<3>,
    server: RpcServer,
    client: RpcClient,
}

impl Ctx {
    fn new() -> Self {
        let shm = Box::leak(Box::new(SharedMemory::<3>::new()));
        shm.init();
        let addr = shm as *const _ as usize;
        Self {
            _shm: shm,
            server: RpcServer::new(addr),
            client: RpcClient::new(addr),
        }
    }
}

/// 方法已匹配但参数不可解码（参数区填 0xFF：连续 0xFF 是非法 postcard
/// varint）→ 服务端 Unhandled + poison；客户端按 rid 快失败。
#[test]
fn deserialize_failed_gets_poison() {
    let mut ctx = Ctx::new();
    let shm = ctx._shm;
    let mut p = [0u8; 255];
    p[0..8].copy_from_slice(&0x7777_u64.to_le_bytes());
    p[8..16].copy_from_slice(&1_u64.to_le_bytes()); // method 1 = PARSE，无协议 flag
    p[16..].fill(0xFF);
    shm.sender(ChannelId::new(0))
        .unwrap()
        .try_send(&Message::raw(MsgType::Request as u8, p))
        .unwrap();

    assert!(matches!(
        ctx.server.process_one::<TargetService>(),
        ProcessResult::Unhandled(1)
    ));
    assert_eq!(ctx.client.poll_responses(), 1);
    assert_eq!(
        ctx.client.recv_for::<u32>(0x7777),
        Err(RecvError::DeserializeFailed)
    );
}

/// 方法未注册（版本错配/漂移 op）→ poison 而非静默无响应。
#[test]
fn unknown_method_gets_poison() {
    let mut ctx = Ctx::new();
    let rid = ctx.client.call(999, &(), || {}).unwrap();
    assert!(matches!(
        ctx.server.process_one::<TargetService>(),
        ProcessResult::Unhandled(999)
    ));
    assert_eq!(ctx.client.poll_responses(), 1);
    assert_eq!(
        ctx.client.recv_for::<u32>(rid),
        Err(RecvError::DeserializeFailed)
    );
}

/// 响应超长（postcard > 2028B）→ ResponseTooLarge → poison。
#[test]
fn oversized_response_gets_poison() {
    let mut ctx = Ctx::new();
    let rid = ctx.client.call(TargetService::BIG, &3000u32, || {}).unwrap();
    assert!(matches!(
        ctx.server.process_one::<TargetService>(),
        ProcessResult::Unhandled(2)
    ));
    assert_eq!(ctx.client.poll_responses(), 1);
    assert_eq!(
        ctx.client.recv_for::<Vec<u8>>(rid),
        Err(RecvError::DeserializeFailed)
    );
}

/// poison 不楔死 FIFO：解码失败的条目按文档语义消费，后续好响应可达
/// （v0.1.x 的 parse-before-dequeue 实现会把失败条目永久留在队首）。
#[test]
fn poison_does_not_wedge_fifo() {
    let mut ctx = Ctx::new();
    let _rid_bad = ctx.client.call(999, &(), || {}).unwrap();
    let _rid_ok = ctx.client.call(TargetService::PARSE, &7u32, || {}).unwrap();
    ctx.server.process_one::<TargetService>();
    ctx.server.process_one::<TargetService>();

    assert_eq!(ctx.client.poll_responses(), 2);
    assert_eq!(ctx.client.recv::<u32>(), Err(RecvError::DeserializeFailed));
    assert_eq!(ctx.client.recv::<u32>(), Ok(Some(7)));
    assert_eq!(ctx.client.recv::<u32>(), Ok(None));
}

/// acall（Deferred）：无即时响应也**无 poison**——CH1 空、客户端继续
/// 等待完成方补发。
#[test]
fn acall_deferred_gets_no_poison() {
    let mut ctx = Ctx::new();
    let rid = ctx.client.call(AsyncService::KICK, &5u32, || {}).unwrap();
    assert!(matches!(
        ctx.server.process_one::<AsyncService>(),
        ProcessResult::Handled(HandledKind::Quiet)
    ));
    assert_eq!(ctx.client.poll_responses(), 0);
    assert_eq!(ctx.client.recv_for::<u32>(rid), Ok(None));
    // CH1 无任何块
    let rx = ctx._shm.receiver(ChannelId::new(1)).unwrap();
    assert_eq!(rx.len(), 0);
}

/// one-way 调用打到未注册方法：不期待响应，也不发 poison。handler 无法
/// 区分"one-way 完成"与"未注册"（服务端按 one_way 位归为 Handled(OneWay)，
/// 调用方本就无从感知）。
#[test]
fn one_way_unknown_method_stays_silent() {
    let mut ctx = Ctx::new();
    ctx.client.send(999, &(), || {}).unwrap();
    assert!(matches!(
        ctx.server.process_one::<TargetService>(),
        ProcessResult::Handled(HandledKind::OneWay)
    ));
    assert_eq!(ctx.client.poll_responses(), 0);
}
