# K3 刷写自动化

一条命令把 rt-async rcpu1 固件刷进 K3 板子（`k3_com260_ifx`）。覆盖原来手动跨三个上下文
（编译 → itb 打包 → fastboot/mtd 串口刷写）的流程。

**完全自包含**：rcpu0 的 esos ELF、板子 dtb、ITS 模板都内置在 `scripts/flash/` 下，
不依赖 esos 仓库或 `output/esos/` 目录。

## 目录结构

```
scripts/flash/
├── k3-flash.sh                  # 主编排：一键编译+打包+刷写
├── k3-pack-itb.sh               # itb 打包（自包含）
├── k3-console.py                # pyserial 串口助手
├── flash.conf                   # 串口配置（编辑这个）
├── esos_k3_com260_ifx.its       # 精简 ITS 模板（仅 com260_ifx 板型，5 节点）
├── payloads/                    # 固定 payload（入库）
│   ├── rt24_os0_rcpu.elf        #   rcpu0 esos 固件（固定复用）
│   ├── k3_rt240_com260_ifx.dtb  #   rcpu0 设备树（本板型）
│   ├── k3_rt241_com260_ifx.dtb  #   rcpu1 设备树（本板型）
│   └── null.spacemit            #   AP 交互 blob
├── esos.itb                     # 生成物（gitignore）
└── README.md
```

`payloads/rt24_os1_rcpu.elf`（rcpu1 rt-async 固件）和所有 `.lzo` 文件由
`k3-pack-itb.sh` 每次生成，不入库（见 `.gitignore`）。

## 前置依赖

| 工具 | 安装 | 用途 |
|---|---|---|
| cargo（rust） | 本项目 toolchain | 编译 rcpu1 ELF |
| mkimage | `brew install u-boot-tools` | 打包 itb |
| lzop | `brew install lzop` | 压缩 itb payload |
| fastboot | `brew install android-platform-tools` | stage itb 到板子 |
| pyserial | `pip3 install pyserial` | 驱动串口 |

macOS 自带 python3。

## 串口接线

板子通过两路 USB 串口连主机：

| 设备 | 角色 | 用途 |
|---|---|---|
| `/dev/tty.usbmodem62B68F06E7BF1` | 主 UART | **U-Boot 控制台**（fastboot、mtd、reset） |
| `/dev/tty.usbserial-114120` | RUART | **rt-async 固件日志**（当前脚本未用，可 tail-log 调试） |

> 跑脚本前**关掉**占用主 UART 的 picocom/screen/minicom 会话，否则 pyserial 打开会报
> `Resource busy`：
> ```
> pkill -f 'picocom /dev/tty.usbmodem' ; pkill -f 'screen /dev/tty.usbmodem'
> ```

## 配置

`flash.conf` 默认值通常无需改（串口设备名已配好）。loadaddr 由 U-Boot 自己的环境变量
`$loadaddr` 决定（脚本发字面字符串 `$loadaddr`，U-Boot 展开），无需配置。如需查看板子当前值：
```
=> printenv loadaddr
```

## 用法

从仓库根：

```bash
# 全流程：编译 + 打包 itb + 刷写 + reset 停在 U-Boot
./scripts/flash/k3-flash.sh

# 跳过编译/打包，只重刷已有的 esos.itb（快速重刷）
./scripts/flash/k3-flash.sh --no-build
```

## 单独调试每个组件

每个组件都可单独跑，方便定位问题：

```bash
# 跟 U-Boot 对话（发一条命令，看回显）
./scripts/flash/k3-console.py run "help"
./scripts/flash/k3-console.py run "mtd list"

# 复位板子并停在 U-Boot（抓 autoboot 窗口）
./scripts/flash/k3-console.py ensure-uboot

# 单独打包 itb（不刷板）
./scripts/flash/k3-pack-itb.sh

# 只 tail RUART 固件日志
./scripts/flash/k3-console.py tail-log
```

## 流程详解

`k3-flash.sh` 编排以下步骤，每步失败即中止并提示卡在哪：

1. **`cargo xtask build k3-sched-demo`** → `build/rt-async-k3-sched-demo.elf`
2. **`k3-pack-itb.sh`**（自包含）：
   - cp 新 rcpu1 ELF → `payloads/rt24_os1_rcpu.elf`
   - lzo 压缩 `payloads/` 里的 `*.elf` 和 `null.spacemit`
   - `mkimage -f esos_k3_com260_ifx.its esos.itb`（在 `scripts/flash/` 下执行）
   - rcpu0 的 esos ELF 复用 `payloads/rt24_os0_rcpu.elf`（固定，不重建）
3. **`ensure-uboot`**：发 `reset`，抓 autoboot 窗口（极短，周期发 `s` 兜底）到 `=>`，回车清 `ssss` 残留
4. 板子进 fastboot：发 `fastboot -l $loadaddr -s 0x100000 usb 0`，host 端轮询 `fastboot devices` 确认
5. 主机 `fastboot stage esos.itb`，然后向板子发 Ctrl-C 退出 fastboot 回 `=>`
6. `mtd erase esos` + `mtd write esos $loadaddr`
7. **`ensure-uboot`**：reset 停在 U-Boot，新固件已写 flash

## 常见问题

- **串口 `Resource busy`**：关掉占用该串口的 picocom/screen（见上文 pkill）。
- **autoboot 没抓住，直接 boot 进系统了**：窗口极短，脚本已周期发 `s` 兜底。
  若仍漏，确认 `K3_AUTOBOOT_RE` 正则匹配你板子的 autoboot 文本；或把板子的
  `bootdelay` 调大（`setenv bootdelay 5; saveenv`）。
- **`fastboot stage` 后卡在 fastboot 模式**：脚本已发 3 次 Ctrl-C。若仍不行，
  手动在串口按 Ctrl-C 确认行为，必要时调整 `k3-console.py` 里 Ctrl-C 次数。
- **`mtd write` 看不到反馈**：已修复——发命令前清空缓冲，确保等到 `mtd write`
  真正完成（`Writing ...` 反馈）后再继续。
- **换板型**：当前 ITS 只含 `com260_ifx` 节点。换其他板型需把对应 dtb 拷进
  `payloads/` 并改 `esos_k3_com260_ifx.its` 的 dtb 节点 load 地址 + loadables 列表。

## 附录：被否决的替代路线（供参考）

开发此脚本时调研过两条"更快"的路线，均被否决，记录结论：

### A. RT24 软复位热重载（否决）

设想：板子停在 U-Boot 后，不重刷 flash，直接用 `mw.l` 复位 rcpu1、重载 ELF 到
RAM 秒级重启固件。**实测走不通**，根因（来自 uboot-2022.10 + esos 源码）：

- `RT24_CORE1_SW_RESET_REG`(0xc088c0d0) + `SW_WAKEUP`(0xc088c0d8) 是
  **halt/run + sleep/wake（PC 保留）**，不是让核从 boot-entry 重新取指的复位。
- 它们实际是 **rcpu0 ↔ rcpu1 协作式低功耗握手**：rcpu1 需先自己经
  `IDLE_CFG |= 0x3`(0xc088c0e0) 投票掉电进深睡，rcpu0(HSM) 才能用
  SW_RESET+WAKEUP 把它唤醒到指定 BOOT_ENTRY。esos 的 rt-async 固件没实现这套
  深睡配合（只进普通 `wfi`），所以 AP 单方面操作无效。
- `PMU_AUDIO_CLK_CTRL`(0xd428294c) 掉电-上电整个 audio/RT24 域：rcpu0/rcpu1
  共享，且实测掉电后核 PC 进入未定义态，不能干净地从 boot-entry 重启。
- U-Boot 驱动 `k3_rproc_stop`/`k3_rproc_reset` 是 `TODO` 空桩——厂商自己在 AP 侧
  没实现单核软复位。参考实现只在 esos 的 `bsp/spacemit/drivers/rpmi/spacemit-hsm.c`
  里，且依赖 rcpu0 协作。

### B. JTAG 直 load（否决，成本过高）

设想：用 JLink + openocd(`k3_rcpu.cfg`) + GDB `load` 把 ELF 直接 load 到
0x100804000 秒级运行。否决原因：

- K3 没有专用 JTAG 接口，JTAG **复用 SD 卡槽（MMC1）引脚**（MUX_MODE5），
  见 `uboot-2022.10/board/spacemit/k3/spl.c:75-99`。
- 该复用需在 SPL 设备树里开 `spacemit,enable-debug-jtag`（`k3_spl.dts:28` 默认注释），
  **需改 SPL 并重刷**（回到 mtd 流程），形成鸡生蛋。
- 还要做 SD 卡形状的 JTAG 转接卡、装 openocd+gdb、实测能否 halt 住 rcpu1（其
  JTAG TAP 能否在 SPL 之后寻址 RT24 核未公开验证）。

故选 Route B：自动化现有可靠的 itb+mtd 流程，并做成自包含。
