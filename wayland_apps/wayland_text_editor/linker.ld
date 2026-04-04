/* Linker script for wayland_text_editor - dirección fija para evitar PIE/reloc */

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
        *(.data .data.* .ldata .ldata.* .sdata .sdata.*)
    }

    .bss : ALIGN(4K)
    {
        KEEP(*(.keep.heap))
        *(.bss .bss.* .lbss .lbss.* .sbss .sbss.*)
        *(COMMON)
    }

    /DISCARD/ :
    {
        *(.eh_frame)
        *(.note*)
    }
}
