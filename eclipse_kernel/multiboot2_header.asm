; Multiboot2 Header for Eclipse Kernel
; Ensures compatibility with Multiboot2-compliant bootloaders

section .multiboot2_header
align 8

; Multiboot2 header magic
dd 0xe85250d6

; Architecture (i386)
dd 0

; Header length
dd header_end - header_start

; Checksum
dd 0x100000000 - (0xe85250d6 + 0 + (header_end - header_start))

header_start:
    ; Information request tag
    align 8
    dw 1    ; type
    dw 0    ; flags
    dd 12   ; size
    dd 4    ; basic meminfo
    dd 6    ; memory map
    dd 8    ; framebuffer

    ; Address tag
    align 8
    dw 2    ; type
    dw 0    ; flags
    dd 24   ; size
    dd 0x100000  ; header_addr
    dd 0x100000  ; load_addr
    dd 0        ; load_end_addr
    dd 0        ; bss_end_addr

    ; Entry address tag
    align 8
    dw 3    ; type
    dw 0    ; flags
    dd 12   ; size
    dd multiboot2_entry ; entry_addr

    ; Console flags tag
    align 8
    dw 4    ; type
    dw 0    ; flags
    dd 8    ; size
    dd 3    ; console_flags (EGA text support)

    ; Framebuffer tag
    align 8
    dw 5    ; type
    dw 0    ; flags
    dd 20   ; size
    dd 0    ; width (0 = prefer bootloader default)
    dd 0    ; height (0 = prefer bootloader default)
    dd 32   ; depth (32-bit color)

    ; Module alignment tag
    align 8
    dw 6    ; type
    dw 0    ; flags
    dd 8    ; size

    ; End tag
    align 8
    dw 0    ; type
    dw 0    ; flags
    dd 8    ; size

; PVH ELF Note for QEMU compatibility
section .note.pvh
align 4

pvh_note_start:
    dd 4                              ; namesz
    dd 8                              ; descsz
    dd 0x4b564850                    ; type (PVH)
    db 'X', 'e', 'n', 0               ; name
    dd 0                              ; version
    dd 0                              ; next_entry_offset
    dd 0                              ; features

pvh_note_end:

header_end:
