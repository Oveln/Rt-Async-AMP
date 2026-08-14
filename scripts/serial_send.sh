#!/usr/bin/env bash
# 经串口传输二进制文件到板上（板有 busybox base64，无编译环境/无网络时用）。
#
# 用法（host 端）：
#   bash scripts/serial_send.sh build/user-test-ipc /tmp/user-test-ipc
#
# 输出一段 shell 脚本到 stdout —— 粘贴到板子串口终端即可还原二进制：
#   1. base64 编码整文件（输出按 76 列换行，串口逐行消化不溢出）
#   2. heredoc 进 base64 -d > 目标文件
#   3. chmod +x
#
# 注意：粘贴大块文本到串口终端时，若 UART RX buffer 溢出会丢字节。
#   - screen:   启动后 :pasteDELAY 100  （或更慢）
#   - picocom:  picocom -b 115200 --send-delay 50 /dev/ttyUSB0
# 若仍丢字节，改用 XMODEM（板上 busybox rx）或分块传输（见文件尾注释）。
#
# 更可靠的选择（优先尝试）：
#   板上: busybox rx /tmp/user-test-ipc  → host 串口终端 XMODEM 发送

set -euo pipefail

FILE="${1:?usage: serial_send.sh <local-file> [remote-path]}"
REMOTE="${2:-/tmp/$(basename "$FILE")}"

[[ -f "$FILE" ]] || { echo "file not found: $FILE" >&2; exit 1; }

B64_SIZE=$(base64 "$FILE" | wc -c)
LINES=$(base64 "$FILE" | wc -l)

cat >&2 <<EOF
── serial_send ──────────────────────────────────────────────
  本地: $FILE ($(wc -c < "$FILE") 字节)
  板上: $REMOTE
  base64: $B64_SIZE 字节, $LINES 行（~$((B64_SIZE*10/115200))s @115200）
  粘贴以下输出到板子串口终端。
  若丢字节：screen :pasteDELAY 200 / picocom --send-delay 100
  或改用 XMODEM: 板上 busybox rx $REMOTE
─────────────────────────────────────────────────────────────
EOF

# 板上还原脚本：heredoc 喂 base64 -d。单引号 'SENDEOF' 禁止变量/命令替换，
# base64 字符集 [A-Za-z0-9+/=] 无 shell 元字符，安全。
echo "base64 -d << 'SENDEOF' > '$REMOTE'"
base64 "$FILE"
echo "SENDEOF"
echo "chmod +x '$REMOTE' && echo DONE: '$REMOTE' ready"
