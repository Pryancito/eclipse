/* Linker script for smithay_app (Eclipse compositor) - dirección fija para evitar PIE/reloc */

ENTRY(_start)

SECTIONS
{
    /* Carga en 256MB (0x10000000) - mismo layout que init y servicios del kernel */
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
