ENTRY(_start)

SECTIONS
{
    . = 0x200000;
    
    .text : {
        *(.text .text.*)
    }
    
    .rodata : {
        *(.rodata .rodata.*)
    }
    
    .data : {
        *(.data .data.*)
    }
    
    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    }
    
    .note : {
        *(.note*)
        *(.note.pvh)
    }

    .note.pvh : {
        *(.note.pvh)
    }

    /DISCARD/ : {
        *(.eh_frame)
        *(.debug_*)
        *(.comment)
    }
}
