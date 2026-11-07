# The hosted (Linux) way out to a guest and back, on aarch64.
#
# Out: `syscall_fn_return(ctx, frame)` parks the kernel's callee-saved
# registers on the kernel stack, notes that stack in `ctx.__reserved`, loads
# the guest's thread pointer and asks the host to "return from a signal" into
# the frame `run_fncall` laid out with the guest's registers in it. The host
# loads every register from that frame, the floating-point ones included, and
# the guest runs.
#
# Back: a guest never calls into the kernel. Everything it does that the
# kernel has to answer -- a `svc`, a fault, a `brk`, the end of its time
# slice -- arrives as a host signal, and the signal handler (the fault bridge
# in kernel-hal) copies the guest's registers out of the signal context, puts
# the kernel's thread pointer back, and points the context's `pc` at
# `fault_fn_entry` with `sp` at the frame this file parked. The host's own
# return from the handler lands here, and `fault_fn_entry` unparks the
# registers and returns to whoever called `syscall_fn_return`.
#
# No register the guest controls is trusted on the way back: the kernel's
# stack is found through the handler's own bookkeeping (keyed on the host
# thread's alternate signal stack), never through the guest's `tpidr_el0`.
# The previous entry read the context pointer from `tpidr_el0 + 48`, which a
# guest can point anywhere with one `msr`.

.global syscall_fn_entry
.global syscall_fn_return
.global fault_fn_entry

# extern "C" fn syscall_fn_return(ctx: &mut UserContext, frame: *const u8)
syscall_fn_return:
    # The AAPCS callee-saved set: x19-x29, x30, and the low halves of v8-v15.
    stp     x29, x30, [sp, #-16]!
    stp     x27, x28, [sp, #-16]!
    stp     x25, x26, [sp, #-16]!
    stp     x23, x24, [sp, #-16]!
    stp     x21, x22, [sp, #-16]!
    stp     x19, x20, [sp, #-16]!
    stp     d14, d15, [sp, #-16]!
    stp     d12, d13, [sp, #-16]!
    stp     d10, d11, [sp, #-16]!
    stp     d8, d9, [sp, #-16]!

    # ctx.__reserved = the kernel stack to come back to
    mov     x9, sp
    str     x9, [x0, #8]

    # The guest's thread pointer. The host does not keep it in the signal
    # frame, so it goes in by hand, last, after the last memory access this
    # side makes that could want the kernel's.
    ldr     x9, [x0, #40]
    msr     tpidr_el0, x9

    # rt_sigreturn(frame): every other register comes from the frame.
    mov     sp, x1
    mov     x8, #139
    svc     #0
    # Not reached: a sigreturn the host refused killed the process.
    brk     #0

fault_fn_entry:
    ldp     d8, d9, [sp], #16
    ldp     d10, d11, [sp], #16
    ldp     d12, d13, [sp], #16
    ldp     d14, d15, [sp], #16
    ldp     x19, x20, [sp], #16
    ldp     x21, x22, [sp], #16
    ldp     x23, x24, [sp], #16
    ldp     x25, x26, [sp], #16
    ldp     x27, x28, [sp], #16
    ldp     x29, x30, [sp], #16
    ret

# The old way in, kept as a symbol because the hosted vDSO rebuild
# (`libzircon-libos.so`) is linked against it on the hosts that still boot it.
# On Linux aarch64 the real vDSO boots and its `svc` is trapped by the host,
# so nothing reaches here: a guest that does is stopped where it stands.
syscall_fn_entry:
    brk     #0
