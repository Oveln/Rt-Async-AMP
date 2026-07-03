#!/usr/bin/env bash
# 构建 K3 RT24 rcpu1 的 rt-async minimal 固件，产出可刷板的 ELF。
#
# 产物：build/rt-async-k3-minimal.elf  (entry/link base 0x100804000)
#
# 刷板：把此 ELF 在 esos 仓库侧 lzo 压缩后替换 output/esos/rt24_os1_rcpu.elf.lzo，
#       再 `./build.sh itb` 重打 esos.itb（rcpu1-fw 节点 load/entry 已是 0x100804000），
#       刷板启动，观察 R_UART0 串口应输出 "hello from rt-async"。
set -euo pipefail
cd "$(dirname "$0")/.."

echo "▶ cargo xtask build k3-minimal..."
cargo xtask build k3-minimal

echo "▶ 验证 ELF entry："
riscv64-elf-readelf -h build/rt-async-k3-minimal.elf | grep "Entry point"
