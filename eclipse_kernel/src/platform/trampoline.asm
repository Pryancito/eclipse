section .text
global trampoline_start
global trampoline_end

bits 16

trampoline_start:
    cli
    
    ; 1. Set DS/CS to proper segment based on our location (0x8000 >> 4 = 0x800)
    ; But INIT/SIPI starts us at 0x8000 with CS=0x800 IP=0x0000
    mov ax, cs
    mov ds, ax
    mov es, ax
    mov ss, ax
    xor sp, sp

    ; 2. Load GDT
    ; We need the offset relative to the start of the section (which corresponds to 0x8000 base)
    ; NASM should be able to calculate (gdt_descriptor - trampoline_start) as a constant.
    lgdt [gdt_descriptor - trampoline_start]

    ; 3. Enable Protected Mode (PE bit in CR0)
    mov eax, cr0
    or eax, 1
    mov cr0, eax

    ; 4. Jump to 32-bit Protected Mode
    ; We use a long jump to reload CS with the 32-bit code segment (0x8)
    ; The target address must be physical 0x8000 + offset
    jmp 0x8:(protected_mode_entry - trampoline_start)

bits 32
protected_mode_entry:
    ; 5. Set up 32-bit data segments
    mov ax, 0x10 ; Data segment offset in GDT
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax
    mov ss, ax

    ; 6. Enable PAE (Physical Address Extension) in CR4
    mov eax, cr4
    or eax, 1 << 5
    mov cr4, eax

    ; 7. Load Page Directory Pointer Table (CR3)
    ; We can reuse the main kernel's PML4!
    ; The Rust code must overwrite this placeholder with the actual CR3 value
    mov eax, [pml4_ptr - trampoline_start]
    mov cr3, eax

    ; 8. Enable Long Mode in EFER MSR (0xC0000080)
    mov ecx, 0xC0000080
    rdmsr
    or eax, 1 << 8
    wrmsr

    ; 9. Enable Paging (PG bit in CR0)
    mov eax, cr0
    or eax, 1 << 31
    mov cr0, eax

    ; 10. Jump to 64-bit Long Mode
    ; Reload CS with 64-bit code segment (0x18)
    jmp 0x18:(long_mode_entry - trampoline_start)

bits 64
long_mode_entry:
    ; 11. Set up 64-bit data segments
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax
    mov ss, ax

    ; 12. Jump to Rust AP Entry Point
    ; Rust code must overwrite this with the address of ap_entry
    ; Calculate offset manually to ensure no relocation
    lea rax, [rel ap_entry_ptr]
    mov rax, [rax]
    call rax

    ; Should never return
    cli
    hlt
    jmp $

align 16
gdt_start:
    ; Null Descriptor
    dq 0x0000000000000000
    ; 32-bit Code Descriptor (0x8)
    dq 0x00CF9A000000FFFF
    ; 32-bit Data Descriptor (0x10)
    dq 0x00CF92000000FFFF
    ; 64-bit Code Descriptor (0x18)
    dq 0x00AF9A000000FFFF
gdt_end:

gdt_descriptor:
    dw gdt_end - gdt_start - 1
    dd gdt_start ; Note: This will be relative to 0x8000 usage if not careful, but assembler handles offsets

align 8
pml4_ptr:
    dq 0x00000000 ; Placeholder for CR3
ap_entry_ptr:
    dq 0x00000000 ; Placeholder for ap_entry address
trampoline_end:
