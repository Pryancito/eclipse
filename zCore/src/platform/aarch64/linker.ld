ENTRY(_start)
BASE_ADDRESS = 0xffff000040080000;

SECTIONS
{
    . = BASE_ADDRESS;
    skernel = .;

    .text : {
        stext = .;
        *(.text.entry)
        *(.text .text.*)
        . = ALIGN(4K);
        etext = .;
    }

    .rodata : {
        srodata = .;
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
        . = ALIGN(4K);
        erodata = .;
    }

    .data : {
        sdata = .;
        *(.data .data.*)
        *(.sdata .sdata.*)
        . = ALIGN(4K);
        edata = .;
    }

    /* The in-kernel symbol table, patched in after the link by
       tools/gen_ksyms.py. Was an orphan section the linker put here on its
       own, between `edata` and .bss, which `remap_the_kernel` never maps:
       the first `ksyms::Addr` a crash report printed re-faulted and the
       report stopped there. Same fix as zCore/src/platform/riscv/linker.ld. */
    .ksyms ALIGN(4K) : {
        sksyms = .;
        KEEP(*(.ksyms))
        . = ALIGN(4K);
        eksyms = .;
    }

    .bss : {
        boot_stack = .;
        *(.bss.stack)
        . = ALIGN(4K);
        boot_stack_top = .;

        sbss = .;
        *(.bss .bss.*)
        *(.sbss .sbss.*)
        . = ALIGN(4K);
        ebss = .;
    }

    ekernel = .;

    /DISCARD/ : {
        *(.eh_frame) *(.eh_frame_hdr)
    }
}