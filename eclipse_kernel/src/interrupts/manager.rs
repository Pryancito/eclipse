//! Gestor principal de interrupciones para Eclipse OS
//!
//! Este módulo coordina todos los componentes del sistema de interrupciones
//! incluyendo PIC, APIC, IRQs y handlers.

use core::sync::atomic::{AtomicBool, Ordering};
use alloc::vec::Vec;
use crate::idt::{IdtManager, setup_userland_idt};
use super::pic::PicManager;
use super::apic::ApicManager;
use super::irq::IrqManager;
use super::handlers::{get_interrupt_stats, InterruptStats};

/// Gestor principal de interrupciones
pub struct InterruptManager {
    idt_manager: IdtManager,
    pic_manager: PicManager,
    apic_manager: ApicManager,
    irq_manager: IrqManager,
    initialized: AtomicBool,
    use_apic: AtomicBool,
}

impl InterruptManager {
    /// Crear nuevo gestor de interrupciones
    pub fn new() -> Self {
        Self {
            idt_manager: IdtManager::new(),
            pic_manager: PicManager::new(),
            apic_manager: ApicManager::new(),
            irq_manager: IrqManager::new(),
            initialized: AtomicBool::new(false),
            use_apic: AtomicBool::new(false),
        }
    }

    /// Inicializar el sistema de interrupciones
    pub fn initialize(&mut self, kernel_code_selector: u16) -> Result<(), &'static str> {
        if self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }

        // Configurar IDT
        self.idt_manager.setup_userland(kernel_code_selector)?;

        // Verificar si APIC está disponible
        if self.apic_manager.is_available() {
            // Usar APIC si está disponible
            self.apic_manager.initialize()?;
            self.use_apic.store(true, Ordering::Release);
        } else {
            // Usar PIC tradicional
            self.pic_manager.initialize()?;
            self.use_apic.store(false, Ordering::Release);
        }

        // Inicializar gestor de IRQs
        self.irq_manager.initialize()?;

        // Habilitar interrupciones
        self.enable_interrupts()?;

        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    /// Habilitar interrupciones del sistema
    pub fn enable_interrupts(&self) -> Result<(), &'static str> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err("Sistema de interrupciones no inicializado");
        }

        unsafe {
            core::arch::asm!("sti", options(nostack, nomem));
        }

        Ok(())
    }

    /// Deshabilitar interrupciones del sistema
    pub fn disable_interrupts(&self) -> Result<(), &'static str> {
        unsafe {
            core::arch::asm!("cli", options(nostack, nomem));
        }

        Ok(())
    }

    /// Habilitar interrupción específica
    pub fn enable_irq(&self, irq: u8) -> Result<(), &'static str> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err("Sistema de interrupciones no inicializado");
        }

        if self.use_apic.load(Ordering::Acquire) {
            // APIC - habilitar en el gestor de IRQs
            self.irq_manager.enable_irq(irq)?;
        } else {
            // PIC - habilitar en el PIC
            self.pic_manager.enable_irq(irq)?;
        }

        Ok(())
    }

    /// Deshabilitar interrupción específica
    pub fn disable_irq(&self, irq: u8) -> Result<(), &'static str> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err("Sistema de interrupciones no inicializado");
        }

        if self.use_apic.load(Ordering::Acquire) {
            // APIC - deshabilitar en el gestor de IRQs
            self.irq_manager.disable_irq(irq)?;
        } else {
            // PIC - deshabilitar en el PIC
            self.pic_manager.disable_irq(irq)?;
        }

        Ok(())
    }

    /// Manejar interrupción IRQ
    pub fn handle_irq(&self, irq: u8) -> Result<(), &'static str> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err("Sistema de interrupciones no inicializado");
        }

        // Manejar en el gestor de IRQs
        self.irq_manager.handle_irq(irq)?;

        // Enviar EOI
        if self.use_apic.load(Ordering::Acquire) {
            self.apic_manager.send_eoi();
        } else {
            self.pic_manager.send_eoi(irq);
        }

        Ok(())
    }

    /// Registrar handler de IRQ
    pub fn register_irq_handler(&self, irq: u8, handler: fn(u8) -> Result<(), &'static str>, name: &'static str) -> Result<(), &'static str> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err("Sistema de interrupciones no inicializado");
        }

        self.irq_manager.register_handler(irq, handler, name)
    }

    /// Desregistrar handler de IRQ
    pub fn unregister_irq_handler(&self, irq: u8) -> Result<(), &'static str> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err("Sistema de interrupciones no inicializado");
        }

        self.irq_manager.unregister_handler(irq)
    }

    /// Configurar timer del APIC
    pub fn setup_apic_timer(&self, vector: u8, initial_count: u32, divide_config: u8) -> Result<(), &'static str> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err("Sistema de interrupciones no inicializado");
        }

        if !self.use_apic.load(Ordering::Acquire) {
            return Err("APIC no está en uso");
        }

        self.apic_manager.setup_timer(vector, initial_count, divide_config)
    }

    /// Enviar IPI (Inter-Processor Interrupt)
    pub fn send_ipi(&self, destination: u8, vector: u8, delivery_mode: u32) -> Result<(), &'static str> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err("Sistema de interrupciones no inicializado");
        }

        if !self.use_apic.load(Ordering::Acquire) {
            return Err("APIC no está en uso");
        }

        self.apic_manager.send_ipi(destination, vector, delivery_mode)
    }

    /// Obtener estadísticas de interrupciones
    pub fn get_stats(&self) -> InterruptStats {
        get_interrupt_stats()
    }

    /// Verificar si el sistema está inicializado
    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// Verificar si se está usando APIC
    pub fn is_using_apic(&self) -> bool {
        self.use_apic.load(Ordering::Acquire)
    }

    /// Obtener información del sistema de interrupciones
    pub fn get_system_info(&self) -> InterruptSystemInfo {
        InterruptSystemInfo {
            initialized: self.is_initialized(),
            using_apic: self.is_using_apic(),
            apic_available: self.apic_manager.is_available(),
            apic_id: if self.is_using_apic() { Some(self.apic_manager.get_apic_id()) } else { None },
            registered_irqs: if self.is_initialized() { self.irq_manager.list_registered_irqs() } else { Vec::new() },
        }
    }

    /// Cargar IDT en el procesador actual (usado por APs)
    pub fn load_idt(&self) {
        self.idt_manager.get_idt().load();
    }
}

impl Default for InterruptManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Información del sistema de interrupciones
#[derive(Debug, Clone)]
pub struct InterruptSystemInfo {
    pub initialized: bool,
    pub using_apic: bool,
    pub apic_available: bool,
    pub apic_id: Option<u32>,
    pub registered_irqs: Vec<u8>,
}

/// Instancia global del gestor de interrupciones
static mut INTERRUPT_MANAGER: Option<InterruptManager> = None;

/// Inicializar el sistema de interrupciones global
pub fn initialize_interrupt_system(kernel_code_selector: u16) -> Result<(), &'static str> {
    unsafe {
        if INTERRUPT_MANAGER.is_some() {
            return Err("Sistema de interrupciones ya inicializado");
        }

        let mut manager = InterruptManager::new();
        manager.initialize(kernel_code_selector)?;
        INTERRUPT_MANAGER = Some(manager);
    }

    Ok(())
}

/// Obtener el gestor de interrupciones global
pub fn get_interrupt_manager() -> Option<&'static InterruptManager> {
    unsafe {
        INTERRUPT_MANAGER.as_ref()
    }
}

/// Obtener el gestor de interrupciones global mutable
pub fn get_interrupt_manager_mut() -> Option<&'static mut InterruptManager> {
    unsafe {
        INTERRUPT_MANAGER.as_mut()
    }
}

/// Función de utilidad para habilitar interrupciones
pub fn enable_interrupts() -> Result<(), &'static str> {
    if let Some(manager) = get_interrupt_manager() {
        manager.enable_interrupts()
    } else {
        Err("Sistema de interrupciones no inicializado")
    }
}

/// Función de utilidad para deshabilitar interrupciones
pub fn disable_interrupts() -> Result<(), &'static str> {
    if let Some(manager) = get_interrupt_manager() {
        manager.disable_interrupts()
    } else {
        Err("Sistema de interrupciones no inicializado")
    }
}

/// Función de utilidad para manejar IRQ
pub fn handle_irq(irq: u8) -> Result<(), &'static str> {
    if let Some(manager) = get_interrupt_manager() {
        manager.handle_irq(irq)
    } else {
        Err("Sistema de interrupciones no inicializado")
    }
}
