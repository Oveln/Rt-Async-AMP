ENTRY(__start);

/* 单核测试: 0x80000000 (QEMU -kernel 默认跳转地址)
 * 双核 AMP: 改为 ORIGIN = 0x80800000，由 OpenSBI 路由
 */
MEMORY
{
    RAM : ORIGIN = 0x80800000, LENGTH = 32M
}

_max_hart_id = 0;
_hart_stack_size = 4096;
