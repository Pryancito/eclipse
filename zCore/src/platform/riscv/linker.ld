OUTPUT_ARCH(riscv)
ENTRY(_start)
INCLUDE zCore/src/platform/riscv/kernel-vars.ld
SECTIONS
{
    . = BASE_ADDRESS;
    start = .;

    .text : {
        stext = .;
        *(.text.entry)
        *(.text .text.*)
        etext = .;
    }

    .rodata ALIGN(4K) : {
        srodata = .;
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)

        /* Zircon kcounters. `loader/src/zircon.rs` hands userboot the
           descriptor table as a VMO, so the block must start on a page
           boundary; it stays INSIDE .rodata so srodata..erodata still covers
           it -- that range is what the kernel maps read-only. Only the x86
           script had these, so linking a zircon kernel for riscv64 failed with
           "undefined symbol: kcounters_desc_vmo_start" and three siblings. */
        . = ALIGN(4K);
        PROVIDE_HIDDEN(kcounters_desc_vmo_start = .);
        KEEP(*(.kcounter.desc.header))
        QUAD(kcounters_desc_end - kcounters_desc_start);
        ASSERT(. - kcounters_desc_vmo_start == 24, "wrong size of the kcounter descriptor VMO header");

        PROVIDE_HIDDEN(kcounters_desc_start = .);
        KEEP(*(SORT_BY_NAME(.kcounter.desc.*)))
        PROVIDE_HIDDEN(kcounters_desc_end = .);

        erodata = .;
    }

    .data ALIGN(4K) : {
        sdata = .;
        *(.data .data.*)
        *(.sdata .sdata.*)
        edata = .;
    }

    .stack ALIGN(4K) : {
        sstack = .;
        *(.bss.stack)
    }

    .bss ALIGN(4K) : {
        bootstack = .;
        *(.bss.bootstack)
        bootstacktop = .;

        . = ALIGN(4K);
        sbss = .;

        /* The counter arena itself: zero-filled, and sized by the ASSERT to
           match the descriptor table above. Inside sbss..ebss so the boot code
           that zeroes .bss covers it. */
        PROVIDE_HIDDEN(kcounters_arena_start = .);
        KEEP(*(SORT_BY_NAME(.bss.kcounter.*)))
        PROVIDE_HIDDEN(kcounters_arena_end = .);
        ASSERT(kcounters_arena_end - kcounters_arena_start == (kcounters_desc_end - kcounters_desc_start) * 8 / 64,
              "kcounters_arena size mismatch");

        *(.bss .bss.*)
        *(.sbss .sbss.*)
        ebss = .;
    }

    PROVIDE(end = .);
}
