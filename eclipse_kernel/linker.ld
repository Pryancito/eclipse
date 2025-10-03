ENTRY(_start)

SECTIONS
{
    . = 0x200000;
    
    .init : {
        *(.init)
    }

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
    
    /DISCARD/ : {
        *(.eh_frame)
        *(.debug_*)
        *(.comment)
        *(.note*)
    }
}
