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
