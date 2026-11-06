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

    /* Reserved slot for the in-kernel symbol table, patched in after the link
       by tools/gen_ksyms.py (see zCore/src/platform/x86/linker.ld). It used to
       be an ORPHAN here: the linker dropped it after .data, past `edata`, and
       `remap_the_kernel` maps the image section by section, so the table was
       never mapped. Every `ksyms::lookup` -- i.e. every crash report, which
       prints its first address through `ksyms::Addr` -- faulted, and the
       report ended in "re-entrant fault while diagnosing" with nothing else
       said. Named here so the kernel can map it. */
    .ksyms ALIGN(4K) : {
        sksyms = .;
        KEEP(*(.ksyms))
        . = ALIGN(4K);
        eksyms = .;
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
