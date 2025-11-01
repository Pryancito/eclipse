;; Función de cambio de contexto para procesos
;; Esta función guarda el contexto actual y carga el nuevo contexto

section .text
global context_switch

;; void context_switch(Context* old_context, Context* new_context)
context_switch:
    ;; Guardar el contexto actual
    ;; RDI contiene old_context, RSI contiene new_context

    ;; Guardar registros generales
    mov [rdi + 0], rax      ;; rax
    mov [rdi + 8], rbx      ;; rbx
    mov [rdi + 16], rcx     ;; rcx
    mov [rdi + 24], rdx     ;; rdx
    mov [rdi + 32], rsi     ;; rsi (se sobreescribe después)
    mov [rdi + 40], rdi     ;; rdi (se sobreescribe después)

    ;; Guardar rsi y rdi correctos (fueron sobreescritos por los parámetros)
    mov rax, [rsp + 8]      ;; rsi original estaba en la pila
    mov [rdi + 32], rax
    mov rax, [rsp + 16]     ;; rdi original estaba en la pila
    mov [rdi + 40], rax

    mov [rdi + 48], rbp     ;; rbp
    mov [rdi + 56], rsp     ;; rsp
    mov [rdi + 64], r8      ;; r8
    mov [rdi + 72], r9      ;; r9
    mov [rdi + 80], r10     ;; r10
    mov [rdi + 88], r11     ;; r11
    mov [rdi + 96], r12     ;; r12
    mov [rdi + 104], r13    ;; r13
    mov [rdi + 112], r14    ;; r14
    mov [rdi + 120], r15    ;; r15

    ;; Guardar el puntero de instrucción (RIP) - se hace desde el caller
    ;; mov rax, [rsp]
    ;; mov [rdi + 128], rax

    ;; Guardar flags
    pushfq
    pop rax
    mov [rdi + 144], rax

    ;; Ahora cargar el nuevo contexto desde RSI

    ;; Cargar registros generales
    mov rax, [rsi + 0]
    mov rbx, [rsi + 8]
    mov rcx, [rsi + 16]
    mov rdx, [rsi + 24]
    ;; rsi y rdi se cargan después
    mov rbp, [rsi + 48]
    ;; rsp se carga después
    mov r8, [rsi + 64]
    mov r9, [rsi + 72]
    mov r10, [rsi + 80]
    mov r11, [rsi + 88]
    mov r12, [rsi + 96]
    mov r13, [rsi + 104]
    mov r14, [rsi + 112]
    mov r15, [rsi + 120]

    ;; Cargar flags
    mov rax, [rsi + 144]
    push rax
    popfq

    ;; Cargar stack pointer
    mov rsp, [rsi + 56]

    ;; Cargar rsi y rdi al final para no sobreescribir
    mov rsi, [rsi + 32]
    mov rdi, [rsi + 40]

    ;; Retornar al nuevo contexto
    ;; El RIP se carga automáticamente cuando se retorna
    ret
