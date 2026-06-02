//! ov-rpc 综合测试

use ov_channels::{ChannelId, Message, SharedMemory};
use ov_rpc::{
    define_service, HandledKind, ProcessResult, RpcClient, RpcServer,
};

// ============================================================================
// 测试服务定义
// ============================================================================

define_service! {
    pub CalcService {
        ECHO:   0 => call echo(val: u32) -> u32;
        ADD:    1 => call add(a: i32, b: i32) -> i32;
        PING:   2 => call ping() -> u32;
        NEGATE: 3 => call negate(val: i32) -> i32;
        SUM3:   4 => call sum3(a: i32, b: i32, c: i32) -> i32;
        LOG:    5 => send log(msg: u32);
        STOP:   6 => urgent stop();
    }
}

static mut LAST_LOG: u32 = 0;
static mut STOPPED: bool = false;

impl CalcService {
    fn echo(val: u32) -> u32 { val }
    fn add(a: i32, b: i32) -> i32 { a.wrapping_add(b) }
    fn ping() -> u32 { 42 }
    fn negate(val: i32) -> i32 { -val }
    fn sum3(a: i32, b: i32, c: i32) -> i32 { a + b + c }
    fn log(msg: u32) { unsafe { LAST_LOG = msg }; }
    fn stop() { unsafe { STOPPED = true }; }
}

define_service! {
    pub RawService {
        IDENT: 0 => call ident(val: u64) -> u64;
    }
}

impl RawService {
    fn ident(val: u64) -> u64 { val }
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
            server: RpcServer::new(addr),
            client: RpcClient::new(addr),
        }
    }
}

// ============================================================================
// call (request-response, server IPI back)
// ============================================================================

#[test]
fn test_call_echo() {
    let mut ctx = TestContext::new();
    let rid = ctx.client.call(CalcService::ECHO, &42u32, || {}).unwrap();
    ctx.server.process_one::<CalcService>();
    ctx.client.poll_responses();
    let val: u32 = ctx.client.recv_for(rid).unwrap().unwrap();
    assert_eq!(val, 42);
}

#[test]
fn test_call_add() {
    let mut ctx = TestContext::new();
    let rid = ctx.client.call(CalcService::ADD, &(3i32, 4i32), || {}).unwrap();
    ctx.server.process_one::<CalcService>();
    ctx.client.poll_responses();
    let val: i32 = ctx.client.recv_for(rid).unwrap().unwrap();
    assert_eq!(val, 7);
}

#[test]
fn test_call_ping() {
    let mut ctx = TestContext::new();
    let rid = ctx.client.call(CalcService::PING, &(), || {}).unwrap();
    ctx.server.process_one::<CalcService>();
    ctx.client.poll_responses();
    let val: u32 = ctx.client.recv_for(rid).unwrap().unwrap();
    assert_eq!(val, 42);
}

#[test]
fn test_call_handled_kind_notify() {
    let ctx = TestContext::new();
    let _rid = ctx.client.call(CalcService::PING, &(), || {}).unwrap();
    let result = ctx.server.process_one::<CalcService>();
    assert!(matches!(result, ProcessResult::Handled(HandledKind::Notify)));
}

// ============================================================================
// call_poll (request-response, no IPI back, client polls)
// ============================================================================

#[test]
fn test_call_poll_echo() {
    let mut ctx = TestContext::new();
    let rid = ctx.client.call_poll(CalcService::ECHO, &99u32, || {}).unwrap();
    let result = ctx.server.process_one::<CalcService>();
    assert!(matches!(result, ProcessResult::Handled(HandledKind::Quiet)));
    ctx.client.poll_responses();
    let val: u32 = ctx.client.recv_for(rid).unwrap().unwrap();
    assert_eq!(val, 99);
}

#[test]
fn test_call_poll_handled_kind_quiet() {
    let ctx = TestContext::new();
    let _rid = ctx.client.call_poll(CalcService::ECHO, &1u32, || {}).unwrap();
    let result = ctx.server.process_one::<CalcService>();
    assert!(matches!(result, ProcessResult::Handled(HandledKind::Quiet)));
}

// ============================================================================
// BUSY flag check (call / call_poll / send / urgent)
// ============================================================================

#[test]
fn test_call_auto_notifies_when_not_busy() {
    let ctx = TestContext::new();
    let mut ipi_sent = false;
    ctx.client.call(CalcService::ECHO, &99u32, || { ipi_sent = true; }).unwrap();
    assert!(ipi_sent);
}

#[test]
fn test_call_skips_notify_when_busy() {
    let ctx = TestContext::new();
    ctx._shm.set_busy();
    let mut ipi_sent = false;
    ctx.client.call(CalcService::ECHO, &99u32, || { ipi_sent = true; }).unwrap();
    assert!(!ipi_sent);
    ctx._shm.clear_busy();
}

#[test]
fn test_send_auto_notifies_when_not_busy() {
    let ctx = TestContext::new();
    let mut ipi_sent = false;
    ctx.client.send(CalcService::LOG, &42u32, || { ipi_sent = true; }).unwrap();
    assert!(ipi_sent);
}

#[test]
fn test_send_skips_notify_when_busy() {
    let ctx = TestContext::new();
    ctx._shm.set_busy();
    let mut ipi_sent = false;
    ctx.client.send(CalcService::LOG, &42u32, || { ipi_sent = true; }).unwrap();
    assert!(!ipi_sent);
    ctx._shm.clear_busy();
}

#[test]
fn test_urgent_auto_notifies_when_not_busy() {
    let ctx = TestContext::new();
    let mut ipi_sent = false;
    ctx.client.urgent(CalcService::STOP, &(), || { ipi_sent = true; }).unwrap();
    assert!(ipi_sent);
}

#[test]
fn test_urgent_skips_notify_when_busy() {
    let ctx = TestContext::new();
    ctx._shm.set_busy();
    let mut ipi_sent = false;
    ctx.client.urgent(CalcService::STOP, &(), || { ipi_sent = true; }).unwrap();
    assert!(!ipi_sent);
    ctx._shm.clear_busy();
}

// ============================================================================
// send (one-way)
// ============================================================================

#[test]
fn test_send_one_way() {
    let ctx = TestContext::new();
    unsafe { LAST_LOG = 0 };
    ctx.client.send(CalcService::LOG, &1234u32, || {}).unwrap();
    let result = ctx.server.process_one::<CalcService>();
    assert!(matches!(result, ProcessResult::Handled(HandledKind::OneWay)));
    assert_eq!(unsafe { LAST_LOG }, 1234);
}

#[test]
fn test_send_no_response_in_channel() {
    let mut ctx = TestContext::new();
    ctx.client.send(CalcService::LOG, &42u32, || {}).unwrap();
    ctx.server.process_one::<CalcService>();
    assert_eq!(ctx.client.poll_responses(), 0);
    assert_eq!(ctx.client.buffered(), 0);
}

// ============================================================================
// urgent
// ============================================================================

#[test]
fn test_urgent_stop() {
    let ctx = TestContext::new();
    unsafe { STOPPED = false };
    ctx.client.urgent(CalcService::STOP, &(), || {}).unwrap();
    let result = ctx.server.process_urgent::<CalcService>();
    assert!(matches!(result, ProcessResult::Handled(HandledKind::OneWay)));
    assert!(unsafe { STOPPED });
}

#[test]
fn test_urgent_channel_separate() {
    let ctx = TestContext::new();
    ctx.client.urgent(CalcService::STOP, &(), || {}).unwrap();
    // 普通通道应该为空
    assert!(matches!(
        ctx.server.process_one::<CalcService>(),
        ProcessResult::NoMessage
    ));
    // 急停通道有消息
    assert!(ctx.server.has_urgent());
    ctx.server.process_urgent::<CalcService>();
    // ov-channels ring buffer pending 标志需要额外 try_recv 清除
    ctx.server.process_urgent::<CalcService>();
    assert!(!ctx.server.has_urgent());
}

// ============================================================================
// ProcessResult / HandledKind
// ============================================================================

#[test]
fn test_process_result_no_message() {
    let ctx = TestContext::new();
    assert!(matches!(
        ctx.server.process_one::<CalcService>(),
        ProcessResult::NoMessage
    ));
    assert!(matches!(
        ctx.server.process_urgent::<CalcService>(),
        ProcessResult::NoMessage
    ));
}

#[test]
fn test_not_rpc() {
    let ctx = TestContext::new();
    let shm = unsafe { SharedMemory::at(ctx._shm as *const _ as usize) };
    let tx = shm.sender(ChannelId::new(0)).unwrap();
    tx.try_send(&Message::notification(99)).unwrap();

    let result = ctx.server.process_one::<CalcService>();
    match result {
        ProcessResult::NotRpc(msg) => assert_eq!(msg.as_notification(), Some(99)),
        other => panic!("expected NotRpc, got {:?}", other),
    }
}

// ============================================================================
// process_all
// ============================================================================

#[test]
fn test_process_all_mixed() {
    let ctx = TestContext::new();
    unsafe { LAST_LOG = 0 };
    let mut ctx = ctx;

    ctx.client.call(CalcService::ECHO, &10u32, || {}).unwrap();
    ctx.client.send(CalcService::LOG, &77u32, || {}).unwrap();
    ctx.client.call(CalcService::PING, &(), || {}).unwrap();

    let mut notify_count = 0;
    let count = ctx.server.process_all::<CalcService, _, _>(
        |_| {},
        || { notify_count += 1; },
    );
    assert_eq!(count, 3);
    // ECHO (Notify) + LOG (OneWay) + PING (Notify) = 2 notifies
    assert_eq!(notify_count, 2);
    assert_eq!(unsafe { LAST_LOG }, 77);

    ctx.client.poll_responses();
    let r1: u32 = ctx.client.recv().unwrap().unwrap();
    let r2: u32 = ctx.client.recv().unwrap().unwrap();
    assert_eq!(r1, 10);
    assert_eq!(r2, 42);
}

#[test]
fn test_process_all_with_urgent_first() {
    let ctx = TestContext::new();
    unsafe { STOPPED = false };
    ctx.client.call(CalcService::ECHO, &5u32, || {}).unwrap();
    ctx.client.urgent(CalcService::STOP, &(), || {}).unwrap();

    let count = ctx.server.process_all::<CalcService, _, _>(|_| {}, || {});
    assert_eq!(count, 2);
    assert!(unsafe { STOPPED });
}

#[test]
fn test_process_all_quiet_no_notify() {
    let ctx = TestContext::new();
    let ctx = ctx;

    // call_poll → Quiet, should NOT trigger notify
    ctx.client.call_poll(CalcService::ECHO, &10u32, || {}).unwrap();
    ctx.client.call_poll(CalcService::PING, &(), || {}).unwrap();

    let mut notify_count = 0;
    let count = ctx.server.process_all::<CalcService, _, _>(
        |_| {},
        || { notify_count += 1; },
    );
    assert_eq!(count, 2);
    assert_eq!(notify_count, 0); // No Notify-mode calls
}

// ============================================================================
// 多请求 + FIFO recv
// ============================================================================

#[test]
fn test_fifo_recv_order() {
    let mut ctx = TestContext::new();
    ctx.client.call(CalcService::ECHO, &10u32, || {}).unwrap();
    ctx.client.call(CalcService::ECHO, &20u32, || {}).unwrap();
    ctx.client.call(CalcService::ECHO, &30u32, || {}).unwrap();

    ctx.server.process_all::<CalcService, _, _>(|_| {}, || {});
    ctx.client.poll_responses();

    assert_eq!(ctx.client.recv::<u32>().unwrap().unwrap(), 10);
    assert_eq!(ctx.client.recv::<u32>().unwrap().unwrap(), 20);
    assert_eq!(ctx.client.recv::<u32>().unwrap().unwrap(), 30);
    assert!(ctx.client.recv::<u32>().unwrap().is_none());
}

#[test]
fn test_recv_for_out_of_order() {
    let mut ctx = TestContext::new();
    let rid1 = ctx.client.call(CalcService::ECHO, &10u32, || {}).unwrap();
    let rid2 = ctx.client.call(CalcService::ECHO, &20u32, || {}).unwrap();
    ctx.server.process_all::<CalcService, _, _>(|_| {}, || {});
    ctx.client.poll_responses();

    let val2: u32 = ctx.client.recv_for(rid2).unwrap().unwrap();
    assert_eq!(val2, 20);
    let val1: u32 = ctx.client.recv_for(rid1).unwrap().unwrap();
    assert_eq!(val1, 10);
}

// ============================================================================
// request_id 唯一性
// ============================================================================

#[test]
fn test_request_ids_are_unique() {
    let ctx = TestContext::new();
    let rid1 = ctx.client.call(CalcService::PING, &(), || {}).unwrap();
    let rid2 = ctx.client.call(CalcService::PING, &(), || {}).unwrap();
    let rid3 = ctx.client.call(CalcService::PING, &(), || {}).unwrap();
    assert_ne!(rid1, rid2);
    assert_ne!(rid2, rid3);
    assert_ne!(rid1, rid3);
}

// ============================================================================
// has_pending / has_urgent
// ============================================================================

#[test]
fn test_has_pending_and_urgent() {
    let ctx = TestContext::new();
    let _ = ctx.server.process_one::<CalcService>();
    assert!(!ctx.server.has_pending());
    assert!(!ctx.server.has_urgent());

    // client 用 TestContext 的 client (同一个 shm)
    // client 发到 CH0 (req_ch)
    let shm = unsafe { SharedMemory::at(ctx._shm as *const _ as usize) };
    shm.sender(ChannelId::new(0)).unwrap()
        .try_send(&Message::request(1, 0, &()).unwrap()).unwrap();
    assert!(ctx.server.has_pending());
    assert!(!ctx.server.has_urgent());

    shm.sender(ChannelId::new(2)).unwrap()
        .try_send(&Message::request(2, 6, &()).unwrap()).unwrap();
    assert!(ctx.server.has_urgent());
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
    assert_eq!(CalcService::LOG, 5);
    assert_eq!(CalcService::STOP, 6);
}

// ============================================================================
// 多服务共存
// ============================================================================

#[test]
fn test_multiple_services() {
    let mut ctx = TestContext::new();
    let rid_calc = ctx.client.call(CalcService::ADD, &(1i32, 2i32), || {}).unwrap();
    let rid_raw = ctx.client.call(RawService::IDENT, &0xDEAD_u64, || {}).unwrap();

    ctx.server.process_one::<CalcService>();
    ctx.server.process_one::<RawService>();

    ctx.client.poll_responses();
    let calc: i32 = ctx.client.recv_for(rid_calc).unwrap().unwrap();
    let raw: u64 = ctx.client.recv_for(rid_raw).unwrap().unwrap();
    assert_eq!(calc, 3);
    assert_eq!(raw, 0xDEAD);
}

// ============================================================================
// define_service_client! 类型安全客户端
// ============================================================================

use ov_rpc::define_service_client;

define_service_client! {
    TestRpc {
        ECHO:   0 => call echo(val: u32) -> u32;
        ADD:    1 => call add(a: i32, b: i32) -> i32;
        PING:   2 => call ping() -> u32;
        LOG:    5 => send log(msg: u32);
        STOP:   6 => urgent stop();
    }
}

#[test]
fn test_typed_client_echo() {
    let ctx = TestContext::new();
    let mut typed = TestRpc::new(ctx._shm as *const _ as usize);
    let rid = typed.echo(42u32, || {}).unwrap();
    ctx.server.process_one::<CalcService>();
    typed.poll_responses();
    let val: u32 = typed.recv_for(rid).unwrap().unwrap();
    assert_eq!(val, 42);
}

#[test]
fn test_typed_client_echo_poll() {
    let ctx = TestContext::new();
    let mut typed = TestRpc::new(ctx._shm as *const _ as usize);
    let rid = typed.echo_poll(99u32, || {}).unwrap();
    let result = ctx.server.process_one::<CalcService>();
    assert!(matches!(result, ProcessResult::Handled(HandledKind::Quiet)));
    typed.poll_responses();
    let val: u32 = typed.recv_for(rid).unwrap().unwrap();
    assert_eq!(val, 99);
}

#[test]
fn test_typed_client_add() {
    let ctx = TestContext::new();
    let mut typed = TestRpc::new(ctx._shm as *const _ as usize);
    let rid = typed.add(3i32, 4i32, || {}).unwrap();
    ctx.server.process_one::<CalcService>();
    typed.poll_responses();
    let val: i32 = typed.recv_for(rid).unwrap().unwrap();
    assert_eq!(val, 7);
}

#[test]
fn test_typed_client_add_poll() {
    let ctx = TestContext::new();
    let mut typed = TestRpc::new(ctx._shm as *const _ as usize);
    let rid = typed.add_poll(10i32, 20i32, || {}).unwrap();
    ctx.server.process_one::<CalcService>();
    typed.poll_responses();
    let val: i32 = typed.recv_for(rid).unwrap().unwrap();
    assert_eq!(val, 30);
}

#[test]
fn test_typed_client_ping() {
    let ctx = TestContext::new();
    let mut typed = TestRpc::new(ctx._shm as *const _ as usize);
    let rid = typed.ping(|| {}).unwrap();
    ctx.server.process_one::<CalcService>();
    typed.poll_responses();
    let val: u32 = typed.recv_for(rid).unwrap().unwrap();
    assert_eq!(val, 42);
}

#[test]
fn test_typed_client_send() {
    let ctx = TestContext::new();
    unsafe { LAST_LOG = 0 };
    let typed = TestRpc::new(ctx._shm as *const _ as usize);
    typed.log(1234u32, || {}).unwrap();
    let result = ctx.server.process_one::<CalcService>();
    assert!(matches!(result, ProcessResult::Handled(HandledKind::OneWay)));
    assert_eq!(unsafe { LAST_LOG }, 1234);
}

#[test]
fn test_typed_client_urgent() {
    let ctx = TestContext::new();
    unsafe { STOPPED = false };
    let typed = TestRpc::new(ctx._shm as *const _ as usize);
    typed.stop(|| {}).unwrap();
    let result = ctx.server.process_urgent::<CalcService>();
    assert!(matches!(result, ProcessResult::Handled(HandledKind::OneWay)));
    assert!(unsafe { STOPPED });
}

#[test]
fn test_typed_client_deref() {
    let ctx = TestContext::new();
    let mut typed = TestRpc::new(ctx._shm as *const _ as usize);
    // poll_responses and recv_for come from Deref to RpcClient
    let rid = typed.echo(5u32, || {}).unwrap();
    ctx.server.process_one::<CalcService>();
    assert_eq!(typed.buffered(), 0);
    typed.poll_responses();
    let val: u32 = typed.recv_for(rid).unwrap().unwrap();
    assert_eq!(val, 5);
}
