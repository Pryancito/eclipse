/* Linker script for Eclipse OS userspace programs */

ENTRY(_start)

SECTIONS
{
    /* Userspace programs load    /* Load at 64MB to avoid kernel compatibility issues */
    . = 0x4000000;

    .text : ALIGN(4K)
    {
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
        *(.note .note.*)
    }
}
