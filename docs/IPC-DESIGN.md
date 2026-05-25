# rt-async ↔ StarryOS AMP IPC 设计文档

> 本文档描述双核 AMP 系统中 rt-async (hart 1, M-mode) 与 StarryOS 用户态进程
> (hart 0, S-mode) 之间的双向异步 IPC 通信机制。

---

## 1. 系统架构

```
┌──────────────────────────────────────────────────────────────────┐
│                        QEMU virt (riscv64)                       │
│                       256MB RAM, 2 harts                         │
│                                                                  │
│  ┌─────────────────────┐     ┌──────────────────────────────┐   │
│  │   hart 1: rt-async  │     │      hart 0: StarryOS        │   │
│  │   (M-mode RTOS)     │     │   (S-mode Linux-like OS)     │   │
│  │                     │     │                              │   │
│  │  UART1 (0x10002000) │     │  UART0 (0x10000000)         │   │
│  │  ov_channal 直接访问 │     │  /dev/rt_shm (mmap+ioctl)   │   │
│  └────────┬────────────┘     └──────────┬───────────────────┘   │
│           │                              │                       │
│           │    ┌──────────────────┐      │                       │
│           └────┤  共享内存         ├──────┘                       │
│                │  0x88000000      │                               │
│                │  67072 bytes     │                               │
│                │  ov_channal SHM  │                               │
│                └──────────────────┘                               │
└──────────────────────────────────────────────────────────────────┘
```

### 1.1 物理内存布局

| 区域 | 起始地址 | 大小 | 说明 |
|------|----------|------|------|
| OpenSBI | `0x80000000` | ~325KB | 固件, hart 1 mret 到 rt-async |
| StarryOS | `0x80200000` | ~1.5MB | `-kernel` 加载 |
| rt-async | `0x82800000` | ~200KB | `-device loader` 加载 |
| IPC 共享内存 | `0x88000000` | 67072B | ov_channal SharedMemory |

> 所有地址定义在 `amp.config`，作为唯一真相源。

### 1.2 启动流程

1. **QEMU** 加载 OpenSBI (`-bios fw_dynamic.bin`)
2. **QEMU** 加载 StarryOS (`-kernel starryos.bin`)，设置 `fw_dynamic_info.next_addr = 0x80200000`
3. **QEMU** 加载 rt-async (`-device loader,addr=0x82800000,file=rt-async.bin`)
4. **OpenSBI** 初始化（patched）：
   - hart 1: `mret` 跳转到 `0x82800000` (rt-async，M-mode)
   - hart 0: `_try_lottery` → 跳转到 `next_addr` = `0x80200000` (StarryOS，S-mode)
5. **rt-async** 启动：初始化 ov_channal 共享内存 (`SharedMemory::init()`)，启动调度器
6. **StarryOS** 启动：SMP 初始化，注册 `/dev/rt_shm` 设备和 S-mode SWI 中断处理器

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
- 每通道容量 128 条消息 (ring buffer, 无锁 SPSC)

### 2.2 消息类型

| kind | 类型 | 格式 |
|------|------|------|
| 0 | Notification | `payload[0..4]` = u32 id |
| 1 | Data | `payload[0..len]` = 原始字节 |
| 2 | Request | `payload[0..8]` = u64 request_id, `[8..16]` = u64 method_id, `[16..]` = postcard 序列化参数 |
| 3 | Response | `payload[0..8]` = u64 request_id, `[8..]` = postcard 序列化结果 |

---

## 3. IPI 通知机制

### 3.1 StarryOS → rt-async (CLINT MSIP1)

StarryOS `/dev/rt_shm` 的 `ioctl(NOTIFY)` 直接写 CLINT MSIP1 寄存器：

```
地址: 0x2000000 + 4 = 0x2000004 (MSIP for hart 1)
操作: 写 1 触发中断
```

`rt_shm.rs` 实现（`send_ipi_to_rt_async`）：
```rust
let vaddr = axhal::mem::phys_to_virt(PhysAddr::from(CLINT_BASE + 0x4));
core::ptr::write_volatile(vaddr.as_ptr() as *mut u32, 1);
```

OpenSBI patch（`aclint_mswi.c`）允许 S-mode 访问 CLINT MSIP 寄存器。

rt-async 收到 M-mode Software Interrupt → `MachineSoft` ISR → `intercom::process_pending()`。

### 3.2 rt-async → StarryOS (CLINT MSIP0 → SSIP)

rt-async 写 CLINT MSIP0 通知 hart 0：

```
地址: 0x2000000 + 0 = 0x2000000 (MSIP for hart 0)
操作: 写 1 触发 M-mode SWI on hart 0
```

hart 0 (OpenSBI) 收到 `IRQ_M_SOFT` → `sbi_ipi_process()` → 发现 `ipi_type == 0`（非 SBI ecall 发的）→ 设置 `MIP.SSIP`。

OpenSBI patch（`sbi_ipi.c`）：
```c
ipi_type = atomic_raw_xchg_ulong(&ipi_data->ipi_type, 0);
if (!ipi_type)
    csr_set(CSR_MIP, MIP_SSIP);  // 转发给 S-mode
```

StarryOS 收到 S-mode Software Interrupt → `ipi_irq_handler()` → 设置 `IPC_PENDING`，唤醒 `IPC_POLLSET`。

---

## 4. StarryOS 用户态接口

### 4.1 设备: `/dev/rt_shm`

| 操作 | 说明 |
|------|------|
| `open("/dev/rt_shm", O_RDWR)` | 打开设备 |
| `mmap(NULL, 67072, PROT_READ\|PROT_WRITE, MAP_SHARED, fd, 0)` | 映射共享内存 |
| `ioctl(fd, IPC_NOTIFY, 0)` | 通知 rt-async (发送 IPI) |
| `ioctl(fd, IPC_AWAIT, 0)` | 阻塞等待 rt-async 回复 |
| `close(fd)` | 关闭设备 |

ioctl 命令号:
- `IPC_NOTIFY` = `0x735001`
- `IPC_AWAIT` = `0x735002`

### 4.2 用户态程序

`apps/user-test-ipc/` — Rust 静态链接程序，交叉编译为 `riscv64gc-unknown-linux-musl`。

用法：
```bash
make user-test-install   # 编译 + 写入 rootfs
make run                  # 启动 QEMU
# StarryOS shell 中执行:
/user-test-ipc 3          # 运行 3 轮 IPC 测试
```

---

## 5. rt-async 端 IPC 处理

`apps/rt-async-app/src/intercom.rs` 提供：

| 函数 | 说明 |
|------|------|
| `init()` | `SharedMemory::at(SHMBASE).init()` |
| `has_pending()` | 检查 Channel 0 是否有消息 |
| `process_pending()` | 轮询 Channel 0，处理所有消息 |
| `send_message(msg)` | 向 Channel 1 发送 + `send_ipi_to_linux()` |
| `send_notification(id)` | 发送通知到 Channel 1 |

### 5.1 RPC 方法表

| method_id | 签名 | 说明 |
|-----------|------|------|
| 0 | `() -> u32` | ECHO: 返回固定值 |
| 1 | `(i32, i32) -> i32` | ADD: 两数相加 |

### 5.2 中断处理

`MachineSoft` ISR（在 `demo.rs` 中定义）：当 `PEND_MARKER == false`（外部 IPI，非调度器信号）时调用 `intercom::process_pending()`。

---

## 6. 完整通信流程

### 6.1 Notification（StarryOS → rt-async → StarryOS）

```
StarryOS 用户态                    rt-async (hart 1)
────────────────                   ──────────────────
1. msg = Notification(id=100)
2. ch0.try_send(&msg)
3. ioctl(NOTIFY)         ───────→  CLINT MSIP1 = 1
                                   MachineSoft ISR
                                   process_pending()
                                   handle_message(Notification)
                                   → send_notification(id)
                                   → ch1.try_send()
                                   → send_ipi_to_linux()
4. ioctl(AWAIT)          ←───────  CLINT MSIP0 = 1 → SSIP
   (阻塞等待)                       ipc_irq_handler() wake
5. ch1.try_recv()
6. resp.as_notification() → Some(100)
```

### 6.2 RPC（ADD 请求）

```
StarryOS 用户态                    rt-async (hart 1)
────────────────                   ──────────────────
1. req = Request(rid=2000, method=1, (10, 17))
2. ch0.try_send(&req)
3. ioctl(NOTIFY)         ───────→  process_pending()
                                   handle_request(1, msg)
                                   → as_request::<(i32,i32)>()
                                   → result = 10 + 17 = 27
                                   → resp = Response(2000, 27)
                                   → ch1.try_send()
                                   → send_ipi_to_linux()
4. ioctl(AWAIT)          ←───────  IPI
5. ch1.try_recv()
6. resp.as_response::<i32>() → (2000, 27)
```

---

## 7. OpenSBI Patch 说明

所有 patch 在 `patches/opensbi-amp.patch`，共修改 5 个文件：

| 文件 | 修改内容 |
|------|----------|
| `firmware/fw_base.S` | hart 1 → mret 到 0x82800000 (rt-async)，hart 0 正常启动 |
| `firmware/fw_dynamic.S` | `next_addr` 默认值改为 0x80200000 (StarryOS) |
| `Makefile` | 禁用 PIE（bare-metal ld 不支持），跳过 PIE 检查 |
| `lib/sbi/sbi_ipi.c` | `sbi_ipi_process()` 中当 `ipi_type == 0` 时设置 SSIP（转发直接 MSIP） |
| `lib/utils/ipi/aclint_mswi.c` | CLINT MSIP 寄存器允许 S/U mode 读写 |

---

## 8. 关键文件索引

| 文件 | 说明 |
|------|------|
| `amp.config` | 地址约定单一真相源 |
| `apps/rt-async-app/src/intercom.rs` | rt-async IPC 模块 |
| `apps/rt-async-app/src/bin/demo.rs` | rt-async 主程序（含 MachineSoft ISR） |
| `apps/user-test-ipc/src/main.rs` | StarryOS 用户态测试程序 |
| `modules/chip-qemu-virt-rt/src/lib.rs` | 芯片支持 + IPI 发送 |
| `StarryOS/kernel/src/pseudofs/dev/rt_shm.rs` | `/dev/rt_shm` 设备实现 |
| `patches/opensbi-amp.patch` | OpenSBI AMP 补丁 |
| `patches/qemu-uart1.patch` | QEMU 第二串口补丁 |
