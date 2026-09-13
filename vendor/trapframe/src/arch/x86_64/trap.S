.text
.global __alltraps
__alltraps:
    push rax

    /*
    kernel stack:
    - ptr to GeneralRegs
    - ss
    - rsp
    - rflags
    - cs
    - rip
    - error code
    - trap num
    - rax
    */
    mov ax, [rsp + 4*8]     # load cs
    and ax, 0x3             # test
    jz __from_kernel        # continue trap

__from_user:
    swapgs                  # swap in kernel gs
    mov rax, [rsp + 6*8]    # rax = user rsp
    mov gs:12, rax          # store user rsp -> scratch at TSS.sp1

    # Two sources, and they must not be confused. The hardware frame is on the
    # stack the CPU switched to, which is TSS.RSP0 for an ordinary ring3 trap
    # but an IST stack for the vectors idt.rs gives one to (#DF=8, #GP=13) --
    # reading it from anywhere else is the bug this fixes. The destination
    # GeneralRegs, on the other hand, is always reached through TSS.RSP0.
    mov rax, rsp            # preserve entry stack pointer (RSP0 or IST)
    mov rsp, gs:4           # rsp = TSS.RSP0's VALUE: the address where
                            # syscall_return pushed rdi (`mov gs:4, rsp`)
    mov rsp, [rsp]          # rsp = that rdi = bottom of trap frame
    add rsp, 22*8           # rsp = top of trap frame
                            # rax still points to the entry frame

    # The window below is the one place where a kernel stack pointer that was
    # already garbage becomes fatal: if rsp was loaded from the wrong place,
    # the first push raises #PF -> #DF. The frame the #DF reports then carries
    # a destroyed rsp, so containment cannot tell which coroutine stack died
    # and has to halt the machine.
    #
    # These two labels bound the window in which `rax` still points at the
    # original entry frame (RSP0 or IST), before it is replaced with user rax.
    # NOTE for kernel-hal's [df-sp] handler: `rax` here is the ENTRY frame, no
    # longer `gs:4`. On an RSP0 entry that is still an address inside the
    # faulting thread's kernel stack, so isolation keeps working; on an IST
    # entry it is the IST stack, which no executor owns, and containment will
    # decline rather than abandon the wrong stack.
.global __from_user_regs_push
__from_user_regs_push:
    # push trap_num, error_code
    push [rax + 2*8]        # push error_code from entry frame
    push [rax + 1*8]        # push trap_num from entry frame
    push rax                # skip gsbase
    push rax                # skip fsbase
    # push general registers
    push [rax + 5*8]        # push rflags from entry frame
    push [rax + 3*8]        # push rip from entry frame
.global __from_user_regs_push_end
__from_user_regs_push_end:
    mov rax, [rax]          # restore user rax from entry frame
    jmp trap_syscall_entry

__from_kernel:
    pop rax
    push 0
    push r15
    push r14
    push r13
    push r12
    push r11
    push r10
    push r9
    push r8
    # Long mode always pushes SS:RSP (even same-CPL). After the 9 pushes
    # above (pad + r15..r8), the hardware frame is at:
    #   [rsp+9]=trap_num [+10]=err [+11]=rip [+12]=cs [+13]=rflags
    #   [+14]=saved_RSP [+15]=ss
    # A prior `lea r8,[rsp+13*8]` stored *&rflags* into tf.rsp. Then
    # try_skip_null_execute_call read [tf.rsp] as a "return address" and
    # treated RFLAGS|RF (often 0x13xxx, inside the .text low32 window) as
    # soft-smash residue — false positive that halted desktop bring-up as
    # `[rsp]=0x13446` while hard guards were never touched. Load the CPU-
    # saved RSP *value* so tf.rsp is the faulting stack pointer.
    mov r8, [rsp + 14*8]
    push r8                 # push rsp (faulting RSP value)
    push rbp
    push rdi
    push rsi
    push rdx
    push rcx
    push rbx
    push rax

    mov rdi, rsp
    call trap_handler

.global trap_return
trap_return:
    pop rax
    pop rbx
    pop rcx
    pop rdx
    pop rsi
    pop rdi
    pop rbp
    pop r8                  # skip rsp
    pop r8
    pop r9
    pop r10
    pop r11
    pop r12
    pop r13
    pop r14
    pop r15

    # skip padding, trap_num, error_code
    add rsp, 24

    iretq
