//! Inicialización del sistema y GDT

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

/// Información del framebuffer recibida del bootloader UEFI
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    pub base_address: u64,
    pub width: u32,
    pub height: u32,
    pub pixels_per_scan_line: u32,
    pub pixel_format: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub reserved_mask: u32,
}

/// Información completa de arranque pasada por el bootloader
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BootInfo {
    pub framebuffer: FramebufferInfo,
    pub pml4_addr: u64,
    pub kernel_phys_base: u64,
}

/// Static storage for BootInfo
/// We use a single Option<BootInfo> to store the copy of the boot info
/// Initialized to Some(dummy) to force it into .data section instead of .bss
/// This prevents it from being zeroed out if BSS initialization happens late or incorrectly
static mut BOOT_INFO: Option<BootInfo> = Some(BootInfo {
    framebuffer: FramebufferInfo {
        base_address: 0xDEADBEEF,
        width: 0,
        height: 0,
        pixels_per_scan_line: 0,
        pixel_format: 0,
        red_mask: 0,
        green_mask: 0,
        blue_mask: 0,
        reserved_mask: 0,
    },
    pml4_addr: 0,
    kernel_phys_base: 0,
});

/// Initialize boot info from the pointer passed by the bootloader
pub fn init(boot_info_ptr: u64) {
    if boot_info_ptr == 0 {
        panic!("BootInfo pointer is null in boot::init");
    }
    
    unsafe {
        crate::serial::serial_print("[BOOT] Initializing BootInfo storage...\n");
        let boot_info_ref = &*(boot_info_ptr as *const BootInfo);
        BOOT_INFO = Some(*boot_info_ref);
        
        crate::serial::serial_print("[BOOT] BootInfo stored at: ");
        crate::serial::serial_print_hex(&raw const BOOT_INFO as u64);
        crate::serial::serial_print("\n");
        
        if let Some(bi) = &BOOT_INFO {
            crate::serial::serial_print("[BOOT] Framebuffer base: ");
            crate::serial::serial_print_hex(bi.framebuffer.base_address);
            crate::serial::serial_print("\n");
        }
    }
}

/// Get access to the global BootInfo
pub fn get_boot_info() -> &'static BootInfo {
    unsafe {
        BOOT_INFO.as_ref().expect("BootInfo not initialized")
    }
}

/// Get framebuffer info pointer (for graphics server/syscalls)
pub fn get_framebuffer_info() -> u64 {
    unsafe {
        if let Some(bi) = &BOOT_INFO {
            return &bi.framebuffer as *const _ as u64;
        }
        0
    }
}

/// Descriptor de la GDT
#[repr(C, packed)]
struct GdtDescriptor {
    size: u16,
    offset: u64,
}

/// Task State Segment (64-bit)
#[repr(C, packed)]
pub struct TaskStateSegment {
    reserved1: u32,
    pub rsp0: u64,
    pub rsp1: u64,
    pub rsp2: u64,
    reserved2: u64,
    pub ist1: u64,
    pub ist2: u64,
    pub ist3: u64,
    pub ist4: u64,
    pub ist5: u64,
    pub ist6: u64,
    pub ist7: u64,
    reserved3: u64,
    reserved4: u16,
    pub iomap_base: u16,
}

impl TaskStateSegment {
    pub const fn new() -> Self {
        Self {
            reserved1: 0,
            rsp0: 0,
            rsp1: 0,
            rsp2: 0,
            reserved2: 0,
            ist1: 0,
            ist2: 0,
            ist3: 0,
            ist4: 0,
            ist5: 0,
            ist6: 0,
            ist7: 0,
            reserved3: 0,
            reserved4: 0,
            iomap_base: 0xFFFF,
        }
    }
}

/// TSS estática
pub static mut TSS: TaskStateSegment = TaskStateSegment::new();

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
    tss_system: GdtEntry, // User system segment (TSS low)
    tss_ignore: GdtEntry, // User system segment (TSS high)
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
    tss_system: GdtEntry::new(0, 0, 0, 0),
    tss_ignore: GdtEntry::new(0, 0, 0, 0),
};

/// Cargar la GDT y TSS
pub fn load_gdt() {
    unsafe {
        // Setup TSS entry in GDT
        let tss_base = &TSS as *const _ as u64;
        let tss_limit = (core::mem::size_of::<TaskStateSegment>() - 1) as u32;
        
        // 0x40: TSS Descriptor (16 bytes)
        // System Segment (0), Type 9 (Available TSS), DPL 0, P 1
        // Access byte: 0b10001001 = 0x89
        GDT.tss_system = GdtEntry::new(tss_base as u32, tss_limit, 0x89, 0x00);
        
        // Upper 32 bits of base address
        let upper_base = (tss_base >> 32) as u32;
        // In 64-bit mode, the upper 8 bytes of the system descriptor contain the upper 32 bits of base
        // GdtEntry struct layout: limit_low(2), base_low(2), base_mid(1), access(1), gran(1), base_high(1)
        // We reuse GdtEntry structure but the fields mean different things for the second half of system descriptor
        // The upper 32 bits of base go into the first 32 bits of the second entry (reserved in struct)
        // We need to be careful with GdtEntry structure packing
        
        // Let's construct it raw to be safe or map it.
        // limit_low (2) + base_low (2) = 4 bytes
        // base_mid (1) + access (1) + gran (1) + base_high (1) = 4 bytes
        
        // For the high part of 64-bit descriptor:
        // Bytes 0-3: Base 32-63
        // Bytes 4-7: Reserved (zero)
        
        GDT.tss_ignore = GdtEntry {
            limit_low: (upper_base & 0xFFFF) as u16,
            base_low: ((upper_base >> 16) & 0xFFFF) as u16,
            base_middle: 0,
            access: 0,
            granularity: 0,
            base_high: 0,
        };

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
        
        // Load TSS
        asm!(
            "mov ax, 0x40",
            "ltr ax",
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Set the kernel stack pointer in the TSS (RSP0)
pub fn set_tss_stack(stack_top: u64) {
    unsafe {
        TSS.rsp0 = stack_top;
    }
}

/// Selectores de segmento
pub const KERNEL_CODE_SELECTOR: u16 = 0x08;
pub const KERNEL_DATA_SELECTOR: u16 = 0x10;
pub const USER_CODE_SELECTOR: u16 = 0x18 | 3; // Ring 3
pub const USER_DATA_SELECTOR: u16 = 0x20 | 3; // Ring 3
pub const TSS_SELECTOR: u16 = 0x40;

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
