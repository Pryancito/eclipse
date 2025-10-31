//! Programmable Interrupt Controller (PIC) para Eclipse OS
//!
//! Este módulo maneja la configuración y el control del PIC 8259A
//! para interrupciones de hardware tradicionales.

use core::arch::asm;
use core::sync::atomic::{AtomicU8, AtomicBool, Ordering};

/// Puertos del PIC Master
const PIC_MASTER_COMMAND: u16 = 0x20;
const PIC_MASTER_DATA: u16 = 0x21;

/// Puertos del PIC Slave
const PIC_SLAVE_COMMAND: u16 = 0xA0;
const PIC_SLAVE_DATA: u16 = 0xA1;

/// Comandos del PIC
const PIC_ICW1_ICW4: u8 = 0x01;  // ICW4 needed
const PIC_ICW1_SINGLE: u8 = 0x02; // Single (cascade) mode
const PIC_ICW1_INTERVAL4: u8 = 0x04; // Call address interval 4 (8)
const PIC_ICW1_LEVEL: u8 = 0x08;  // Level triggered (edge) mode
const PIC_ICW1_INIT: u8 = 0x10;   // Initialization

const PIC_ICW4_8086: u8 = 0x01;   // 8086/88 (MCS-80/85) mode
const PIC_ICW4_AUTO: u8 = 0x02;   // Auto (normal) EOI
const PIC_ICW4_BUF_SLAVE: u8 = 0x08; // Buffered mode/slave
const PIC_ICW4_BUF_MASTER: u8 = 0x0C; // Buffered mode/master
const PIC_ICW4_SFNM: u8 = 0x10;   // Special fully nested (not)

/// Comando EOI (End of Interrupt)
const PIC_EOI: u8 = 0x20;

/// IRQ base para PIC Master
const PIC_MASTER_IRQ_BASE: u8 = 0x20;

/// IRQ base para PIC Slave
const PIC_SLAVE_IRQ_BASE: u8 = 0x28;

/// Gestor del PIC
pub struct PicManager {
    master_mask: AtomicU8,
    slave_mask: AtomicU8,
    initialized: AtomicBool,
}

impl PicManager {
    /// Crear nuevo gestor de PIC
    pub fn new() -> Self {
        Self {
            master_mask: AtomicU8::new(0xFF), // Todas las interrupciones deshabilitadas
            slave_mask: AtomicU8::new(0xFF),
            initialized: AtomicBool::new(false),
        }
    }

    /// Inicializar el PIC
    pub fn initialize(&self) -> Result<(), &'static str> {
        if self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }

        unsafe {
            // Guardar máscaras actuales
            let master_mask = self.read_mask(PIC_MASTER_DATA);
            let slave_mask = self.read_mask(PIC_SLAVE_DATA);

            // Inicializar PIC Master
            self.write_command(PIC_MASTER_COMMAND, PIC_ICW1_INIT | PIC_ICW1_ICW4);
            self.write_data(PIC_MASTER_DATA, PIC_MASTER_IRQ_BASE);
            self.write_data(PIC_MASTER_DATA, 0x04); // Slave en IRQ2
            self.write_data(PIC_MASTER_DATA, PIC_ICW4_8086);

            // Inicializar PIC Slave
            self.write_command(PIC_SLAVE_COMMAND, PIC_ICW1_INIT | PIC_ICW1_ICW4);
            self.write_data(PIC_SLAVE_DATA, PIC_SLAVE_IRQ_BASE);
            self.write_data(PIC_SLAVE_DATA, 0x02); // ID del slave
            self.write_data(PIC_SLAVE_DATA, PIC_ICW4_8086);

            // Restaurar máscaras
            self.write_mask(PIC_MASTER_DATA, master_mask);
            self.write_mask(PIC_SLAVE_DATA, slave_mask);
        }

        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    /// Habilitar interrupción específica
    pub fn enable_irq(&self, irq: u8) -> Result<(), &'static str> {
        if irq >= 16 {
            return Err("IRQ inválido");
        }

        if irq < 8 {
            // IRQ en PIC Master
            let current_mask = self.master_mask.load(Ordering::Acquire);
            let new_mask = current_mask & !(1 << irq);
            self.write_mask(PIC_MASTER_DATA, new_mask);
            self.master_mask.store(new_mask, Ordering::Release);
        } else {
            // IRQ en PIC Slave
            let slave_irq = irq - 8;
            let current_mask = self.slave_mask.load(Ordering::Acquire);
            let new_mask = current_mask & !(1 << slave_irq);
            self.write_mask(PIC_SLAVE_DATA, new_mask);
            self.slave_mask.store(new_mask, Ordering::Release);

            // Habilitar IRQ2 en Master para que lleguen interrupciones del Slave
            let master_mask = self.master_mask.load(Ordering::Acquire);
            let new_master_mask = master_mask & !(1 << 2);
            self.write_mask(PIC_MASTER_DATA, new_master_mask);
            self.master_mask.store(new_master_mask, Ordering::Release);
        }

        Ok(())
    }

    /// Deshabilitar interrupción específica
    pub fn disable_irq(&self, irq: u8) -> Result<(), &'static str> {
        if irq >= 16 {
            return Err("IRQ inválido");
        }

        if irq < 8 {
            // IRQ en PIC Master
            let current_mask = self.master_mask.load(Ordering::Acquire);
            let new_mask = current_mask | (1 << irq);
            self.write_mask(PIC_MASTER_DATA, new_mask);
            self.master_mask.store(new_mask, Ordering::Release);
        } else {
            // IRQ en PIC Slave
            let slave_irq = irq - 8;
            let current_mask = self.slave_mask.load(Ordering::Acquire);
            let new_mask = current_mask | (1 << slave_irq);
            self.write_mask(PIC_SLAVE_DATA, new_mask);
            self.slave_mask.store(new_mask, Ordering::Release);
        }

        Ok(())
    }

    /// Enviar EOI (End of Interrupt)
    pub fn send_eoi(&self, irq: u8) {
        if irq >= 8 {
            // Interrupción del Slave - enviar EOI a ambos
            unsafe {
                asm!("mov al, 0x20; out 0xA0, al", options(nostack, nomem));
                asm!("mov al, 0x20; out 0x20, al", options(nostack, nomem));
            }
        } else {
            // Interrupción del Master
            unsafe {
                asm!("mov al, 0x20; out 0x20, al", options(nostack, nomem));
            }
        }
    }

    /// Leer máscara del PIC
    fn read_mask(&self, port: u16) -> u8 {
        unsafe {
            let mut value: u8;
            asm!("in al, dx", out("al") value, in("dx") port, options(nostack, nomem));
            value
        }
    }

    /// Escribir máscara del PIC
    fn write_mask(&self, port: u16, mask: u8) {
        unsafe {
            asm!("out dx, al", in("dx") port, in("al") mask, options(nostack, nomem));
        }
    }

    /// Escribir comando al PIC
    fn write_command(&self, port: u16, command: u8) {
        unsafe {
            asm!("out dx, al", in("dx") port, in("al") command, options(nostack, nomem));
        }
    }

    /// Escribir dato al PIC
    fn write_data(&self, port: u16, data: u8) {
        unsafe {
            asm!("out dx, al", in("dx") port, in("al") data, options(nostack, nomem));
        }
    }

    /// Verificar si el PIC está inicializado
    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// Obtener máscara del Master
    pub fn get_master_mask(&self) -> u8 {
        self.master_mask.load(Ordering::Acquire)
    }

    /// Obtener máscara del Slave
    pub fn get_slave_mask(&self) -> u8 {
        self.slave_mask.load(Ordering::Acquire)
    }
}

impl Default for PicManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Función de utilidad para inicializar el PIC
pub fn initialize_pic() -> Result<(), &'static str> {
    let pic_manager = PicManager::new();
    pic_manager.initialize()
}

/// Función de utilidad para enviar EOI
pub fn send_eoi(irq: u8) {
    let pic_manager = PicManager::new();
    pic_manager.send_eoi(irq);
}
