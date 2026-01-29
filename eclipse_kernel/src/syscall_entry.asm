; Syscall entry point for Eclipse OS
; This file handles the transition from userland to kernel via the SYSCALL instruction
;
; Calling convention (x86_64 System V):
; - syscall number in RAX
; - arguments in RDI, RSI, RDX, R10, R8, R9
; - return value in RAX
; - RCX and R11 are clobbered by SYSCALL/SYSRET

BITS 64

global syscall_entry
extern rust_syscall_handler

section .text

syscall_entry:
    ; Save user RCX and R11 (clobbered by SYSCALL)
    ; RCX contains return RIP, R11 contains RFLAGS
    ; These are saved by the CPU automatically
    
    ; Switch to kernel stack
    ; We need to save user RSP first
    swapgs                      ; Swap GS base to access kernel data
    mov [gs:0x08], rsp         ; Save user RSP to kernel data area
    mov rsp, [gs:0x00]         ; Load kernel RSP from kernel data area
    
    ; Build stack frame for syscall handler
    ; Push all registers we need to preserve
    push r11                    ; RFLAGS (from SYSCALL)
    push rcx                    ; Return RIP (from SYSCALL)
    push rbp
    push rbx
    push r15
    push r14
    push r13
    push r12
    push r10                    ; arg4
    push r9                     ; arg6
    push r8                     ; arg5
    push rdx                    ; arg3
    push rsi                    ; arg2
    push rdi                    ; arg1
    push rax                    ; syscall number
    
    ; Call Rust syscall handler
    ; Arguments: syscall_num (RAX already in RDI), args on stack
    mov rdi, rax                ; syscall number
    mov rsi, rsp                ; pointer to saved registers
    call rust_syscall_handler   ; Call Rust handler
    
    ; Return value is in RAX, save it
    mov rbx, rax
    
    ; Restore registers (except RAX which has the return value)
    pop rax                     ; discard saved syscall number
    pop rdi
    pop rsi
    pop rdx
    pop r8
    pop r9
    pop r10
    pop r12
    pop r13
    pop r14
    pop r15
    add rsp, 8                  ; discard saved RBX
    pop rbp
    pop rcx                     ; Restore return RIP
    pop r11                     ; Restore RFLAGS
    
    ; Restore user RSP
    mov rsp, [gs:0x08]
    swapgs                      ; Swap GS back to user
    
    ; Put return value in RAX
    mov rax, rbx
    
    ; Return to userland
    sysretq
