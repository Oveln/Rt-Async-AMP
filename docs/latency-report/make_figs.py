#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""K3 IPC 延迟归因战役——可视化（2026-08-16 → 08-21 板上实测数据）。

数据来源：user-test-bench dd/mb/s1/s2 场景 + rtbench + litmus，
全部为板上实测（闭环残差 0.0 校验）。单位如无说明均为 µs。
运行：python3 make_figs.py   （在 docs/latency-report/ 下生成 5 张 PNG）
"""
import matplotlib
import matplotlib.pyplot as plt
import numpy as np

matplotlib.rcParams["font.family"] = "Noto Sans CJK SC"
matplotlib.rcParams["axes.unicode_minus"] = False

# 调色板：按「消耗类别」统一着色（全文档一致，方便讲解）
C_DATA = "#4CAF50"    # 数据搬运（裸访存）
C_FENCE = "#FF9800"   # 内存序 fence（Acquire/Release 载荷）
C_TIMER = "#E91E63"   # 计时器（mtime/mcycle 冷读税）
C_BELL = "#2196F3"    # 门铃/中断/寄存器
C_COMP = "#9C27B0"    # 计算（postcard/dispatch）
C_AP = "#607D8B"      # AP 侧段
C_EST = "#B0BEC5"     # 估计值
C_TRUE = "#37474F"    # 真实执行

# ============================================================================
# 图 1：D1 路径 rtt=240µs 瀑布分解（dd 场景闭环恒等式，六段精确闭合）
# ============================================================================
def fig1_budget():
    # D1（睡眠唤醒路径）六段（dd n=30, P1 后, 闭环残差 0.0）
    segs = [
        ("AP 用户态发送 send",        8.5,  C_AP),
        ("ISR 舞步 ddrain",           3.6,  C_BELL),
        ("trap+调度+MSIP 落地 ddisp", 27.1, C_BELL),
        ("发现前缀 dpre",             24.3, C_TIMER),
        ("取包 try_recv drx",         45.6, C_FENCE),
        ("分发反序列化 dserde",       38.0, C_COMP),
        ("服务尾段+响应+门铃 S",      67.7, C_FENCE),
        ("AP 回程唤醒 APret",         25.3, C_AP),
    ]
    total = sum(v for _, v, _ in segs)
    assert abs(total - 240.0) < 0.2, total

    fig, ax = plt.subplots(figsize=(11, 4.2))
    left = 0
    for name, v, c in segs:
        ax.barh(0, v, left=left, color=c, edgecolor="white", height=0.55)
        ax.text(left + v / 2, 0.28, f"{name}\n{v:.1f}", ha="center", va="bottom",
                fontsize=8.5, rotation=0, linespacing=1.1)
        left += v
    ax.set_xlim(0, 245)
    ax.set_ylim(-0.75, 1.35)
    ax.set_yticks([])
    ax.set_xlabel("时间 (µs)")
    ax.set_title("图1  D1 路径单条消息往返 240µs 完整分解（dd 闭环恒等式，六段精确闭合）\n"
                 "dpre/drx/dserde 的分段值被 mtime 戳税污染，真实执行见 图3", fontsize=11)
    ax.axvline(total, color="k", lw=0.8)
    ax.text(total, -0.72, f"rtt = {total:.1f} µs", ha="right", fontsize=10, weight="bold")
    ax.spines[["top", "right", "left"]].set_visible(False)
    fig.tight_layout()
    fig.savefig("01_budget_waterfall.png", dpi=150)

# ============================================================================
# 图 2：每种操作的实测单价（log 轴，按类别着色）
# ============================================================================
def fig2_unit_price():
    ops = [
        # (名称, 单价 µs, 类别色)
        ("裸读 SRAM 同址（合并）",   0.022, C_DATA),
        ("裸读 SRAM 顺序跨行",        0.195, C_DATA),
        ("裸读 SRAM 冷跨行",          0.45, C_DATA),
        ("256B 槽整块读",              1.2,  C_DATA),
        ("256B 槽整块写",              6.3,  C_DATA),
        ("本地原子 RMW（CS 后端）",    0.09, C_FENCE),
        ("Acquire 原子读（ld+fence）", 2.2,  C_FENCE),
        ("Release 原子写（fence+sd）", 2.2,  C_FENCE),
        ("纯 fence",                   2.1,  C_FENCE),
        ("mtime MMIO 读（热循环）",    0.106, C_TIMER),
        ("mtime MMIO 读（间隔>15µs）", 24.0, C_TIMER),
        ("mcycle CSR 读（间隔态）",    3000.0, C_TIMER),
        ("mailbox 寄存器读",           0.18, C_BELL),
        ("门铃 notify（fence+MMIO）",  3.5,  C_BELL),
        ("postcard 构+解 双向",        9.4,  C_COMP),
        ("dispatch 全程（含 handler）", 18.2, C_COMP),
    ]
    names = [o[0] for o in ops][::-1]
    vals = [o[1] for o in ops][::-1]
    cols = [o[2] for o in ops][::-1]
    fig, ax = plt.subplots(figsize=(10, 7))
    y = np.arange(len(ops))
    ax.barh(y, vals, color=cols, edgecolor="white")
    ax.set_yticks(y, names, fontsize=9)
    ax.set_xscale("log")
    ax.set_xlabel("单价 (µs, 对数刻度)")
    ax.set_title("图2  每种操作的实测单价——跨度 5 个数量级\n"
                 "同址读 22ns ↔ mcycle 冷读 3ms；核心税：fence 2.2µs、mtime 冷读 24µs", fontsize=11)
    for yi, v in zip(y, vals):
        ax.text(v * 1.15, yi, f"{v:g}", va="center", fontsize=8.5)
    ax.set_xlim(0.015, 9000)
    from matplotlib.patches import Patch
    ax.legend(handles=[Patch(color=C_DATA, label="数据搬运（裸访存）"),
                       Patch(color=C_FENCE, label="内存序 fence"),
                       Patch(color=C_TIMER, label="计时器读"),
                       Patch(color=C_BELL, label="门铃/寄存器"),
                       Patch(color=C_COMP, label="计算")],
              loc="lower right", fontsize=9)
    ax.grid(axis="x", ls=":", alpha=0.5)
    fig.tight_layout()
    fig.savefig("02_op_unit_price.png", dpi=150)

# ============================================================================
# 图 3：RP 侧每消息账本——实测（戳污染）vs 真实执行 vs 优化后
# ============================================================================
def fig3_rp_budget():
    # 左柱：实测分段（mtime 戳税混入，dd 实测）
    meas = [("dpre 实测 24.3", 24.3, C_TIMER), ("drx 实测 45.6", 45.6, C_FENCE),
            ("dserde 实测 38.0", 38.0, C_COMP)]
    # 中柱：剥离 mtime 税后的真实执行（每段减去段边界那笔冷 mtime ≈24µs 的执行）
    real = [
        ("弹性前缀（set_busy+ch2 查）", 11.0, C_FENCE),
        ("取包（4 fence+槽读）",        19.8, C_FENCE),
        ("分发（match+postcard）",      11.0, C_COMP),
        ("响应发送（4 fence+槽写）",    15.1, C_FENCE),
        ("门铃 notify",                  3.4, C_BELL),
        ("ch2 收尾检查",                 6.6, C_FENCE),
        ("生产计时税（mtime 1-2 笔冷）", 24.0, C_TIMER),
    ]
    # 右柱：P3+fence 清理后的目标（magic 缓存/自产索引 Relaxed/自旋瘦身）
    opt = [
        ("取包（2 fence+槽读）",        14.6, C_FENCE),
        ("分发",                        11.0, C_COMP),
        ("响应发送（2 fence+槽写）",    10.7, C_FENCE),
        ("门铃 notify",                  3.4, C_BELL),
        ("计时税（采样制后）",           2.0,  C_TIMER),
    ]
    bars = [
        ("实测分段\n（mtime 戳污染）", meas),
        ("真实执行\n（剥离戳税）", real),
        ("P3 优化后\n（目标）", opt),
    ]
    fig, ax = plt.subplots(figsize=(11, 6))
    for i, (title, segs) in enumerate(bars):
        bottom = 0
        for name, v, c in segs:
            ax.bar(i, v, bottom=bottom, color=c, edgecolor="white", width=0.55)
            if v >= 8:
                ax.text(i, bottom + v / 2, f"{name}\n{v:.1f}", ha="center", va="center",
                        fontsize=7.8, color="white", weight="bold")
            bottom += v
        ax.text(i, bottom + 2, f"Σ={bottom:.1f}", ha="center", fontsize=10, weight="bold")
    ax.set_xticks(range(len(bars)), [b[0] for b in bars], fontsize=10)
    ax.set_ylabel("µs")
    ax.set_title("图3  RP 侧每条消息的账本\n"
                 "左：dd 实测三段（每段多出 ~24µs 的 mtime 冷读税）\n"
                 "中：剥离后的真实执行 ≈91µs（含生产计时税 24）\n"
                 "右：P3 优化后目标 ≈42µs", fontsize=11)
    ax.set_ylim(0, 130)
    ax.grid(axis="y", ls=":", alpha=0.5)
    ax.spines[["top", "right"]].set_visible(False)
    fig.tight_layout()
    fig.savefig("03_rp_message_budget.png", dpi=150)

# ============================================================================
# 图 4：mtime 陷阱与 H8 衰减
# ============================================================================
def fig4_mtime_trap():
    fig, (ax1, ax2) = plt.subplots(1, 2, figsize=(11, 4.2))
    # 左：读计时器的单价 vs 间隔
    xs = [0.1, 7.8, 20, 20000]
    ys = [0.106, 1.0, 24.0, 24.0]
    ax1.plot([0.1, 7.8, 20, 50], [0.106, 1.0, 24, 24], "o-", color=C_TIMER, lw=2)
    ax1.set_xscale("log"); ax1.set_yscale("log")
    ax1.set_xlabel("距上一笔读的间隔 (µs, log)")
    ax1.set_ylabel("单笔读耗时 (µs, log)")
    ax1.set_title("mtime 读的「间隔税」\n热 106ns → 间隔 20µs 后 24µs（231×）\n（mcycle 更慢：~3ms/笔，CSR 域同病）", fontsize=10)
    ax1.axvspan(15, 60, color=C_TIMER, alpha=0.08)
    ax1.text(16, 0.15, "深睡阈值\n~15µs", fontsize=8.5, color=C_TIMER)
    ax1.grid(ls=":", alpha=0.5)
    # 右：H8 新鲜写衰减（修正列错位后的有效数据）
    D = [0, 30, 300, 1000, 3000, 10000, 50000]
    t = [23.0, 14.2, 12.6, 11.7, 11.8, 11.8, 11.6]
    ax2.semilogx([max(d, 1) for d in D], t, "o-", color=C_DATA, lw=2, label="FRESH 单笔 try_recv")
    ax2.axhline(11.7, ls="--", color="gray", lw=1)
    ax2.text(2000, 12.1, "基线 11.7（recv_seq 复刻价）", fontsize=8.5, color="gray")
    ax2.annotate("D=0 的 +11.4µs\n大半是计时戳假象\n（真新鲜税 ~几 µs）",
                 xy=(1, 23.0), xytext=(150, 21), fontsize=8.5, color="#333",
                 arrowprops=dict(arrowstyle="->", color="#333"))
    ax2.set_xlabel("AP 写入 → RP 收取的间隔 D (µs, log)")
    ax2.set_ylabel("单笔 try_recv (µs)")
    ax2.set_title("「新鲜写衰减」扫描（H8）\n证伪了 posted-写落地延迟主因说", fontsize=10)
    ax2.grid(ls=":", alpha=0.5)
    fig.suptitle("图4  两个关键陷阱的实测曲线", fontsize=11, y=1.02)
    fig.tight_layout()
    fig.savefig("04_mtime_trap.png", dpi=150, bbox_inches="tight")

# ============================================================================
# 图 5：优化路线收益叠加（D1/D2 两条轨迹）
# ============================================================================
def fig5_roadmap():
    fig, ax = plt.subplots(figsize=(11, 5))
    # ISR 直派已否决（2026-08-21 用户决策：保 rt-async 任务模型，实际处理留
    # task 上下文），从轨迹中移除该步。
    steps = ["P1 现状\n(08-20)", "计时瘦身\n(P1.5)", "fence 去冗余\n(P3)", "双向轮询\n(W2)"]
    d1 = [240, 205, 195, 184]
    d2 = [189, 155, 145, 134]
    x = np.arange(len(steps))
    ax.plot(x, d1, "o-", lw=2.2, color=C_AP, label="D1 路径（睡眠唤醒）")
    ax.plot(x, d2, "s-", lw=2.2, color=C_FENCE, label="D2 路径（弹性自旋）")
    for xi, (a, b) in enumerate(zip(d1, d2)):
        ax.annotate(f"{a}", (xi, a), textcoords="offset points", xytext=(0, 8),
                    ha="center", fontsize=9, color=C_AP)
        ax.annotate(f"{b}", (xi, b), textcoords="offset points", xytext=(0, -14),
                    ha="center", fontsize=9, color=C_FENCE)
    ax.set_xticks(x, steps, fontsize=9.5)
    ax.set_ylabel("rtt (µs)")
    ax.set_title("图5  优化路线收益叠加（预期值，µs）\n"
                 "计时瘦身=每消息 1-2 笔冷 mtime 税（24-48µs，生产构建立即可做）\n"
                 "P3=fence 4→2 笔/消息 + 自旋 6→2 笔/轮；W2 已实测 −11µs（spin-await）；ISR 直派已否决——处理留 task（保 rt-async 模型）", fontsize=10.5)
    ax.grid(ls=":", alpha=0.5)
    ax.legend(fontsize=10)
    ax.set_ylim(110, 260)
    ax.spines[["top", "right"]].set_visible(False)
    fig.tight_layout()
    fig.savefig("05_roadmap.png", dpi=150)

if __name__ == "__main__":
    for f in (fig1_budget, fig2_unit_price, fig3_rp_budget, fig4_mtime_trap, fig5_roadmap):
        f()
        print(f"{f.__name__} ✓")
    print("全部图已生成")
