//! Controlador PIC (Programmable Interrupt Controller) para Eclipse OS
//! 
//! Maneja las interrupciones hardware básicas usando el PIC 8259

/// Puertos de control del PIC
const PIC1_COMMAND: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_COMMAND: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

/// Comandos del PIC
const PIC_EOI: u8 = 0x20;  // End of Interrupt
const PIC_ICW1: u8 = 0x11; // Initialization Command Word 1
const PIC_ICW4: u8 = 0x01; // Initialization Command Word 4

/// Máscaras de interrupciones
const PIC1_OFFSET: u8 = 32; // IRQ 0-7 mapeados a 32-39
const PIC2_OFFSET: u8 = 40; // IRQ 8-15 mapeados a 40-47

/// Estado del PIC
static mut PIC_INITIALIZED: bool = false;

/// Inicializar el PIC
pub fn init_pic() -> Result<(), &'static str> {
    unsafe {
        // Configurar PIC1
        outb(PIC1_COMMAND, PIC_ICW1);
        outb(PIC1_DATA, PIC1_OFFSET);
        outb(PIC1_DATA, 1 << 2); // PIC1 tiene un slave en IRQ2
        outb(PIC1_DATA, PIC_ICW4);
        
        // Configurar PIC2
        outb(PIC2_COMMAND, PIC_ICW1);
        outb(PIC2_DATA, PIC2_OFFSET);
        outb(PIC2_DATA, 2); // PIC2 es slave del PIC1
        outb(PIC2_DATA, PIC_ICW4);
        
        // Configurar máscaras de interrupciones
        outb(PIC1_DATA, 0xFF); // Deshabilitar todas las interrupciones temporalmente
        outb(PIC2_DATA, 0xFF);
        
        PIC_INITIALIZED = true;
    }
    
    Ok(())
}

/// Enviar EOI (End of Interrupt) al PIC
pub fn send_eoi(irq: u8) {
    if !is_pic_initialized() {
        return;
    }
    
    unsafe {
        if irq >= 8 {
            outb(PIC2_COMMAND, PIC_EOI);
        }
        outb(PIC1_COMMAND, PIC_EOI);
    }
}

/// Habilitar una interrupción específica
pub fn enable_irq(irq: u8) -> Result<(), &'static str> {
    if !is_pic_initialized() {
        return Err("PIC no inicializado");
    }
    
    if irq > 15 {
        return Err("IRQ inválido");
    }
    
    unsafe {
        let port = if irq < 8 { PIC1_DATA } else { PIC2_DATA };
        let mask = 1 << (irq % 8);
        let current_mask = inb(port);
        outb(port, current_mask & !mask);
    }
    
    Ok(())
}

/// Deshabilitar una interrupción específica
pub fn disable_irq(irq: u8) -> Result<(), &'static str> {
    if !is_pic_initialized() {
        return Err("PIC no inicializado");
    }
    
    if irq > 15 {
        return Err("IRQ inválido");
    }
    
    unsafe {
        let port = if irq < 8 { PIC1_DATA } else { PIC2_DATA };
        let mask = 1 << (irq % 8);
        let current_mask = inb(port);
        outb(port, current_mask | mask);
    }
    
    Ok(())
}

/// Obtener el estado de una interrupción
pub fn is_irq_enabled(irq: u8) -> bool {
    if !is_pic_initialized() || irq > 15 {
        return false;
    }
    
    unsafe {
        let port = if irq < 8 { PIC1_DATA } else { PIC2_DATA };
        let mask = 1 << (irq % 8);
        let current_mask = inb(port);
        (current_mask & mask) == 0
    }
}

/// Obtener el estado de inicialización del PIC
pub fn is_pic_initialized() -> bool {
    unsafe { PIC_INITIALIZED }
}

/// Obtener estadísticas del PIC
pub fn get_pic_stats() -> PicStats {
    PicStats {
        initialized: is_pic_initialized(),
        irq_enabled: [
            is_irq_enabled(0), is_irq_enabled(1), is_irq_enabled(2), is_irq_enabled(3),
            is_irq_enabled(4), is_irq_enabled(5), is_irq_enabled(6), is_irq_enabled(7),
            is_irq_enabled(8), is_irq_enabled(9), is_irq_enabled(10), is_irq_enabled(11),
            is_irq_enabled(12), is_irq_enabled(13), is_irq_enabled(14), is_irq_enabled(15),
        ],
    }
}

/// Estadísticas del PIC
#[derive(Debug, Clone, Copy)]
pub struct PicStats {
    pub initialized: bool,
    pub irq_enabled: [bool; 16],
}

impl Default for PicStats {
    fn default() -> Self {
        Self {
            initialized: false,
            irq_enabled: [false; 16],
        }
    }
}

/// Funciones de E/S de puerto (simuladas para compilación)
unsafe fn outb(port: u16, value: u8) {
    // En un sistema real, esto usaría instrucciones de E/S
    // core::arch::x86_64::_out8(port, value);
    // Por ahora, simulamos la operación
    let _ = port;
    let _ = value;
}

unsafe fn inb(port: u16) -> u8 {
    // En un sistema real, esto usaría instrucciones de E/S
    // core::arch::x86_64::_in8(port)
    // Por ahora, simulamos la operación
    let _ = port;
    0
}

/// Configurar el PIC para modo protegido
pub fn configure_protected_mode() -> Result<(), &'static str> {
    if !is_pic_initialized() {
        return Err("PIC no inicializado");
    }
    
    // En modo protegido, necesitamos configurar el PIC de manera diferente
    // Esto se haría normalmente en la inicialización del kernel
    Ok(())
}

/// Deshabilitar todas las interrupciones del PIC
pub fn disable_all_irqs() {
    if !is_pic_initialized() {
        return;
    }
    
    unsafe {
        outb(PIC1_DATA, 0xFF);
        outb(PIC2_DATA, 0xFF);
    }
}

/// Habilitar todas las interrupciones del PIC
pub fn enable_all_irqs() {
    if !is_pic_initialized() {
        return;
    }
    
    unsafe {
        outb(PIC1_DATA, 0x00);
        outb(PIC2_DATA, 0x00);
    }
}
