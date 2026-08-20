//! stamps 手写展开 try_recv 的布局/语义对账（host 单测）。
//!
//! 背景（2026-08-20 板上实锤）：server.rs 在 feature "stamps" 下用裸指针
//! 手写展开 `Channel::try_recv`，槽区偏移曾按 buffer@0x10 误算（实际
//! `Message align(256)` 垫到 +0x100，真相源 `ov_channels::RB_SLOTS_OFF`），
//! 消息错位致 NotRpc 回显死等。本测试把同一裸指针公式对 host 内存里的
//! 真实 `SharedMemory` 跑一遍：取出的消息须与库 API 逐字节等价、槽地址
//! 公式须命中消息 kind 字段——布局再漂移（或公式再错）立即在此失败，
//! 而不是板上卡死。
//!
//! 运行：`cargo test -p ov-rpc --features stamps`。

use core::sync::atomic::{AtomicU16, AtomicUsize, Ordering};

use ov_channels::{ChannelId, Message, SharedMemory, MAGIC};
use ov_rpc::RpcServer;

/// 与 server.rs 手写展开完全同构的裸指针取包（公式复刻，勿"简化"）。
fn unrolled_try_recv(shm: &SharedMemory<3>, ch: ChannelId) -> Option<Message> {
    let base = unsafe { shm.channel_unchecked(ch) as *const ov_channels::Channel as usize };
    // SAFETY: 测试内 base 来自本测试构造的 SharedMemory 引用。
    unsafe {
        let magic = &*(base as *const AtomicU16);
        if magic.load(Ordering::Acquire) != MAGIC {
            return None;
        }
        let rb = (base + 0x100) as *const AtomicUsize;
        let read = (*rb).load(Ordering::Acquire);
        let write = (*rb.add(1)).load(Ordering::Acquire);
        if read == write {
            return None;
        }
        let slot = (base + 0x100 + ov_channels::RB_SLOTS_OFF
            + read * core::mem::size_of::<Message>()) as *const Message;
        let m = slot.read_volatile();
        (*rb).store((read + 1) % ov_channels::CHANNEL_CAPACITY, Ordering::Release);
        Some(m)
    }
}

#[test]
fn unroll_matches_library_on_real_layout() {
    // flags 开（与固件/bench 同 feature 面），busy 头后 channels 按 256 对齐。
    let shm: &'static SharedMemory<3> = Box::leak(Box::new(SharedMemory::new()));
    shm.init();

    // 库路径发两条不同请求（不同槽），手写展开逐一取出对账。
    let tx = shm.sender(ChannelId::new(0)).unwrap();
    let m0 = Message::request(0xAA00, 3, &(1u64,)).unwrap();
    let m1 = Message::request(0xAA01, 3, &(2u64,)).unwrap();
    tx.try_send(&m0).unwrap();
    tx.try_send(&m1).unwrap();

    let r0 = unrolled_try_recv(shm, ChannelId::new(0)).expect("手写展开应取出第 1 条");
    let r1 = unrolled_try_recv(shm, ChannelId::new(0)).expect("手写展开应取出第 2 条");
    assert_eq!(r0, m0, "槽 0：手写展开取出的消息与库写入不等价（布局错位？）");
    assert_eq!(r1, m1, "槽 1：手写展开取出的消息与库写入不等价（布局错位？）");

    // 取空后应返回 None 且不推进索引。
    assert!(unrolled_try_recv(shm, ChannelId::new(0)).is_none());
    // 库 API 视角：read 索引应恰好推进 2（手写展开的 Release store 落账）。
    let rx = shm.receiver(ChannelId::new(0)).unwrap();
    assert!(!rx.has_pending(), "手写展开应已消费全部消息");
}

#[test]
fn unroll_in_process_channel_dispatches() {
    // 端到端：库写入 → RpcServer（stamps 构建下手写展开取包）→ dispatch。
    // handle 返回 None + 非 one_way → Unhandled，但消息消费本身即证明
    // 取包路径正确（错误布局下消息错位、method_id 必然解析失败/NotRpc）。
    let shm: &'static SharedMemory<3> = Box::leak(Box::new(SharedMemory::new()));
    shm.init();
    let tx = shm.sender(ChannelId::new(0)).unwrap();
    let req = Message::request(0xBB01, 3, &(7u64,)).unwrap();
    tx.try_send(&req).unwrap();

    struct Echo;
    impl ov_rpc::RpcHandler for Echo {
        fn handle(
            method: u64,
            msg: Message,
        ) -> Result<Option<Message>, ov_rpc::DeserializeFailed> {
            let (rid, _mid, a): (u64, u64, (u64,)) = msg.as_request().unwrap();
            assert_eq!(method, 3, "method 应被正确剥离");
            assert_eq!(rid, 0xBB01);
            assert_eq!(a, (7,));
            Ok(None) // 未注册方法 → Unhandled
        }
    }

    let srv = RpcServer::new(shm as *const _ as usize);
    let r = srv.process_one::<Echo>();
    match r {
        ov_rpc::ProcessResult::Unhandled(3) => {} // 预期：消费+dispatch 成功
        other => panic!("process_one 应消费并 dispatch（手写展开取包正确），得到 {other:?}"),
    }
}
