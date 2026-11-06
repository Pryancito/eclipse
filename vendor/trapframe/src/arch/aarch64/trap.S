.section .text

# The frame: 38 words (trap num, kernel sp, elr, spsr, sp, tpidr and the 32
# general registers) and then the floating-point area, 66 words: q0-q31 as
# 64 doublewords, fpcr, fpsr. FP_OFF is where the area starts, FP_BYTES its
# size, FRAME_BYTES the whole thing -- `TrapFrame`/`UserContext` in trap.rs
# assert the same numbers.
#
# Why the floating-point registers are saved on EVERY trap, user or kernel:
# the kernel is built with `+neon,+fp-armv8` (zCore/aarch64.json) and the
# compiler hands q registers to its memcpy, struct copies and loops --
# thirty-eight thousand SIMD instructions in the release kernel -- while
# CPACR_EL1 leaves the unit untrapped, so every syscall and every interrupt
# used to clobber whatever the interrupted code, user or kernel, had in
# q0-q31. musl's memcpy/strlen/memchr on aarch64 live in those registers;
# a timer tick in the middle of one came back with another thread's halves.
.equ FP_OFF, 38 * 8
.equ FP_BYTES, 66 * 8
.equ FRAME_BYTES, FP_OFF + FP_BYTES

# Save q0-q31, fpcr and fpsr into the area at FP_OFF(sp). Clobbers x1-x3,
# which are already in the frame when this runs.
.macro SAVE_FP
    add     x1, sp, #FP_OFF
    stp     q0, q1, [x1, #0]
    stp     q2, q3, [x1, #32]
    stp     q4, q5, [x1, #64]
    stp     q6, q7, [x1, #96]
    stp     q8, q9, [x1, #128]
    stp     q10, q11, [x1, #160]
    stp     q12, q13, [x1, #192]
    stp     q14, q15, [x1, #224]
    stp     q16, q17, [x1, #256]
    stp     q18, q19, [x1, #288]
    stp     q20, q21, [x1, #320]
    stp     q22, q23, [x1, #352]
    stp     q24, q25, [x1, #384]
    stp     q26, q27, [x1, #416]
    stp     q28, q29, [x1, #448]
    stp     q30, q31, [x1, #480]
    mrs     x2, fpcr
    mrs     x3, fpsr
    # `stp` takes a signed 7-bit scaled offset (at most 504): two `str`s.
    str     x2, [x1, #512]
    str     x3, [x1, #520]
.endm

# The mirror of SAVE_FP. Clobbers x1-x3, which are restored afterwards.
.macro LOAD_FP
    add     x1, sp, #FP_OFF
    ldr     x2, [x1, #512]
    ldr     x3, [x1, #520]
    msr     fpcr, x2
    msr     fpsr, x3
    ldp     q0, q1, [x1, #0]
    ldp     q2, q3, [x1, #32]
    ldp     q4, q5, [x1, #64]
    ldp     q6, q7, [x1, #96]
    ldp     q8, q9, [x1, #128]
    ldp     q10, q11, [x1, #160]
    ldp     q12, q13, [x1, #192]
    ldp     q14, q15, [x1, #224]
    ldp     q16, q17, [x1, #256]
    ldp     q18, q19, [x1, #288]
    ldp     q20, q21, [x1, #320]
    ldp     q22, q23, [x1, #352]
    ldp     q24, q25, [x1, #384]
    ldp     q26, q27, [x1, #416]
    ldp     q28, q29, [x1, #448]
    ldp     q30, q31, [x1, #480]
.endm

.global __alltraps
__alltraps:
    # x30 and x0 are saved in __vectors
    # x0 is trap num now
    # skip __reversed
    str     x29, [sp, #-16]!
    stp     x27, x28, [sp, #-16]!
    stp     x25, x26, [sp, #-16]!
    stp     x23, x24, [sp, #-16]!
    stp     x21, x22, [sp, #-16]!
    stp     x19, x20, [sp, #-16]!
    stp     x17, x18, [sp, #-16]!
    stp     x15, x16, [sp, #-16]!
    stp     x13, x14, [sp, #-16]!
    stp     x11, x12, [sp, #-16]!
    stp     x9, x10, [sp, #-16]!
    stp     x7, x8, [sp, #-16]!
    stp     x5, x6, [sp, #-16]!
    stp     x3, x4, [sp, #-16]!
    stp     x1, x2, [sp, #-16]!

    # skip tpidr and sp
    add     sp, sp, #-16

    # read spsr and elr
    mrs     x2, spsr_el1
    mrs     x1, elr_el1
    stp     x1, x2, [sp, #-16]!

    # save trap num
    str     x0, [sp, #-16]!

    # the floating-point state, user's or kernel's
    SAVE_FP

    # check source is 2
    mov     x1, #0x3
    and     x1, x1, x0
    cmp     x1, #2
    beq     trap_from_user

trap_from_kernel:
    # read tpidr and sp
    mrs     x2, tpidr_el1
    add     x1, sp, #FRAME_BYTES
    stp     x1, x2, [sp, #32]
    # go to rust
    mov     x0, sp
    bl      trap_handler
    # load tpidr
    ldr     x1, [sp, #40]
    msr     tpidr_el1, x1
    # go to trap_return
    b       trap_return

trap_from_user:
    # read tpidr and sp
    mrs     x2, tpidr_el0
    mrs     x1, sp_el0
    stp     x1, x2, [sp, #32]
    # read sp
    ldr     x2, [sp, #8]
    mov     sp, x2

    # load callee-saved registers
    ldp     x19, x20, [sp], #16
    ldp     x21, x22, [sp], #16
    ldp     x23, x24, [sp], #16
    ldp     x25, x26, [sp], #16
    ldp     x27, x28, [sp], #16
    ldp     x29, x30, [sp], #16

    ret

.macro HANDLER source kind
    .align 7
    # sp is set to SP_EL1 upon trap. Reserve the floating-point area first
    # (see FP_BYTES): it is the top of the frame, above `lr, x0`.
    sub     sp, sp, #FP_BYTES
    stp     lr, x0, [sp, #-16]!
    mov     x0, #\source
    movk    x0, #\kind, lsl #16
    b       __alltraps
.endm

.global __vectors
.align 11
__vectors:
    HANDLER 0 0
    HANDLER 0 1
    HANDLER 0 2
    HANDLER 0 3
    HANDLER 1 0
    HANDLER 1 1
    HANDLER 1 2
    HANDLER 1 3
    HANDLER 2 0
    HANDLER 2 1
    HANDLER 2 2
    HANDLER 2 3
    HANDLER 3 0
    HANDLER 3 1
    HANDLER 3 2
    HANDLER 3 3

.global run_user
run_user:
    # x0 points to TrapFrame
    # save callee-saved registers x19-x29
    stp     x29, x30, [sp, #-16]!
    stp     x27, x28, [sp, #-16]!
    stp     x25, x26, [sp, #-16]!
    stp     x23, x24, [sp, #-16]!
    stp     x21, x22, [sp, #-16]!
    stp     x19, x20, [sp, #-16]!

    # save kernel sp to TrapFrame
    mov     x1, sp
    mov     sp, x0
    str     x1, [sp, #8]

    # load sp and tpidr
    ldp     x1, x2, [sp, #32]
    msr     sp_el0, x1
    msr     tpidr_el0, x2

trap_return:
    # sp points to TrapFrame
    # the floating-point state first, while sp is still the frame base
    LOAD_FP

    # skip trap num, don't restore
    add     sp, sp, #16

    # elr and spsr
    ldp     x1, x2, [sp], #16
    msr     elr_el1, x1
    msr     spsr_el1, x2

    # skip sp and tpidr
    add     sp, sp, #16

    # general purpose registers
    ldp     x1, x2, [sp], #16
    ldp     x3, x4, [sp], #16
    ldp     x5, x6, [sp], #16
    ldp     x7, x8, [sp], #16
    ldp     x9, x10, [sp], #16
    ldp     x11, x12, [sp], #16
    ldp     x13, x14, [sp], #16
    ldp     x15, x16, [sp], #16
    ldp     x17, x18, [sp], #16
    ldp     x19, x20, [sp], #16
    ldp     x21, x22, [sp], #16
    ldp     x23, x24, [sp], #16
    ldp     x25, x26, [sp], #16
    ldp     x27, x28, [sp], #16
    ldr     x29, [sp], #16
    ldp     lr, x0, [sp], #16

    # step over the floating-point area: sp is the top of the frame again
    add     sp, sp, #FP_BYTES

    # return
    eret