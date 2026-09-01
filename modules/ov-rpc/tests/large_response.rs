//! 大响应测试：单块/多块流帧/超限三段边界（依赖 ov-channels 0.3 块层）。
//!
//! 线格式约定（v0.2.0）：
//! - 载荷（rid 8B + postcard 结果）≤ 255B → **单块**，与 0.2.x 逐字节相同；
//! - 255B < 载荷 ≤ 2036B → 字节流帧 `[len u32][载荷]`，2..=8 块原子发布；
//! - 载荷 > 2036B → `RpcError::ResponseTooLarge` → poison 响应。
//!
//! 测试返回类型用 `Vec<u8>`（postcard 编码 = len varint + 原始字节，
//! 长度可精确推算，便于卡边界）。

use ov_channels::{stream, ChannelId, SharedMemory};
use ov_rpc::{define_service, HandledKind, ProcessResult, RecvError, RpcClient, RpcServer};

define_service! {
    pub BlobService {
        BLOB:  1 => call blob(n: u32) -> Vec<u8>;
        SMALL: 2 => call small(v: u32) -> u32;
    }
}

impl BlobService {
    fn blob(n: u32) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }
    fn small(v: u32) -> u32 { v }
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

    /// CH1 当前块数（响应通道的块计数语义）。
    fn resp_blocks(&self) -> usize {
        self.shm.receiver(ChannelId::new(1)).unwrap().len()
    }
}

/// postcard varint 字节数（7bit/组 + 续传位）。
fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 128 {
        v >>= 7;
        n += 1;
    }
    n
}

/// 一次 BLOB(n) 往返：块数断言 + 内容逐字节校验。
/// 期望块数：载荷 8+varint(n)+n ≤ 255 → 1；否则流帧 block_count(载荷)。
fn roundtrip(ctx: &mut Ctx, n: u32) {
    let rid = ctx.client.call(BlobService::BLOB, &n, || {}).unwrap();
    assert!(matches!(
        ctx.server.process_one::<BlobService>(),
        ProcessResult::Handled(HandledKind::Notify)
    ));

    let payload_len = 8 + varint_len(n as u64) + n as usize;
    let expect_blocks = if payload_len <= 255 {
        1
    } else {
        stream::block_count(payload_len).expect("边界内必可成帧")
    };
    assert_eq!(ctx.resp_blocks(), expect_blocks, "n={n} 载荷 {payload_len}B");

    assert_eq!(ctx.client.poll_responses(), 1);
    let v: Vec<u8> = ctx.client.recv_for(rid).unwrap().unwrap();
    assert_eq!(v.len(), n as usize, "n={n}");
    for (i, b) in v.iter().enumerate() {
        assert_eq!(*b, (i % 251) as u8, "n={n} byte {i}");
    }
}

#[test]
fn single_block_small() {
    let mut ctx = Ctx::new();
    roundtrip(&mut ctx, 10); // 载荷 19B，1 块
}

/// 恰好卡单块上界：载荷 = 8 + varint(245)=2 + 245 = 255B。
#[test]
fn single_block_boundary_255() {
    let mut ctx = Ctx::new();
    roundtrip(&mut ctx, 245);
}

/// 恰好跨入流帧：载荷 256B → 帧 260B → 2 块。
#[test]
fn stream_boundary_256() {
    let mut ctx = Ctx::new();
    roundtrip(&mut ctx, 246);
}

/// 流帧上界：载荷 = 8 + varint(2026)=2 + 2026 = 2036B → 帧 2040B → 恰 8 块。
#[test]
fn stream_max_8_blocks() {
    let mut ctx = Ctx::new();
    roundtrip(&mut ctx, 2026);
}

/// 超过流帧上界：ResponseTooLarge → poison。
#[test]
fn over_limit_gets_poison() {
    let mut ctx = Ctx::new();
    let rid = ctx.client.call(BlobService::BLOB, &2027u32, || {}).unwrap();
    assert!(matches!(
        ctx.server.process_one::<BlobService>(),
        ProcessResult::Unhandled(1)
    ));
    assert_eq!(ctx.client.poll_responses(), 1);
    assert_eq!(
        ctx.client.recv_for::<Vec<u8>>(rid),
        Err(RecvError::DeserializeFailed)
    );
}

/// 小响应与大响应交错：FIFO 顺序与内容互不干扰（覆盖客户端缓冲归一化
/// 的两条路径交替）。
#[test]
fn interleaved_single_and_stream() {
    let mut ctx = Ctx::new();
    let rid_s = ctx.client.call(BlobService::SMALL, &1234u32, || {}).unwrap();
    let rid_l = ctx.client.call(BlobService::BLOB, &600u32, || {}).unwrap();
    let rid_s2 = ctx.client.call(BlobService::SMALL, &5678u32, || {}).unwrap();
    ctx.server.process_one::<BlobService>();
    ctx.server.process_one::<BlobService>();
    ctx.server.process_one::<BlobService>();

    assert_eq!(ctx.client.poll_responses(), 3);
    assert_eq!(ctx.client.recv_for::<u32>(rid_s), Ok(Some(1234)));
    let big: Vec<u8> = ctx.client.recv_for(rid_l).unwrap().unwrap();
    assert_eq!(big.len(), 600);
    assert_eq!(&big[..8], &[0, 1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(ctx.client.recv_for::<u32>(rid_s2), Ok(Some(5678)));
}

/// 连续多条大响应排满响应环：块计数按消息累加、逐条取回完整
/// （验证多块原子发布在批处理下的隔离性）。
#[test]
fn consecutive_large_responses() {
    let mut ctx = Ctx::new();
    let mut rids = Vec::new();
    for i in 1..=5u32 {
        rids.push(ctx.client.call(BlobService::BLOB, &(i * 300), || {}).unwrap());
    }
    for _ in 0..5 {
        ctx.server.process_one::<BlobService>();
    }
    // 5 条流帧响应各 2 块（载荷 ~908B → 帧 912B → 4 块；只断言非零）
    assert!(ctx.resp_blocks() >= 5);
    assert_eq!(ctx.client.poll_responses(), 5);
    for (i, rid) in rids.iter().enumerate() {
        let v: Vec<u8> = ctx.client.recv_for(*rid).unwrap().unwrap();
        assert_eq!(v.len(), (i + 1) * 300);
    }
}
