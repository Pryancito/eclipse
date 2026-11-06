# Constants / Macros defined in Rust code:
#   XLENB
#   LOAD
#   STORE

    .section .text
    .global trap_entry
    .balign 4
trap_entry:
    # If coming from userspace, preserve the user stack pointer and load
    # the kernel stack pointer. If we came from the kernel, sscratch
    # will contain 0, and we should continue on the current stack.
    csrrw sp, sscratch, sp
    bnez sp, trap_from_user
trap_from_kernel:
    csrr sp, sscratch
    addi sp, sp, -FRAME_WORDS * XLENB
    # sscratch = previous-sp, sp = kernel-sp
trap_from_user:
    # save general registers except sp(x2)
    STORE_SP x1, 1
    STORE_SP x3, 3
    STORE_SP x4, 4
    STORE_SP x5, 5
    STORE_SP x6, 6
    STORE_SP x7, 7
    STORE_SP x8, 8
    STORE_SP x9, 9
    STORE_SP x10, 10
    STORE_SP x11, 11
    STORE_SP x12, 12
    STORE_SP x13, 13
    STORE_SP x14, 14
    STORE_SP x15, 15
    STORE_SP x16, 16
    STORE_SP x17, 17
    STORE_SP x18, 18
    STORE_SP x19, 19
    STORE_SP x20, 20
    STORE_SP x21, 21
    STORE_SP x22, 22
    STORE_SP x23, 23
    STORE_SP x24, 24
    STORE_SP x25, 25
    STORE_SP x26, 26
    STORE_SP x27, 27
    STORE_SP x28, 28
    STORE_SP x29, 29
    STORE_SP x30, 30
    STORE_SP x31, 31

    # save sp, sstatus, sepc
    csrrw t0, sscratch, x0  # sscratch = 0 (kernel)
    csrr t1, sstatus
    csrr t2, sepc
    STORE_SP t0, 2          # save sp
    STORE_SP t1, 32         # save sstatus
    STORE_SP t2, 33         # save sepc

.ifdef HAS_FP
    # Save the floating-point state. Nothing saved it before, and user mode
    # runs with `sstatus.FS` set, so two guest threads on one hart overwrote
    # each other's `f` registers and `fcsr`. See `FpRegs`.
    #
    # `t1` still holds the sstatus just read, and the branch below reuses it
    # for the SPP test, so `fcsr` goes through `t0`, which is free now that
    # the saved sp is in the frame.
    STORE_FP f0, 34
    STORE_FP f1, 35
    STORE_FP f2, 36
    STORE_FP f3, 37
    STORE_FP f4, 38
    STORE_FP f5, 39
    STORE_FP f6, 40
    STORE_FP f7, 41
    STORE_FP f8, 42
    STORE_FP f9, 43
    STORE_FP f10, 44
    STORE_FP f11, 45
    STORE_FP f12, 46
    STORE_FP f13, 47
    STORE_FP f14, 48
    STORE_FP f15, 49
    STORE_FP f16, 50
    STORE_FP f17, 51
    STORE_FP f18, 52
    STORE_FP f19, 53
    STORE_FP f20, 54
    STORE_FP f21, 55
    STORE_FP f22, 56
    STORE_FP f23, 57
    STORE_FP f24, 58
    STORE_FP f25, 59
    STORE_FP f26, 60
    STORE_FP f27, 61
    STORE_FP f28, 62
    STORE_FP f29, 63
    STORE_FP f30, 64
    STORE_FP f31, 65
    .option push
    .option arch, +f
    frcsr t0
    .option pop
    STORE_SP t0, 66
.endif

    andi t1, t1, 1 << 8     # sstatus.SPP = 1
    beqz t1, end_trap_from_user
end_trap_from_kernel:
    mv a0, sp               # first arg is TrapFrame
    la ra, trap_return      # set return address
    # `j trap_handler` assembles to JAL, whose immediate reaches only +-1 MiB.
    # `trap_handler` lives in the kernel, not in this crate, so the distance
    # grows with the kernel's `.text`: once it passed 1 MiB the link failed
    # outright with "relocation R_RISCV_JAL out of range ... references
    # 'trap_handler'". `tail` assembles to auipc+jalr, which is PC-relative
    # over +-2 GiB, so the jump no longer depends on how big the kernel is.
    # It clobbers t1, which is already saved in the trap frame above and is
    # caller-saved as far as `trap_handler` is concerned.
    tail trap_handler

end_trap_from_user:
    # load callee-saved registers
    LOAD_SP sp, 0
    LOAD_SP s0, 0
    LOAD_SP s1, 1
    LOAD_SP s2, 2
    LOAD_SP s3, 3
    LOAD_SP s4, 4
    LOAD_SP s5, 5
    LOAD_SP s6, 6
    LOAD_SP s7, 7
    LOAD_SP s8, 8
    LOAD_SP s9, 9
    LOAD_SP s10, 10
    LOAD_SP s11, 11
    LOAD_SP ra, 12
    # not callee-saved, but is used to store mhartid
    LOAD_SP tp, 13
    addi sp, sp, 14 * XLENB

    ret

.global run_user
run_user:
    # save callee-saved registers
    addi sp, sp, -14 * XLENB
    STORE_SP s0, 0
    STORE_SP s1, 1
    STORE_SP s2, 2
    STORE_SP s3, 3
    STORE_SP s4, 4
    STORE_SP s5, 5
    STORE_SP s6, 6
    STORE_SP s7, 7
    STORE_SP s8, 8
    STORE_SP s9, 9
    STORE_SP s10, 10
    STORE_SP s11, 11
    STORE_SP ra, 12
    # not callee-saved, but is used to store mhartid
    STORE_SP tp, 13

    mv t0, sp
    mv sp, a0
    STORE_SP t0, 0          # save kernel-sp
    csrw sscratch, sp       # sscratch = bottom of trap frame

trap_return:
.ifdef HAS_FP
    # Restore the floating-point state saved on the way in, before `sstatus`
    # is written: these instructions need THIS hart's `FS`, which the kernel
    # has set, not the one the frame is about to install. Restoring after the
    # write would fault for any context whose own `FS` is Off.
    LOAD_SP t0, 66
    .option push
    .option arch, +f
    fscsr t0
    .option pop
    LOAD_FP f0, 34
    LOAD_FP f1, 35
    LOAD_FP f2, 36
    LOAD_FP f3, 37
    LOAD_FP f4, 38
    LOAD_FP f5, 39
    LOAD_FP f6, 40
    LOAD_FP f7, 41
    LOAD_FP f8, 42
    LOAD_FP f9, 43
    LOAD_FP f10, 44
    LOAD_FP f11, 45
    LOAD_FP f12, 46
    LOAD_FP f13, 47
    LOAD_FP f14, 48
    LOAD_FP f15, 49
    LOAD_FP f16, 50
    LOAD_FP f17, 51
    LOAD_FP f18, 52
    LOAD_FP f19, 53
    LOAD_FP f20, 54
    LOAD_FP f21, 55
    LOAD_FP f22, 56
    LOAD_FP f23, 57
    LOAD_FP f24, 58
    LOAD_FP f25, 59
    LOAD_FP f26, 60
    LOAD_FP f27, 61
    LOAD_FP f28, 62
    LOAD_FP f29, 63
    LOAD_FP f30, 64
    LOAD_FP f31, 65
.endif
    LOAD_SP t0, 32          # t0 = sstatus
    LOAD_SP t1, 33          # t1 = sepc
    csrw sstatus, t0        # load sstatus
    csrw sepc, t1           # load sepc

    # restore general registers except sp(x2)
    LOAD_SP x1, 1
    LOAD_SP x3, 3
    LOAD_SP x4, 4
    LOAD_SP x5, 5
    LOAD_SP x6, 6
    LOAD_SP x7, 7
    LOAD_SP x8, 8
    LOAD_SP x9, 9
    LOAD_SP x10, 10
    LOAD_SP x11, 11
    LOAD_SP x12, 12
    LOAD_SP x13, 13
    LOAD_SP x14, 14
    LOAD_SP x15, 15
    LOAD_SP x16, 16
    LOAD_SP x17, 17
    LOAD_SP x18, 18
    LOAD_SP x19, 19
    LOAD_SP x20, 20
    LOAD_SP x21, 21
    LOAD_SP x22, 22
    LOAD_SP x23, 23
    LOAD_SP x24, 24
    LOAD_SP x25, 25
    LOAD_SP x26, 26
    LOAD_SP x27, 27
    LOAD_SP x28, 28
    LOAD_SP x29, 29
    LOAD_SP x30, 30
    LOAD_SP x31, 31
    # restore sp last
    LOAD_SP x2, 2

    # return from supervisor call
    sret
