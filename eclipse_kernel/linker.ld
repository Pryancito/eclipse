ENTRY(_start)

SECTIONS
{
    . = 0x200000;
    
    .text : ALIGN(16) {
        *(.text .text.*)
        . = ALIGN(16);
    }
    
    .rodata : ALIGN(16) {
        *(.rodata .rodata.*)
        . = ALIGN(16);
    }
    
    .data : ALIGN(16) {
        *(.data .data.*)
        . = ALIGN(16);
    }
    
    .bss : ALIGN(16) {
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
    }
    
    .note : ALIGN(16) {
        *(.note*)
        *(.note.pvh)
        . = ALIGN(16);
    }

    .note.pvh : ALIGN(16) {
        *(.note.pvh)
        . = ALIGN(16);
    }

    /DISCARD/ : {
        *(.eh_frame)
        *(.debug_*)
        *(.comment)
    }
}
