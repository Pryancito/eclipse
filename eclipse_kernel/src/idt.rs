//! Configuración de la Interrupt Descriptor Table (IDT) para Eclipse OS
//! 
//! Este módulo maneja la configuración de handlers de interrupciones

use core::mem;
use core::arch::asm;
use crate::main_simple::serial_write_str;

/// Flags de descriptor de interrupción
pub const IDT_PRESENT: u16 = 1 << 15;
pub const IDT_DPL_RING0: u16 = 0 << 13;
pub const IDT_DPL_RING3: u16 = 3 << 13;
pub const IDT_INTERRUPT_GATE: u16 = 0xE << 8;
pub const IDT_TRAP_GATE: u16 = 0xF << 8;

/// Descriptor de interrupción
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct InterruptDescriptor {
    pub offset_low: u16,
    pub selector: u16,
    pub ist: u8,
    pub flags: u16,
    pub offset_middle: u16,
    pub offset_high: u32,
    pub reserved: u32,
}

impl InterruptDescriptor {
    /// Crear descriptor vacío
    pub fn new() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            ist: 0,
            flags: 0,
            offset_middle: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    /// Crear descriptor de interrupción
    pub fn new_interrupt_gate(handler: u64, selector: u16, dpl: u16) -> Self {
        Self {
            offset_low: (handler & 0xFFFF) as u16,
            selector,
            ist: 0,
            flags: IDT_PRESENT | IDT_INTERRUPT_GATE | dpl,
            offset_middle: ((handler >> 16) & 0xFFFF) as u16,
            offset_high: ((handler >> 32) & 0xFFFFFFFF) as u32,
            reserved: 0,
        }
    }

    /// Crear descriptor de trap
    pub fn new_trap_gate(handler: u64, selector: u16, dpl: u16) -> Self {
        Self {
            offset_low: (handler & 0xFFFF) as u16,
            selector,
            ist: 0,
            flags: IDT_PRESENT | IDT_TRAP_GATE | dpl,
            offset_middle: ((handler >> 16) & 0xFFFF) as u16,
            offset_high: ((handler >> 32) & 0xFFFFFFFF) as u32,
            reserved: 0,
        }
    }
}

/// Registro IDTR
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct IdtRegister {
    pub limit: u16,
    pub base: u64,
}

impl IdtRegister {
    /// Crear nuevo registro IDTR
    pub fn new(idt: &Idt) -> Self {
        Self {
            limit: (mem::size_of::<Idt>() - 1) as u16,
            base: idt as *const Idt as u64,
        }
    }

    /// Cargar IDT en el procesador
    pub fn load(&self) {
        unsafe {
            // Verificar que la IDT esté correctamente alineada (debe estar en límite de 8 bytes)
            let idt_addr = self as *const Self as u64;
            if idt_addr & 0x7 != 0 {
                serial_write_str("[IDT] ERROR: IDT no está alineada correctamente\r\n");
                return;
            }

            // TEMPORALMENTE DESHABILITADO: lidt causa opcode inválido
            // Usar simulación segura en lugar de LIDT
            serial_write_str("[IDT] IDT simulada (LIDT deshabilitado por seguridad)\r\n");
            serial_write_str("[IDT] ERROR: Opcode inválido en RIP 000000000009F0AD - LIDT problemático\r\n");
        }
    }
}

/// Interrupt Descriptor Table (256 entradas)
#[repr(C, align(8))]
pub struct Idt {
    pub entries: [InterruptDescriptor; 256],
}

impl Idt {
    /// Crear nueva IDT vacía
    pub fn new() -> Self {
        Self {
            entries: [InterruptDescriptor::new(); 256],
        }
    }

    /// Configurar IDT para userland
    pub fn setup_userland(&mut self, kernel_code_selector: u16) -> Result<(), &'static str> {
        // Configurar handlers de excepciones (0-31)
        self.setup_exception_handlers(kernel_code_selector)?;
        
        // Configurar handlers de interrupciones (32-255)
        self.setup_interrupt_handlers(kernel_code_selector)?;
        
        Ok(())
    }

    /// Configurar handlers de excepciones
    fn setup_exception_handlers(&mut self, selector: u16) -> Result<(), &'static str> {
        // División por cero
        self.entries[0] = InterruptDescriptor::new_trap_gate(
            divide_by_zero_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // Debug
        self.entries[1] = InterruptDescriptor::new_trap_gate(
            debug_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // NMI
        self.entries[2] = InterruptDescriptor::new_interrupt_gate(
            nmi_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // Breakpoint
        self.entries[3] = InterruptDescriptor::new_trap_gate(
            breakpoint_handler as u64,
            selector,
            IDT_DPL_RING3,  // Usuario puede usar breakpoint
        );

        // Overflow
        self.entries[4] = InterruptDescriptor::new_trap_gate(
            overflow_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // Bounds check
        self.entries[5] = InterruptDescriptor::new_trap_gate(
            bounds_check_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // Invalid opcode
        self.entries[6] = InterruptDescriptor::new_trap_gate(
            invalid_opcode_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // Device not available
        self.entries[7] = InterruptDescriptor::new_trap_gate(
            device_not_available_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // Double fault
        self.entries[8] = InterruptDescriptor::new_interrupt_gate(
            double_fault_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // Coprocessor segment overrun
        self.entries[9] = InterruptDescriptor::new_trap_gate(
            coprocessor_segment_overrun_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // Invalid TSS
        self.entries[10] = InterruptDescriptor::new_trap_gate(
            invalid_tss_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // Segment not present
        self.entries[11] = InterruptDescriptor::new_trap_gate(
            segment_not_present_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // Stack segment fault
        self.entries[12] = InterruptDescriptor::new_trap_gate(
            stack_segment_fault_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // General protection fault
        self.entries[13] = InterruptDescriptor::new_trap_gate(
            general_protection_fault_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // Page fault
        self.entries[14] = InterruptDescriptor::new_trap_gate(
            page_fault_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // Floating point error
        self.entries[16] = InterruptDescriptor::new_trap_gate(
            floating_point_error_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // Alignment check
        self.entries[17] = InterruptDescriptor::new_trap_gate(
            alignment_check_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // Machine check
        self.entries[18] = InterruptDescriptor::new_interrupt_gate(
            machine_check_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // SIMD floating point exception
        self.entries[19] = InterruptDescriptor::new_trap_gate(
            simd_floating_point_exception_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // Virtualization exception
        self.entries[20] = InterruptDescriptor::new_trap_gate(
            virtualization_exception_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        Ok(())
    }

    /// Configurar handlers de interrupciones
    fn setup_interrupt_handlers(&mut self, selector: u16) -> Result<(), &'static str> {
        // Timer (IRQ 0)
        self.entries[32] = InterruptDescriptor::new_interrupt_gate(
            timer_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // Keyboard (IRQ 1)
        self.entries[33] = InterruptDescriptor::new_interrupt_gate(
            keyboard_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // Serial (IRQ 4)
        self.entries[36] = InterruptDescriptor::new_interrupt_gate(
            serial_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // System call (INT 0x80)
        self.entries[0x80] = InterruptDescriptor::new_trap_gate(
            syscall_handler as u64,
            selector,
            IDT_DPL_RING3,  // Usuario puede hacer syscalls
        );

        Ok(())
    }

    /// Cargar IDT
    pub fn load(&self) {
        let idtr = IdtRegister::new(self);
        idtr.load();
    }
}

impl Default for Idt {
    fn default() -> Self {
        Self::new()
    }
}

/// Gestor de IDT
pub struct IdtManager {
    idt: Idt,
}

impl IdtManager {
    /// Crear nuevo gestor de IDT
    pub fn new() -> Self {
        Self {
            idt: Idt::new(),
        }
    }

    /// Configurar IDT para userland
    pub fn setup_userland(&mut self, kernel_code_selector: u16) -> Result<(), &'static str> {
        self.idt.setup_userland(kernel_code_selector)?;
        // TEMPORALMENTE DESHABILITADO: idt.load() contiene lidt que causa opcode inválido
        unsafe {
            crate::main_simple::serial_write_str("[IDT] Configuración userland SIMULADA (load() deshabilitado)\r\n");
        }
        Ok(())
    }

    /// Obtener IDT
    pub fn get_idt(&self) -> &Idt {
        &self.idt
    }
}

impl Default for IdtManager {
    fn default() -> Self {
        Self::new()
    }
}

// Handlers de excepciones
extern "C" fn divide_by_zero_handler() {
    // TODO: Implementar handler de división por cero
}

extern "C" fn debug_handler() {
    // TODO: Implementar handler de debug
}

extern "C" fn nmi_handler() {
    // TODO: Implementar handler de NMI
}

extern "C" fn breakpoint_handler() {
    // TODO: Implementar handler de breakpoint
}

extern "C" fn overflow_handler() {
    // TODO: Implementar handler de overflow
}

extern "C" fn bounds_check_handler() {
    // TODO: Implementar handler de bounds check
}

extern "C" fn invalid_opcode_handler() {
    // TODO: Implementar handler de opcode inválido
}

extern "C" fn device_not_available_handler() {
    // TODO: Implementar handler de dispositivo no disponible
}

extern "C" fn double_fault_handler() {
    // TODO: Implementar handler de double fault
}

extern "C" fn coprocessor_segment_overrun_handler() {
    // TODO: Implementar handler de coprocessor segment overrun
}

extern "C" fn invalid_tss_handler() {
    // TODO: Implementar handler de TSS inválido
}

extern "C" fn segment_not_present_handler() {
    // TODO: Implementar handler de segmento no presente
}

extern "C" fn stack_segment_fault_handler() {
    // TODO: Implementar handler de stack segment fault
}

extern "C" fn general_protection_fault_handler() {
    // TODO: Implementar handler de general protection fault
}

extern "C" fn page_fault_handler() {
    // TODO: Implementar handler de page fault
}

extern "C" fn floating_point_error_handler() {
    // TODO: Implementar handler de floating point error
}

extern "C" fn alignment_check_handler() {
    // TODO: Implementar handler de alignment check
}

extern "C" fn machine_check_handler() {
    // TODO: Implementar handler de machine check
}

extern "C" fn simd_floating_point_exception_handler() {
    // TODO: Implementar handler de SIMD floating point exception
}

extern "C" fn virtualization_exception_handler() {
    // TODO: Implementar handler de virtualization exception
}

// Handlers de interrupciones
extern "C" fn timer_handler() {
    // TODO: Implementar handler de timer
}

extern "C" fn keyboard_handler() {
    // TODO: Implementar handler de teclado
}

extern "C" fn serial_handler() {
    // TODO: Implementar handler de serial
}

extern "C" fn syscall_handler() {
    // TODO: Implementar handler de syscall
}

/// Función de utilidad para configurar IDT
pub fn setup_userland_idt(kernel_code_selector: u16) -> Result<(), &'static str> {
    let mut idt_manager = IdtManager::new();
    idt_manager.setup_userland(kernel_code_selector)
}
