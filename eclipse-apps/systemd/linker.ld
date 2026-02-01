/* Linker script for eclipse-systemd userland binary */

ENTRY(_start)

SECTIONS
{
    /* Load at 256MB (0x10000000) to avoid kernel compatibility issues */
    . = 0x10000000;

    .text : ALIGN(4K)
    {
        *(.text._start)
        *(.text .text.*)
    }

    .rodata : ALIGN(4K)
    {
        *(.rodata .rodata.*)
    }

    .data : ALIGN(4K)
    {
        *(.data .data.*)
    }

    .bss : ALIGN(4K)
    {
        *(.bss .bss.*)
        *(COMMON)
    }

    /DISCARD/ :
    {
        *(.eh_frame)
        *(.note*)
    }
}
