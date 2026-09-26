# Context switch
#
#   void swtch(struct ContextData *old, struct ContextData *new);
#
# Save current registers in old. Load from new.

    .text
    .globl switch
    .balign 4
switch:
        add     x0, x0, #112
        mrs     x2, ttbr0_el1
        mov     x3, sp
        stp     x3, x2, [x0, #-16]!
        stp     x29, x30, [x0, #-16]!
        stp     x27, x28, [x0, #-16]!
        stp     x25, x26, [x0, #-16]!
        stp     x23, x24, [x0, #-16]!
        stp     x21, x22, [x0, #-16]!
        stp     x19, x20, [x0, #-16]!

        ldr     x9, [x1, #104]
        msr     ttbr0_el1, x9
        // Invalidate the WHOLE of this PE's EL1 translations, exactly as
        // riscv64 does with `sfence.vma x0, x0` and as x86_64 gets for free
        // from writing CR3. This used to be `tlbi vaae1is, x10` with
        // `x10 = ttbr0 >> 12`, i.e. the PHYSICAL page number of the root
        // table handed to an instruction that takes a VIRTUAL one: it
        // invalidated one arbitrary address and left every translation of the
        // outgoing address space live. Nothing else would have removed them
        // -- the kernel gives out no ASIDs, and user leaves are not marked
        // non-global (`PTF::NG` is declared in kernel-hal's `utils::pte` and
        // never set), so the TLB has no way to tell one TTBR0 from the next.
        //
        // Local (`nsh`), not inner-shareable: TTBR0 is per-PE state, so the
        // entries this write makes stale are this PE's own. Broadcasting
        // would cost every other core a flush on every context switch.
        dsb     nshst
        tlbi    vmalle1
        dsb     nsh
        isb

        ldp     x19, x20, [x1], #16
        ldp     x21, x22, [x1], #16
        ldp     x23, x24, [x1], #16
        ldp     x25, x26, [x1], #16
        ldp     x27, x28, [x1], #16
        ldp     x29, x30, [x1], #16
        ldr     x0, [x1], #8
        mov     sp, x0
        ret