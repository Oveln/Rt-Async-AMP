/*
 * startup.S — RT24 rcpu1 MSIP probe 的启动 + trap 入口
 *
 * 基于 esOS os1_rcpu/baremetal/startup.S（握手/寄存器清零/bss 清零不变），
 * 只是把死循环 trap_entry 换成真正的"保存上下文 → 调 trap_handler_c →
 * 恢复上下文 → mret"。TrapFrame 布局参考 rt-async riscv64-rt/trap/entry.rs
 * （30 GP + mepc + mstatus），但这里因为不需要嵌套/调度，用最小集：
 * 只存 ra + a0-a7 + t0-t6 + mepc + mstatus（够 C handler 用，且 handler
 * 只调简单函数，不破坏 s* 寄存器约定——保险起见全部存）。
 *
 * 入口 _start 由 baremetal.ld ENTRY(_start) 指定，加载/入口 = 0x100804000。
 */

    .section ".text"
    .global _start
_start:
    csrw mie, 0
    csrw mip, 0
    la t0, trap_entry
    csrw mtvec, t0

    /* 清零通用寄存器 */
    li x1, 0
    li x2, 0
    li x3, 0
    li x4, 0
    li x5, 0
    li x6, 0
    li x7, 0
    li x8, 0
    li x9, 0
    li x10,0
    li x11,0
    li x12,0
    li x13,0
    li x14,0
    li x15,0
    li x16,0
    li x17,0
    li x18,0
    li x19,0
    li x20,0
    li x21,0
    li x22,0
    li x23,0
    li x24,0
    li x25,0
    li x26,0
    li x27,0
    li x28,0
    li x29,0
    li x30,0
    li x31,0

.option push
.option norelax
    la gp, __global_pointer$
.option pop

    la sp, __StackTop

    /* 清 bss */
    la a0, __bss_start__
    la a1, __bss_end__
    bgeu a0, a1, 2f
1:
    sw zero, (a0)
    addi a0, a0, 4
    bltu a0, a1, 1b
2:
    jal main

__exit:
    j __exit

    /*
     * trap_entry — 保存上下文，调 C handler，恢复，mret。
     * 栈帧 = 31 个 GP 寄存器 (x1..x31) + mepc + mstatus = 33 个 slot。
     * 排列：[ra, gp, tp, x5..x31, mepc, mstatus]，每个 8 字节，共 264 字节。
     * 强制 16 字节对齐。sp 原始值存到尾部 padding 区（与 rt-async 同思路）。
     */
    .align 4
trap_entry:
    mv t0, sp
    addi sp, sp, -288          /* 33*8=264 + 16 对齐余量 + 8 原始sp槽 → 288 够用 */
    andi sp, sp, -16
    sd t0, 280(sp)             /* 保存原始 sp */

    sd x1,   0(sp)
    sd x3,   8(sp)
    sd x4,  16(sp)
    sd x5,  24(sp)
    sd x6,  32(sp)
    sd x7,  40(sp)
    sd x8,  48(sp)
    sd x9,  56(sp)
    sd x10, 64(sp)
    sd x11, 72(sp)
    sd x12, 80(sp)
    sd x13, 88(sp)
    sd x14, 96(sp)
    sd x15,104(sp)
    sd x16,112(sp)
    sd x17,120(sp)
    sd x18,128(sp)
    sd x19,136(sp)
    sd x20,144(sp)
    sd x21,152(sp)
    sd x22,160(sp)
    sd x23,168(sp)
    sd x24,176(sp)
    sd x25,184(sp)
    sd x26,192(sp)
    sd x27,200(sp)
    sd x28,208(sp)
    sd x29,216(sp)
    sd x30,224(sp)
    sd x31,232(sp)

    csrr t0, mepc
    sd t0, 240(sp)
    csrr t0, mstatus
    sd t0, 248(sp)

    /* 调 C handler（无参；它自己读 mcause/mhartid）*/
    mv  a0, sp
    call trap_handler_c

    /* 恢复 CSR（先 mepc 再 mstatus，用 t0 临时）*/
    ld t0, 240(sp)
    csrw mepc, t0
    ld t0, 248(sp)
    csrw mstatus, t0

    ld x1,   0(sp)
    ld x3,   8(sp)
    ld x4,  16(sp)
    ld x5,  24(sp)
    ld x6,  32(sp)
    ld x7,  40(sp)
    ld x8,  48(sp)
    ld x9,  56(sp)
    ld x10, 64(sp)
    ld x11, 72(sp)
    ld x12, 80(sp)
    ld x13, 88(sp)
    ld x14, 96(sp)
    ld x15,104(sp)
    ld x16,112(sp)
    ld x17,120(sp)
    ld x18,128(sp)
    ld x19,136(sp)
    ld x20,144(sp)
    ld x21,152(sp)
    ld x22,160(sp)
    ld x23,168(sp)
    ld x24,176(sp)
    ld x25,184(sp)
    ld x26,192(sp)
    ld x27,200(sp)
    ld x28,208(sp)
    ld x29,216(sp)
    ld x30,224(sp)
    ld x31,232(sp)

    ld sp, 280(sp)
    mret
