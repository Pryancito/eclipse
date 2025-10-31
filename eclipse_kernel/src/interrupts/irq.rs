//! Gestión de IRQs (Interrupt Request Lines) para Eclipse OS
//!
//! Este módulo proporciona un sistema unificado para manejar IRQs
//! tanto del PIC tradicional como del APIC moderno.

use core::sync::atomic::{AtomicU32, AtomicBool, Ordering};
use alloc::vec::Vec;
use alloc::boxed::Box;

/// Tipo de función handler de IRQ
pub type IrqHandler = fn(irq: u8) -> Result<(), &'static str>;

/// Información de un handler de IRQ
#[derive(Debug, Clone)]
pub struct IrqHandlerInfo {
    pub handler: IrqHandler,
    pub name: &'static str,
    pub enabled: bool,
}

/// Gestor de IRQs
pub struct IrqManager {
    handlers: [Option<IrqHandlerInfo>; 256],
    handler_counts: [AtomicU32; 256],
    initialized: AtomicBool,
}

impl IrqManager {
    /// Crear nuevo gestor de IRQs
    pub fn new() -> Self {
        Self {
            handlers: [const { None }; 256],
            handler_counts: [const { AtomicU32::new(0) }; 256],
            initialized: AtomicBool::new(false),
        }
    }

    /// Inicializar el gestor de IRQs
    pub fn initialize(&self) -> Result<(), &'static str> {
        if self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }

        // Registrar handlers por defecto para IRQs comunes
        self.register_default_handlers()?;

        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    /// Registrar handler por defecto
    fn register_default_handlers(&self) -> Result<(), &'static str> {
        // Timer (IRQ 0)
        self.register_handler(0, timer_irq_handler, "Timer")?;

        // Keyboard (IRQ 1)
        self.register_handler(1, keyboard_irq_handler, "Keyboard")?;

        // Serial (IRQ 4)
        self.register_handler(4, serial_irq_handler, "Serial")?;

        // Floppy (IRQ 6)
        self.register_handler(6, floppy_irq_handler, "Floppy")?;

        // Parallel (IRQ 7)
        self.register_handler(7, parallel_irq_handler, "Parallel")?;

        // RTC (IRQ 8)
        self.register_handler(8, rtc_irq_handler, "RTC")?;

        // Mouse (IRQ 12)
        self.register_handler(12, mouse_irq_handler, "Mouse")?;

        // IDE Primary (IRQ 14)
        self.register_handler(14, ide_primary_irq_handler, "IDE Primary")?;

        // IDE Secondary (IRQ 15)
        self.register_handler(15, ide_secondary_irq_handler, "IDE Secondary")?;

        Ok(())
    }

    /// Registrar un handler de IRQ
    pub fn register_handler(&self, irq: u8, handler: IrqHandler, name: &'static str) -> Result<(), &'static str> {
        // u8 no puede ser >= 256, así que no necesitamos verificar

        let handler_info = IrqHandlerInfo {
            handler,
            name,
            enabled: true,
        };

        // En una implementación real, esto requeriría sincronización
        // Por ahora, asumimos que solo se llama durante la inicialización
        unsafe {
            let handlers_ptr = &self.handlers as *const _ as *mut [Option<IrqHandlerInfo>; 256];
            (*handlers_ptr)[irq as usize] = Some(handler_info);
        }

        Ok(())
    }

    /// Desregistrar un handler de IRQ
    pub fn unregister_handler(&self, irq: u8) -> Result<(), &'static str> {
        // u8 no puede ser >= 256, así que no necesitamos verificar

        unsafe {
            let handlers_ptr = &self.handlers as *const _ as *mut [Option<IrqHandlerInfo>; 256];
            (*handlers_ptr)[irq as usize] = None;
        }

        Ok(())
    }

    /// Manejar una interrupción IRQ
    pub fn handle_irq(&self, irq: u8) -> Result<(), &'static str> {
        // u8 no puede ser >= 256, así que no necesitamos verificar

        // Incrementar contador
        self.handler_counts[irq as usize].fetch_add(1, Ordering::Relaxed);

        // Obtener handler
        let handler_info = unsafe {
            let handlers_ptr = &self.handlers as *const _ as *const [Option<IrqHandlerInfo>; 256];
            (*handlers_ptr)[irq as usize].clone()
        };

        match handler_info {
            Some(info) if info.enabled => {
                // Llamar al handler
                (info.handler)(irq)?;
                Ok(())
            }
            Some(_) => {
                // Handler deshabilitado
                Err("Handler deshabilitado")
            }
            None => {
                // No hay handler registrado
                Err("No hay handler registrado")
            }
        }
    }

    /// Habilitar un IRQ
    pub fn enable_irq(&self, irq: u8) -> Result<(), &'static str> {
        // u8 no puede ser >= 256, así que no necesitamos verificar

        unsafe {
            let handlers_ptr = &self.handlers as *const _ as *mut [Option<IrqHandlerInfo>; 256];
            if let Some(ref mut info) = (*handlers_ptr)[irq as usize] {
                info.enabled = true;
            }
        }

        Ok(())
    }

    /// Deshabilitar un IRQ
    pub fn disable_irq(&self, irq: u8) -> Result<(), &'static str> {
        // u8 no puede ser >= 256, así que no necesitamos verificar

        unsafe {
            let handlers_ptr = &self.handlers as *const _ as *mut [Option<IrqHandlerInfo>; 256];
            if let Some(ref mut info) = (*handlers_ptr)[irq as usize] {
                info.enabled = false;
            }
        }

        Ok(())
    }

    /// Obtener información de un IRQ
    pub fn get_irq_info(&self, irq: u8) -> Option<IrqHandlerInfo> {
        if irq == 255 {
            return None;
        }

        unsafe {
            let handlers_ptr = &self.handlers as *const _ as *const [Option<IrqHandlerInfo>; 256];
            (*handlers_ptr)[irq as usize].clone()
        }
    }

    /// Obtener estadísticas de un IRQ
    pub fn get_irq_stats(&self, irq: u8) -> Option<u32> {
        if irq == 255 {
            return None;
        }

        Some(self.handler_counts[irq as usize].load(Ordering::Relaxed))
    }

    /// Listar todos los IRQs registrados
    pub fn list_registered_irqs(&self) -> Vec<u8> {
        let mut irqs = Vec::new();
        
        for i in 0..256 {
            if let Some(_) = self.get_irq_info(i as u8) {
                irqs.push(i as u8);
            }
        }
        
        irqs
    }

    /// Verificar si el gestor está inicializado
    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }
}

impl Default for IrqManager {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// HANDLERS DE IRQ POR DEFECTO
// ============================================================================

/// Handler de IRQ del timer
fn timer_irq_handler(_irq: u8) -> Result<(), &'static str> {
    // Timer interrupt - usado para scheduling
    // En una implementación completa, esto actualizaría el scheduler
    Ok(())
}

/// Handler de IRQ del teclado
fn keyboard_irq_handler(_irq: u8) -> Result<(), &'static str> {
    // Keyboard interrupt - leer scancode
    // En una implementación completa, esto procesaría el scancode
    Ok(())
}

/// Handler de IRQ del puerto serial
fn serial_irq_handler(_irq: u8) -> Result<(), &'static str> {
    // Serial interrupt - leer datos del puerto serial
    // En una implementación completa, esto procesaría datos seriales
    Ok(())
}

/// Handler de IRQ del floppy
fn floppy_irq_handler(_irq: u8) -> Result<(), &'static str> {
    // Floppy interrupt - manejar operaciones de disco
    Ok(())
}

/// Handler de IRQ del puerto paralelo
fn parallel_irq_handler(_irq: u8) -> Result<(), &'static str> {
    // Parallel port interrupt
    Ok(())
}

/// Handler de IRQ del RTC
fn rtc_irq_handler(_irq: u8) -> Result<(), &'static str> {
    // RTC interrupt - actualizar reloj del sistema
    Ok(())
}

/// Handler de IRQ del mouse
fn mouse_irq_handler(_irq: u8) -> Result<(), &'static str> {
    // Mouse interrupt - procesar movimientos del mouse
    Ok(())
}

/// Handler de IRQ del IDE primario
fn ide_primary_irq_handler(_irq: u8) -> Result<(), &'static str> {
    // IDE primary interrupt - manejar operaciones de disco
    Ok(())
}

/// Handler de IRQ del IDE secundario
fn ide_secondary_irq_handler(_irq: u8) -> Result<(), &'static str> {
    // IDE secondary interrupt - manejar operaciones de disco
    Ok(())
}
