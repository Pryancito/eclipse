; Punto de entrada para Eclipse Kernel compatible con UEFI
; Define el símbolo _start que será el punto de entrada

section .text
global _start

_start:
    ; Para UEFI, saltar directamente a la función uefi_entry de Rust
    ; El bootloader UEFI pasa información del framebuffer en RDI
    jmp uefi_entry
