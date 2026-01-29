//! Configuración de la Interrupt Descriptor Table (IDT) para Eclipse OS
//!
//! Este módulo maneja la configuración de handlers de interrupciones

use core::arch::asm;
use core::mem;
use core::sync::atomic::{AtomicU64, AtomicU32, Ordering};
use alloc::string::String;
use alloc::vec::Vec;

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
            asm!("lidt [{}]", in(reg) self as *const Self as u64, options(nomem, nostack));
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
        self.entries[1] =
            InterruptDescriptor::new_trap_gate(debug_handler as u64, selector, IDT_DPL_RING0);

        // NMI
        self.entries[2] =
            InterruptDescriptor::new_interrupt_gate(nmi_handler as u64, selector, IDT_DPL_RING0);

        // Breakpoint
        self.entries[3] = InterruptDescriptor::new_trap_gate(
            breakpoint_handler as u64,
            selector,
            IDT_DPL_RING3, // Usuario puede usar breakpoint
        );

        // Overflow
        self.entries[4] =
            InterruptDescriptor::new_trap_gate(overflow_handler as u64, selector, IDT_DPL_RING0);

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
        self.entries[10] =
            InterruptDescriptor::new_trap_gate(invalid_tss_handler as u64, selector, IDT_DPL_RING0);

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
        self.entries[14] =
            InterruptDescriptor::new_trap_gate(page_fault_handler as u64, selector, IDT_DPL_RING0);

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
        self.entries[32] =
            InterruptDescriptor::new_interrupt_gate(timer_handler as u64, selector, IDT_DPL_RING0);

        // Keyboard (IRQ 1)
        self.entries[33] = InterruptDescriptor::new_interrupt_gate(
            keyboard_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // Serial (IRQ 4)
        self.entries[36] =
            InterruptDescriptor::new_interrupt_gate(serial_handler as u64, selector, IDT_DPL_RING0);

        // Mouse (IRQ 12 = IRQ 12 + 32 = 44)
        self.entries[44] = InterruptDescriptor::new_interrupt_gate(
            mouse_handler as u64,
            selector,
            IDT_DPL_RING0,
        );

        // System call (INT 0x80)
        self.entries[0x80] = InterruptDescriptor::new_trap_gate(
            syscall_handler as u64,
            selector,
            IDT_DPL_RING3, // Usuario puede hacer syscalls
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
        Self { idt: Idt::new() }
    }

    /// Configurar IDT para userland
    pub fn setup_userland(&mut self, kernel_code_selector: u16) -> Result<(), &'static str> {
        self.idt.setup_userland(kernel_code_selector)?;
        self.idt.load();
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


/// Función de utilidad para configurar IDT
pub fn setup_userland_idt(kernel_code_selector: u16) -> Result<(), &'static str> {
    let mut idt_manager = IdtManager::new();
    idt_manager.setup_userland(kernel_code_selector)
}

// ============================================================================
// SISTEMA DE ESTADÍSTICAS Y LOGGING DE INTERRUPCIONES
// ============================================================================

/// Estadísticas de interrupciones
#[derive(Debug, Clone, Copy)]
pub struct InterruptStats {
    pub total_interrupts: u64,
    pub exceptions: u64,
    pub timer_interrupts: u64,
    pub keyboard_interrupts: u64,
    pub mouse_interrupts: u64,
    pub serial_interrupts: u64,
    pub syscalls: u64,
    pub page_faults: u64,
    pub general_protection_faults: u64,
}

impl Default for InterruptStats {
    fn default() -> Self {
        Self {
            total_interrupts: 0,
            exceptions: 0,
            timer_interrupts: 0,
            keyboard_interrupts: 0,
            mouse_interrupts: 0,
            serial_interrupts: 0,
            syscalls: 0,
            page_faults: 0,
            general_protection_faults: 0,
        }
    }
}

/// Contadores atómicos para estadísticas
static INTERRUPT_STATS: InterruptStats = InterruptStats {
    total_interrupts: 0,
    exceptions: 0,
    timer_interrupts: 0,
    keyboard_interrupts: 0,
    mouse_interrupts: 0,
    serial_interrupts: 0,
    syscalls: 0,
    page_faults: 0,
    general_protection_faults: 0,
};

static TOTAL_INTERRUPTS: AtomicU64 = AtomicU64::new(0);
static EXCEPTIONS: AtomicU64 = AtomicU64::new(0);
static TIMER_INTERRUPTS: AtomicU64 = AtomicU64::new(0);
static KEYBOARD_INTERRUPTS: AtomicU64 = AtomicU64::new(0);
static MOUSE_INTERRUPTS: AtomicU64 = AtomicU64::new(0);
static SERIAL_INTERRUPTS: AtomicU64 = AtomicU64::new(0);
static SYSCALLS: AtomicU64 = AtomicU64::new(0);
static PAGE_FAULTS: AtomicU64 = AtomicU64::new(0);
static GP_FAULTS: AtomicU64 = AtomicU64::new(0);

/// Obtener estadísticas de interrupciones
pub fn get_interrupt_stats() -> InterruptStats {
    InterruptStats {
        total_interrupts: TOTAL_INTERRUPTS.load(Ordering::Relaxed),
        exceptions: EXCEPTIONS.load(Ordering::Relaxed),
        timer_interrupts: TIMER_INTERRUPTS.load(Ordering::Relaxed),
        keyboard_interrupts: KEYBOARD_INTERRUPTS.load(Ordering::Relaxed),
        mouse_interrupts: MOUSE_INTERRUPTS.load(Ordering::Relaxed),
        serial_interrupts: SERIAL_INTERRUPTS.load(Ordering::Relaxed),
        syscalls: SYSCALLS.load(Ordering::Relaxed),
        page_faults: PAGE_FAULTS.load(Ordering::Relaxed),
        general_protection_faults: GP_FAULTS.load(Ordering::Relaxed),
    }
}

/// Función de logging para interrupciones
fn log_interrupt(interrupt_type: &str, details: &str) {
    // Por ahora solo incrementamos contadores
    // En una implementación completa, esto escribiría a un buffer de log
    TOTAL_INTERRUPTS.fetch_add(1, Ordering::Relaxed);
}

// ============================================================================
// HANDLERS DE EXCEPCIONES IMPLEMENTADOS
// ============================================================================

/// Handler de división por cero
extern "C" fn divide_by_zero_handler() {
    EXCEPTIONS.fetch_add(1, Ordering::Relaxed);
    log_interrupt("DIVIDE_BY_ZERO", "División por cero detectada");
    
    // En una implementación completa, esto mataría el proceso
    // Por ahora solo registramos el evento
    unsafe {
        // Escribir mensaje de error a la consola serial
        let msg = b"ERROR: Division by zero exception\n";
        for &byte in msg {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
        }
    }
}

/// Handler de debug
extern "C" fn debug_handler() {
    EXCEPTIONS.fetch_add(1, Ordering::Relaxed);
    log_interrupt("DEBUG", "Debug exception");
    
    // Debug exception - normalmente usado por debugger
    unsafe {
        let msg = b"DEBUG: Debug exception triggered\n";
        for &byte in msg {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
        }
    }
}

/// Handler de NMI (Non-Maskable Interrupt)
extern "C" fn nmi_handler() {
    EXCEPTIONS.fetch_add(1, Ordering::Relaxed);
    log_interrupt("NMI", "Non-maskable interrupt");
    
    // NMI - interrupción crítica del sistema
    unsafe {
        let msg = b"CRITICAL: NMI received\n";
        for &byte in msg {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
        }
    }
}

/// Handler de breakpoint
extern "C" fn breakpoint_handler() {
    EXCEPTIONS.fetch_add(1, Ordering::Relaxed);
    log_interrupt("BREAKPOINT", "Breakpoint exception");
    
    // Breakpoint - usado por debugger
    unsafe {
        let msg = b"DEBUG: Breakpoint hit\n";
        for &byte in msg {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
        }
    }
}

/// Handler de overflow
extern "C" fn overflow_handler() {
    EXCEPTIONS.fetch_add(1, Ordering::Relaxed);
    log_interrupt("OVERFLOW", "Integer overflow");
    
    unsafe {
        let msg = b"ERROR: Integer overflow\n";
        for &byte in msg {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
        }
    }
}

/// Handler de bounds check
extern "C" fn bounds_check_handler() {
    EXCEPTIONS.fetch_add(1, Ordering::Relaxed);
    log_interrupt("BOUNDS_CHECK", "Bounds check violation");
    
    unsafe {
        let msg = b"ERROR: Bounds check violation\n";
        for &byte in msg {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
        }
    }
}

/// Handler de opcode inválido
extern "C" fn invalid_opcode_handler() {
    EXCEPTIONS.fetch_add(1, Ordering::Relaxed);
    log_interrupt("INVALID_OPCODE", "Invalid opcode executed");
    
    unsafe {
        let msg = b"ERROR: Invalid opcode\n";
        for &byte in msg {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
        }
    }
}

/// Handler de dispositivo no disponible
extern "C" fn device_not_available_handler() {
    EXCEPTIONS.fetch_add(1, Ordering::Relaxed);
    log_interrupt("DEVICE_NOT_AVAILABLE", "Device not available");
    
    unsafe {
        let msg = b"WARNING: Device not available\n";
        for &byte in msg {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
        }
    }
}

/// Handler de double fault
extern "C" fn double_fault_handler() {
    EXCEPTIONS.fetch_add(1, Ordering::Relaxed);
    log_interrupt("DOUBLE_FAULT", "Double fault - system may be unstable");
    
    // Double fault es crítico - el sistema puede estar inestable
    unsafe {
        let msg = b"CRITICAL: Double fault - system unstable\n";
        for &byte in msg {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
        }
        
        // En una implementación completa, esto podría intentar recuperación
        // o reiniciar el sistema
    }
}

/// Handler de coprocessor segment overrun
extern "C" fn coprocessor_segment_overrun_handler() {
    EXCEPTIONS.fetch_add(1, Ordering::Relaxed);
    log_interrupt("COPROCESSOR_OVERRUN", "Coprocessor segment overrun");
    
    unsafe {
        let msg = b"ERROR: Coprocessor segment overrun\n";
        for &byte in msg {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
        }
    }
}

/// Handler de TSS inválido
extern "C" fn invalid_tss_handler() {
    EXCEPTIONS.fetch_add(1, Ordering::Relaxed);
    log_interrupt("INVALID_TSS", "Invalid TSS");
    
    unsafe {
        let msg = b"ERROR: Invalid TSS\n";
        for &byte in msg {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
        }
    }
}

/// Handler de segmento no presente
extern "C" fn segment_not_present_handler() {
    EXCEPTIONS.fetch_add(1, Ordering::Relaxed);
    log_interrupt("SEGMENT_NOT_PRESENT", "Segment not present");
    
    unsafe {
        let msg = b"ERROR: Segment not present\n";
        for &byte in msg {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
        }
    }
}

/// Handler de stack segment fault
extern "C" fn stack_segment_fault_handler() {
    EXCEPTIONS.fetch_add(1, Ordering::Relaxed);
    log_interrupt("STACK_SEGMENT_FAULT", "Stack segment fault");
    
    unsafe {
        let msg = b"ERROR: Stack segment fault\n";
        for &byte in msg {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
        }
    }
}

/// Handler de general protection fault
extern "C" fn general_protection_fault_handler() {
    GP_FAULTS.fetch_add(1, Ordering::Relaxed);
    log_interrupt("GENERAL_PROTECTION_FAULT", "General protection fault");
    
    unsafe {
        let msg = b"ERROR: General protection fault\n";
        for &byte in msg {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
        }
    }
}

/// Handler de page fault
extern "C" fn page_fault_handler() {
    PAGE_FAULTS.fetch_add(1, Ordering::Relaxed);
    log_interrupt("PAGE_FAULT", "Page fault");
    
    // Page fault es común y manejable
    unsafe {
        let msg = b"INFO: Page fault handled\n";
        for &byte in msg {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
        }
    }
}

/// Handler de floating point error
extern "C" fn floating_point_error_handler() {
    EXCEPTIONS.fetch_add(1, Ordering::Relaxed);
    log_interrupt("FLOATING_POINT_ERROR", "Floating point error");
    
    unsafe {
        let msg = b"ERROR: Floating point error\n";
        for &byte in msg {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
        }
    }
}

/// Handler de alignment check
extern "C" fn alignment_check_handler() {
    EXCEPTIONS.fetch_add(1, Ordering::Relaxed);
    log_interrupt("ALIGNMENT_CHECK", "Alignment check failed");
    
    unsafe {
        let msg = b"ERROR: Alignment check failed\n";
        for &byte in msg {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
        }
    }
}

/// Handler de machine check
extern "C" fn machine_check_handler() {
    EXCEPTIONS.fetch_add(1, Ordering::Relaxed);
    log_interrupt("MACHINE_CHECK", "Machine check error");
    
    // Machine check es crítico - error de hardware
    unsafe {
        let msg = b"CRITICAL: Machine check error\n";
        for &byte in msg {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
        }
    }
}

/// Handler de SIMD floating point exception
extern "C" fn simd_floating_point_exception_handler() {
    EXCEPTIONS.fetch_add(1, Ordering::Relaxed);
    log_interrupt("SIMD_FP_EXCEPTION", "SIMD floating point exception");
    
    unsafe {
        let msg = b"ERROR: SIMD floating point exception\n";
        for &byte in msg {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
        }
    }
}

/// Handler de virtualization exception
extern "C" fn virtualization_exception_handler() {
    EXCEPTIONS.fetch_add(1, Ordering::Relaxed);
    log_interrupt("VIRTUALIZATION_EXCEPTION", "Virtualization exception");
    
    unsafe {
        let msg = b"ERROR: Virtualization exception\n";
        for &byte in msg {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
        }
    }
}

// ============================================================================
// HANDLERS DE INTERRUPCIONES IMPLEMENTADOS
// ============================================================================

/// Handler de timer
extern "C" fn timer_handler() {
    TIMER_INTERRUPTS.fetch_add(1, Ordering::Relaxed);
    
    // Timer interrupt - usado para scheduling
    // En una implementación completa, esto actualizaría el scheduler
    unsafe {
        // Acknowledge interrupt
        asm!("mov al, 0x20; out 0x20, al", options(nostack, nomem));
    }
}

/// Handler de teclado
extern "C" fn keyboard_handler() {
    KEYBOARD_INTERRUPTS.fetch_add(1, Ordering::Relaxed);
    
    // Llamar al manejador PS/2 del teclado
    crate::drivers::ps2_integration::handle_ps2_keyboard_interrupt();
    
    // Acknowledge interrupt (PIC primario)
    unsafe {
        asm!("mov al, 0x20; out 0x20, al", options(nostack, nomem));
    }
}

/// Handler de ratón
extern "C" fn mouse_handler() {
    MOUSE_INTERRUPTS.fetch_add(1, Ordering::Relaxed);
    
    // Llamar al manejador PS/2 del ratón
    crate::drivers::ps2_integration::handle_ps2_mouse_interrupt();
    
    // Acknowledge interrupt (PIC secundario y primario para IRQ 12)
    unsafe {
        asm!("mov al, 0x20; out 0xA0, al", options(nostack, nomem)); // PIC secundario
        asm!("mov al, 0x20; out 0x20, al", options(nostack, nomem)); // PIC primario
    }
}

/// Handler de serial
extern "C" fn serial_handler() {
    SERIAL_INTERRUPTS.fetch_add(1, Ordering::Relaxed);
    
    // Serial interrupt - leer datos del puerto serial
    unsafe {
        // Acknowledge interrupt
        asm!("mov al, 0x20; out 0x20, al", options(nostack, nomem));
        
        // En una implementación completa, esto leería datos del puerto serial
    }
}

/// Handler de syscall
extern "C" fn syscall_handler() {
    SYSCALLS.fetch_add(1, Ordering::Relaxed);
    
    // System call handler
    // En una implementación completa, esto procesaría syscalls del userland
    unsafe {
        let msg = b"INFO: System call received\n";
        for &byte in msg {
            asm!("out dx, al", in("dx") 0x3F8u16, in("al") byte, options(nostack, nomem));
        }
    }
}
