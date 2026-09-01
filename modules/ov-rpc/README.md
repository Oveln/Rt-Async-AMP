# ov-rpc

基于 [ov-channels](https://github.com/oveln/ov-channels) 共享内存通道的实时 AMP RPC 框架。

为 AMP（非对称多处理）系统设计，`no_std` 兼容，支持优先级通道和单向调用。

## 通道布局

```
CH0: 普通请求  Client ──▶ Server
CH1: 普通响应  Server ──▶ Client
CH2: 急停通道  Client ──▶ Server (单向, 高优先级)
```

共享内存中包含 3 个 channel，每个 channel 是容量 128 块的 `RingBuffer`（块 = 256 字节定长 `Message`）。序列化使用 [postcard](https://crates.io/crates/postcard)。

## 消息尺寸分层与错误路径（v0.2.0，依赖 ov-channels 0.3 块层）

| 方向 | postcard 上限 | 线格式 |
|------|---------------|--------|
| 请求参数（client→server） | 239B | 恒单块（与 0.2.x 相同） |
| 响应结果（server→client） | ≤ 247B | 单块前缀直写（kind + 实际字节进环槽，尾部留陈旧——postcard 自定界，接收端解码自停） |
| 响应结果（server→client） | 247B < n ≤ 2028B | 字节流帧 2..=8 块**原子发布**（要么完整可见要么不可见） |
| 响应结果（server→client） | > 2028B | `RpcError::ResponseTooLarge`（服务定义的返回类型过大，配置错误） |

大响应对 `call`/`call_poll` 的调用方**透明**：服务端 handler 照常返回结果
（宏经出参 `Response::write` 序列化），客户端 `recv::<T>`/`recv_for::<T>`
照常解码，单块/多块由线格式自动分派。acall 完成方（服务端后台补发）结果
>247B 时应经 `Response::write`/`write_raw` + `ov_rpc::send_response` 发送。

**零成本响应路径**（v0.2.0 定案前，K3 板 2026-09-01 回归教训）：`Response` 内部
`MaybeUninit` + `len` 追踪（写多少初始化多少，`empty()` 零成本构造），
由调用方持有、`handle` 经出参写入、返回**无字段**的 `Reply`（寄存器返回）；
≤255B 载荷经 `ov_channels` 的 `try_send_raw` 前缀直写环槽。小响应全程
**无 memset、无按值搬移、窗口内恰好写一次**——在无缓存 SRAM 上与手写
最小路径等价（此前按值 `Response`/`Reply` 版本实测每条响应 ~170µs 写放大）。

**错误路径（poison 响应）**：服务端对双向调用在三种情况下回 poison 响应
（Response kind + 原请求 rid + 不可解码载荷）——方法未注册（版本错配下
漂移 op 的快失败面）、参数反序列化失败、响应超长。客户端按 rid 命中后得
到 `RecvError::DeserializeFailed`，替代 v0.1.x 的挂死等超时（v0.1.x 的
`notification(0)` 错误响应过不了客户端 `request_id` 检查，实际从未生效）。
one-way 调用与 acall（`Reply::Deferred`，响应由完成方补发）不回 poison。

`RpcHandler::handle` 返回值从 `Result<Option<Message>, DeserializeFailed>`
改为 `Result<Reply, RpcError>` 且响应经出参 `&mut Response` 写入
（三态 `Reply::Silent | Deferred | Written` 区分 one-way/未注册、异步受理、
已写入出参）——v0.2.0 破坏性变更，手写 `impl RpcHandler` 的代码需
同步（`define_service!` 用户无感）。

## 服务发现（INIT，method 0 保留）

`define_service!` 在编译期从同一张方法表 const 生成服务描述符
（`Service::DESCRIPTOR`，单一真相源），`RpcServer` 对 method 0 的 INIT
请求在 dispatch 前拦截、把描述符经响应通路回发（>255B 自动走多块流帧）：

```rust
// 客户端（std）
let rid = client.discover(notify)?;
// ... await/poll 后
let bytes = client.recv_raw_for(rid).unwrap();
let d = ov_rpc::descriptor::parse(bytes).unwrap();
for m in d.methods() {
    println!("{:>3}  {:<6} {}", m.mid, m.kind_name(), m.name);
}
```

描述符紧凑格式（v1）：`[proto u8][desc_len varint][count varint]` +
每方法 `[mid varint][flags u8][name_len varint][name]`（flags：one-way /
urgent / acall）。**方法表从 1 起编号，0 为协议保留**——v0.2.0 起 op
全量重编号，固件与客户端工具必须成对更新。

旧固件（无 INIT 拦截）下 discover 得到 poison 响应（描述符解不开）或
被 ov-channels 版本门直接拒绝——快失败而非挂死。

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
| `discover(notify)` | INIT 服务发现（method 0） |
| `poll_responses()` | drain CH1 到内部缓冲区（单块/多块归一化） |
| `recv::<T>()` | FIFO 按序取下一条响应 |
| `recv_for::<T>(rid)` | 按 rid 匹配取响应 |
| `recv_raw_for(rid)` | 按 rid 取原始载荷字节（服务发现用，不消费） |

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

- `logging` — 启用 `log` 依赖，服务端在参数反序列化失败/响应超长/错误响应发送失败时输出警告

## 依赖

- [`ov-channels`](https://github.com/oveln/ov-channels) — 共享内存通道
- [`postcard`](https://crates.io/crates/postcard) — `no_std` 序列化
- [`serde`](https://crates.io/crates/serde) — 序列化框架
- [`paste`](https://crates.io/crates/paste) — 客户端宏中生成 `method_poll` 方法名
