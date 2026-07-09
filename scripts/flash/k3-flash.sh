#!/usr/bin/env bash
# k3-flash.sh — one-command K3 flashing for rt-async rcpu1 development.
#
#   ./scripts/flash/k3-flash.sh             # build + pack itb + flash + reset to =>
#   ./scripts/flash/k3-flash.sh --no-build  # skip build+pack, reflash existing itb
#   ./scripts/flash/k3-flash.sh --boot      # (legacy, now a no-op: flow always
#                                           #  ends stopped at U-Boot prompt)
#
# Pipeline:
#   1. cargo xtask build k3-sched-demo              (rcpu1 ELF)
#   2. k3-pack-itb.sh                               (cp ELF + lzo + mkimage → esos.itb)
#   3. k3-console.py ensure-uboot                   (reset, catch autoboot, land at =>)
#   4. fastboot usb 0 on the board                  (board enters fastboot gadget)
#   5. fastboot stage <itb> + Ctrl-C board back     (host stage, then leave fastboot)
#   6. mtd erase esos / mtd write esos $loadaddr    (program flash)
#   7. ensure-uboot                                 (reset, stop at => for next run)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

# load config (env vars take precedence over flash.conf defaults)
# shellcheck disable=SC1091
[ -f "$SCRIPT_DIR/flash.conf" ] && source "$SCRIPT_DIR/flash.conf" || true

CONSOLE=(python3 "$SCRIPT_DIR/k3-console.py")
ITB="$SCRIPT_DIR/esos.itb"

# ── arg parse ───────────────────────────────────────────────────────────────
BOOT=0
NO_BUILD=0
for a in "$@"; do
    case "$a" in
        --boot)    BOOT=1 ;;
        --no-build) NO_BUILD=1 ;;
        -h|--help)
            sed -n '2,11p' "$0"; exit 0 ;;
        *) echo "unknown arg: $a" >&2; exit 2 ;;
    esac
done

# ── preflight ───────────────────────────────────────────────────────────────
need() { command -v "$1" >/dev/null 2>&1 || { echo "✗ 缺少: $1" >&2; exit 1; }; }
need cargo
need fastboot
need python3
python3 -c 'import serial' 2>/dev/null || { echo "✗ 缺少 pyserial: pip3 install pyserial" >&2; exit 1; }

step() { printf '\n\033[1;36m▶ %s\033[0m\n' "$*"; }

# ── 1+2. build + pack ─────────────────────────────────────────────────────────
if [ "$NO_BUILD" -eq 0 ]; then
    step "build k3-sched-demo"
    cargo xtask build k3-sched-demo

    step "pack esos.itb (cp ELF + lzo + mkimage)"
    bash "$SCRIPT_DIR/k3-pack-itb.sh"
else
    step "skip build (--no-build); reusing $ITB"
    [ -f "$ITB" ] || { echo "✗ itb 不存在: $ITB (先去掉 --no-build)" >&2; exit 1; }
fi

# ── 3. ensure uboot prompt ──────────────────────────────────────────────────
step "ensure board at U-Boot prompt"
"${CONSOLE[@]}" ensure-uboot

# ── 4. board enters fastboot ────────────────────────────────────────────────
# `fastboot usb 0` is a BLOCKING command on the board — it does not return to
# `=>`. So we send it and confirm via host-side `fastboot devices` (handled
# inside enter-fastboot). `$loadaddr` is a literal string expanded by U-Boot.
step "board: fastboot -l \$loadaddr -s 0x100000 usb 0"
"${CONSOLE[@]}" enter-fastboot 0x100000

# ── 5. host stage + Ctrl-C board back to => ─────────────────────────────────
step "host: fastboot stage $ITB (then Ctrl-C board back to =>)"
"${CONSOLE[@]}" send-stage "$ITB"

# ── 6. program flash ────────────────────────────────────────────────────────
step "board: mtd erase esos"
"${CONSOLE[@]}" run "mtd erase esos"

step "board: mtd write esos \$loadaddr"
"${CONSOLE[@]}" run "mtd write esos \$loadaddr"

# ── 7. reset back into U-Boot so the new firmware is ready on next boot ──────
# ensure-uboot does: reset → spam 's' to catch the autoboot window → land at =>
# (with a CR to clear the 's' residue). This leaves the board stopped at the
# U-Boot prompt, new firmware freshly written to flash.
step "reset board, stop at U-Boot prompt"
"${CONSOLE[@]}" ensure-uboot

printf '\n\033[1;32m✓ done — board at U-Boot prompt, new firmware in flash\033[0m\n'
