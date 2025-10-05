ENTRY(_start)

PHDRS
{
    text PT_LOAD FLAGS(5);
    rodata PT_LOAD FLAGS(4);
    data PT_LOAD FLAGS(6);
    note PT_NOTE FLAGS(4);
}

SECTIONS
{
    . = 0x200000;
    
    .init : {
        *(.init)
    } :text

    .text : {
        *(.text .text.*)
    } :text
    
    .rodata : {
        *(.rodata .rodata.*)
    } :rodata
    
    .data : {
        *(.data .data.*)
    } :data
    
    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data
    
    /* Nota PVH para QEMU */
    .note : {
        *(.note)
        *(.note.*)
    } :note
    
    /DISCARD/ : {
        *(.eh_frame)
        *(.debug_*)
        *(.comment)
    }
}
