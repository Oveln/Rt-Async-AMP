# rt-async ↔ StarryOS AMP IPC 设计文档

> 本文档描述双核 AMP 系统中 rt-async (hart 0, M-mode) 与 StarryOS 用户态进程
> (hart 1, S-mode) 之间的双向异步 IPC 通信机制。可作为后续实现的 plan。

---

## 1. 系统架构总览

```
┌──────────────────────────────────────────────────────────────────┐
│                        QEMU virt (riscv64)                       │
│                       256MB RAM, 2 harts                         │
│                                                                  │
│  ┌─────────────────────┐     ┌──────────────────────────────┐   │
│  │   hart 0: rt-async  │     │      hart 1: StarryOS        │   │
│  │   (M-mode RTOS)     │     │   (S-mode Linux-like OS)     │   │
│  │                     │     │                              │   │
│  │  UART1 (0x10002000) │     │  UART0 (0x10000000)         │   │
│  │  优先级抢占调度      │     │  用户态进程 + syscall        │   │
│  │  ov_channal 直接访问 │     │  /dev/rt_shm (mmap+ioctl)   │   │
│  └────────┬────────────┘     └──────────┬───────────────────┘   │
│           │                              │                       │
│           │    ┌──────────────────┐      │                       │
│           │    │  共享内存         │      │                       │
│           └────┤  0x88000000      ├──────┘                       │
│                │  67072 bytes     │                               │
│                │  ov_channal SHM  │                               │
│                └──────────────────┘                               │
│                        ▲                                         │
│                        │ IPI 通知                                 │
│         ┌──────────────┴──────────────────┐                      │
│         │                                 │                      │
│    rt-async → StarryOS               StarryOS → rt-async        │
│    CLINT MSIP1 (0x2000004)           SBI ecall (IPI ext)        │
│    (M-mode 直接写寄存器)              (S-mode → OpenSBI → M-mode)│
└──────────────────────────────────────────────────────────────────┘
```

### 1.1 物理内存布局

| 区域 | 起始地址 | 大小 | 说明 |
|------|----------|------|------|
| OpenSBI | `0x80000000` | 512KB | 固件, hart 0 mret 到 rt-async |
| StarryOS | `0x80200000` | ~38MB | `-kernel` 加载 |
| rt-async | `0x82800000` | 8MB | `-device loader` 加载 |
| IPC 共享内存 | `0x88000000` | 67072B | ov_channal SharedMemory |

> 所有地址定义在 `amp.config`，作为唯一真相源。

### 1.2 启动流程

1. **QEMU** 加载 OpenSBI (`-bios fw_dynamic.bin`)
2. **QEMU** 加载 StarryOS (`-kernel starryos.bin`)，设置 `fw_dynamic_info.next_addr = 0x80200000`
3. **QEMU** 加载 rt-async (`-device loader,addr=0x82800000,file=rt-async.bin`)
4. **OpenSBI** 初始化：
   - hart 0: `mret` 跳转到 `0x82800000` (rt-async) — `fw_base.S` 修改
   - hart 1+: `_try_lottery` → 跳转到 `next_addr` = `0x80200000` (StarryOS)
5. **rt-async** 启动：初始化 ov_channal 共享内存 (`SharedMemory::init()`)，启动调度器
6. **StarryOS** 启动：SMP 初始化 (hart 1)，注册 `/dev/rt_shm` 设备和 IPI 中断处理器

---

## 2. 共享内存通信 (ov_channal)

### 2.1 数据结构

```
SharedMemory (67072 bytes @ 0x88000000)
├── Channel 0: StarryOS → rt-async (RingBuffer, cap=128, msg=256B)
│   ├── magic: 0x4F56 ("OV")
│   ├── version: 1
│   └── buffer: RingBuffer<128> of Message
└── Channel 1: rt-async → StarryOS (RingBuffer, cap=128, msg=256B)
    ├── magic: 0x4F56
    ├── version: 1
    └── buffer: RingBuffer<128> of Message
```

- **Channel 0**: StarryOS 用户态写入，rt-async 读取
- **Channel 1**: rt-async 写入，StarryOS 用户态读取
- 每条消息 256 字节 (`Message` = 1B kind + 255B payload)
- 每通道容量 128 条消息 (ring buffer, 无锁)

### 2.2 消息类型

| kind | 类型 | 格式 |
|------|------|------|
| 0 | Notification | `payload[0..4]` = u32 id |
| 1 | Data | `payload[0..len]` = 原始字节 |
| 2 | Request | `payload[0..8]` = u64 request_id, `[8..16]` = u64 method_id, `[16..]` = postcard 序列化参数 |
| 3 | Response | `payload[0..8]` = u64 request_id, `[8..]` = postcard 序列化结果 |

### 2.3 消息 API (ov_channal crate)

```rust
// 创建消息
let msg = Message::notification(42u32);                    // 通知
let msg = Message::data(b"hello");                         // 数据
let msg = Message::request(rid, method_id, &(a, b))?;      // RPC 请求
let msg = Message::response(rid, &result)?;                // RPC 响应

// 解析消息
msg.ty()               // → Option<MsgType>
msg.as_notification()  // → Option<u32>
msg.as_data()          // → Option<&[u8; 255]>
msg.as_request::<T>()  // → Option<(u64, u64, T)>
msg.as_response::<T>() // → Option<(u64, T)>

// 发送/接收 (通过 SharedMemory)
let shm = unsafe { SharedMemory::at(SHMBASE) };
shm.sender(ChannelId::new(0)).unwrap().try_send(&msg)?;   // 发送
let msg = shm.receiver(ChannelId::new(0)).unwrap().try_recv(); // 接收
```

---

## 3. 通知机制 (IPI)

### 3.1 rt-async → StarryOS (CLINT MSIP1)

**机制**: M-mode 直接写 CLINT MSIP 寄存器

```
地址: 0x2000000 + 4 = 0x2000004 (MSIP for hart 1)
操作: 写 1 触发中断, 写 0 清除
```

**rt-async 端** (`chip-qemu-virt-rt/src/lib.rs`):
```rust
pub unsafe fn send_ipi_to_linux() {
    core::ptr::write_volatile((CLINTBASE + 4) as *mut u32, 1);
}
```

**StarryOS 端**: IPI 到达 hart 1 的 S-mode，触发 Supervisor Software Interrupt (IRQ = `0x8000_0000_0000_0001`)。`rt_shm.rs` 在初始化时注册了此中断处理器 `ipi_irq_handler`，该处理器：
1. 清除 SIP.SSIP (`csrc sip, 2`)
2. 设置 `IPC_PENDING = true`
3. 唤醒 `IPC_POLLSET` (唤醒阻塞在 `IPC_AWAIT` 的线程)

### 3.2 StarryOS → rt-async (SBI ecall)

**机制**: S-mode 通过 SBI ecall 发送 IPI 给 hart 0

```
SBI Extension: 0x735049 (自定义)
SBI Function:  0x00 (send_ipi)
参数: a0 = hart_mask (0x1), a1 = hart_mask_base (0x0)
```

**StarryOS 端** (`rt_shm.rs`):
```rust
fn sbi_send_ipi_to_hart0() -> VfsResult<usize> {
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("a0") 0x1_usize => error,
            inlateout("a1") 0x0_usize => _value,
            in("a6") 0x00,       // func_id
            in("a7") 0x735049,   // ext_id
        );
    }
}
```

**rt-async 端**: OpenSBI 收到 ecall 后向 hart 0 发送 M-mode Software Interrupt (MSI)。rt-async 的 `MachineSoft` ISR 处理：
1. 检查 `PEND_MARKER`：如果是调度器内部抢占信号 → 执行调度
2. 如果不是 → 调用 `__Inner_MachineSoft` (用户定义的外部 IPI 处理器)

---

## 4. StarryOS 用户态 IPC 接口

### 4.1 设备: `/dev/rt_shm`

| 操作 | 说明 |
|------|------|
| `open("/dev/rt_shm", O_RDWR)` | 独占打开，同一时刻仅允许一个进程 |
| `mmap(NULL, 67072, PROT_READ\|PROT_WRITE, MAP_SHARED, fd, 0)` | 映射共享内存到用户态地址空间 |
| `ioctl(fd, IPC_NOTIFY, 0)` | 通知 rt-async: 发送 SBI IPI 给 hart 0 |
| `ioctl(fd, IPC_AWAIT, 0)` | 阻塞等待 rt-async 通知: 等 IPI 中断唤醒 |
| `close(fd)` | 关闭设备，释放独占锁 |

ioctl 命令号:
- `IPC_NOTIFY` = `0x735001`
- `IPC_AWAIT` = `0x735002`

### 4.2 mmap 映射原理

`RtShmDevice::mmap()` 返回 `DeviceMmap::Physical(0x88000000, 67072)`。

StarryOS `sys_mmap` 处理 `DeviceMmap::Physical` 时创建 `LinearBackend`，建立
线性映射：`VA - offset = PA`。用户态进程直接读写 mmap 返回的地址即可操作
0x88000000 处的共享内存。

映射 flags: `NodeFlags::NON_CACHEABLE` — 设备内存不经过 cache。

### 4.3 IPC_AWAIT 阻塞机制

```
用户态:  ioctl(fd, IPC_AWAIT, 0)
           ↓ sys_ioctl → DeviceOps::ioctl(RT_SHM_IOC_AWAIT)
           ↓ block_on(interruptible(poll_fn(|cx| {
           ↓   IPC_POLLSET.register(cx.waker());
           ↓   if IPC_PENDING.swap(false, AcqRel) {
           ↓     Poll::Ready(0)
           ↓   } else {
           ↓     Poll::Pending   ← 当前线程挂起
           ↓   }
           ↓ })))

中断到达:  rt-async 写 CLINT MSIP1
           ↓ hart 1 收到 S-mode Software Interrupt
           ↓ ipi_irq_handler():
           ↓   clear SIP.SSIP
           ↓   IPC_PENDING = true
           ↓   IPC_POLLSET.wake()  ← 唤醒阻塞的线程

线程恢复:  poll_fn 再次 poll → IPC_PENDING == true → Ready(0) → ioctl 返回
```

---

## 5. rt-async 端 IPC 处理

### 5.1 当前实现

`intercom.rs` 提供三个核心函数:

```rust
fn init()         // SharedMemory::at(SHMBASE).init()
fn process_pending()  // 轮询 Channel 0, 处理所有消息
fn send_message(msg)  // 向 Channel 1 发送 + send_ipi_to_linux()
```

### 5.2 消息处理

```rust
fn handle_message(msg: Message) {
    match msg.ty() {
        Notification => { /* 回显通知 */ send_notification(id) }
        Request(method_id) => match method_id {
            0 => { /* ECHO: 回显 request_id */ }
            1 => { /* ADD: (i32, i32) -> i32 */ }
        }
        Data => { /* 记录日志 */ }
    }
}
```

### 5.3 任务集成

`task_ipc` 异步任务每秒轮询一次:

```rust
#[executor::task]
async fn task_ipc() {
    intercom::init();
    loop {
        intercom::process_pending();
        // 每 10 秒主动发送一次通知
        if tick.is_multiple_of(10) { intercom::send_notification(tick); }
        futures::timer::after(1000.millis()).await;
    }
}
```

---

## 6. 完整通信流程

### 6.1 StarryOS 用户态 → rt-async (请求/响应)

```
StarryOS 用户态                    rt-async (hart 0)
────────────────                   ──────────────────
1. fd = open("/dev/rt_shm", O_RDWR)
2. shm = mmap(NULL, 67072, PROT_READ|PROT_WRITE,
              MAP_SHARED, fd, 0)
3. ov_channal::SharedMemory::at(shm)
4. tx = shm.sender(ChannelId::new(0))
5. msg = Message::request(rid, method, &args)
6. tx.try_send(&msg)
7. ioctl(fd, IPC_NOTIFY, 0)  ────→  SBI ecall → OpenSBI → MSI hart 0
                                      MachineSoft ISR
                                      → __Inner_MachineSoft
                                      → (需要实现: 检查 Channel 0)
8. ioctl(fd, IPC_AWAIT, 0)    ←────  rt-async 处理请求
   (阻塞等待 IPI)                    rx = shm.receiver(ChannelId::new(0))
                                     msg = rx.try_recv()
                                     result = handle_request(msg)
                                     resp = Message::response(rid, &result)
                                     tx1 = shm.sender(ChannelId::new(1))
                                     tx1.try_send(&resp)
                                     send_ipi_to_linux()
                                      → CLINT MSIP1 = 1
                                      → StarryOS ipi_irq_handler
                                      → IPC_PENDING = true
                                      → IPC_POLLSET.wake()
9. (IPI 到达, ioctl 返回)
10. rx = shm.receiver(ChannelId::new(1))
11. resp = rx.try_recv()
12. (rid, result) = resp.as_response::<T>()
```

### 6.2 rt-async → StarryOS 用户态 (通知/数据)

```
rt-async (hart 0)                  StarryOS 用户态
─────────────────                   ────────────────
1. tx = shm.sender(ChannelId::new(1))
2. msg = Message::notification(id)
3. tx.try_send(&msg)
4. send_ipi_to_linux()      ────→  CLINT MSIP1 = 1
   (写 0x2000004 = 1)               StarryOS S-mode SW interrupt
                                     ipi_irq_handler()
                                     IPC_PENDING = true
                                     IPC_POLLSET.wake()

                                    5. ioctl(fd, IPC_AWAIT, 0) 返回
                                    6. rx = shm.receiver(ChannelId::new(1))
                                    7. msg = rx.try_recv()
                                    8. id = msg.as_notification()
```

### 6.3 典型用户态事件循环

```c
// 伪代码 - StarryOS 用户态进程
int fd = open("/dev/rt_shm", O_RDWR);
void *shm = mmap(NULL, 67072, PROT_READ|PROT_WRITE, MAP_SHARED, fd, 0);

// 初始化 ov_channal (通过 C binding 或 Rust lib)
ov_shm *ov = ov_shm_at(shm);
ov_sender *tx = ov_sender(ov, 0);  // Channel 0: → rt-async
ov_receiver *rx = ov_receiver(ov, 1); // Channel 1: ← rt-async

while (1) {
    // 1. 发送请求
    ov_message req = ov_request(1, method_add, args);
    ov_send(tx, &req);
    ioctl(fd, IPC_NOTIFY, 0);

    // 2. 等待响应
    ioctl(fd, IPC_AWAIT, 0);  // 阻塞直到 rt-async 回复

    // 3. 读取响应
    ov_message resp;
    while (ov_recv(rx, &resp)) {
        handle_response(&resp);
    }
}
```

---

## 7. 实现状态

### 7.1 已完成 ✓

| 组件 | 文件 | 状态 |
|------|------|------|
| `/dev/rt_shm` 设备 | `StarryOS/kernel/src/pseudofs/dev/rt_shm.rs` | ✓ 完整实现 |
| 设备注册 | `StarryOS/kernel/src/pseudofs/dev/mod.rs` | ✓ 注册为 (10,201) |
| mmap → Physical | `rt_shm.rs` → `DeviceMmap::Physical(0x88000000, 67072)` | ✓ |
| IPC_NOTIFY ioctl | `rt_shm.rs` → `sbi_send_ipi_to_hart0()` | ✓ |
| IPC_AWAIT ioctl | `rt_shm.rs` → `block_on(interruptible(poll_fn))` | ✓ |
| IPI IRQ handler | `rt_shm.rs` → `ipi_irq_handler()` | ✓ |
| Pollable impl | `rt_shm.rs` → `poll()` + `register()` | ✓ |
| 独占访问 | `rt_shm.rs` → `try_claim_device()` / `release_device()` | ✓ |
| rt-async intercom | `apps/rt-async-app/src/intercom.rs` | ✓ 基础框架 |
| rt-async IPI 发送 | `chip-qemu-virt-rt/src/lib.rs` → `send_ipi_to_linux()` | ✓ |
| amp.config | `amp.config` | ✓ 单一真相源 |
| 构建集成 | `build.rs` (chip-qemu-virt-rt, rt-async-app) | ✓ amp_gen.rs |
| QEMU 启动 | `run.sh` + `Makefile` | ✓ |

### 7.2 需要完成 ✗

| # | 任务 | 优先级 | 说明 |
|---|------|--------|------|
| 1 | **rt-async IPI 中断接收** | P0 | `__Inner_MachineSoft` 中处理 StarryOS 的 IPI，检查 Channel 0 并唤醒 IPC 任务 |
| 2 | **StarryOS SMP 配置修复** | P0 | hart 1 启动时 cpumask panic: `index < SIZE`，需调整 SMP 初始化（只有 hart 1 可用） |
| 3 | **用户态 C/Rust 测试程序** | P1 | 写一个 StarryOS 用户态程序，open→mmap→ioctl 完整测试 |
| 4 | **独占访问集成到 open/close** | P1 | `try_claim_device()` / `release_device()` 需要接入 VFS open/close 路径 |
| 5 | **IPC_AWAIT 中断安全性** | P1 | 验证 `interruptible()` 能正确处理信号中断（用户态 Ctrl+C 等） |
| 6 | **MSIP1 清除时序** | P2 | rt-async 写 MSIP1=1 后需适时写 0 清除；目前 StarryOS 端在 IRQ handler 中清 SIP |
| 7 | **消息序列化协议扩展** | P2 | 定义 method_id 分配表、标准 RPC 接口 |
| 8 | **错误恢复** | P3 | 超时重试、通道满处理、连接断开检测 |

---

## 8. 详细实现计划

### Phase 1: 基础通信打通 (P0)

#### 8.1 rt-async IPI 接收处理

**目标**: 当 StarryOS 通过 SBI ecall 发送 IPI 时，rt-async 能在 `__Inner_MachineSoft` 中接收并处理。

**当前问题**: `demo.rs` 中没有定义 `MachineSoft` 的 `#[executor::interrupt]` 处理器。
`__Inner_MachineSoft` 使用弱符号默认实现（会 abort）。

**实现方案**:

在 `apps/rt-async-app/src/bin/demo.rs` 添加:

```rust
#[executor::interrupt]
fn MachineSoft(_tf: &mut TrapFrame) {
    // 被 `#[executor::interrupt]` 重命名为 `__Inner_MachineSoft`
    // 当 PEND_MARKER == false (外部 IPI, 非调度器信号) 时被调用
    
    // 方案 A: 直接在 ISR 中轮询 Channel 0
    rt_async_app::intercom::process_pending();
    
    // 方案 B: 设置 flag，让 task_ipc 异步任务处理
    // rt_async_app::intercom::signal_pending();
}
```

**注意事项**:
- ISR 中不能做耗时操作
- 如果选择方案 B，需要一个 `AtomicBool` 作为信号量，`task_ipc` 需要改为事件驱动（收到信号时立即处理，而不是每秒轮询）
- `process_pending()` 内部使用 `postcard` 反序列化，需要 alloc，在 ISR 中是否安全取决于分配器实现

#### 8.2 StarryOS SMP cpumask 修复

**目标**: 让 StarryOS 在 hart 1 上正确启动（hart 0 被 rt-async 占用）。

**分析**:
- StarryOS SMP 初始化假设 hart 从 0 开始编号
- 当前 hart 1 是 StarryOS 看到的第一个（也是唯一一个）CPU
- `cpumask assertion failed: index < SIZE` 说明 hart ID 超出了 cpumask 的范围
- 需要让 StarryOS 将物理 hart 1 映射为逻辑 CPU 0

**方案**:
1. 找到 StarryOS SMP 初始化代码中 hart ID 到逻辑 CPU ID 的映射
2. 添加偏移：`logical_id = physical_hart_id - 1`（因为 hart 0 给了 rt-async）
3. 或者扩大 cpumask 的 SIZE

### Phase 2: 端到端测试 (P1)

#### 8.3 用户态测试程序

写一个简单的 StarryOS 用户态 C 程序:

```c
// test_rt_shm.c
#include <stdio.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/ioctl.h>
#include <unistd.h>

#define IPC_NOTIFY 0x735001
#define IPC_AWAIT  0x735002
#define SHM_SIZE   67072

int main() {
    int fd = open("/dev/rt_shm", O_RDWR);
    if (fd < 0) { perror("open"); return 1; }

    void *shm = mmap(NULL, SHM_SIZE, PROT_READ|PROT_WRITE, MAP_SHARED, fd, 0);
    if (shm == MAP_FAILED) { perror("mmap"); return 1; }

    // 直接操作 ov_channal 共享内存结构
    // Channel 0 header: magic(2B) + version(2B) + ring buffer state + messages
    // 发送一条 Notification(id=1) 到 Channel 0

    printf("Sending notification to rt-async...\n");
    // ... 写入消息到 channel 0 的 ring buffer ...

    ioctl(fd, IPC_NOTIFY, 0);  // 通知 rt-async
    printf("Waiting for response...\n");
    ioctl(fd, IPC_AWAIT, 0);   // 等待 rt-async 回复

    // 读取 Channel 1 的响应
    printf("Got response from rt-async!\n");

    close(fd);
    return 0;
}
```

> 注意：实际实现可能需要 C binding 到 ov_channal 或手动按照数据结构写入。

#### 8.4 独占访问接入 open/close

当前 `try_claim_device()` 和 `release_device()` 已定义但未接入 VFS 生命周期。

**需要修改**:
- `rt_shm.rs` 中的 `DeviceOps` 或 StarryOS VFS 层，在 open 时调用 `try_claim_device()`，在 close 时调用 `release_device()`
- 如果 StarryOS 的 DeviceOps 没有 open/close 钩子，可能需要在 `sys_open` / `sys_close` 路径中添加对 CharacterDevice 的特殊处理

### Phase 3: 生产化 (P2-P3)

#### 8.5 事件驱动的 IPC 任务

将 rt-async 的 `task_ipc` 从定时轮询改为事件驱动:

```rust
static IPC_SIGNAL: AtomicBool = AtomicBool::new(false);

// 在 __Inner_MachineSoft ISR 中:
fn MachineSoft(_tf: &mut TrapFrame) {
    IPC_SIGNAL.store(true, Ordering::Release);
    // 需要一个机制唤醒 task_ipc
}

#[executor::task]
async fn task_ipc() {
    intercom::init();
    loop {
        // 等待 IPC 信号（需要 rt-async 框架支持异步等待外部信号）
        wait_for_signal(&IPC_SIGNAL).await;
        IPC_SIGNAL.store(false, Ordering::Release);
        intercom::process_pending();
    }
}
```

#### 8.6 MSIP 时序

当前 rt-async 写 MSIP1=1 后没有写 0 清除。StarryOS IRQ handler 中清 SIP，
但 CLINT MSIP 本身可能保持为 1 导致重复中断。需要:

1. 在 `ipi_irq_handler()` 中额外写 `CLINT_MSIP1 = 0`
2. 或在 rt-async 中延迟清除

#### 8.7 协议扩展

定义 method_id 分配表:

| method_id | 方向 | 签名 | 说明 |
|-----------|------|------|------|
| 0 | req→resp | `(i32) -> i32` | ECHO |
| 1 | req→resp | `(i32, i32) -> i32` | ADD |
| 100-199 | req→resp | (自定义) | 用户定义 |
| 200+ | req→resp | (自定义) | 扩展 |

---

## 9. 关键约束和注意事项

### 9.1 内存模型

- **共享内存**: 两个系统直接操作同一块物理内存 (0x88000000)
- **ov_channal**: 无锁 ring buffer，基于 `portable-atomic` 的原子操作
- **缓存一致性**: `rt_shm` 设备标记 `NON_CACHEABLE`，确保 CPU 间数据可见性
- **对齐**: `SharedMemory` 要求 256 字节对齐 (`#[repr(C, align(256))]`)

### 9.2 并发安全

- ov_channal 的 ring buffer 使用原子操作保证单生产者单消费者的线程安全
- Channel 0: StarryOS 写入, rt-async 读取 (SPSC)
- Channel 1: rt-async 写入, StarryOS 读取 (SPSC)
- **绝不能**两个系统同时写同一个 Channel

### 9.3 中断优先级

- rt-async 运行在 M-mode，中断优先级最高
- StarryOS 运行在 S-mode，中断需要经过 M-mode (OpenSBI) 转发
- rt-async 的 `MachineSoft` ISR 中应尽快处理，避免影响实时性

### 9.4 amp.config 同步

- StarryOS 中的常量 (`rt_shm.rs`) 是手动同步的（注释标注 amp.config 键名）
- rt-async 侧通过 `build.rs` 自动从 `amp.config` 生成 `amp_gen.rs`
- 修改 `amp.config` 后必须同时更新 `rt_shm.rs` 中的常量

---

## 10. 参考文件索引

| 文件 | 说明 |
|------|------|
| `amp.config` | 地址约定单一真相源 |
| `StarryOS/kernel/src/pseudofs/dev/rt_shm.rs` | `/dev/rt_shm` 设备实现 |
| `StarryOS/kernel/src/pseudofs/dev/mod.rs` | 设备注册 |
| `StarryOS/kernel/src/syscall/mm/mmap.rs` | mmap syscall (LinearBackend) |
| `StarryOS/kernel/src/syscall/fs/ctl.rs` | ioctl syscall |
| `apps/rt-async-app/src/intercom.rs` | rt-async IPC 模块 |
| `apps/rt-async-app/src/bin/demo.rs` | rt-async 主程序 |
| `modules/chip-qemu-virt-rt/src/lib.rs` | 芯片支持 + IPI 发送 |
| `modules/chip-qemu-virt-rt/build.rs` | amp.config → amp_gen.rs |
| `run.sh` | QEMU 启动脚本 |
| `Makefile` | 构建系统 |
| `opensbi/firmware/fw_base.S` | hart 0 mret → rt-async 路由 |
