//! ov-rpc 综合测试
//!
//! 覆盖：define_service! 宏、RpcServer（含 ProcessResult）、RpcClient、混合消息处理

use std::cell::UnsafeCell;

use ov_channels::{ChannelId, Message, MsgType, SharedMemory};
use ov_rpc::{define_service, ProcessResult, RpcClient, RpcServer};

// ============================================================================
// 测试服务定义
// ============================================================================

define_service! {
    pub CalcService {
        ECHO: 0 => fn echo(val: u32) -> u32;
        ADD: 1 => fn add(a: i32, b: i32) -> i32;
        PING: 2 => fn ping() -> u32;
        NEGATE: 3 => fn negate(val: i32) -> i32;
        SUM3: 4 => fn sum3(a: i32, b: i32, c: i32) -> i32;
    }
}

impl CalcService {
    fn echo(val: u32) -> u32 {
        val
    }
    fn add(a: i32, b: i32) -> i32 {
        a.wrapping_add(b)
    }
    fn ping() -> u32 {
        42
    }
    fn negate(val: i32) -> i32 {
        -val
    }
    fn sum3(a: i32, b: i32, c: i32) -> i32 {
        a + b + c
    }
}

// 另一个服务定义，测试多服务共存

define_service! {
    pub RawService {
        IDENT: 0 => fn ident(val: u64) -> u64;
    }
}

impl RawService {
    fn ident(val: u64) -> u64 {
        val
    }
}

// ============================================================================
// 测试基础设施
// ============================================================================

struct TestContext {
    _shm: &'static SharedMemory,
    server: RpcServer,
    client: RpcClient,
}

impl TestContext {
    fn new() -> Self {
        let shm = Box::leak(Box::new(SharedMemory::new()));
        shm.init();
        let addr = shm as *const _ as usize;
        Self {
            _shm: shm,
            server: RpcServer::new(addr, ChannelId::new(0), ChannelId::new(1)),
            client: RpcClient::new(addr, ChannelId::new(0), ChannelId::new(1)),
        }
    }
}

// ============================================================================
// 基础 RPC 调用
// ============================================================================

#[test]
fn test_echo_single_arg() {
    let ctx = TestContext::new();

    let rid = ctx.client.call_async(CalcService::ECHO, &42u32).unwrap();
    let result = ctx.server.process_one::<CalcService>();
    assert!(matches!(result, ProcessResult::Handled));

    let val: u32 = ctx.client.wait_response(rid).unwrap();
    assert_eq!(val, 42);
}

#[test]
fn test_add_two_args() {
    let ctx = TestContext::new();

    let rid = ctx.client.call_async(CalcService::ADD, &(3i32, 4i32)).unwrap();
    ctx.server.process_one::<CalcService>();

    let val: i32 = ctx.client.wait_response(rid).unwrap();
    assert_eq!(val, 7);
}

#[test]
fn test_ping_zero_args() {
    let ctx = TestContext::new();

    let rid = ctx.client
        .call_async(CalcService::PING, &())
        .unwrap();
    ctx.server.process_one::<CalcService>();

    let val: u32 = ctx.client.wait_response(rid).unwrap();
    assert_eq!(val, 42);
}

#[test]
fn test_three_args() {
    let ctx = TestContext::new();

    let rid = ctx.client
        .call_async(CalcService::SUM3, &(1i32, 2i32, 3i32))
        .unwrap();
    ctx.server.process_one::<CalcService>();

    let val: i32 = ctx.client.wait_response(rid).unwrap();
    assert_eq!(val, 6);
}

#[test]
fn test_negate_negative() {
    let ctx = TestContext::new();

    let rid = ctx.client
        .call_async(CalcService::NEGATE, &(-7i32))
        .unwrap();
    ctx.server.process_one::<CalcService>();

    let val: i32 = ctx.client.wait_response(rid).unwrap();
    assert_eq!(val, 7);
}

// ============================================================================
// 多请求批量处理
// ============================================================================

#[test]
fn test_multiple_requests() {
    let ctx = TestContext::new();

    let rid1 = ctx.client.call_async(CalcService::ECHO, &10u32).unwrap();
    let rid2 = ctx.client.call_async(CalcService::ADD, &(20i32, 30i32)).unwrap();
    let rid3 = ctx.client.call_async(CalcService::PING, &()).unwrap();

    let mut non_rpc = Vec::new();
    let count = ctx.server.process_all::<CalcService, _>(|msg| non_rpc.push(msg));
    assert_eq!(count, 3);
    assert!(non_rpc.is_empty());

    let r1: u32 = ctx.client.wait_response(rid1).unwrap();
    let r2: i32 = ctx.client.wait_response(rid2).unwrap();
    let r3: u32 = ctx.client.wait_response(rid3).unwrap();
    assert_eq!(r1, 10);
    assert_eq!(r2, 50);
    assert_eq!(r3, 42);
}

// ============================================================================
// ProcessResult 枚举
// ============================================================================

#[test]
fn test_process_result_no_message() {
    let ctx = TestContext::new();
    let result = ctx.server.process_one::<CalcService>();
    assert!(matches!(result, ProcessResult::NoMessage));
}

#[test]
fn test_process_result_handled() {
    let ctx = TestContext::new();
    ctx.client.call_async(CalcService::PING, &()).unwrap();

    let result = ctx.server.process_one::<CalcService>();
    assert!(matches!(result, ProcessResult::Handled));
}

#[test]
fn test_process_result_not_rpc() {
    let ctx = TestContext::new();

    let shm = unsafe { SharedMemory::at(ctx._shm as *const _ as usize) };
    let tx = shm.sender(ChannelId::new(0)).unwrap();
    tx.try_send(&Message::notification(99)).unwrap();

    let result = ctx.server.process_one::<CalcService>();
    match result {
        ProcessResult::NotRpc(msg) => {
            assert_eq!(msg.as_notification(), Some(99));
        }
        other => panic!("expected NotRpc, got {:?}", other),
    }
}

// ============================================================================
// 混合消息处理（核心：非RPC消息不丢失）
// ============================================================================

#[test]
fn test_mixed_notification_then_rpc() {
    let ctx = TestContext::new();

    let shm = unsafe { SharedMemory::at(ctx._shm as *const _ as usize) };
    let tx = shm.sender(ChannelId::new(0)).unwrap();

    tx.try_send(&Message::notification(100)).unwrap();
    let rid = ctx.client.call_async(CalcService::ECHO, &77u32).unwrap();

    let mut notifs = Vec::new();
    let rpc_count = ctx.server.process_all::<CalcService, _>(|msg| {
        if let Some(id) = msg.as_notification() {
            notifs.push(id);
        }
    });

    assert_eq!(rpc_count, 1);
    assert_eq!(notifs, vec![100]);

    let val: u32 = ctx.client.wait_response(rid).unwrap();
    assert_eq!(val, 77);
}

#[test]
fn test_mixed_interleaved_messages() {
    let ctx = TestContext::new();

    let shm = unsafe { SharedMemory::at(ctx._shm as *const _ as usize) };
    let tx = shm.sender(ChannelId::new(0)).unwrap();

    // 交替放入: Notif(1), RPC, Notif(2), RPC, Data
    tx.try_send(&Message::notification(1)).unwrap();
    let rid1 = ctx.client.call_async(CalcService::ECHO, &11u32).unwrap();
    tx.try_send(&Message::notification(2)).unwrap();
    let rid2 = ctx.client.call_async(CalcService::ADD, &(5i32, 3i32)).unwrap();
    tx.try_send(&Message::data(&[0xAA, 0xBB])).unwrap();

    let mut notifs = Vec::new();
    let mut data_count = 0;
    let rpc_count = ctx.server.process_all::<CalcService, _>(|msg| match msg.ty() {
        Some(MsgType::Notification) => notifs.push(msg.as_notification().unwrap()),
        Some(MsgType::Data) => data_count += 1,
        _ => {}
    });

    assert_eq!(rpc_count, 2);
    assert_eq!(notifs, vec![1, 2]);
    assert_eq!(data_count, 1);

    let r1: u32 = ctx.client.wait_response(rid1).unwrap();
    let r2: i32 = ctx.client.wait_response(rid2).unwrap();
    assert_eq!(r1, 11);
    assert_eq!(r2, 8);
}

#[test]
fn test_only_notifications_no_rpc() {
    let ctx = TestContext::new();

    let shm = unsafe { SharedMemory::at(ctx._shm as *const _ as usize) };
    let tx = shm.sender(ChannelId::new(0)).unwrap();

    tx.try_send(&Message::notification(1)).unwrap();
    tx.try_send(&Message::notification(2)).unwrap();
    tx.try_send(&Message::notification(3)).unwrap();

    let mut notifs = Vec::new();
    let rpc_count = ctx.server.process_all::<CalcService, _>(|msg| {
        if let Some(id) = msg.as_notification() {
            notifs.push(id);
        }
    });

    assert_eq!(rpc_count, 0);
    assert_eq!(notifs, vec![1, 2, 3]);
}

// ============================================================================
// 未知方法处理
// ============================================================================

#[test]
fn test_unknown_method_no_response() {
    let ctx = TestContext::new();

    let rid = ctx.client.call_async(999u64, &()).unwrap();
    let result = ctx.server.process_one::<CalcService>();

    assert!(matches!(result, ProcessResult::Handled));

    let resp: Option<u32> = ctx.client.wait_response(rid);
    assert!(resp.is_none());
}

// ============================================================================
// 客户端 API
// ============================================================================

#[test]
fn test_client_try_recv_response() {
    let ctx = TestContext::new();

    assert!(ctx.client.try_recv_response::<u32>().is_none());

    ctx.client.call_async(CalcService::ECHO, &55u32).unwrap();
    ctx.server.process_one::<CalcService>();

    let (rid, val) = ctx.client.try_recv_response::<u32>().unwrap();
    assert_eq!(val, 55);
    assert!(rid > 0);

    assert!(ctx.client.try_recv_response::<u32>().is_none());
}

#[test]
fn test_request_ids_are_unique() {
    let ctx = TestContext::new();

    let rid1 = ctx.client.call_async(CalcService::PING, &()).unwrap();
    let rid2 = ctx.client.call_async(CalcService::PING, &()).unwrap();
    let rid3 = ctx.client.call_async(CalcService::PING, &()).unwrap();

    assert_ne!(rid1, rid2);
    assert_ne!(rid2, rid3);
    assert_ne!(rid1, rid3);
}

// ============================================================================
// has_pending
// ============================================================================

#[test]
fn test_has_pending() {
    let ctx = TestContext::new();

    // has_pending() 在初始空 channel 上返回 false
    // （需先触发一次 try_recv 清除残留 pending 标志）
    let _ = ctx.server.process_one::<CalcService>();
    assert!(!ctx.server.has_pending());

    ctx.client.call_async(CalcService::PING, &()).unwrap();
    assert!(ctx.server.has_pending());

    ctx.server.process_one::<CalcService>();

    // ov-channal ring buffer 的 pending 标志在消费最后一条消息后仍为 true，
    // 需要再调用一次 try_recv（发现空）才会清除
    let result = ctx.server.process_one::<CalcService>();
    assert!(matches!(result, ProcessResult::NoMessage));
    assert!(!ctx.server.has_pending());
}

// ============================================================================
// 通道容量
// ============================================================================

#[test]
fn test_channel_capacity() {
    let ctx = TestContext::new();

    let mut sent = 0usize;
    loop {
        match ctx.client.call_async(CalcService::PING, &()) {
            Ok(_) => sent += 1,
            Err(ov_channels::SendError::Full) => break,
            Err(e) => panic!("unexpected error: {:?}", e),
        }
    }

    assert!(sent > 0, "should send at least one message");
    assert!(sent < 128, "ring buffer capacity is 128 (usable 127)");
}

// ============================================================================
// 多服务共存
// ============================================================================

#[test]
fn test_multiple_services() {
    let ctx = TestContext::new();

    let rid_calc = ctx.client.call_async(CalcService::ADD, &(1i32, 2i32)).unwrap();
    let rid_raw = ctx.client.call_async(RawService::IDENT, &0xDEAD_u64).unwrap();

    ctx.server.process_one::<CalcService>();
    ctx.server.process_one::<RawService>();

    let calc_result: i32 = ctx.client.wait_response(rid_calc).unwrap();
    let raw_result: u64 = ctx.client.wait_response(rid_raw).unwrap();

    assert_eq!(calc_result, 3);
    assert_eq!(raw_result, 0xDEAD);
}

// ============================================================================
// 边界值
// ============================================================================

#[test]
fn test_echo_zero() {
    let ctx = TestContext::new();

    let rid = ctx.client.call_async(CalcService::ECHO, &0u32).unwrap();
    ctx.server.process_one::<CalcService>();

    let val: u32 = ctx.client.wait_response(rid).unwrap();
    assert_eq!(val, 0);
}

#[test]
fn test_echo_max_u32() {
    let ctx = TestContext::new();

    let rid = ctx.client.call_async(CalcService::ECHO, &u32::MAX).unwrap();
    ctx.server.process_one::<CalcService>();

    let val: u32 = ctx.client.wait_response(rid).unwrap();
    assert_eq!(val, u32::MAX);
}

#[test]
fn test_add_overflow() {
    let ctx = TestContext::new();

    let rid = ctx.client
        .call_async(CalcService::ADD, &(i32::MAX, 1i32))
        .unwrap();
    ctx.server.process_one::<CalcService>();

    let val: i32 = ctx.client.wait_response(rid).unwrap();
    assert_eq!(val, i32::MIN);
}

#[test]
fn test_large_batch_roundtrip() {
    let ctx = TestContext::new();

    let mut rids = Vec::new();
    for i in 0u32..50 {
        let rid = ctx.client.call_async(CalcService::ECHO, &i).unwrap();
        rids.push(rid);
    }

    let count = ctx.server.process_all::<CalcService, _>(|_| {});
    assert_eq!(count, 50);

    for (i, rid) in rids.into_iter().enumerate() {
        let val: u32 = ctx.client.wait_response(rid).unwrap();
        assert_eq!(val, i as u32);
    }
}

// ============================================================================
// 常量可访问性
// ============================================================================

#[test]
fn test_method_id_constants() {
    assert_eq!(CalcService::ECHO, 0);
    assert_eq!(CalcService::ADD, 1);
    assert_eq!(CalcService::PING, 2);
    assert_eq!(CalcService::NEGATE, 3);
    assert_eq!(CalcService::SUM3, 4);
    assert_eq!(RawService::IDENT, 0);
}
