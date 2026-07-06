#!/usr/bin/env bash
# Pack a new rcpu1 ELF into esos.itb —— 完全自包含，不依赖 esos 仓库或 output/esos/。
#
# 所有 payload 都在 scripts/flash/payloads/ 下：
#   - rt24_os0_rcpu.elf         (rcpu0 esos 固件，固定复用)
#   - k3_rt240_com260_ifx.dtb   (本板型 rcpu0 设备树，固定复用)
#   - k3_rt241_com260_ifx.dtb   (本板型 rcpu1 设备树，固定复用)
#   - null.spacemit             (AP 交互 blob，固定复用)
#   - rt24_os1_rcpu.elf         (rcpu1 rt-async 固件，每次从本仓库 build/ 拷入)
#
# ITS 模板用本目录下的 esos_k3_com260_ifx.its（精简版，仅 com260_ifx 节点）。
# mkimage 在本目录（scripts/flash/）执行，ITS 的 incbin 路径 payloads/... 相对于此。
#
# 输出：scripts/flash/esos.itb
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PAYLOADS="$SCRIPT_DIR/payloads"
ITS="$SCRIPT_DIR/esos_k3_com260_ifx.its"
ITB_OUT="$SCRIPT_DIR/esos.itb"

ELF_SRC="${ELF_SRC:-build/rt-async-k3-minimal.elf}"   # 相对 repo 根

# ── 0. preflight ────────────────────────────────────────────────────────────
for t in mkimage lzop; do
    if ! command -v "$t" >/dev/null 2>&1; then
        echo "✗ 缺少工具: $t (brew install u-boot-tools lzop)" >&2
        exit 1
    fi
done

[ -f "$ITS" ] || { echo "✗ ITS 模板不存在: $ITS" >&2; exit 1; }
[ -d "$PAYLOADS" ] || { echo "✗ payload 目录不存在: $PAYLOADS" >&2; exit 1; }

# 固定 payload 自检
for f in rt24_os0_rcpu.elf k3_rt240_com260_ifx.dtb k3_rt241_com260_ifx.dtb null.spacemit; do
    [ -f "$PAYLOADS/$f" ] || { echo "✗ 固定 payload 缺失: $PAYLOADS/$f" >&2; exit 1; }
done

# 新 rcpu1 ELF
RCPU1_ELF_SRC="${REPO_ROOT}/${ELF_SRC}"
[ -f "$RCPU1_ELF_SRC" ] || {
    echo "✗ rcpu1 ELF 缺失: $RCPU1_ELF_SRC" >&2
    echo "  先跑: cargo xtask build k3-minimal" >&2
    exit 1
}

# ── 1. 拷贝新 rcpu1 ELF 到 payloads/ ──────────────────────────────────────────
echo "▶ cp rcpu1 ELF → payloads/rt24_os1_rcpu.elf"
cp "$RCPU1_ELF_SRC" "$PAYLOADS/rt24_os1_rcpu.elf"

# ── 2. lzo 压缩 payloads/ 下的 *.elf 和 null.spacemit ────────────────────────
# ITS 里：rcpu0-fw/rcpu1-fw/rcpu-data-null 用 .lzo；两个 com260_ifx dtb 是
# compression="none"，直接用原始 .dtb。所以只压缩 *.elf 和 null.spacemit。
# lzop -9 -f 保留原文件，只生成 .lzo 副本。
echo "▶ lzo compress payloads/"
cd "$PAYLOADS"
for f in rt24_os0_rcpu.elf rt24_os1_rcpu.elf null.spacemit; do
    lzop -9 -f "$f" >/dev/null
done

# 自检 ITS 需要的 .lzo 都在
for need in rt24_os0_rcpu.elf.lzo rt24_os1_rcpu.elf.lzo null.spacemit.lzo; do
    [ -f "$need" ] || { echo "✗ 压缩后缺失: $need" >&2; exit 1; }
done

# ── 3. mkimage（在本目录执行，ITS 的 payloads/... 相对路径才对）──────────────
echo "▶ mkimage (cwd=$SCRIPT_DIR)"
cd "$SCRIPT_DIR"
mkimage -f "$ITS" "$ITB_OUT" >&2

[ -f "$ITB_OUT" ] || { echo "✗ mkimage 未产出 itb: $ITB_OUT" >&2; exit 1; }
echo "✓ ITB: $ITB_OUT ($(stat -f%z "$ITB_OUT" 2>/dev/null || stat -c%s "$ITB_OUT") bytes)"
