//! Módulo de arranque - GDT y configuración inicial

use core::arch::asm;

/// Descriptor de la GDT
#[repr(C, packed)]
struct GdtDescriptor {
    limit: u16,
    base: u64,
}

/// Entrada de la GDT
#[repr(C, align(16))]
struct Gdt {
    entries: [u64; 5],
}

/// GDT estática del kernel
static mut KERNEL_GDT: Gdt = Gdt {
    entries: [
        0x0000000000000000, // Null descriptor
        0x00AF9A000000FFFF, // Code segment (64-bit, ring 0)
        0x00AF92000000FFFF, // Data segment (64-bit, ring 0)
        0x00AFFA000000FFFF, // Code segment (64-bit, ring 3)
        0x00AFF2000000FFFF, // Data segment (64-bit, ring 3)
    ],
};

/// Cargar la GDT en el procesador
pub fn load_gdt() {
    unsafe {
        let gdt_descriptor = GdtDescriptor {
            limit: (core::mem::size_of::<Gdt>() - 1) as u16,
            base: &KERNEL_GDT as *const _ as u64,
        };
        
        // Cargar GDTR
        asm!(
            "lgdt [{}]",
            in(reg) &gdt_descriptor,
            options(nostack, preserves_flags)
        );
        
        // Recargar selectores de segmento
        asm!(
            "push 0x08",           // Selector de código (entry 1)
            "lea rax, [rip + 2f]",
            "push rax",
            "retfq",               // Far return para recargar CS
            "2:",
            "mov ax, 0x10",        // Selector de datos (entry 2)
            "mov ds, ax",
            "mov es, ax",
            "mov fs, ax",
            "mov gs, ax",
            "mov ss, ax",
            out("rax") _,
            options(nostack)
        );
    }
}
