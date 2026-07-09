/* msip_probe.ld — 与 esOS os1_rcpu/baremetal/baremetal.ld 一致。
 * ENTRY_POINT 由 Makefile 经 --defsym 注入 (=0x100804000)。 */

__STACKSIZE__ = 8192;

OUTPUT_ARCH( "riscv" )
ENTRY(_start)

MEMORY
{
   RAM : ORIGIN = ENTRY_POINT, LENGTH = 3M
}

SECTIONS
{
    . = ENTRY_POINT ;
    .text :
    {
        KEEP(*startup.o(*.text*))
        *(.text)
        *(.text.*)
        *(.rodata)
        *(.rodata*)
        . = ALIGN(8);
        _etext = .;
    } > RAM

    .data :
    {
        *(.data)
        *(.data.*)
        *(.sdata)
        *(.sdata.*)
        . = ALIGN(8);
        PROVIDE( __global_pointer$ = . + 0x800 );
    } > RAM

    .bss :
    {
        __bss_start__ = .;
        *(.bss)
        *(.bss.*)
        *(COMMON)
        __bss_end__ = .;
    } > RAM

    .stack :
    {
        . = ALIGN(8);
        . += __STACKSIZE__;
        __StackTop = .;
    } > RAM
}
