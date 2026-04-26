//! Inicialización del sistema y GDT

use crate::memory::PHYS_MEM_OFFSET;

use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

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
    pub rsdp_addr: u64,
    /// Total bytes of UEFI "conventional" RAM (MemoryType::CONVENTIONAL_MEMORY).
    /// Used to report RAM total in system statistics.
    pub conventional_mem_total_bytes: u64,
    /// Physical address of the kernel heap region allocated by the bootloader.
    /// The region is accessible at PHYS_MEM_OFFSET + heap_phys_base in the kernel.
    pub heap_phys_base: u64,
    /// Size in bytes of the heap region allocated by the bootloader.
    pub heap_phys_size: u64,
}

/// Framebuffer source for gpu_present
#[derive(Clone, Copy, PartialEq)]
pub enum FbSource {
    VirtIO,
    Uefi,
    Nvidia,
}

/// VirtIO GPU resource ID for display buffer (must match virtio.rs)
pub const VIRTIO_DISPLAY_RESOURCE_ID: u32 = 2;

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
    rsdp_addr: 0,
    conventional_mem_total_bytes: 0,
    heap_phys_base: 0,
    heap_phys_size: 0,
});

/// True once `load_gdt()` has initialized GS base to point at valid per-CPU data.
/// Using `gs:[..]` before this can fault if the null page is unmapped.
static GS_BASE_READY: AtomicBool = AtomicBool::new(false);

#[inline(always)]
pub fn gs_base_ready() -> bool {
    GS_BASE_READY.load(Ordering::Relaxed)
}

/// Initialize boot info from the pointer passed by the bootloader
pub fn init(boot_info_ptr: u64) {
    if boot_info_ptr == 0 {
        return;
    }
    
    unsafe {
        let boot_info_ref = &*(boot_info_ptr as *const BootInfo);
        BOOT_INFO = Some(*boot_info_ref);
    }
}

/// Get access to the global BootInfo
pub fn get_boot_info() -> &'static BootInfo {
    unsafe {
        BOOT_INFO.as_ref().expect("BootInfo not initialized")
    }
}

/// Return true when the EFI GOP framebuffer reported by the bootloader is usable:
/// the base address must be a real physical address (not zero or the sentinel
/// 0xDEADBEEF) and the display dimensions must be non-zero.
pub fn gop_framebuffer_valid() -> bool {
    unsafe {
        if let Some(bi) = &BOOT_INFO {
            let fi = &bi.framebuffer;
            fi.base_address != 0
                && fi.base_address != 0xDEADBEEF
                && fi.width > 0
                && fi.height > 0
        } else {
            false
        }
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

/// Try to get framebuffer info. Prefer GOP (bootloader) over VirtIO for real hardware (e.g. NVIDIA).
pub fn get_fb_info() -> Option<(u64, u32, u32, u32, usize, FbSource)> {
    // 1. NVIDIA BAR1 (linear VRAM aperture) – real hardware without EFI GOP or when native driver is loaded
    if let Some((phys, _bar1_phys, w, h, pitch)) = crate::nvidia::get_nvidia_fb_info() {
        if phys != 0 && w > 0 && h > 0 {
            let size = (pitch * h) as usize;
            return Some((phys, w, h, pitch, size, FbSource::Nvidia));
        }
    }
    // 2. VirtIO display (e.g. QEMU)
    if let Some((phys, w, h, pitch, size)) = crate::virtio::get_primary_virtio_display() {
        if phys != 0 && w > 0 && h > 0 {
            return Some((phys, w, h, pitch, size, FbSource::VirtIO));
        }
    }
    // 3. GOP framebuffer from bootloader (UEFI) - fallback
    let fi = unsafe { &BOOT_INFO.as_ref()?.framebuffer };
    if gop_framebuffer_valid() {
        let phys = if fi.base_address >= PHYS_MEM_OFFSET {
            fi.base_address.saturating_sub(PHYS_MEM_OFFSET)
        } else {
            fi.base_address
        };
        let pitch = fi.pixels_per_scan_line * 4;
        let size = (pitch * fi.height) as usize;
        return Some((phys, fi.width, fi.height, pitch, size, FbSource::Uefi));
    }
    None
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

/// Maximum number of CPUs supported (indexed by APIC ID % MAX_SMP_CPUS)
pub const MAX_SMP_CPUS: usize = 32;

/// Per-CPU data accessible via GS segment during SYSCALL handling.
/// Field offsets are part of the ABI used in syscall_entry (interrupts.rs):
///   offset 0  – kernel RSP0 (loaded into RSP on syscall entry)
///   offset 8  – scratch area for user RSP saved on syscall entry
#[repr(C)]
pub struct CpuData {
    pub rsp0: u64,        // offset 0
    pub scratch_rsp: u64, // offset 8
    pub cpu_id: u32,      // offset 16
    pub current_pid: u32, // offset 20 (0xFFFFFFFF = None)
}

impl CpuData {
    const fn new() -> Self {
        Self { 
            rsp0: 0, 
            scratch_rsp: 0, 
            cpu_id: 0xFFFF_FFFF, 
            current_pid: 0xFFFF_FFFF 
        }
    }
}

/// Per-CPU data array (GS base points to CPU_DATA[cpu_id] during kernel execution)
pub static mut CPU_DATA: [CpuData; MAX_SMP_CPUS] = [const { CpuData::new() }; MAX_SMP_CPUS];

/// Per-CPU Task State Segments
pub static mut CPU_TSSES: [TaskStateSegment; MAX_SMP_CPUS] =
    [const { TaskStateSegment::new() }; MAX_SMP_CPUS];

/// BSP-only legacy TSS (kept for backward compatibility; per-CPU code uses CPU_TSSES)
pub static mut TSS: TaskStateSegment = TaskStateSegment::new();

/// Per-CPU dedicated stacks for the double-fault handler (IST 1).
/// Using static storage avoids a heap dependency during early per-CPU setup
/// (load_gdt() may be called before the heap is initialised on some boot paths).
/// 8 KB per CPU is enough for the minimal #DF handler.
const DF_STACK_SIZE: usize = 8192;
static mut DF_STACKS: [[u8; DF_STACK_SIZE]; MAX_SMP_CPUS] = [[0u8; DF_STACK_SIZE]; MAX_SMP_CPUS];

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
#[derive(Clone, Copy)]
struct Gdt {
    entries: [GdtEntry; 8],
    tss_system: GdtEntry, // User system segment (TSS low)
    tss_ignore: GdtEntry, // User system segment (TSS high)
}

/// GDT template (segment descriptors; TSS entry is filled in per-CPU by load_gdt)
const GDT_TEMPLATE: Gdt = Gdt {
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

/// Per-CPU GDT copies (each has its TSS entry pointing to its own CPU_TSSES slot)
static mut CPU_GDTS: [Gdt; MAX_SMP_CPUS] = [GDT_TEMPLATE; MAX_SMP_CPUS];

/// Return the per-CPU array index for the current CPU.
/// The index is derived from the Initial APIC ID (CPUID leaf 1, EBX[31:24])
/// modulo MAX_SMP_CPUS.  All per-CPU arrays (CPU_DATA, CPU_TSSES, CPU_GDTS)
/// are indexed with this value.
pub fn get_cpu_id() -> usize {
    unsafe {
        // Use CPUID leaf 1 to get the initial Local APIC ID.
        // This is safe to call even before the Local APIC is fully initialized or mapped.
        // Intel SDM Vol 2A: CPUID EBX bits 31-24 contain the Initial Local APIC ID.
        let result = core::arch::x86_64::__cpuid(1);
        let id = (result.ebx >> 24) & 0xFF;
        
        // On modern systems (x2APIC), the 8-bit ID from leaf 1 might be truncated.
        // Check if x2APIC is supported and enabled (MSR 0x1B bit 10).
        let mut low: u32;
        let mut high: u32;
        core::arch::asm!("rdmsr", in("ecx") 0x1Bu32, out("eax") low, out("edx") high, options(nomem, nostack, preserves_flags));
        
        let x2apic_enabled = (low & (1 << 10)) != 0;
        if x2apic_enabled {
            // Read full 32-bit x2APIC ID from MSR 0x802
            let id32: u32;
            core::arch::asm!("rdmsr", in("ecx") 0x802u32, out("eax") id32, out("edx") high, options(nomem, nostack, preserves_flags));
            id32 as usize % MAX_SMP_CPUS
        } else {
            id as usize % MAX_SMP_CPUS
        }
    }
}

/// Faster version of get_cpu_id using the GS segment.
/// ONLY safe to call after load_gdt() has initialized the GS base!
pub fn get_cpu_id_gs() -> usize {
    let mut cpu_id: u32;
    unsafe {
        // Try to read CPU ID from GS:[16]
        core::arch::asm!(
            "mov {0:e}, gs:[16]",
            out(reg) cpu_id,
            options(nomem, nostack, preserves_flags)
        );
        
        // If GS is not yet initialized (base=0 or pointing to uninitialized memory),
        // we might read 0xFFFF_FFFF (our sentinel) or even 0 if memory is zeroed.
        // On real hardware, an uninitialized GS base often leads to 0 or 0xFFFFFFFF.
        if cpu_id == 0xFFFF_FFFF || cpu_id >= MAX_SMP_CPUS as u32 {
            return get_cpu_id();
        }
    }
    cpu_id as usize
}

/// Cargar la GDT y TSS
pub fn load_gdt() {
    let cpu_id = get_cpu_id();
    unsafe {
        // Build per-CPU TSS descriptor entry
        let tss_base = &CPU_TSSES[cpu_id] as *const _ as u64;
        let tss_limit = (core::mem::size_of::<TaskStateSegment>() - 1) as u32;
        
        // 0x40: TSS Descriptor (16 bytes)
        // System Segment (0), Type 9 (Available TSS), DPL 0, P 1
        // Access byte: 0b10001001 = 0x89
        CPU_GDTS[cpu_id].tss_system = GdtEntry::new(tss_base as u32, tss_limit, 0x89, 0x00);
        
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
        
        CPU_GDTS[cpu_id].tss_ignore = GdtEntry {
            limit_low: (upper_base & 0xFFFF) as u16,
            base_low: ((upper_base >> 16) & 0xFFFF) as u16,
            base_middle: 0,
            access: 0,
            granularity: 0,
            base_high: 0,
        };

        let descriptor = GdtDescriptor {
            size: (core::mem::size_of::<Gdt>() - 1) as u16,
            offset: &CPU_GDTS[cpu_id] as *const _ as u64,
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

        // Populate IST 1 with a dedicated double-fault stack so that a #DF caused
        // by a kernel stack overflow (the most common trigger) lands on a known-good
        // stack instead of triple-faulting immediately.
        // The KERNEL_IDT entry[8].ist was set to 1 in interrupts::init().
        let df_stack_top = DF_STACKS[cpu_id].as_ptr() as u64 + DF_STACK_SIZE as u64;
        CPU_TSSES[cpu_id].ist1 = df_stack_top & !0xF; // 16-byte aligned
        
        // Set GS.base to point to this CPU's CpuData.
        // We set BOTH IA32_GS_BASE (active) and IA32_KERNEL_GS_BASE (swap)
        // so that the kernel can access per-CPU data (gs:[16] etc) immediately
        // and also handle swapgs correctly during syscall/interrupt entry.
        let cpu_data = &raw mut CPU_DATA[cpu_id];
        (*cpu_data).cpu_id = cpu_id as u32;
        (*cpu_data).current_pid = 0xFFFF_FFFF; // None initially

        let cpu_data_ptr = cpu_data as u64;
        let gs_low = cpu_data_ptr as u32;
        let gs_high = (cpu_data_ptr >> 32) as u32;
        
        // 1. Set IA32_KERNEL_GS_BASE (0xC0000102) - used by swapgs to bring in kernel base
        asm!(
            "wrmsr",
            in("ecx") 0xC0000102u32, 
            in("eax") gs_low,
            in("edx") gs_high,
            options(nomem, nostack, preserves_flags),
        );

        // 2. Set IA32_GS_BASE (0xC0000101) - the active GS base for the current (kernel) mode
        asm!(
            "wrmsr",
            in("ecx") 0xC0000101u32,
            in("eax") gs_low,
            in("edx") gs_high,
            options(nomem, nostack, preserves_flags),
        );

        // GS base is now valid for get_cpu_id_gs() / per-CPU accesses.
        GS_BASE_READY.store(true, Ordering::SeqCst);
    }
}

/// Set the kernel stack pointer for the current CPU's TSS and CpuData.
/// Must be called on context switch so that ring-3 → ring-0 transitions
/// (interrupts, SYSCALL) land on the correct per-CPU kernel stack.
pub fn set_tss_stack(stack_top: u64) {
    let cpu_id = get_cpu_id_gs();
    unsafe {
        CPU_TSSES[cpu_id].rsp0 = stack_top;
        CPU_DATA[cpu_id].rsp0 = stack_top;
    }
}

/// Selectores de segmento
pub const KERNEL_CODE_SELECTOR: u16 = 0x08;
pub const KERNEL_DATA_SELECTOR: u16 = 0x10;
pub const USER_CODE_SELECTOR: u16 = 0x18 | 3; // Ring 3
pub const USER_DATA_SELECTOR: u16 = 0x20 | 3; // Ring 3
pub const TSS_SELECTOR: u16 = 0x40;

/// Habilitar instrucciones SSE, AVX, XSAVE y FSGSBASE según disponibilidad de la CPU.
pub fn enable_cpu_features() {
    unsafe {
        // 1. Habilitar SSE bit en CR0 (Monitor Coprocessor) y limpiar Emulation
        let mut cr0: u64;
        asm!("mov {}, cr0", out(reg) cr0);
        cr0 &= !(1 << 2); // LIMPIAR EM (bit 2)
        cr0 |= 1 << 1;  // SET MP (bit 1)
        asm!("mov cr0, {}", in(reg) cr0);

        // 2. Habilitar características en CR4
        let mut cr4: u64;
        asm!("mov {}, cr4", out(reg) cr4);
        cr4 |= 1 << 9;  // OSFXSR: Soporte para FXSAVE/FXRSTOR
        cr4 |= 1 << 10; // OSXMMEXCPT: Soporte para excepciones SIMD (#XM)

        let cpuid_1 = core::arch::x86_64::__cpuid(1);

        // Habilitar XSAVE y AVX si están soportados
        if (cpuid_1.ecx & (1 << 26)) != 0 {
            // OSXSAVE (bit 18): Requerido para usar XGETBV/XSETBV y AVX
            cr4 |= 1 << 18;

            // Una vez activado OSXSAVE, configuramos XCR0
            // Queremos habilitar: x87 (bit 0), SSE (bit 1)
            let mut xcr0: u64 = 0x01 | 0x02;

            // AVX (bit 28 de ECX en leaf 1)
            if (cpuid_1.ecx & (1 << 28)) != 0 {
                xcr0 |= 0x04; // Habilitar AVX en XCR0
            }

            let low = xcr0 as u32;
            let high = (xcr0 >> 32) as u32;
            // XSETBV: ECX indica el registro (0 para XCR0)
            asm!("xsetbv", in("ecx") 0, in("eax") low, in("edx") high);
        }

        // Habilitar FSGSBASE (bit 0 de EBX en CPUID leaf 7)
        // Permite RDFSBASE/WRFSBASE en userspace para gestionar TLS
        let cpuid_7 = core::arch::x86_64::__cpuid_count(7, 0);
        if (cpuid_7.ebx & (1 << 0)) != 0 {
            cr4 |= 1 << 16;
        }

        asm!("mov cr4, {}", in(reg) cr4);
    }
}
