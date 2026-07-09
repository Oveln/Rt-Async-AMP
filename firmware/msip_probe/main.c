/*
 * msip_probe.c — RT24 rcpu1 MSIP 触发机制验证程序
 *
 * 目的：在 K3 RT24 rcpu1 上确定"如何触发一次 MachineSoft 中断
 *       (mcause=3 / mip.MSIP)"。
 *
 * 背景：rt-async 的调度器依赖 IPI 自中断 (pend() → 写 MSIP → MachineSoft
 *       ISR → 跑抢占式调度)。QEMU virt 上 MSIP 就是标准 CLINT MSIP
 *       (base+0，写 1 置位)。但 K3 RT24 没有 CLINT，只有一个 SysTimer 块
 *       @ 0xe4000000，且 esOS 的 clint.h 只给了 mtime(+0xbff8) / mtimecmp
 *       (+0x4000，stride hart<<27)，**完全没提 MSIP**。
 *
 * 因此 MSIP 的偏移/per-hart 步长都是未知。本程序逐一测试若干候选地址，
 * 对每个候选：写 1 → 看是否进入 MachineSoft ISR → 记录结果 → 清 0。
 *
 * ── 候选猜想 ────────────────────────────────────────────────────────
 *   A. NMSIS 标准 CLINT MSIP：  SYSTIMER + 0x1000 + hart*4
 *      （n308/Nuclei SDK 的 SysTimer_CLINT_MSIP_OFS = 0x1000）
 *   B. RT24 per-hart 窗口：      SYSTIMER + (hart<<27) + 0x1000
 *      （沿用 rt24 clint.h 的 mtimecmp stride 约定）
 *   C. 窗口基址偏移：            SYSTIMER + (hart<<27) + 0x0
 *   D. 直接基址：                SYSTIMER + 0x0   （CLINT msip hart0 标准位）
 *
 * 本程序跑在 rcpu1（hart_id=1），故hart<<27 = 1<<27 = 0x8000000。
 *
 * ── 判定方式 ────────────────────────────────────────────────────────
 *   每个候选触发后，spin 等待最多 ~10ms（用 SysTimer mtime 计时）。
 *   - 若 ISR 被触发：UART 打印 "[OK] 候选X 触发了 MSI"，并记入 SHM 面包屑。
 *     ISR 自己也会打印它的 mcause 供核对。
 *   - 若超时未触发：打印 "[--] 候选X 未触发"，尝试清 0 后继续下一个。
 *   mhartid 也打印出来，确认我们确实在 rcpu1。
 *
 * ── 启动模板 ────────────────────────────────────────────────────────
 *   完全复用 esOS os1_rcpu/baremetal 的启动链：握手回写 + UART0 时钟/pinmux/
 *   波特率/UUE（已验证可输出）。startup.S / baremetal.ld 不变，只是 trap_entry
 *   改成真正保存上下文 + 调 C handler（baremetal 版的 trap_entry 是死循环）。
 */

#include <stdint.h>

/* ── 常量（与 esOS baremetal main.c 一致，已验证）────────────────── */

#define UART0_BASE   0xc0881000
#define UART0_CLK_RST 0xc0881f00
#define RUART_14_CLK_CTRL 0xc088003c
#define RCPU_CORE0_BOOT_ENTRY_LO 0xc088007c
#define PINCTRL_BASE 0xd401e000

#define SHM_BREADCRUMB 0xc086c000  /* AP 可 md.l 回读的面包屑 */

/* SysTimer（rt24 clint.h） */
#define SYSTIMER_BASE 0xe4000000
#define SYSTIMER_MTIME (SYSTIMER_BASE + 0xbff8)          /* 共用，所有 hart */
/* mtimecmp = base + 0x4000 + (hart<<27)；本程序不使能定时器中断，仅用 mtime 计时 */

/* UART 寄存器偏移（PXA-uart，stride=4） */
#define THR 0x000
#define IER 0x004
#define FCR 0x008
#define LCR 0x00C
#define MCR 0x010
#define LSR 0x014
#define DLL 0x000
#define DLH 0x004

#define UART_IER_UUE  0x40
#define UART_MCR_OUT2 0x08
#define DIVISOR 8  /* 14.48MHz / (16*115200) ≈ 8 */

#define UART0_TX_PIN 122
#define UART0_RX_PIN 123
#define UART0_PIN_VAL 0xD044

/* ── MSIP 候选地址（hart_id=1）────────────────────────────────────── */
#define HART_SHIFT27 (1u << 27)  /* rcpu1 = hart 1 */
#define CAND_A  (SYSTIMER_BASE + 0x1000 + 1u*4)        /* NMSIS CLINT MSIP */
#define CAND_B  (SYSTIMER_BASE + HART_SHIFT27 + 0x1000)/* RT24 窗口 + NMSIP 偏移 */
#define CAND_C  (SYSTIMER_BASE + HART_SHIFT27 + 0x0)   /* RT24 窗口基址 */
#define CAND_D  (SYSTIMER_BASE + 0x0)                  /* CLINT msip hart0 标准位 */

/* ── 基本访存 ─────────────────────────────────────────────────────── */
static inline void write32(uintptr_t addr, uint32_t val) {
    *(volatile uint32_t *)addr = val;
}
static inline uint32_t read32(uintptr_t addr) {
    return *(volatile uint32_t *)addr;
}
static inline uint64_t read64(uintptr_t addr) {
    return *(volatile uint64_t *)addr;
}

static void delay(void) {
    for (volatile int i = 0; i < 10000; i++) { __asm__ volatile("nop"); }
}

/* ── UART（复用 esOS baremetal 序列）─────────────────────────────── */
static void uart_init(void) {
    /* 上游 ruart_14 DDN gate */
    write32(RUART_14_CLK_CTRL, read32(RUART_14_CLK_CTRL) | (1u << 31));
    delay();
    /* UART0 末端 gate */
    write32(UART0_CLK_RST, 0x00000003);
    delay();
    /* pinmux */
    write32(PINCTRL_BASE + UART0_TX_PIN*4, UART0_PIN_VAL);
    write32(PINCTRL_BASE + UART0_RX_PIN*4, UART0_PIN_VAL);
    /* 波特率 8N1 */
    write32(UART0_BASE + LCR, 0x80);
    write32(UART0_BASE + DLL, DIVISOR & 0xFF);
    write32(UART0_BASE + DLH, (DIVISOR >> 8) & 0xFF);
    write32(UART0_BASE + LCR, 0x03);
    write32(UART0_BASE + FCR, 0x07);
    /* UUE + OUT2（PXA 专属使能）*/
    write32(UART0_BASE + IER, UART_IER_UUE);
    write32(UART0_BASE + MCR, UART_MCR_OUT2);
}

static void uart_putc(char c) {
    if (c == '\n') uart_putc('\r');
    while (!(read32(UART0_BASE + LSR) & 0x20)) { }
    write32(UART0_BASE + THR, (uint32_t)c);
}
static void uart_puts(const char *s) {
    while (*s) uart_putc(*s++);
}

/* 简单 hex 打印（不依赖 printf）*/
static void uart_put_hex64(uint64_t v) {
    char buf[17];
    for (int i = 15; i >= 0; i--) {
        uint8_t d = v & 0xf;
        buf[i] = d < 10 ? '0' + d : 'a' + (d - 10);
        v >>= 4;
    }
    buf[16] = 0;
    uart_puts("0x"); uart_puts(buf);
}
static void uart_put_dec(uint32_t v) {
    char buf[11]; int i = 10; buf[i] = 0;
    if (v == 0) { uart_putc('0'); return; }
    while (v) { buf[--i] = '0' + (v % 10); v /= 10; }
    uart_puts(&buf[i]);
}

/* ── CSR 读写（内联汇编）────────────────────────────────────────── */
static inline uint64_t read_mhartid(void) {
    uint64_t v; __asm__ volatile("csrr %0, mhartid" : "=r"(v)); return v;
}
static inline uint64_t read_mcause(void) {
    uint64_t v; __asm__ volatile("csrr %0, mcause" : "=r"(v)); return v;
}
static inline uint64_t read_mip(void) {
    uint64_t v; __asm__ volatile("csrr %0, mip" : "=r"(v)); return v;
}
static inline void set_mie_msie(void) {
    __asm__ volatile("csrs mie, %0" :: "r"(1u<<3));  /* MIE_MSIE = bit3 */
}
static inline void clr_mie_msie(void) {
    __asm__ volatile("csrc mie, %0" :: "r"(1u<<3));
}
static inline void set_mstatus_mie(void) {
    __asm__ volatile("csrs mstatus, %0" :: "r"(1u<<3)); /* MSTATUS_MIE = bit3 */
}
static inline void clr_mstatus_mie(void) {
    __asm__ volatile("csrc mstatus, %0" :: "r"(1u<<3));
}

/* ── ISR 与主程序之间的通信（关中断下访问，单 hart 安全）────────── */
/* trap_handler 在 MachineSoft 时把 msip_hit_addr 置为触发它的候选地址。
 * 主程序据此判断哪个候选成功。0 = 未命中。 */
volatile uintptr_t msip_hit_addr = 0;

/* C trap handler，由 startup.S 的 trap_entry 调用（a0 = mepc，可选）。
 * 我们只关心 mcause==3 (MachineSoft)。其余一律忽略（避免复位）。*/
void trap_handler_c(void) {
    uint64_t cause = read_mcause();
    uint32_t code = (uint32_t)(cause & 0x7fffffffffffffffUL); /* 最高位 0=异常,1=中断 */
    if (code == 3) {
        /* MachineSoft — 标记命中（供主程序 spin 检测），并打印。
         * UART 是轮询输出，ISR 里安全。打印 mhartid 用于确认在 rcpu1。*/
        msip_hit_addr = 0xDEADBEEF;  /* 非 0 哨兵，让 probe_candidate 的 spin 命中 */
        uart_puts("\n  >>> [ISR] MachineSoft triggered on hart ");
        uart_put_dec((uint32_t)read_mhartid());
        uart_puts(", mip=");
        uart_put_hex64(read_mip());
        uart_puts("\n");
        /* 不在这里清 MSIP——主程序据此重测；由主程序写 0 清除。
         * 但若不清，开中断后会再次进 ISR（套娃）。故这里关掉 MSIE。*/
        clr_mie_msie();
    } else {
        /* 其它中断/异常：打印后忽略（不复位，继续测试）。*/
        uart_puts("\n  >>> [ISR] unexpected cause=");
        uart_put_hex64(cause);
        uart_puts("\n");
    }
}

/* ── 单个候选测试 ─────────────────────────────────────────────────── */
/* 返回 1 若该候选成功触发 MSI，0 否。 */
static int probe_candidate(const char *name, uintptr_t addr) {
    /* 清除可能的残留 pending：先写 0 */
    write32(addr, 0);
    /* 读回确认可写（不可写的地址常读回 0 或 0xffffffff）*/
    uint32_t rb = read32(addr);

    msip_hit_addr = 0;
    /* 开 MSIE + MIE，再写 1 触发 */
    set_mie_msie();
    set_mstatus_mie();   /* 全局中断使能 */

    uart_puts("probe "); uart_puts(name); uart_puts(" @ ");
    uart_put_hex64(addr);
    uart_puts(" (readback before="); uart_put_hex64(rb); uart_puts("): write 1 ... ");

    write32(addr, 1);

    /* spin 等待最多 ~10ms（SysTimer 24MHz → 240000 ticks）*/
    uint64_t start = read64(SYSTIMER_MTIME);
    uint64_t deadline = start + 240000;
    int hit = 0;
    while (1) {
        if (msip_hit_addr != 0) { hit = 1; break; }
        if ((read64(SYSTIMER_MTIME) - start) > (deadline - start)) break;
    }

    /* 无论命中与否，先关全局中断 + MSIE，再清 MSIP */
    clr_mstatus_mie();
    clr_mie_msie();
    write32(addr, 0);

    if (hit) {
        uart_puts("[OK] MSI triggered\n");
        return 1;
    } else {
        uart_puts("[--] no MSI (mip=");
        uart_put_hex64(read_mip());
        uart_puts(")\n");
        return 0;
    }
}

/* ── main ─────────────────────────────────────────────────────────── */
int main(void) {
    /* 1. SPL 握手回写（最先，解锁 AP）*/
    write32(RCPU_CORE0_BOOT_ENTRY_LO, 1);

    /* 2. UART */
    uart_init();

    write32(SHM_BREADCRUMB, 0x0BAD1000);

    uint64_t hart = read_mhartid();
    uart_puts("\n\n=== RT24 MSIP probe (hart=");
    uart_put_dec((uint32_t)hart);
    uart_puts(") ===\n");
    uart_puts("SYSTIMER_BASE="); uart_put_hex64(SYSTIMER_BASE);
    uart_puts(", mtime="); uart_put_hex64(read64(SYSTIMER_MTIME)); uart_puts("\n");
    uart_puts("(hart<<27)="); uart_put_hex64(HART_SHIFT27); uart_puts("\n");

    int results[4] = {0,0,0,0};

    results[0] = probe_candidate("A NMSIS +0x1000+hart*4", CAND_A);
    results[1] = probe_candidate("B (hart<<27)+0x1000   ", CAND_B);
    results[2] = probe_candidate("C (hart<<27)+0x0      ", CAND_C);
    results[3] = probe_candidate("D base+0x0            ", CAND_D);

    /* 3. 汇总 + 写面包屑供 AP 读 */
    uart_puts("\n=== SUMMARY ===\n");
    uart_puts("A="); uart_put_dec(results[0]);
    uart_puts(" B="); uart_put_dec(results[1]);
    uart_puts(" C="); uart_put_dec(results[2]);
    uart_puts(" D="); uart_put_dec(results[3]);
    uart_puts("\n");
    /* 编码进面包屑：0xPRBA 结果位图（A=bit0..D=bit3）*/
    uint32_t bitmap = (results[0]?1:0) | (results[1]?2:0) | (results[2]?4:0) | (results[3]?8:0);
    write32(SHM_BREADCRUMB,     0x0BAD1000 | bitmap);
    write32(SHM_BREADCRUMB + 4, hart);
    write32(SHM_BREADCRUMB + 8, CAND_A);
    write32(SHM_BREADCRUMB +12, CAND_B);
    write32(SHM_BREADCRUMB +16, CAND_C);
    write32(SHM_BREADCRUMB +20, CAND_D);

    uart_puts("done. spinning (AP: md.l c086c000 1 -> 0x0BAD100X bitmap)\n");
    while (1) { __asm__ volatile("wfi"); }
    return 0;
}
