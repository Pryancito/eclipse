.text
    # extern "sysv64" fn syscall_return(&mut GeneralRegs)
.global syscall_return
syscall_return:
    # disable interrupt
    cli

    # save callee-saved registers
    #
    # The kernel's own FSBASE is not saved here, nor restored on the way back
    # in: no kernel code addresses through `fs` (the kernel image has no
    # `fs:` operand), so the user's base can stay loaded while the CPU runs
    # the kernel, as on Linux. That was one RDMSR and one WRMSR on every
    # syscall and every trap from user mode. Its slot stays, as padding, so
    # the kernel rsp stored in TSS.sp0 below keeps its 16-byte alignment.
    push rax                # pad (was fsbase)
    push r15
    push r14
    push r13
    push r12
    push rbp
    push rbx

    push rdi
    push rdi                # keep rsp 16 bytes align
    mov gs:4, rsp           # store kernel rsp -> TSS.sp0
    mov rsp, rdi            # set rsp = bottom of trap frame

    # pop fsbase gsbase
    #
    # Each base is written only when it differs from what this CPU's MSR
    # already holds (`gs:[FS_CACHE]`, `gs:[GS_CACHE]`). A thread going back to
    # the CPU it trapped on -- every syscall that does not block -- finds both
    # equal and pays for no WRMSR at all; it paid two on every return, and two
    # RDMSRs on every entry to read back values that cannot have changed.
    #
    # What makes the cache answerable is what the entry path does with the two
    # slots, and that now depends on CR4.FSGSBASE:
    #   - clear: ring 3 has no way to move either base except a syscall that
    #     writes the saved context and so comes back through here, so the entry
    #     paths leave the slots alone and they still hold what was loaded from
    #     them.
    #   - set: ring 3 can move them with WRFSBASE/WRGSBASE, so the entry path
    #     reads the live bases back into the slots AND into this cache (see
    #     trap_syscall_entry below). The compare then still decides correctly:
    #     it is comparing against what the MSR really holds, which is the only
    #     thing it ever claimed to be.
    #
    # The user GSBASE goes into KERNEL_GS_BASE while the kernel's GS is still
    # active (`gs:` is the per-CPU region until the `swapgs`), and the `swapgs`
    # below makes it the active one.
    mov rax, [rsp + 18*8]   # fsbase
    cmp rax, gs:[{FS_CACHE}]
    je 1f
    mov gs:[{FS_CACHE}], rax
    mov rdx, rax
    shr rdx, 32
    mov ecx, 0xC0000100
    wrmsr                   # pop fsbase
1:
    mov rax, [rsp + 19*8]   # gsbase
    cmp rax, gs:[{GS_CACHE}]
    je 2f
    mov gs:[{GS_CACHE}], rax
    mov rdx, rax
    shr rdx, 32
    mov ecx, 0xC0000102
    wrmsr                   # pop gsbase (into KERNEL_GS_BASE)
2:
    swapgs                  # store kernel gsbase, load user gsbase

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
    # rip
    # rflags
    # fsbase
    # gsbase
    # trap_num
    # error_code

    # determain sysret or iret
    cmp dword ptr [rsp + 4*8], 0x100  # syscall?
    je sysret
iret:
    # get user cs from STAR MSR
    mov ecx, 0xC0000081
    rdmsr                   # msr[ecx] => edx:eax
    shr edx, 16             # dx = user_cs32
    lea ax, [edx + 8]       # ax = user_ss
    add dx, 16              # dx = user_cs64

    # construct trap frame
    push rax                # push ss
    push [rsp - 8*8]        # push rsp
    push [rsp + 3*8]        # push rflags
    push rdx                # push cs
    push [rsp + 4*8]        # push rip

    # recover rcx, rdx, rax
    mov rax, [rsp - 11*8]
    mov rcx, [rsp - 9*8]
    mov rdx, [rsp - 8*8]

    iretq

sysret:
    pop rcx                 # rcx = rip
    pop r11                 # r11 = rflags
    mov rsp, [rsp - 11*8]   # load rsp

    sysretq

    # sysretq instruction do:
    # - load cs, ss
    # - load rflags <- r11
    # - load rip <- rcx


.global syscall_entry
syscall_entry:
    # syscall instruction do:
    # - load cs
    # - store rflags -> r11
    # - mask rflags
    # - store rip -> rcx
    # - load rip

    swapgs                  # swap in kernel gs
    mov gs:12, rsp          # store user rsp -> scratch at TSS.sp1
    mov rsp, gs:4           # load kernel rsp <- TSS.sp0
    pop rsp                 # load rsp = bottom of trap frame
    add rsp, 22*8           # rsp = top of trap frame

    # push trap_num, error_code
    push 0                  # push error_code
    push 0x100              # push trap_num
    sub rsp, 16             # skip fsbase, gsbase
    # push general registers
    push r11                # push rflags
    push rcx                # push rip

.global trap_syscall_entry
trap_syscall_entry:
    push r15
    push r14
    push r13
    push r12
    push r11
    push r10
    push r9
    push r8
    push gs:12              # push rsp
    push rbp
    push rdi
    push rsi
    push rdx
    push rcx
    push rbx
    push rax

    # DEBUG: registrar (per-CPU, via GS) la direccion base donde el asm GUARDA
    # el GeneralRegs (rsp apunta ahora al slot de rax = base de GeneralRegs).
    mov gs:[{DBG_OFF}], rsp

    # Read the two user bases back, but only on a CPU where ring 3 can move
    # them itself (CR4.FSGSBASE; per-CPU flag set by
    # gdt::enable_user_fsgsbase). With the flag clear the slots already hold
    # what syscall_return loaded, and this would be an RDFSBASE and an RDMSR on
    # every single syscall for nothing, which is what the comment up in
    # syscall_return is about. With the flag set they are the price of letting
    # ring 3 move a base the kernel then has to find out about; FSGSBASE=0 on
    # the kernel command line buys the old fixed cost back.
    cmp byte ptr gs:[{FSGS_FLAG}], 0
    je 7f
    rdfsbase rax
    mov [rsp + 18*8], rax
    mov gs:[{FS_CACHE}], rax
    # The user GSBASE is in KERNEL_GS_BASE, put there by the swapgs of the
    # entry, so rdgsbase here would read the kernel's. Read the MSR rather than
    # bracket a rdgsbase in a swapgs pair: an NMI or #MC taken inside that pair
    # enters through __from_kernel, which does not swap, and would run the
    # kernel handler with the user base loaded in GS.
    mov ecx, 0xC0000102
    rdmsr                   # edx:eax = user gsbase
    shl rdx, 32
    or rax, rdx
    mov [rsp + 19*8], rax
    mov gs:[{GS_CACHE}], rax
7:
    # rax, rcx and rdx are already saved in the frame above, so the block is
    # free to use them.

    # restore callee-saved registers
    mov rsp, gs:4           # load kernel rsp <- TSS.sp0
    pop rbx
    pop rbx
    pop rbx
    pop rbp
    pop r12
    pop r13
    pop r14
    pop r15
    pop rax                 # pad (was fsbase)

    # go back to Rust
    ret
