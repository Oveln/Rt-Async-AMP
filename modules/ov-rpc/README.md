# ov-rpc

基于 [ov-channels](https://github.com/oveln/ov-channels) 共享内存通道的实时 AMP RPC 框架。

为 AMP（非对称多处理）系统设计，`no_std` 兼容，支持优先级通道和单向调用。

## 通道布局

```
CH0: 普通请求  Client ──▶ Server
CH1: 普通响应  Server ──▶ Client
CH2: 急停通道  Client ──▶ Server (单向, 高优先级)
```

共享内存中包含 4 个 channel，每个 channel 是容量 127 的 `RingBuffer<Message>`（256 字节定长消息）。序列化使用 [postcard](https://crates.io/crates/postcard)。

## 调用模式

| 方法 | 通道 | 响应 | IPI 策略 | 用途 |
|------|------|------|----------|------|
| `call` | CH0 → CH1 | 有，服务端回 IPI | 客户端自动根据 BUSY 标志决定是否发 IPI | 低频请求-响应（查询状态） |
| `call_poll` | CH0 → CH1 | 有，服务端不回 IPI | 客户端自动根据 BUSY 标志决定是否发 IPI | 高频请求-响应（busy-poll） |
| `send` | CH0 | 无 | 客户端自动根据 BUSY 标志决定是否发 IPI | 单向写操作（设 PWM、日志） |
| `urgent` | CH2 | 无 | 客户端自动根据 BUSY 标志决定是否发 IPI | 急停、高优先级指令 |

客户端在写入请求后检查共享内存的 BUSY 标志：若服务端正在忙等（BUSY=1），跳过 IPI；否则自动调用 `notify` 发送 IPI 唤醒服务端。

## 快速使用

### 1. 定义服务

#### 服务端：`define_service!`

```rust
use ov_rpc::define_service;

define_service! {
    pub MotorService {
        SET_SPEED: 0 => send set_speed(motor: u8, speed: i32);   // 单向
        STOP:      1 => urgent stop();                            // 急停
        GET_SPEED: 2 => call get_speed(motor: u8) -> i32;        // 请求-响应
    }
}
```

- `call` — 请求-响应模式，handler 返回结果
- `send` — 单向，handler 执行操作，不返回响应
- `urgent` — 急停，走高优先级通道 (CH2)，不返回响应

支持 0~4 个参数，多参数用元组传递。

#### 客户端：`define_service_client!`

```rust
use ov_rpc::define_service_client;

define_service_client! {
    pub MotorService {
        SET_SPEED: 0 => send set_speed(motor: u8, speed: i32);
        STOP:      1 => urgent stop();
        GET_SPEED: 2 => call get_speed(motor: u8) -> i32;
    }
}
```

生成类型安全的客户端 struct，内嵌 `RpcClient`，通过 `Deref`/`DerefMut` 暴露收响应方法。

- `call` 方法生成 `method()` + `method_poll()` 两个变体
- `send` / `urgent` 方法生成 `method(notify)`

### 2. 实现业务逻辑（服务端）

```rust
impl MotorService {
    pub fn set_speed(motor: u8, speed: i32) { /* 驱动电机 */ }
    pub fn stop() { /* 紧急停止 */ }
    pub fn get_speed(motor: u8) -> i32 { /* 读取速度 */ }
}
```

### 3. 服务端（rt-async 侧）

```rust
use ov_rpc::{RpcServer, ProcessResult, HandledKind};

static SERVER: RpcServer = RpcServer::new(SHM_ADDR);

loop {
    // process_all 先处理急停 (CH2)，再处理普通 (CH0)
    // 每个 Notify 请求处理完后立即回 IPI
    let count = SERVER.process_all::<MotorService, _, _>(
        |msg| { /* 处理非 RPC 消息 */ },
        || send_ipi_to_linux(),  // on_notify 回调
    );
}
```

### 4. 客户端（Linux 侧）

使用 `define_service_client!` 生成的类型安全客户端：

```rust
let mut client = MotorService::new(shm_addr);
let notify = || rt.notify();

// call 模式（服务端回 IPI）
let rid = client.get_speed(1u8, notify)?;
rt.await_ipi();
client.poll_responses();
let speed: i32 = client.recv_for(rid)?.unwrap();

// call_poll 模式（自行轮询）
let rid = client.get_speed_poll(1u8, notify)?;
while client.poll_responses() == 0 {} // 忙等
let speed: i32 = client.recv_for(rid)?.unwrap();

// send（单向）
client.set_speed(1u8, 100i32, notify)?;

// urgent（急停）
client.stop(notify)?;
```

或直接使用底层 `RpcClient`：

```rust
use ov_rpc::RpcClient;

let mut client = RpcClient::new(shm_addr);
let notify = || rt.notify();

// call
let rid = client.call(MotorService::GET_SPEED, &1u8, notify)?;
rt.await_ipi();
client.poll_responses();
let speed: i32 = client.recv_for(rid)?.unwrap();

// send
client.send(MotorService::SET_SPEED, &(1u8, 100i32), notify)?;

// urgent
client.urgent(MotorService::STOP, &(), notify)?;
```

## API

### RpcClient

| 方法 | 说明 |
|------|------|
| `call(method_id, args, notify)` | 请求-响应，服务端回 IPI |
| `call_poll(method_id, args, notify)` | 请求-响应，服务端不回 IPI，调用者自行 poll |
| `send(method_id, args, notify)` | 单向，不期待响应 |
| `urgent(method_id, args, notify)` | 急停，走 CH2 |
| `poll_responses()` | drain CH1 到内部缓冲区 |
| `recv::<T>()` | FIFO 按序取下一条响应 |
| `recv_for::<T>(rid)` | 按 rid 匹配取响应 |

所有方法在写入请求后自动检查 BUSY 标志：若 BUSY=0 则调用 `notify` 发送 IPI 唤醒服务端。

### RpcServer

| 方法 | 说明 |
|------|------|
| `process_one::<H>()` | 处理普通通道 (CH0) 一条消息 |
| `process_urgent::<H>()` | 处理急停通道 (CH2) 一条消息 |
| `process_all::<H, _, _>(on_other, on_notify)` | 先急停后普通，每个 Notify 请求立即调用 `on_notify`，返回已处理数量 |
| `has_pending()` | 普通通道是否有消息 |
| `has_urgent()` | 急停通道是否有消息 |

### ProcessResult

```rust
pub enum ProcessResult {
    NoMessage,
    Handled(HandledKind),  // Notify | Quiet | OneWay
    Unhandled(MethodId),
    NotRpc(Message),
}
```

### HandledKind

```rust
pub enum HandledKind {
    Notify,  // call 模式，服务端需回 IPI
    Quiet,   // call_poll 模式，不回 IPI
    OneWay,  // send/urgent 单向调用
}
```

## 协议约定

`method_id` 的 bit 分配：

```
bit 63: REPLY_NOTIFY — 响应后是否回 IPI (call 模式)
bit 62: ONE_WAY      — 是否不需要响应
bit 0-61: actual method_id
```

由 `call` / `call_poll` / `send` / `urgent` 自动设置，用户无需关心。

## Features

- `logging` — 启用 `log` 依赖，服务端遇到未知方法时输出警告

## 依赖

- [`ov-channels`](https://github.com/oveln/ov-channels) — 共享内存通道
- [`postcard`](https://crates.io/crates/postcard) — `no_std` 序列化
- [`serde`](https://crates.io/crates/serde) — 序列化框架
- [`paste`](https://crates.io/crates/paste) — 客户端宏中生成 `method_poll` 方法名
