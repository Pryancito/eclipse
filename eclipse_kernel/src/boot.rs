//! Inicialización del sistema y GDT

use core::arch::asm;

/// Descriptor de la GDT
#[repr(C, packed)]
struct GdtDescriptor {
    size: u16,
    offset: u64,
}

/// Entrada de la GDT
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct GdtEntry {
    limit_low: u16,
    base_low: u16,
    base_middle: u8,
    access: u8,
    granularity: u8,
    base_high: u8,
}

impl GdtEntry {
    const fn new(base: u32, limit: u32, access: u8, gran: u8) -> Self {
        GdtEntry {
            limit_low: (limit & 0xFFFF) as u16,
            base_low: (base & 0xFFFF) as u16,
            base_middle: ((base >> 16) & 0xFF) as u8,
            access,
            granularity: (((limit >> 16) & 0x0F) as u8) | (gran & 0xF0),
            base_high: ((base >> 24) & 0xFF) as u8,
        }
    }
}

/// Global Descriptor Table
#[repr(C, align(16))]
struct Gdt {
    entries: [GdtEntry; 8],
}

/// GDT estática
static mut GDT: Gdt = Gdt {
    entries: [
        // 0x00: Null descriptor
        GdtEntry::new(0, 0, 0, 0),
        // 0x08: Kernel code segment (ring 0)
        GdtEntry::new(0, 0xFFFFF, 0x9A, 0xA0),
        // 0x10: Kernel data segment (ring 0)
        GdtEntry::new(0, 0xFFFFF, 0x92, 0xC0),
        // 0x18: User code segment (ring 3)
        GdtEntry::new(0, 0xFFFFF, 0xFA, 0xA0),
        // 0x20: User data segment (ring 3)
        GdtEntry::new(0, 0xFFFFF, 0xF2, 0xC0),
        // Unused
        GdtEntry::new(0, 0, 0, 0),
        GdtEntry::new(0, 0, 0, 0),
        GdtEntry::new(0, 0, 0, 0),
    ],
};

/// Cargar la GDT
pub fn load_gdt() {
    unsafe {
        let descriptor = GdtDescriptor {
            size: (core::mem::size_of::<Gdt>() - 1) as u16,
            offset: &GDT as *const _ as u64,
        };
        
        asm!(
            "lgdt [{}]",
            in(reg) &descriptor,
            options(nostack, preserves_flags)
        );
        
        // Recargar segmentos
        asm!(
            "push 0x08",
            "lea rax, [rip + 2f]",
            "push rax",
            "retfq",
            "2:",
            "mov ax, 0x10",
            "mov ds, ax",
            "mov es, ax",
            "mov fs, ax",
            "mov gs, ax",
            "mov ss, ax",
            out("rax") _,
        );
    }
}

/// Selectores de segmento
pub const KERNEL_CODE_SELECTOR: u16 = 0x08;
pub const KERNEL_DATA_SELECTOR: u16 = 0x10;
pub const USER_CODE_SELECTOR: u16 = 0x18 | 3; // Ring 3
pub const USER_DATA_SELECTOR: u16 = 0x20 | 3; // Ring 3

/// Habilitar instrucciones SSE
pub fn enable_sse() {
    unsafe {
        // Habilitar SSE bit en CR0 (Monitor Coprocessor) y limpiar Emulation
        let mut cr0: u64;
        asm!("mov {}, cr0", out(reg) cr0);
        cr0 &= !(1 << 2); // LIMPIAR EM (bit 2)
        cr0 |= (1 << 1);  // SET MP (bit 1)
        asm!("mov cr0, {}", in(reg) cr0);

        // Habilitar SSE en CR4 (OSFXSR y OSXMMEXCPT)
        let mut cr4: u64;
        asm!("mov {}, cr4", out(reg) cr4);
        cr4 |= (1 << 9);  // OSFXSR
        cr4 |= (1 << 10); // OSXMMEXCPT
        asm!("mov cr4, {}", in(reg) cr4);
    }
}
