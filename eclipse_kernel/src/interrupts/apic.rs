//! Advanced Programmable Interrupt Controller (APIC) para Eclipse OS
//!
//! Este módulo maneja la configuración y el control del APIC local y I/O APIC
//! para interrupciones de hardware modernas.

use core::arch::asm;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicBool, Ordering};

/// Registros del Local APIC
const APIC_ID_REG: u32 = 0x020;
const APIC_VERSION_REG: u32 = 0x030;
const APIC_TPR_REG: u32 = 0x080;
const APIC_APR_REG: u32 = 0x090;
const APIC_PPR_REG: u32 = 0x0A0;
const APIC_EOI_REG: u32 = 0x0B0;
const APIC_LDR_REG: u32 = 0x0D0;
const APIC_DFR_REG: u32 = 0x0E0;
const APIC_SIVR_REG: u32 = 0x0F0;
const APIC_ISR_BASE: u32 = 0x100;
const APIC_TMR_BASE: u32 = 0x180;
const APIC_IRR_BASE: u32 = 0x200;
const APIC_ESR_REG: u32 = 0x280;
const APIC_ICR_LOW: u32 = 0x300;
const APIC_ICR_HIGH: u32 = 0x310;
const APIC_LVT_TIMER: u32 = 0x320;
const APIC_LVT_THERMAL: u32 = 0x330;
const APIC_LVT_PERF: u32 = 0x340;
const APIC_LVT_LINT0: u32 = 0x350;
const APIC_LVT_LINT1: u32 = 0x360;
const APIC_LVT_ERROR: u32 = 0x370;
const APIC_TIMER_INITIAL_COUNT: u32 = 0x380;
const APIC_TIMER_CURRENT_COUNT: u32 = 0x390;
const APIC_TIMER_DIVIDE_CONFIG: u32 = 0x3E0;

/// Flags del Local APIC
const APIC_SIVR_ENABLE: u32 = 0x100;
const APIC_SIVR_FOCUS_DISABLED: u32 = 0x200;

/// Flags del ICR (Interrupt Command Register)
const APIC_ICR_DELIVERY_MODE_FIXED: u32 = 0x000;
const APIC_ICR_DELIVERY_MODE_LOWEST_PRIORITY: u32 = 0x001;
const APIC_ICR_DELIVERY_MODE_SMI: u32 = 0x002;
const APIC_ICR_DELIVERY_MODE_NMI: u32 = 0x004;
const APIC_ICR_DELIVERY_MODE_INIT: u32 = 0x005;
const APIC_ICR_DELIVERY_MODE_STARTUP: u32 = 0x006;
const APIC_ICR_DELIVERY_MODE_EXTINT: u32 = 0x007;

const APIC_ICR_DEST_MODE_PHYSICAL: u32 = 0x000;
const APIC_ICR_DEST_MODE_LOGICAL: u32 = 0x800;

const APIC_ICR_LEVEL_DEASSERT: u32 = 0x000;
const APIC_ICR_LEVEL_ASSERT: u32 = 0x4000;

const APIC_ICR_TRIGGER_EDGE: u32 = 0x000;
const APIC_ICR_TRIGGER_LEVEL: u32 = 0x8000;

/// Flags del LVT (Local Vector Table)
const APIC_LVT_MASKED: u32 = 0x10000;
const APIC_LVT_TIMER_MODE_PERIODIC: u32 = 0x20000;
const APIC_LVT_TIMER_MODE_ONE_SHOT: u32 = 0x00000;

/// Gestor del Local APIC
pub struct ApicManager {
    apic_base: AtomicU64,
    initialized: AtomicBool,
    apic_id: AtomicU32,
}

impl ApicManager {
    /// Crear nuevo gestor de APIC
    pub fn new() -> Self {
        Self {
            apic_base: AtomicU64::new(0),
            initialized: AtomicBool::new(false),
            apic_id: AtomicU32::new(0),
        }
    }

    /// Inicializar el Local APIC
    pub fn initialize(&self) -> Result<(), &'static str> {
        if self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }

        // Obtener base del APIC desde MSR
        let apic_base = self.read_msr(0x1B)?;
        self.apic_base.store(apic_base & 0xFFFFF000, Ordering::Release);

        // Habilitar APIC
        self.write_msr(0x1B, (apic_base & 0xFFFFF000) | 0x800)?;

        // Obtener ID del APIC
        let apic_id = self.read_apic_register(APIC_ID_REG) >> 24;
        self.apic_id.store(apic_id, Ordering::Release);

        // Configurar SIVR (Spurious Interrupt Vector Register)
        let sivr = self.read_apic_register(APIC_SIVR_REG);
        self.write_apic_register(APIC_SIVR_REG, sivr | APIC_SIVR_ENABLE | APIC_SIVR_FOCUS_DISABLED);

        // Configurar LVT entries
        self.setup_lvt_entries()?;

        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    /// Configurar entradas LVT
    fn setup_lvt_entries(&self) -> Result<(), &'static str> {
        // LVT Timer - vector 32
        self.write_apic_register(APIC_LVT_TIMER, 32 | APIC_LVT_TIMER_MODE_ONE_SHOT);

        // LVT LINT0 - NMI
        self.write_apic_register(APIC_LVT_LINT0, 2 | APIC_ICR_DELIVERY_MODE_NMI);

        // LVT LINT1 - masked
        self.write_apic_register(APIC_LVT_LINT1, APIC_LVT_MASKED);

        // LVT Error - vector 33
        self.write_apic_register(APIC_LVT_ERROR, 33);

        // LVT Thermal - masked
        self.write_apic_register(APIC_LVT_THERMAL, APIC_LVT_MASKED);

        // LVT Performance - masked
        self.write_apic_register(APIC_LVT_PERF, APIC_LVT_MASKED);

        Ok(())
    }

    /// Enviar EOI (End of Interrupt)
    pub fn send_eoi(&self) {
        if self.initialized.load(Ordering::Acquire) {
            self.write_apic_register(APIC_EOI_REG, 0);
        }
    }

    /// Configurar timer del APIC
    pub fn setup_timer(&self, vector: u8, initial_count: u32, divide_config: u8) -> Result<(), &'static str> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err("APIC no inicializado");
        }

        // Configurar divisor del timer
        self.write_apic_register(APIC_TIMER_DIVIDE_CONFIG, divide_config as u32);

        // Configurar LVT Timer
        self.write_apic_register(APIC_LVT_TIMER, vector as u32 | APIC_LVT_TIMER_MODE_ONE_SHOT);

        // Establecer contador inicial
        self.write_apic_register(APIC_TIMER_INITIAL_COUNT, initial_count);

        Ok(())
    }

    /// Enviar IPI (Inter-Processor Interrupt)
    pub fn send_ipi(&self, destination: u8, vector: u8, delivery_mode: u32) -> Result<(), &'static str> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err("APIC no inicializado");
        }

        // Configurar ICR High (destino)
        self.write_apic_register(APIC_ICR_HIGH, (destination as u32) << 24);

        // Configurar ICR Low (vector y modo)
        let icr_low = vector as u32 | delivery_mode | APIC_ICR_DEST_MODE_PHYSICAL | APIC_ICR_LEVEL_ASSERT | APIC_ICR_TRIGGER_EDGE;
        self.write_apic_register(APIC_ICR_LOW, icr_low);

        // Esperar a que se envíe
        while (self.read_apic_register(APIC_ICR_LOW) & 0x1000) != 0 {
            core::hint::spin_loop();
        }

        Ok(())
    }

    /// Leer registro del APIC
    fn read_apic_register(&self, offset: u32) -> u32 {
        let base = self.apic_base.load(Ordering::Acquire);
        unsafe {
            core::ptr::read_volatile((base + offset as u64) as *const u32)
        }
    }

    /// Escribir registro del APIC
    fn write_apic_register(&self, offset: u32, value: u32) {
        let base = self.apic_base.load(Ordering::Acquire);
        unsafe {
            core::ptr::write_volatile((base + offset as u64) as *mut u32, value);
        }
    }

    /// Leer MSR
    fn read_msr(&self, msr: u32) -> Result<u64, &'static str> {
        unsafe {
            let mut low: u32;
            let mut high: u32;
            asm!(
                "rdmsr",
                out("eax") low,
                out("edx") high,
                in("ecx") msr,
                options(nostack, preserves_flags)
            );
            Ok(((high as u64) << 32) | (low as u64))
        }
    }

    /// Escribir MSR
    fn write_msr(&self, msr: u32, value: u64) -> Result<(), &'static str> {
        unsafe {
            let low = value as u32;
            let high = (value >> 32) as u32;
            asm!(
                "wrmsr",
                in("eax") low,
                in("edx") high,
                in("ecx") msr,
                options(nostack, preserves_flags)
            );
        }
        Ok(())
    }

    /// Verificar si el APIC está inicializado
    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// Obtener ID del APIC
    pub fn get_apic_id(&self) -> u32 {
        self.apic_id.load(Ordering::Acquire)
    }

    /// Obtener base del APIC
    pub fn get_apic_base(&self) -> u64 {
        self.apic_base.load(Ordering::Acquire)
    }

    /// Verificar si el APIC está disponible
    pub fn is_available(&self) -> bool {
        // Verificar bit 9 en CPUID.1:EDX (APIC bit)
        unsafe {
            let mut eax: u32;
            let mut ecx: u32;
            let mut edx: u32;
            
            asm!(
                "cpuid",
                inout("eax") 1 => eax,
                out("ecx") ecx,
                out("edx") edx,
                options(nostack, preserves_flags)
            );
            
            (edx & (1 << 9)) != 0
        }
    }
}

impl Default for ApicManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Delivery Modes para IPI
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u32)]
pub enum IpiDeliveryMode {
    Fixed = 0x000,
    LowestPriority = 0x001,
    Smi = 0x002,
    Nmi = 0x004,
    Init = 0x005,
    StartUp = 0x006,
    ExtInt = 0x007,
}

static mut GLOBAL_APIC_MANAGER: Option<ApicManager> = None;

/// Inicializar el APIC globalmente
pub fn initialize_apic() -> Result<(), &'static str> {
    let mut manager = ApicManager::new();
    if manager.is_available() {
        manager.initialize()?;
        unsafe {
            GLOBAL_APIC_MANAGER = Some(manager);
        }
        Ok(())
    } else {
        Err("APIC no disponible")
    }
}

/// Obtener la base del APIC
pub fn get_apic_base() -> u64 {
    unsafe {
        if let Some(manager) = &GLOBAL_APIC_MANAGER {
            manager.get_apic_base()
        } else {
            0
        }
    }
}

/// Enviar IPI (Inter-Processor Interrupt)
pub fn send_ipi(destination: u8, vector: u8, mode: IpiDeliveryMode) {
    unsafe {
        if let Some(manager) = &GLOBAL_APIC_MANAGER {
             // We map our Enum to the u32 expected by internal send_ipi
             // But internal send_ipi expects u32 mode. 
             // Let's modify the internal one or casting.
             // The internal `send_ipi` took `u32`.
             let mode_val = mode as u32;
             // Note: internal send_ipi returns Result, here we swallow it or panic? 
             // For now we swallow as signature in smp.rs is unsafe block.
             let _ = manager.send_ipi(destination, vector, mode_val);
        }
    }
}

/// Función de utilidad para enviar EOI
pub fn send_apic_eoi() {
    unsafe {
        if let Some(manager) = &GLOBAL_APIC_MANAGER {
            manager.send_eoi();
        }
    }
}

/// Obtener el LAPIC ID del procesador actual usando CPUID
/// Esta función lee el LAPIC ID directamente del procesador,
/// útil para identificar el BSP (Bootstrap Processor)
pub fn get_current_lapic_id() -> u8 {
    unsafe {
        let mut ebx_val: u32;
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "mov {0:e}, ebx",
            "pop rbx",
            out(reg) ebx_val,
            inout("eax") 1 => _,
            out("ecx") _,
            out("edx") _,
            options(nostack, preserves_flags)
        );
        // LAPIC ID está en bits 24-31 de EBX
        ((ebx_val >> 24) & 0xFF) as u8
    }
}
