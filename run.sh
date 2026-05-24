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
    -smp 2
    -m 256M
    -bios "$FW"
    -device loader,addr=0x80800000,file="$APP_BIN"
)

if [ -f "$STARRYOS" ]; then
    QEMU_ARGS+=(-device loader,addr=0x80200000,file="$STARRYOS")
    echo "Loading: OpenSBI + rt-async @ 0x80800000 + StarryOS @ 0x80200000"
else
    echo "Loading: OpenSBI + rt-async @ 0x80800000 (no StarryOS)"
fi

echo "  UART0 (serial0) → stdio  (OpenSBI/StarryOS)"
echo "  UART1 (serial1) → ${UART_LOG}  (rt-async, tail -f to watch)"
echo "Starting QEMU..."
exec "$QEMU_BIN" "${QEMU_ARGS[@]}"
