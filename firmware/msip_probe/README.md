# RT24 rcpu1 MSIP 触发机制验证程序

## 为什么需要这个

rt-async 的调度器依赖 **IPI 自中断**：`pend()` 写 MSIP → 触发 MachineSoft
中断（mcause=3）→ ISR 跑抢占式调度。QEMU virt 上 MSIP 就是标准 CLINT MSIP
（`0x2000000 + hart*4`，写 1 置位）。

但 K3 RT24 **没有 CLINT**，只有一个 SysTimer 块 `0xe4000000`，且 esOS 的
`clint.h` 只给了 `mtime`(+0xbff8) / `mtimecmp`(+0x4000，stride `hart<<27`)，
**完全没提 MSIP**。所以"RT24 上到底怎么触发一次 MachineSoft 中断"是未知。

本程序逐一测试若干候选地址，上板即可确定正确答案。

## 候选猜想

rt-async 跑在 **rcpu1（hart_id=1）**，故 `hart<<27 = 0x8000000`。

| 候选 | 地址 | 依据 |
|------|------|------|
| **A** | `0xe4000000 + 0x1000 + 1*4` = `0xe4001004` | NMSIS/Nuclei SDK 标准 CLINT MSIP 偏移（`SysTimer_CLINT_MSIP_OFS=0x1000`，stride `hart<<2`）|
| **B** | `0xe4000000 + (1<<27) + 0x1000` = `0xec001000` | RT24 per-hart 窗口（`hart<<27`）+ NMSIS 偏移 |
| **C** | `0xe4000000 + (1<<27) + 0x0` = `0xec000000` | RT24 窗口基址（标准 CLINT msip 就在 base+0）|
| **D** | `0xe4000000 + 0x0` = `0xe4000000` | 直接基址（CLINT msip hart0 标准位）|

每个候选：先写 0（清残留）→ 读回 → 开 `mie.MSIE` + `mstatus.MIE` → 写 1 →
spin 等 ≤10ms（用 SysTimer mtime 计时）看是否进 ISR → 关中断清 0。

## 构建

需要 `riscv64-elf-gcc`（Homebrew：`brew install riscv64-elf-gcc`）。

```bash
cd firmware/msip_probe
make
```

产物 `msip_probe.elf` / `.bin`。入口/加载地址 `0x100804000`（3MB 区），
与 esOS os1_rcpu 完全一致。

## 上板（两条路）

### 路线 1：塞进 esos 的 its（最省事，复用已有打包/烧录链）

`esos_rt24.its` 里把 `rcpu1-fw` 节点的 `data` 指向本程序的 `.elf.lzo`：

```
rcpu1-fw {
    ...
    load  = <0x1 0x804000>;
    entry = <0x1 0x804000>;
    data  = /incbin/("路径/msip_probe.bin.lzo");   /* lzo 压缩 */
};
```

压缩：`make lzo`（需 `lzop`）或 `lzop -f msip_probe.bin`。

然后照常 `mkimage -f esos_rt24.its ...` → 烧录 → 复位。

### 路线 2：U-Boot 直接加载

若不打包 itb，U-Boot 下：

```
# 把 .bin（或 .elf 的纯二进制段）tftp 到 AP 视图的 0x1804000
tftpboot 0x1804000 msip_probe.bin
# 通过 remoteproc 启动 rcpu1（具体命令取决于 K3 U-Boot 的 rproc 命令）
```

> 注意：`bootm` 不能直接加载 ELF；要么用 itb，要么用 `rproc load/start`
> 命令加载裸二进制。

## 看输出

K3 的 R_UART0（板子上对应调试串口），**115200 8N1**。

成功的话你会看到类似：

```
=== RT24 MSIP probe (hart=1) ===
SYSTIMER_BASE=0xe4000000, mtime=0x12345678
(hart<<27)=0x8000000
probe A NMSIS +0x1000+hart*4 @ 0xe4001004 (readback before=0x0): write 1 ...

  >>> [ISR] MachineSoft triggered on hart 1, mip=0x8
[OK] MSI triggered
probe B (hart<<27)+0x1000    @ 0xec001000 (readback before=0xffffffff): write 1 ... [--] no MSI (mip=0x0)
...

=== SUMMARY ===
A=1 B=0 C=0 D=0
done. spinning (AP: md.l c086c000 1 -> 0x0BAD100X bitmap)
```

## 判定结果

- **UART 输出**：看 SUMMARY 里哪个 `A/B/C/D=1`。**第一个 `=1` 的候选就是
  RT24 上触发 MachineSoft 的正确 MSIP 地址**，rt-async 的 IPI 驱动直接用。
- **AP 侧面包屑**（UART 不工作时仍可读）：在 U-Boot/Linux 里
  `md.l 0xc086c000 1`，低 4 位是结果位图（A=bit0..D=bit3）。例如
  `0x0BAD1001` 表示候选 A 命中。
- **ISR 行打印**：每条命中会打印 hart 号 + `mip`。确认 hart=1（确在 rcpu1）
  且 `mip` 的 bit3（MSIP）置位。
- **readback before=0xffffffff**：该地址不可写/无设备，候选必然落空，
  排除该方向。

## 万一全部失败

四个候选都没触发，说明 RT24 的 MachineSoft 既不在 SysTimer 窗口，
也不是标准 CLINT 布局。可能的退路：

1. **mailbox 自中断**：写 mailbox7（`0xc0760000`）给自己发消息，触发
   mailbox IRQ（int_src 66）走 **MachineExternal** 而非 MachineSoft。
   这条路 esOS 已证实可用（rcpu0↔rcpu1 走 mailbox7）。但 rt-async 调度器
   现在固定用 MachineSoft，需改成 MachineExternal 或复用。
2. **mstatus 自陷 / SIP 注入**：少数 CVA6 配置允许 M-mode 写 `mip` 的
   MSIP 位（`csrs mip, (1<<3)`）自触发。可加一个候选 E 测试。
3. **AP 协助**：让 AP（X100）经某个寄存器给 rcpu1 发软中断。需查 K3
   手册的"CPU 间中断"章节。

若走到这一步，建议先测候选 E（`csrs mip`），再考虑改用 mailbox 走
MachineExternal。

## 文件

| 文件 | 说明 |
|------|------|
| `main.c` | 主程序：UART + ISR + 4 个候选探测 |
| `startup.S` | 启动（握手/bss/mtvec）+ 真实 trap_entry（保存上下文→`trap_handler_c`→恢复→mret）|
| `msip_probe.ld` | 链接脚本（与 esOS baremetal 一致，ENTRY=0x100804000）|
| `Makefile` | `riscv64-elf-gcc` 构建 |
