//! Configuración de la Global Descriptor Table (GDT) para Eclipse OS
//!
//! Este módulo maneja la configuración de descriptores de segmento para kernel y userland

use core::arch::asm;
use core::mem;

/// Flags de descriptor de segmento
pub const GDT_ACCESSED: u8 = 1 << 0;
pub const GDT_READ_WRITE: u8 = 1 << 1;
pub const GDT_CONFORMING: u8 = 1 << 2;
pub const GDT_EXECUTABLE: u8 = 1 << 3;
pub const GDT_CODE_DATA: u8 = 1 << 4;
pub const GDT_DPL_RING0: u8 = 0 << 5;
pub const GDT_DPL_RING3: u8 = 3 << 5;
pub const GDT_PRESENT: u8 = 1 << 7;

/// Flags de granularidad
pub const GDT_GRANULARITY: u8 = 1 << 3;
pub const GDT_32BIT: u8 = 0 << 2;
pub const GDT_64BIT: u8 = 1 << 2;

/// Descriptor de segmento
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct SegmentDescriptor {
    pub limit_low: u16,
    pub base_low: u16,
    pub base_middle: u8,
    pub access: u8,
    pub granularity: u8,
    pub base_high: u8,
}

impl SegmentDescriptor {
    /// Crear descriptor vacío
    pub fn new() -> Self {
        Self {
            limit_low: 0,
            base_low: 0,
            base_middle: 0,
            access: 0,
            granularity: 0,
            base_high: 0,
        }
    }

    /// Crear descriptor de código
    pub fn new_code_segment(base: u32, limit: u32, dpl: u8) -> Self {
        let mut desc = Self::new();

        desc.limit_low = (limit & 0xFFFF) as u16;
        desc.base_low = (base & 0xFFFF) as u16;
        desc.base_middle = ((base >> 16) & 0xFF) as u8;
        desc.base_high = ((base >> 24) & 0xFF) as u8;

        desc.access = GDT_PRESENT | GDT_CODE_DATA | GDT_EXECUTABLE | GDT_READ_WRITE | dpl;
        desc.granularity = GDT_GRANULARITY | GDT_64BIT | (((limit >> 16) & 0xF) as u8);

        desc
    }

    /// Crear descriptor de datos
    pub fn new_data_segment(base: u32, limit: u32, dpl: u8) -> Self {
        let mut desc = Self::new();

        desc.limit_low = (limit & 0xFFFF) as u16;
        desc.base_low = (base & 0xFFFF) as u16;
        desc.base_middle = ((base >> 16) & 0xFF) as u8;
        desc.base_high = ((base >> 24) & 0xFF) as u8;

        desc.access = GDT_PRESENT | GDT_CODE_DATA | GDT_READ_WRITE | dpl;
        desc.granularity = GDT_GRANULARITY | GDT_64BIT | (((limit >> 16) & 0xF) as u8);

        desc
    }

    /// Crear descriptor de TSS (Task State Segment)
    pub fn new_tss_segment(base: u64, limit: u32) -> Self {
        let mut desc = Self::new();

        desc.limit_low = (limit & 0xFFFF) as u16;
        desc.base_low = (base & 0xFFFF) as u16;
        desc.base_middle = ((base >> 16) & 0xFF) as u8;
        desc.base_high = ((base >> 24) & 0xFF) as u8;

        desc.access = GDT_PRESENT | GDT_CODE_DATA | GDT_ACCESSED | GDT_DPL_RING0;
        desc.granularity = GDT_GRANULARITY | GDT_64BIT | (((limit >> 16) & 0xF) as u8);

        desc
    }
}

/// Registro GDTR
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct GdtRegister {
    pub limit: u16,
    pub base: u64,
}

impl GdtRegister {
    /// Crear nuevo registro GDTR
    pub fn new(gdt: &Gdt) -> Self {
        Self {
            limit: (mem::size_of::<Gdt>() - 1) as u16,
            base: gdt as *const Gdt as u64,
        }
    }

    /// Cargar GDT en el procesador
    pub fn load(&self) {
        unsafe {
            asm!("lgdt [{}]", in(reg) self as *const Self as u64, options(nomem, nostack));
        }
    }
}

/// Global Descriptor Table
#[repr(C, align(8))]
pub struct Gdt {
    pub null: SegmentDescriptor,
    pub kernel_code: SegmentDescriptor,
    pub kernel_data: SegmentDescriptor,
    pub user_code: SegmentDescriptor,
    pub user_data: SegmentDescriptor,
    pub tss: SegmentDescriptor,
}

impl Gdt {
    /// Crear nueva GDT
    pub fn new() -> Self {
        Self {
            null: SegmentDescriptor::new(),
            kernel_code: SegmentDescriptor::new_code_segment(0, 0xFFFFF, GDT_DPL_RING0),
            kernel_data: SegmentDescriptor::new_data_segment(0, 0xFFFFF, GDT_DPL_RING0),
            user_code: SegmentDescriptor::new_code_segment(0, 0xFFFFF, GDT_DPL_RING3),
            user_data: SegmentDescriptor::new_data_segment(0, 0xFFFFF, GDT_DPL_RING3),
            tss: SegmentDescriptor::new(),
        }
    }

    /// Configurar GDT para userland
    pub fn setup_userland(&mut self) -> Result<(), &'static str> {
        // Configurar descriptores de userland
        self.user_code = SegmentDescriptor::new_code_segment(0, 0xFFFFF, GDT_DPL_RING3);
        self.user_data = SegmentDescriptor::new_data_segment(0, 0xFFFFF, GDT_DPL_RING3);

        Ok(())
    }

    /// Cargar GDT
    pub fn load(&self) {
        let gdtr = GdtRegister::new(self);
        gdtr.load();
    }

    /// Obtener selector de código de kernel
    pub fn get_kernel_code_selector(&self) -> u16 {
        0x08 // Índice 1, RPL 0, TI 0
    }

    /// Obtener selector de datos de kernel
    pub fn get_kernel_data_selector(&self) -> u16 {
        0x10 // Índice 2, RPL 0, TI 0
    }

    /// Obtener selector de código de usuario
    pub fn get_user_code_selector(&self) -> u16 {
        0x2B // Índice 5, RPL 3, TI 0
    }

    /// Obtener selector de datos de usuario
    pub fn get_user_data_selector(&self) -> u16 {
        0x23 // Índice 4, RPL 3, TI 0
    }

    /// Obtener selector de TSS
    pub fn get_tss_selector(&self) -> u16 {
        0x30 // Índice 6, RPL 0, TI 0
    }
}

impl Default for Gdt {
    fn default() -> Self {
        Self::new()
    }
}

/// Gestor de GDT
pub struct GdtManager {
    gdt: Gdt,
}

impl GdtManager {
    /// Crear nuevo gestor de GDT
    pub fn new() -> Self {
        Self { gdt: Gdt::new() }
    }

    /// Configurar GDT para userland
    pub fn setup_userland(&mut self) -> Result<(), &'static str> {
        self.gdt.setup_userland()?;
        self.gdt.load();
        Ok(())
    }

    /// Obtener GDT
    pub fn get_gdt(&self) -> &Gdt {
        &self.gdt
    }

    /// Obtener GDT mutable
    pub fn get_gdt_mut(&mut self) -> &mut Gdt {
        &mut self.gdt
    }
}

impl Default for GdtManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Función de utilidad para configurar GDT
pub fn setup_userland_gdt() -> Result<(), &'static str> {
    let mut gdt_manager = GdtManager::new();
    gdt_manager.setup_userland()
}
