.intel_syntax noprefix

.section .text

.global _setjmp
.type _setjmp, @function
_setjmp:
    mov [rdi], rbx
    mov [rdi + 8], rbp
    mov [rdi + 16], r12
    mov [rdi + 24], r13
    mov [rdi + 32], r14
    mov [rdi + 40], r15
    lea rdx, [rsp + 8]
    mov [rdi + 48], rdx
    mov rdx, [rsp]
    mov [rdi + 56], rdx
    xor eax, eax
    ret

.global __longjmp_chk
.type __longjmp_chk, @function
__longjmp_chk:
.global longjmp
.type longjmp, @function
longjmp:
    mov rax, rsi
    test rax, rax
    jnz 1f
    inc rax
1:
    mov rbx, [rdi]
    mov rbp, [rdi + 8]
    mov r12, [rdi + 16]
    mov r13, [rdi + 24]
    mov r14, [rdi + 32]
    mov r15, [rdi + 40]
    mov rsp, [rdi + 48]
    jmp qword ptr [rdi + 56]

// _start -- ELF entry point for C programs compiled against eclipse-relibc.
//
// Stack layout at entry (System V AMD64 ABI / Linux kernel convention):
//   [RSP+0]   = argc (u64)
//   [RSP+8]   = argv[0]  ...  argv[argc-1]  NULL
//                         envp[0] ... envp[n] NULL
//                         auxv pairs          AT_NULL
//
// We pass (argc, argv, envp) to __libc_start_main which initialises the
// C runtime and calls the application's main().
.global _start
.weak   _start
.type   _start, @function
_start:
    // Zero the frame pointer as required by the ABI (outermost frame).
    xor rbp, rbp

    // RSP points at argc. Save it in a callee-saved register so we can
    // compute argv and envp after aligning the stack.
    mov r12, rsp          // r12 = original RSP (points at argc)

    // Read argc.
    mov rdi, [rsp]        // argc → rdi (arg1)

    // argv = RSP + 8.
    lea rsi, [rsp + 8]    // argv → rsi (arg2)

    // envp = RSP + 8 + (argc + 1) * 8.
    mov rax, rdi
    add rax, 1
    shl rax, 3
    lea rdx, [rsi + rax]  // envp → rdx (arg3)

    // Align stack to 16 bytes before the call.
    and rsp, -16

    // Call __libc_start_main(argc, argv, envp).
    call __libc_start_main

    // Should not return; call exit(0) as a safety net.
    xor rdi, rdi
    call exit
    ud2

// sigsetjmp(sigjmp_buf env, int savesigs)
// Save callee-saved registers and optionally save the signal mask.
// env[8] is used to flag whether sigmask was saved.
.global sigsetjmp
.type   sigsetjmp, @function
sigsetjmp:
    // Save registers into env[0..7] (same layout as setjmp).
    mov [rdi], rbx
    mov [rdi + 8], rbp
    mov [rdi + 16], r12
    mov [rdi + 24], r13
    mov [rdi + 32], r14
    mov [rdi + 40], r15
    lea rdx, [rsp + 8]
    mov [rdi + 48], rdx
    mov rdx, [rsp]
    mov [rdi + 56], rdx
    // Store savesigs flag in env[8].
    mov [rdi + 64], rsi
    // If savesigs != 0, save current signal mask into env[9] via sigprocmask.
    test rsi, rsi
    jz 1f
    // sigprocmask(SIG_BLOCK=0, NULL, &env[72])
    push rdi
    lea rdx, [rdi + 72]   // oldset
    xor rsi, rsi          // set = NULL
    xor rdi, rdi          // how = SIG_BLOCK
    call sigprocmask
    pop rdi
1:
    xor eax, eax
    ret

// siglongjmp(sigjmp_buf env, int val)
.global siglongjmp
.type   siglongjmp, @function
siglongjmp:
    // If savesigs was set, restore signal mask from env[72].
    cmp qword ptr [rdi + 64], 0
    jz 1f
    push rdi
    push rsi
    lea rdx, [rdi + 72]   // set (saved mask)
    xor rcx, rcx          // oldset = NULL — use rcx as scratch (not arg register here)
    // sigprocmask(SIG_SETMASK=2, &env[72], NULL)
    mov rsi, rdx          // set
    xor rdx, rdx          // oldset = NULL
    mov rdi, 2            // SIG_SETMASK
    call sigprocmask
    pop rsi
    pop rdi
1:
    // longjmp using the jmp_buf part of sigjmp_buf.
    mov rax, rsi
    test rax, rax
    jnz 2f
    inc rax
2:
    mov rbx, [rdi]
    mov rbp, [rdi + 8]
    mov r12, [rdi + 16]
    mov r13, [rdi + 24]
    mov r14, [rdi + 32]
    mov r15, [rdi + 40]
    mov rsp, [rdi + 48]
    jmp qword ptr [rdi + 56]
