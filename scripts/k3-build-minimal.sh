#!/usr/bin/env bash
# 构建 K3 RT24 rcpu1 的 rt-async minimal 固件，产出可刷板的 flat binary。
#
# 产物：build/rt24_os1_rcpu_rtasync_k3.bin  (entry/link base 0x100804000)
#
# 刷板：把此 bin 在 esos 仓库侧 lzo 压缩后替换 output/esos/rt24_os1_rcpu.elf.lzo，
#       再 `./build.sh itb` 重打 esos.itb（rcpu1-fw 节点 load/entry 已是 0x100804000），
#       刷板启动，观察 R_UART0 串口应输出 "hello from rt-async"。
set -euo pipefail
cd "$(dirname "$0")/.."

echo "▶ cargo build (release) rt-async-k3 minimal..."
(cd apps/rt-async-k3 && cargo build --release --bin minimal)

mkdir -p build
echo "▶ objcopy → build/rt24_os1_rcpu_rtasync_k3.bin"
riscv64-elf-objcopy -O binary \
    target/riscv64imac-unknown-none-elf/release/minimal \
    build/rt24_os1_rcpu_rtasync_k3.bin

echo "✓ done: build/rt24_os1_rcpu_rtasync_k3.bin"
riscv64-elf-readelf -h target/riscv64imac-unknown-none-elf/release/minimal | grep "Entry point"
