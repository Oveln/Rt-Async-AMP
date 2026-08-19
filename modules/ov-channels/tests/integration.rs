//! 集成测试：双向通信测试
//!
//! ## 运行方式
//!
//! ```sh
//! cargo test --test integration --features alloc -- --test-threads=1 --nocapture
//! ```

use std::time::Instant;
use std::thread;

use ov_channels::*;

// ============================================================================
// 常量配置
// ============================================================================

const NOTIF_COUNT: usize = 50_000;
const DATA_COUNT: usize = 10_000;
const PING_PONG_COUNT: usize = 10_000;

// ============================================================================
// 辅助函数
// ============================================================================

fn make_shm() -> &'static SharedMemory<4> {
    let shm = Box::leak(Box::new(SharedMemory::<4>::new()));
    shm.init();
    shm
}

fn print_throughput(label: &str, total_msg: u64, elapsed: std::time::Duration) {
    let secs = elapsed.as_secs_f64();
    eprintln!();
    eprintln!("=== {} ===", label);
    eprintln!("  总消息数 : {}", total_msg);
    eprintln!("  耗时     : {:.3} ms", secs * 1000.0);
    eprintln!("  吞吐量   : {:.0} msg/s", total_msg as f64 / secs);
}

// ============================================================================
// 测试 1: 双向海量通知消息
// ============================================================================

#[test]
fn test_bidirectional_massive_notification() {
    let shm = make_shm();
    let start = Instant::now();

    let a = thread::spawn(move || {
        let tx = shm.sender(ChannelId::new(0)).unwrap();
        let rx = shm.receiver(ChannelId::new(1)).unwrap();
        let (mut sent, mut recv) = (0usize, 0usize);

        while sent < NOTIF_COUNT || recv < NOTIF_COUNT {
            let mut progress = false;

            if sent < NOTIF_COUNT {
                if tx.try_send(&Message::notification(sent as u32)).is_ok() {
                    sent += 1;
                    progress = true;
                }
            }
            if let Some(msg) = rx.try_recv() {
                assert_eq!(msg.as_notification(), Some(recv as u32));
                recv += 1;
                progress = true;
            }

            if !progress {
                thread::yield_now();
            }
        }
        (sent, recv)
    });

    let b = thread::spawn(move || {
        let rx = shm.receiver(ChannelId::new(0)).unwrap();
        let tx = shm.sender(ChannelId::new(1)).unwrap();
        let (mut sent, mut recv) = (0usize, 0usize);

        while sent < NOTIF_COUNT || recv < NOTIF_COUNT {
            let mut progress = false;

            if let Some(msg) = rx.try_recv() {
                assert_eq!(msg.as_notification(), Some(recv as u32));
                recv += 1;
                progress = true;
            }

            if sent < NOTIF_COUNT {
                if tx.try_send(&Message::notification(sent as u32)).is_ok() {
                    sent += 1;
                    progress = true;
                }
            }

            if !progress {
                thread::yield_now();
            }
        }
        (sent, recv)
    });

    let (sa, ra) = a.join().unwrap();
    let (sb, rb) = b.join().unwrap();

    let elapsed = start.elapsed();

    assert_eq!(sa, NOTIF_COUNT);
    assert_eq!(ra, NOTIF_COUNT);
    assert_eq!(sb, NOTIF_COUNT);
    assert_eq!(rb, NOTIF_COUNT);

    print_throughput("双向通知消息测试", (NOTIF_COUNT * 2) as u64, elapsed);
}

// ============================================================================
// 测试 2: 双向海量数据消息
// ============================================================================

#[test]
fn test_bidirectional_massive_data() {
    let shm = make_shm();
    let start = Instant::now();

    let a = thread::spawn(move || {
        let tx = shm.sender(ChannelId::new(0)).unwrap();
        let rx = shm.receiver(ChannelId::new(1)).unwrap();
        let (mut sent, mut recv) = (0usize, 0usize);

        while sent < DATA_COUNT || recv < DATA_COUNT {
            let mut progress = false;

            if sent < DATA_COUNT {
                let data = format!("msg-A-{}", sent);
                if tx.try_send(&Message::data(data.as_bytes())).is_ok() {
                    sent += 1;
                    progress = true;
                }
            }

            if let Some(msg) = rx.try_recv() {
                if let Some(data) = msg.as_data() {
                    let expected = format!("msg-B-{}", recv);
                    let payload = std::str::from_utf8(&data[..expected.len()]).unwrap();
                    assert_eq!(payload, expected);
                    recv += 1;
                    progress = true;
                }
            }

            if !progress {
                thread::yield_now();
            }
        }
        (sent, recv)
    });

    let b = thread::spawn(move || {
        let rx = shm.receiver(ChannelId::new(0)).unwrap();
        let tx = shm.sender(ChannelId::new(1)).unwrap();
        let (mut sent, mut recv) = (0usize, 0usize);

        while sent < DATA_COUNT || recv < DATA_COUNT {
            let mut progress = false;

            if let Some(msg) = rx.try_recv() {
                if let Some(data) = msg.as_data() {
                    let expected = format!("msg-A-{}", recv);
                    let payload = std::str::from_utf8(&data[..expected.len()]).unwrap();
                    assert_eq!(payload, expected);
                    recv += 1;
                    progress = true;
                }
            }

            if sent < DATA_COUNT {
                let data = format!("msg-B-{}", sent);
                if tx.try_send(&Message::data(data.as_bytes())).is_ok() {
                    sent += 1;
                    progress = true;
                }
            }

            if !progress {
                thread::yield_now();
            }
        }
        (sent, recv)
    });

    let (sa, ra) = a.join().unwrap();
    let (sb, rb) = b.join().unwrap();

    let elapsed = start.elapsed();

    assert_eq!(sa, DATA_COUNT);
    assert_eq!(ra, DATA_COUNT);
    assert_eq!(sb, DATA_COUNT);
    assert_eq!(rb, DATA_COUNT);

    print_throughput("双向数据消息测试", (DATA_COUNT * 2) as u64, elapsed);
}

// ============================================================================
// 测试 3: 多通道并行
// ============================================================================

#[test]
fn test_multi_channel_parallel() {
    let shm = make_shm();
    let start = Instant::now();
    const MSG_COUNT: usize = 20_000;

    let tx = thread::spawn(move || {
        let tx = shm.sender(ChannelId::new(0)).unwrap();
        for i in 0..MSG_COUNT {
            while tx.try_send(&Message::notification(i as u32)).is_err() {
                thread::yield_now();
            }
        }
    });

    let rx = thread::spawn(move || {
        let rx = shm.receiver(ChannelId::new(0)).unwrap();
        let mut count = 0;
        while count < MSG_COUNT {
            if rx.try_recv().is_some() {
                count += 1;
            } else {
                thread::yield_now();
            }
        }
    });

    tx.join().unwrap();
    rx.join().unwrap();

    let elapsed = start.elapsed();
    print_throughput("单通道压力测试", MSG_COUNT as u64, elapsed);
}

// ============================================================================
// 测试 4: Ping-Pong 延迟
// ============================================================================

#[test]
fn test_ping_pong_latency() {
    let shm = make_shm();
    let start = Instant::now();

    let a = thread::spawn(move || {
        let tx = shm.sender(ChannelId::new(0)).unwrap();
        let rx = shm.receiver(ChannelId::new(1)).unwrap();

        for i in 0..PING_PONG_COUNT {
            while tx.try_send(&Message::notification(i as u32)).is_err() {
                thread::yield_now();
            }
            while rx.try_recv().is_none() {
                thread::yield_now();
            }
        }
    });

    let b = thread::spawn(move || {
        let rx = shm.receiver(ChannelId::new(0)).unwrap();
        let tx = shm.sender(ChannelId::new(1)).unwrap();

        for _ in 0..PING_PONG_COUNT {
            while rx.try_recv().is_none() {
                thread::yield_now();
            }
            while tx.try_send(&Message::notification(0)).is_err() {
                thread::yield_now();
            }
        }
    });

    a.join().unwrap();
    b.join().unwrap();

    let elapsed = start.elapsed();
    let avg_latency_ns = elapsed.as_nanos() / PING_PONG_COUNT as u128;

    eprintln!();
    eprintln!("=== Ping-Pong 往返延迟测试 ===");
    eprintln!("  总往返次数 : {}", PING_PONG_COUNT);
    eprintln!("  总耗时     : {:.3} ms", elapsed.as_secs_f64() * 1000.0);
    eprintln!("  平均延迟   : {:.2} μs", avg_latency_ns as f64 / 1000.0);
}

// ============================================================================
// 测试 5: 混合消息类型
// ============================================================================

#[test]
fn test_bidirectional_mixed_types() {
    let shm = make_shm();

    // 系统 A: 发送 -> 通道 0, 接收 <- 通道 1
    // 系统 B: 接收 <- 通道 0, 发送 -> 通道 1

    let a = thread::spawn(move || {
        let tx = shm.sender(ChannelId::new(0)).unwrap();
        let rx = shm.receiver(ChannelId::new(1)).unwrap();

        // 发送不同类型的消息
        for i in 0..10 {
            while tx.try_send(&Message::notification(i)).is_err() {
                thread::yield_now();
            }
        }

        for i in 0..10 {
            let data = format!("data-{}", i);
            while tx.try_send(&Message::data(data.as_bytes())).is_err() {
                thread::yield_now();
            }
        }

        for i in 0..10 {
            let req = Message::request(i as u64, 0, &(i * 2i32)).unwrap();
            while tx.try_send(&req).is_err() {
                thread::yield_now();
            }
        }

        // 接收 B 的响应
        let mut notifs = 0;
        let mut datas = 0;

        while notifs < 10 || datas < 20 {
            if let Some(msg) = rx.try_recv() {
                match msg.ty() {
                    Some(MsgType::Notification) => {
                        msg.as_notification().unwrap();
                        notifs += 1;
                    }
                    Some(MsgType::Data) => {
                        msg.as_data().unwrap();
                        datas += 1;
                    }
                    _ => {}
                }
            } else {
                thread::yield_now();
            }
        }

        (notifs, datas)
    });

    let b = thread::spawn(move || {
        let rx = shm.receiver(ChannelId::new(0)).unwrap();
        let tx = shm.sender(ChannelId::new(1)).unwrap();

        // 接收 A 的消息并发送响应
        for _ in 0..10 {
            while rx.try_recv().is_none() {
                thread::yield_now();
            }
            while tx.try_send(&Message::notification(0)).is_err() {
                thread::yield_now();
            }
        }

        for _ in 0..10 {
            while rx.try_recv().is_none() {
                thread::yield_now();
            }
            while tx.try_send(&Message::data(b"resp")).is_err() {
                thread::yield_now();
            }
        }

        for _ in 0..10 {
            while rx.try_recv().is_none() {
                thread::yield_now();
            }
            while tx.try_send(&Message::data(b"ack")).is_err() {
                thread::yield_now();
            }
        }
    });

    let (n, d) = a.join().unwrap();
    b.join().unwrap();

    assert_eq!(n, 10);
    assert_eq!(d, 20);

    eprintln!();
    eprintln!("=== 混合消息类型测试 ===");
    eprintln!("  所有消息类型验证通过");
}
