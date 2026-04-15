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

.global setjmp
.type setjmp, @function
setjmp:
    jmp _setjmp

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

