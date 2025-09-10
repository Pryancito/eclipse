; Punto de entrada para Eclipse Kernel compatible con UEFI
; Define el símbolo _start que será el punto de entrada

section .text
global _start

_start:
    ; Punto de entrada estándar - saltar directamente a la función _start de Rust
    ; Esta función está definida en main.rs
    jmp _start
