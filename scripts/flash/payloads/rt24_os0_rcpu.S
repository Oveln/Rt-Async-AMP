/* rt24_os0_rcpu.S — K3 RT24 rcpu0 握手 + wfi 占位固件。
 *
 * rcpu0 当前不运行完整 esos（隔离测试 rcpu1 时用本占位）。但 U-Boot
 * k3_rproc_start() case 0 启动 rcpu0 后会死等 RCPU_CORE1_BOOT_ENTRY_LO
 * 非 0（try_count=6000, mdelay(1), 最多 6s）。本固件在入口立即写 1 到该
 * 寄存器解锁轮询，然后永久 wfi。
 *
 * 交叉规则（见 uboot k3-rproc.c + chip-k3-rt24 handshake.rs）：
 *   rcpu0 → 写 CORE1_BOOT_ENTRY_LO (0xc088_008c) 通知 AP 就绪
 *   rcpu1 → 写 CORE0_BOOT_ENTRY_LO (0xc088_007c) 通知 AP 就绪
 *
 * 构建（从 scripts/flash/ 目录执行）：
 *   riscv64-unknown-elf-gcc -nostdlib -nostartfiles \
 *       -march=rv64imac -mabi=lp64 -Wl,-Ttext=0x100200000 \
 *       -o payloads/rt24_os0_rcpu.elf payloads/rt24_os0_rcpu.S
 */

    .section .text
    .global _start
_start:
    /* 写 1 到 RCPU_CORE1_BOOT_ENTRY_LO (0xc088_008c) —— 握手解锁 AP 轮询 */
    li      t0, 0xc088008c
    li      t1, 1
    sw      t1, 0(t0)

    /* 排空 store buffer，保证 AP readl 立即看到写入 */
    fence   iorw, iorw

    /* 永久 wfi：rcpu0 不参与 AMP 通信（mailbox3 留给未来 esos rpmsg） */
1:
    wfi
    j       1b
