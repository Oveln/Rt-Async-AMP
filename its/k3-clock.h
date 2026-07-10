/* SPDX-License-Identifier: MIT */
/*
 * K3 RT24 rcpu 时钟 ID（对应手册 17.4.2 RCPU 各时钟控制域）。
 *
 * 经设备树 clocks = <&ccu K3_CLK_xxx> 属性引用。CCU driver 的
 * CLK_TABLE（modules/chip-k3-rt24/src/clock.rs）用同值常量查表。
 *
 * ID 不连续（按域分段），便于将来扩充。值与 clock.rs 的 clk_id 模块一致。
 */

#ifndef K3_CLOCK_H
#define K3_CLOCK_H

/* R_UART0~5: RCPU_UARTCTRL(0xc088_1f00) + offset 0x00/0x04/... */
#define K3_CLK_RUART0   0
#define K3_CLK_RUART1   1
#define K3_CLK_RUART2   2
#define K3_CLK_RUART3   3
#define K3_CLK_RUART4   4
#define K3_CLK_RUART5   5

/* R_I2C0~2: RCPU_I2CCTRL(0xc088_6f00) + offset 0x00/0x04/0x08 */
#define K3_CLK_RI2C0    10
#define K3_CLK_RI2C1    11
#define K3_CLK_RI2C2    12

/* R_SSP0~1: RCPU_SPICTRL(0xc088_5f00) + offset 0x00/0x04 */
#define K3_CLK_RSSP0    20
#define K3_CLK_RSSP1    21

#endif /* K3_CLOCK_H */
