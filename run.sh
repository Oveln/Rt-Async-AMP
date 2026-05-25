#!/bin/bash
# rt-async-amp QEMU 启动脚本
#
# 用法: ./run.sh [starryos.bin]

set -e

BUILD_DIR="build"
APP_BIN="${BUILD_DIR}/rt-async.bin"
FW="${BUILD_DIR}/fw_dynamic.bin"
STARRYOS="${1:-${BUILD_DIR}/starryos.bin}"
UART_LOG="${BUILD_DIR}/rt-async-uart.log"
QEMU_BIN="qemu/build/qemu-system-riscv64"
CONFIG="amp.config"

# 从 amp.config 读取约定值
read_config() {
    sed -n "s/^$1=//p" "$CONFIG" | head -1
}

RTASYNCBASE=$(read_config RTASYNCBASE)
QEMUSMP=$(read_config QEMUSMP)
QEMURAM=$(read_config QEMURAM)

if [ ! -f "$QEMU_BIN" ]; then
    echo "Error: Custom QEMU not found at $QEMU_BIN"
    echo "Run 'make qemu' first."
    exit 1
fi

if [ ! -f "$FW" ]; then
    echo "Error: OpenSBI firmware not found at $FW"
    echo "Run 'make opensbi' first."
    exit 1
fi

if [ ! -f "$APP_BIN" ]; then
    echo "Error: rt-async binary not found at $APP_BIN"
    echo "Run 'make rt-async' first."
    exit 1
fi

QEMU_ARGS=(
    -machine virt
    -display none
    -serial mon:stdio
    -serial "file:${UART_LOG}"
    -smp "$QEMUSMP"
    -m "$QEMURAM"
    -bios "$FW"
)

if [ -f "$STARRYOS" ]; then
    QEMU_ARGS+=(-kernel "$STARRYOS")
    echo "Loading: OpenSBI + rt-async @ $RTASYNCBASE + StarryOS (-kernel)"
else
    echo "Loading: OpenSBI + rt-async @ $RTASYNCBASE (no StarryOS)"
fi

QEMU_ARGS+=(-device loader,addr="$RTASYNCBASE",file="$APP_BIN")

ROOTFS="StarryOS/rootfs-riscv64.img"
if [ -f "$ROOTFS" ]; then
    QEMU_ARGS+=(
        -drive file="$ROOTFS",format=raw,if=none,id=hd0
        -device virtio-blk-pci,drive=hd0
    )
fi

echo "  UART0 (serial0) → stdio  (OpenSBI/StarryOS)"
echo "  UART1 (serial1) → ${UART_LOG}  (rt-async, tail -f to watch)"
echo "Starting QEMU..."
exec "$QEMU_BIN" "${QEMU_ARGS[@]}"
