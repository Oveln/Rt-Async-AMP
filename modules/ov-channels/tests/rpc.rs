//! RPC 集成测试：模拟服务端和客户端

use std::thread;
use std::time::Instant;

use ov_channels::{ChannelId, Message};

extern crate alloc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

// ============================================================================
// 常量配置
// ============================================================================

const RPC_CALL_COUNT: usize = 1_000;

const METHOD_ADD: u64 = 0;
const METHOD_SUBTRACT: u64 = 1;
const METHOD_MULTIPLY: u64 = 2;
const METHOD_CONCAT: u64 = 3;
const METHOD_GET_ARRAY: u64 = 4;

// ============================================================================
// 辅助函数
// ============================================================================

fn make_shm() -> &'static ov_channels::SharedMemory<4> {
    let shm = Box::leak(Box::new(ov_channels::SharedMemory::<4>::new()));
    shm.init();
    shm
}

/// 模拟服务端：接收请求并返回响应
fn simulate_server(shm: &'static ov_channels::SharedMemory<4>, num_calls: usize) {
    let req_rx = shm.receiver(ChannelId::new(0)).unwrap();
    let resp_tx = shm.sender(ChannelId::new(1)).unwrap();
    let mut handled = 0;

    while handled < num_calls {
        if let Some(req_msg) = req_rx.try_recv() {
            // 先获取 method_id
            let Some(method_id) = req_msg.method_id() else {
                continue;
            };

            // 根据 method_id 反序列化参数
            match method_id {
                METHOD_ADD | METHOD_SUBTRACT | METHOD_MULTIPLY => {
                    let Some((request_id, _, (a, b))) = req_msg.as_request::<(i32, i32)>() else {
                        continue;
                    };
                    let resp = match method_id {
                        METHOD_ADD => Message::response(request_id, &(a + b)).unwrap(),
                        METHOD_SUBTRACT => Message::response(request_id, &(a - b)).unwrap(),
                        METHOD_MULTIPLY => Message::response(request_id, &(a * b)).unwrap(),
                        _ => unreachable!(),
                    };
                    while resp_tx.try_send(&resp).is_err() { thread::yield_now(); }
                    handled += 1;
                }
                METHOD_CONCAT => {
                    let Some((request_id, _, (a, b))) = req_msg.as_request::<(String, String)>() else {
                        continue;
                    };
                    let result = alloc::format!("{}{}", a, b);
                    let resp = Message::response(request_id, &result).unwrap();
                    while resp_tx.try_send(&resp).is_err() { thread::yield_now(); }
                    handled += 1;
                }
                METHOD_GET_ARRAY => {
                    let Some((request_id, _, (len, value))) = req_msg.as_request::<(usize, u8)>() else {
                        continue;
                    };
                    let arr = vec![value; len];
                    let resp = Message::response(request_id, &arr).unwrap();
                    while resp_tx.try_send(&resp).is_err() { thread::yield_now(); }
                    handled += 1;
                }
                _ => {}
            }
        } else {
            thread::yield_now();
        }
    }
}

/// 模拟客户端：发送请求并接收响应
fn simulate_client(shm: &'static ov_channels::SharedMemory<4>, num_calls: usize) {
    let req_tx = shm.sender(ChannelId::new(0)).unwrap();
    let resp_rx = shm.receiver(ChannelId::new(1)).unwrap();

    for i in 0..num_calls {
        let request_id = i as u64;
        let method_id = i % 5;

        let req_msg = match method_id {
            0 => Message::request(request_id, METHOD_ADD, &(42i32, 99i32)).unwrap(),
            1 => Message::request(request_id, METHOD_SUBTRACT, &(100i32, 37i32)).unwrap(),
            2 => Message::request(request_id, METHOD_MULTIPLY, &(7i32, 6i32)).unwrap(),
            3 => Message::request(request_id, METHOD_CONCAT, &(String::from("hello"), String::from("world"))).unwrap(),
            4 => Message::request(request_id, METHOD_GET_ARRAY, &(5usize, 42u8)).unwrap(),
            _ => unreachable!(),
        };

        while req_tx.try_send(&req_msg).is_err() {
            thread::yield_now();
        }

        loop {
            if let Some(resp_msg) = resp_rx.try_recv() {
                match method_id {
                    0 => {
                        let (rid, result): (u64, i32) = resp_msg.as_response().unwrap();
                        assert_eq!(rid, request_id);
                        assert_eq!(result, 141);
                    }
                    1 => {
                        let (rid, result): (u64, i32) = resp_msg.as_response().unwrap();
                        assert_eq!(rid, request_id);
                        assert_eq!(result, 63);
                    }
                    2 => {
                        let (rid, result): (u64, i32) = resp_msg.as_response().unwrap();
                        assert_eq!(rid, request_id);
                        assert_eq!(result, 42);
                    }
                    3 => {
                        let (rid, result): (u64, String) = resp_msg.as_response().unwrap();
                        assert_eq!(rid, request_id);
                        assert_eq!(result, "helloworld");
                    }
                    4 => {
                        let (rid, result): (u64, Vec<u8>) = resp_msg.as_response().unwrap();
                        assert_eq!(rid, request_id);
                        assert_eq!(result, vec![42u8; 5]);
                    }
                    _ => unreachable!(),
                }
                break;
            } else {
                thread::yield_now();
            }
        }
    }
}

// ============================================================================
// 测试 1: 基本 RPC 调用
// ============================================================================

#[test]
fn test_rpc_basic_call() {
    let shm = make_shm();

    let server = thread::spawn(move || {
        simulate_server(shm, 5);
    });

    let client = thread::spawn(move || {
        let req_tx = shm.sender(ChannelId::new(0)).unwrap();
        let resp_rx = shm.receiver(ChannelId::new(1)).unwrap();

        fn call_and_wait<T>(req_tx: &ov_channels::Sender, resp_rx: &ov_channels::Receiver, req: ov_channels::Message) -> T
        where
            T: serde::de::DeserializeOwned,
        {
            req_tx.try_send(&req).unwrap();
            loop {
                if let Some(resp) = resp_rx.try_recv() {
                    let (_, result) = resp.as_response().unwrap();
                    return result;
                }
                thread::yield_now();
            }
        }

        let req = Message::request(1, METHOD_ADD, &(10i32, 20i32)).unwrap();
        let result: i32 = call_and_wait(&req_tx, &resp_rx, req);
        assert_eq!(result, 30);

        let req = Message::request(2, METHOD_SUBTRACT, &(50i32, 15i32)).unwrap();
        let result: i32 = call_and_wait(&req_tx, &resp_rx, req);
        assert_eq!(result, 35);

        let req = Message::request(3, METHOD_MULTIPLY, &(3i32, 7i32)).unwrap();
        let result: i32 = call_and_wait(&req_tx, &resp_rx, req);
        assert_eq!(result, 21);

        let req = Message::request(4, METHOD_CONCAT, &(String::from("foo"), String::from("bar"))).unwrap();
        let result: String = call_and_wait(&req_tx, &resp_rx, req);
        assert_eq!(result, "foobar");

        let req = Message::request(5, METHOD_GET_ARRAY, &(3usize, 99u8)).unwrap();
        let result: Vec<u8> = call_and_wait(&req_tx, &resp_rx, req);
        assert_eq!(result, vec![99, 99, 99]);
    });

    client.join().unwrap();
    server.join().unwrap();

    eprintln!();
    eprintln!("=== 基本 RPC 调用测试 ===");
    eprintln!("  所有方法调用成功");
}

// ============================================================================
// 测试 2: 高并发 RPC 调用
// ============================================================================

#[test]
fn test_rpc_high_concurrency() {
    let shm = make_shm();
    let start = Instant::now();

    let server = thread::spawn(move || {
        simulate_server(shm, RPC_CALL_COUNT);
    });

    let client = thread::spawn(move || {
        simulate_client(shm, RPC_CALL_COUNT);
    });

    client.join().unwrap();
    server.join().unwrap();

    let elapsed = start.elapsed();

    eprintln!();
    eprintln!("=== 高并发 RPC 调用测试 ===");
    eprintln!("  总调用次数 : {}", RPC_CALL_COUNT);
    eprintln!("  总耗时     : {:.3} ms", elapsed.as_secs_f64() * 1000.0);
    eprintln!("  平均延迟   : {:.2} μs/call", elapsed.as_micros() as f64 / RPC_CALL_COUNT as f64);
    eprintln!("  吞吐量     : {:.0} call/s", RPC_CALL_COUNT as f64 / elapsed.as_secs_f64());
}

// ============================================================================
// 测试 3: 双向 RPC
// ============================================================================

#[test]
fn test_rpc_bidirectional() {
    let shm = make_shm();

    // channel 0: A sends → B receives (A 的请求 + A 对 B 的响应)
    // channel 1: B sends → A receives (B 的请求 + B 对 A 的响应)
    const N: u32 = 10;

    let a = thread::spawn(move || {
        let tx = shm.sender(ChannelId::new(0)).unwrap();
        let rx = shm.receiver(ChannelId::new(1)).unwrap();
        let mut sent = 0u32;
        let mut recv_resp = 0u32;
        let mut recv_req = 0u32;

        while recv_resp < N || recv_req < N {
            // 优先处理收到的消息，避免对方通道堵满
            while let Some(msg) = rx.try_recv() {
                if msg.method_id().is_some() {
                    let (rid, _, (a, b)): (u64, u64, (i32, i32)) = msg.as_request().unwrap();
                    let resp = Message::response(rid, &(a + b)).unwrap();
                    while tx.try_send(&resp).is_err() {
                        thread::yield_now();
                    }
                    recv_req += 1;
                } else {
                    let (rid, result): (u64, i32) = msg.as_response().unwrap();
                    assert!((1000..1000 + N as u64).contains(&rid));
                    assert_eq!(result, ((rid - 1000) as i32) - ((rid - 1000) as i32) * 2);
                    recv_resp += 1;
                }
            }

            if sent < N {
                let req_id = 1000 + sent as u64;
                let req = Message::request(req_id, METHOD_ADD, &(sent as i32, sent as i32 * 2)).unwrap();
                if tx.try_send(&req).is_ok() {
                    sent += 1;
                }
            }
            thread::yield_now();
        }
    });

    let b = thread::spawn(move || {
        let tx = shm.sender(ChannelId::new(1)).unwrap();
        let rx = shm.receiver(ChannelId::new(0)).unwrap();
        let mut sent = 0u32;
        let mut recv_resp = 0u32;
        let mut recv_req = 0u32;

        while recv_resp < N || recv_req < N {
            while let Some(msg) = rx.try_recv() {
                if msg.method_id().is_some() {
                    let (rid, _, (a, b)): (u64, u64, (i32, i32)) = msg.as_request().unwrap();
                    let resp = Message::response(rid, &(a - b)).unwrap();
                    while tx.try_send(&resp).is_err() {
                        thread::yield_now();
                    }
                    recv_req += 1;
                } else {
                    let (rid, result): (u64, i32) = msg.as_response().unwrap();
                    assert!((2000..2000 + N as u64).contains(&rid));
                    assert_eq!(result, ((rid - 2000) as i32) * 3 + ((rid - 2000) as i32));
                    recv_resp += 1;
                }
            }

            if sent < N {
                let req_id = 2000 + sent as u64;
                let req = Message::request(req_id, METHOD_ADD, &(sent as i32 * 3, sent as i32)).unwrap();
                if tx.try_send(&req).is_ok() {
                    sent += 1;
                }
            }
            thread::yield_now();
        }
    });

    a.join().unwrap();
    b.join().unwrap();

    eprintln!();
    eprintln!("=== 双向 RPC 测试 ===");
    eprintln!("  双向通信成功");
}

// ============================================================================
// 测试 4: 复杂类型序列化
// ============================================================================

#[test]
fn test_rpc_complex_types() {
    let shm = make_shm();

    let server = thread::spawn(move || {
        let req_rx = shm.receiver(ChannelId::new(0)).unwrap();
        let resp_tx = shm.sender(ChannelId::new(1)).unwrap();

        let req_msg = loop {
            if let Some(msg) = req_rx.try_recv() {
                break msg;
            }
            thread::yield_now();
        };

        let (req_id, _, args): (u64, u64, (i32, [u8; 4], String)) = req_msg.as_request().unwrap();
        assert_eq!(args.0, 42);
        assert_eq!(args.1, [1, 2, 3, 4]);
        assert_eq!(args.2, "test");

        let result = vec!["hello", "world", "rpc"];
        let resp = Message::response(req_id, &result).unwrap();
        resp_tx.try_send(&resp).unwrap();
    });

    let client = thread::spawn(move || {
        let req_tx = shm.sender(ChannelId::new(0)).unwrap();
        let resp_rx = shm.receiver(ChannelId::new(1)).unwrap();

        let args = (42i32, [1u8, 2, 3, 4], String::from("test"));
        let req = Message::request(999, 0, &args).unwrap();
        req_tx.try_send(&req).unwrap();

        let resp = loop {
            if let Some(msg) = resp_rx.try_recv() {
                break msg;
            }
            thread::yield_now();
        };

        let result: Vec<String> = resp.as_response().unwrap().1;
        assert_eq!(result, vec!["hello", "world", "rpc"]);
    });

    client.join().unwrap();
    server.join().unwrap();

    eprintln!();
    eprintln!("=== 复杂类型序列化测试 ===");
    eprintln!("  复杂类型序列化/反序列化成功");
}
