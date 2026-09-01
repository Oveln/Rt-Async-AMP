//! 服务发现测试：DESCRIPTOR const 生成、INIT（method 0）拦截、
//! discover 往返（单块与多块描述符两段）、默认空描述符。

use ov_channels::SharedMemory;
use ov_rpc::{
    define_service, descriptor, HandledKind, ProcessResult, RpcClient, RpcHandler, RpcServer,
};

define_service! {
    pub SmallService {
        ECHO:  1 => call echo(val: u32) -> u32;
        ADD:   2 => call add(a: i32, b: i32) -> i32;
        LOG:   3 => send log(msg: u32);
        STOP:  4 => urgent stop();
        KICK:  5 => acall kick(nonce: u32);
    }
}

impl SmallService {
    fn echo(val: u32) -> u32 { val }
    fn add(a: i32, b: i32) -> i32 { a.wrapping_add(b) }
    fn log(_msg: u32) {}
    fn stop() {}
    fn kick(_rid: u64, _nonce: u32) {}
}

// 长名字方法表：描述符 > 255B，INIT 响应必须走多块流帧。
define_service! {
    pub LongNameService {
        SENSOR_READ_TEMPERATURE_WITH_CALIBRATION_OFFSET_AAAA: 1 => call srt_a(n: u32) -> u32;
        SENSOR_READ_PRESSURE_WITH_TEMPERATURE_COMPENSATION_BB: 2 => call srp_b(n: u32) -> u32;
        ACTUATOR_SET_SERVO_ANGLE_WITH_VELOCITY_LIMIT_CCCCCC:   3 => call ass_c(n: u32) -> u32;
        TELEMETRY_STREAM_CHUNKED_TRANSFER_DIAGNOSTIC_DDDDDDDD: 4 => call tst_d(n: u32) -> u32;
        WATCHDOG_FEED_WITH_DEADLINE_MONITORING_EEEEEEEEEEEEEE: 5 => call wdf_e(n: u32) -> u32;
        CONFIG_BLOB_EXPORT_INCLUDES_ALL_FIELDS_FFFFFFFFFFFFFF: 6 => call cbe_f(n: u32) -> u32;
    }
}

impl LongNameService {
    fn srt_a(n: u32) -> u32 { n }
    fn srp_b(n: u32) -> u32 { n }
    fn ass_c(n: u32) -> u32 { n }
    fn tst_d(n: u32) -> u32 { n }
    fn wdf_e(n: u32) -> u32 { n }
    fn cbe_f(n: u32) -> u32 { n }
}

/// 手写 impl（无 descriptor 覆盖）：INIT 应得到默认空描述符。
struct Manual;
impl RpcHandler for Manual {
    fn handle(
        _method: u64,
        _msg: ov_channels::Message,
        _out: &mut ov_rpc::Response,
    ) -> Result<ov_rpc::Reply, ov_rpc::RpcError> {
        Ok(ov_rpc::Reply::Silent)
    }
}

struct Ctx {
    shm: &'static SharedMemory<3>,
    server: RpcServer,
    client: RpcClient,
}

impl Ctx {
    fn new() -> Self {
        let shm = Box::leak(Box::new(SharedMemory::<3>::new()));
        shm.init();
        let addr = shm as *const _ as usize;
        Self {
            shm,
            server: RpcServer::new(addr),
            client: RpcClient::new(addr),
        }
    }
}

/// const 生成的描述符与宏方法表逐项一致（单一真相源校验）。
#[test]
fn descriptor_const_matches_table() {
    let d = descriptor::parse(SmallService::DESCRIPTOR).expect("DESCRIPTOR 必须可解析");
    assert_eq!(d.proto(), descriptor::PROTOCOL_VERSION);
    let ms: Vec<_> = d.methods().collect();
    assert_eq!(ms.len(), 5);
    assert_eq!(ms[0].name, "ECHO");
    assert_eq!(ms[0].mid, 1);
    assert_eq!(ms[0].kind_name(), "call");
    assert_eq!(ms[1].name, "ADD");
    assert_eq!(ms[2].name, "LOG");
    assert!(ms[2].is_one_way() && !ms[2].is_urgent());
    assert_eq!(ms[2].kind_name(), "send");
    assert_eq!(ms[3].name, "STOP");
    assert!(ms[3].is_urgent());
    assert_eq!(ms[3].kind_name(), "urgent");
    assert_eq!(ms[4].name, "KICK");
    assert!(ms[4].is_deferred());
    assert_eq!(ms[4].kind_name(), "acall");
}

/// discover 往返（单块描述符，≤247B）。
#[test]
fn discover_roundtrip_single_block() {
    let mut ctx = Ctx::new();
    let rid = ctx.client.discover(|| {}).unwrap();
    assert!(matches!(
        ctx.server.process_one::<SmallService>(),
        ProcessResult::Handled(HandledKind::Notify)
    ));
    // 单块：CH1 恰 1 块（描述符 ~50B）
    assert_eq!(
        ctx.shm
            .receiver(ov_channels::ChannelId::new(1))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(ctx.client.poll_responses(), 1);
    let bytes = ctx.client.recv_raw_for(rid).expect("INIT 响应应已缓冲");
    // 单块存储带零填充（定长 255B − rid 8B），描述符按自带 desc_len 截取
    assert!(bytes.starts_with(SmallService::DESCRIPTOR));
    let d = descriptor::parse(bytes).expect("描述符应可解析");
    assert_eq!(d.method_count(), 5);
}

/// discover 往返（多块描述符，>255B 流帧路径）。
#[test]
fn discover_roundtrip_multi_block() {
    let mut ctx = Ctx::new();
    assert!(LongNameService::DESCRIPTOR.len() > 255, "长名字表必须跨块");

    let rid = ctx.client.discover(|| {}).unwrap();
    assert!(matches!(
        ctx.server.process_one::<LongNameService>(),
        ProcessResult::Handled(HandledKind::Notify)
    ));
    assert!(ctx.client.poll_responses() >= 1);
    let bytes = ctx.client.recv_raw_for(rid).expect("INIT 响应应已缓冲");
    assert_eq!(bytes.len(), LongNameService::DESCRIPTOR.len());
    assert_eq!(bytes, LongNameService::DESCRIPTOR);
    let d = descriptor::parse(bytes).unwrap();
    assert_eq!(d.method_count(), 6);
    assert_eq!(
        d.methods().next().unwrap().name,
        "SENSOR_READ_TEMPERATURE_WITH_CALIBRATION_OFFSET_AAAA"
    );
}

/// discover 与普通调用交错：raw 收取不影响类型化解码。
#[test]
fn discover_interleaved_with_calls() {
    let mut ctx = Ctx::new();
    let rid_disc = ctx.client.discover(|| {}).unwrap();
    let rid_echo = ctx.client.call(SmallService::ECHO, &7u32, || {}).unwrap();
    ctx.server.process_one::<SmallService>();
    ctx.server.process_one::<SmallService>();
    assert_eq!(ctx.client.poll_responses(), 2);

    let bytes = ctx.client.recv_raw_for(rid_disc).unwrap();
    assert_eq!(descriptor::parse(bytes).unwrap().method_count(), 5);
    assert_eq!(ctx.client.recv_for::<u32>(rid_echo), Ok(Some(7)));
}

/// 手写 handler 未覆盖 descriptor()：INIT 得到默认空描述符（可解析、
/// 0 方法），调用方据此识别"服务端不提供发现"。
#[test]
fn manual_handler_gets_empty_descriptor() {
    let mut ctx = Ctx::new();
    let rid = ctx.client.discover(|| {}).unwrap();
    assert!(matches!(
        ctx.server.process_one::<Manual>(),
        ProcessResult::Handled(HandledKind::Notify)
    ));
    ctx.client.poll_responses();
    let bytes = ctx.client.recv_raw_for(rid).unwrap();
    let d = descriptor::parse(bytes).expect("空描述符格式必须合法");
    assert_eq!(d.method_count(), 0);
}
