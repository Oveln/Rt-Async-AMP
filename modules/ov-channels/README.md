# OV Channel

双系统共享内存通信库，用于裸机环境下的高效通信。

## 特性

- `no_std` 设计，适合裸机环境
- 基于环形缓冲区的无锁通信
- 支持最多 2 个独立通道
- 四种消息类型：通知、数据、RPC 请求/响应
- **RPC 支持**：类型安全的远程过程调用，使用 postcard 二进制序列化

## 架构

```
                    SharedMemory (共享内存)
                              │
           ┌──────────────────┼──────────────────┐
           │                  │                  │
       Channel 0          Channel 1           (更多...)
       ┌───────┐          ┌───────┐
       │ Ring  │          │ Ring  │
       │Buffer │          │Buffer │
       └───────┘          └───────┘

每个 Channel 包含一个 RingBuffer，独立存储消息
```

## 配置常量

| 常量 | 默认值 | 说明 |
|------|--------|------|
| `MAX_CHANNELS` | 2 | 最大通道数 |
| `CHANNEL_CAPACITY` | 128 | 每通道消息数 |
| `PAYLOAD_SIZE` | 255 | 消息负载大小 |
| `MESSAGE_ALIGN` | 256 | 消息对齐大小 |
| `MAGIC` | 0x4F56 | 魔术值，用于验证共享内存有效性 |
| `VERSION` | 1 | 版本号 |

## 内存布局

### Message 结构体

```rust
#[repr(C, align(256))]
pub struct Message {
    kind: u8,           // 消息类型 (1 字节)
    payload: [u8; PAYLOAD_SIZE],  // 255 字节
}  // 总大小: 256 字节 (无额外 padding)
```

```
┌─────────────────────────────────────────────────────────────┐
│                        Message (256 bytes)                  │
├─────────────────────────────────────────────────────────────┤
│ kind (1B) │              payload (255B)                     │
├───────────┴───────────────────────────────────────────────────┤
│ 消息类型  │               实际数据内容                       │
└─────────────────────────────────────────────────────────────┘
```

### 消息类型 (kind)

| 值 | 类型 | payload 格式 |
|----|------|-------------|
| 0 | Notification | 自定义数据 |
| 1 | Data | 任意数据 |
| 2 | Request | `request_id(u64) + method_id(u64) + serialized_args` |
| 3 | Response | `request_id(u64) + serialized_result` |

### RPC 消息编码

```
=== RPC 请求 (kind=2) ===
┌─────────────────────────────────────────────────────────────┐
│ kind=2 │              payload (255B)                        │
├─────────────────────────────────────────────────────────────┤
│                        payload 内容:                        │
├──────────────┬──────────────┬───────────────────────────────┤
│ request_id   │ method_id    │ serialized_args               │
│ (u64, 8B, LE)│ (u64, 8B, LE)│   (postcard 格式)            │
└──────────────┴──────────────┴───────────────────────────────┘

=== RPC 响应 (kind=3) ===
┌─────────────────────────────────────────────────────────────┐
│ kind=3 │              payload (255B)                        │
├─────────────────────────────────────────────────────────────┤
│                        payload 内容:                        │
├──────────────┬──────────────────────────────────────────────┤
│ request_id   │ serialized_result                            │
│ (u64, 8B, LE)│   (postcard 格式)                            │
└──────────────┴──────────────────────────────────────────────┘
```

### 共享内存布局

```
┌──────────────────────────────────────────────────────────────┐
│                    SharedMemory (256B aligned)              │
├──────────────────────────────────────────────────────────────┤
│ channels[MAX_CHANNELS]                                       │
│   ┌──────────────────────────────────────────────────────┐  │
│   │ Channel 0 │ Channel 1 │ (更多...)                    │  │
│   └──────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────┐
│                    Channel (256B aligned)                    │
├───────────┬──────────────────────────────────────────────────┤
│ magic     │ version   │ RingBuffer<CHANNEL_CAPACITY>         │
│ (AtomicU16)│(AtomicU16)│  (128 × Message)                    │
├───────────┴───────────┴──────────────────────────────────────┤
│ 0x4F56 ("OV")        │ 1            │ 每消息 256B           │
└──────────────────────────────────────────────────────────────┘
```

## 快速开始

### 基础消息通信

```rust
use ov_channels::{SharedMemory, ChannelId, Message, MsgType};

const SHARED_ADDR: usize = 0xc8000000;

// 系统 A - 发送
let shm = unsafe { SharedMemory::at(SHARED_ADDR) };
let tx = shm.sender(ChannelId::new(0)).unwrap();
tx.try_send(&Message::notification(1)).unwrap();

// 系统 B - 接收
let shm = unsafe { SharedMemory::at(SHARED_ADDR) };
let rx = shm.receiver(ChannelId::new(0)).unwrap();
while let Some(msg) = rx.try_recv() {
    // 处理消息
}
```

### RPC 调用

需要启用 `alloc` 特性以支持序列化：

```toml
ov-channels = { version = "0.1", features = ["alloc"] }
```

`request_id` 用于匹配请求和响应，支持并发 RPC 调用。客户端使用单调递增的计数器生成，服务端在响应中回显相同的值。

#### 使用示例

```rust
use ov_channels::{SharedMemory, ChannelId, Message};

const SHARED_ADDR: usize = 0xc8000000;

// 定义方法 ID
const METHOD_ADD: u64 = 0;

// === 客户端 ===
let shm = unsafe { SharedMemory::at(SHARED_ADDR) };
let tx = shm.sender(ChannelId::new(0)).unwrap();
let rx = shm.receiver(ChannelId::new(1)).unwrap();

// 生成 request_id
let request_id = 12345u64;

// 序列化并发送请求: add(42, 99)
let args = (42i32, 99i32);
let req_msg = Message::request(request_id, METHOD_ADD, &args).unwrap();
tx.try_send(&req_msg).unwrap();

// 接收并反序列化响应
let resp_msg = rx.try_recv().unwrap();
let (rid, result): (u64, i32) = resp_msg.as_response().unwrap();
assert_eq!(rid, request_id);
assert_eq!(result, 141);

// === 服务端 ===
let shm = unsafe { SharedMemory::at(SHARED_ADDR) };
let req_rx = shm.receiver(ChannelId::new(0)).unwrap();
let resp_tx = shm.sender(ChannelId::new(1)).unwrap();

// 接收请求
let req_msg = req_rx.try_recv().unwrap();

// 先获取 method_id，再决定如何反序列化参数
let Some(method_id) = req_msg.method_id() else {
    // 无效请求
    return;
};

match method_id {
    METHOD_ADD => {
        let (request_id, _, (a, b)): (u64, u64, (i32, i32)) = req_msg.as_request().unwrap();
        // 处理并发送响应
        let result = a + b;
        let resp_msg = Message::response(request_id, &result).unwrap();
        resp_tx.try_send(&resp_msg).unwrap();
    }
    // ... 其他方法
    _ => {}
}
```

**类型支持**：
- 无 `alloc`：固定大小类型（`i32`、`(u8, u16)`、`[u8; N]` 等）
- 有 `alloc`：`Vec<T>`、`String` 等

## 消息 API

### 创建消息

```rust
// 简单通知
Message::notification(42u32)

// 带数据
Message::data(b"hello")

// RPC 请求 (自动序列化参数)
Message::request(request_id: u64, method_id: u64, args: &T) -> Result<Message, postcard::Error>

// RPC 响应 (自动序列化结果)
Message::response(request_id: u64, result: &T) -> Result<Message, postcard::Error>
```

### 解析消息

```rust
// 获取消息类型
msg.ty() -> Option<MsgType>

// 获取通知内容
msg.as_notification() -> Option<u32>

// 获取数据内容
msg.as_data() -> Option<&Payload>

// 获取 RPC 请求的 method_id (不反序列化参数)
msg.method_id() -> Option<u64>

// 获取 RPC 请求/响应的 request_id (不反序列化参数/结果)
msg.request_id() -> Option<u64>

// 获取 RPC 请求 (完整反序列化)
msg.as_request::<T>() -> Option<(u64, u64, T)>  // (request_id, method_id, args)

// 获取 RPC 响应 (完整反序列化)
msg.as_response::<T>() -> Option<(u64, T)>  // (request_id, result)
```

## 迭代接收

```rust
let rx = shm.receiver(ChannelId::new(0)).unwrap();
for msg in rx.iter() {
    match msg.ty() {
        Some(MsgType::Notification) => { /* ... */ }
        Some(MsgType::Data) => { /* ... */ }
        Some(MsgType::Request) => {
            // 先获取 method_id
            let Some(method_id) = msg.method_id() else { continue };
            // 根据方法 ID 处理...
        }
        Some(MsgType::Response) => { /* ... */ }
        _ => {}
    }
}
```

## 编译选项

```sh
# 默认 (critical-section，适合裸机)
cargo build

# 启用 alloc 支持 (需要 Vec/String)
cargo build --features "alloc"

# 单核模式 (性能最优)
cargo build --features "assume-single-core"

# 使用平台原生原子指令
cargo build --no-default-features --features "std"
```

## 测试

```sh
# 基础测试 (必须串行运行)
cargo test -- --test-threads=1

# RPC 集成测试 (需要 alloc 特性)
cargo test --test rpc --features "alloc"
```

## License

MIT

## AI generated

本项目使用AI工具生成文本以及相关注释、测试
