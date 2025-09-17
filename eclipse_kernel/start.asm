; Punto de entrada para Eclipse Kernel con Multiboot2
; Define el símbolo _start que será el punto de entrada

section .text
global _start

_start:
    ; Saltar a la función Rust
    jmp multiboot2_entry
