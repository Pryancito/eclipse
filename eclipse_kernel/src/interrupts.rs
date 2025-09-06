//! Configuración de interrupciones y timers para Eclipse OS
//! 
//! Este módulo maneja la configuración de interrupciones y timers del sistema

use core::arch::asm;

/// Puerto del PIC Master
const PIC_MASTER_COMMAND: u16 = 0x20;
const PIC_MASTER_DATA: u16 = 0x21;

/// Puerto del PIC Slave
const PIC_SLAVE_COMMAND: u16 = 0xA0;
const PIC_SLAVE_DATA: u16 = 0xA1;

/// Comandos del PIC
const PIC_ICW1_ICW4: u8 = 0x01;
const PIC_ICW1_SINGLE: u8 = 0x02;
const PIC_ICW1_INTERVAL4: u8 = 0x04;
const PIC_ICW1_LEVEL: u8 = 0x08;
const PIC_ICW1_INIT: u8 = 0x10;

const PIC_ICW4_8086: u8 = 0x01;
const PIC_ICW4_AUTO: u8 = 0x02;
const PIC_ICW4_BUF_SLAVE: u8 = 0x08;
const PIC_ICW4_BUF_MASTER: u8 = 0x0C;
const PIC_ICW4_SFNM: u8 = 0x10;

/// Comandos de EOI
const PIC_EOI: u8 = 0x20;

/// Puerto del PIT (Programmable Interval Timer)
const PIT_CHANNEL0: u16 = 0x40;
const PIT_CHANNEL1: u16 = 0x41;
const PIT_CHANNEL2: u16 = 0x42;
const PIT_COMMAND: u16 = 0x43;

/// Comandos del PIT
const PIT_BCD: u8 = 0x01;
const PIT_MODE0: u8 = 0x00;
const PIT_MODE1: u8 = 0x02;
const PIT_MODE2: u8 = 0x04;
const PIT_MODE3: u8 = 0x06;
const PIT_MODE4: u8 = 0x08;
const PIT_MODE5: u8 = 0x0A;
const PIT_LATCH: u8 = 0x00;
const PIT_LOBYTE: u8 = 0x10;
const PIT_HIBYTE: u8 = 0x20;
const PIT_BOTH: u8 = 0x30;

/// Frecuencia base del PIT (1193182 Hz)
const PIT_FREQUENCY: u32 = 1193182;

/// Gestor de interrupciones
pub struct InterruptManager {
    timer_frequency: u32,
    timer_ticks: u64,
}

impl InterruptManager {
    /// Crear nuevo gestor de interrupciones
    pub fn new() -> Self {
        Self {
            timer_frequency: 100,  // 100 Hz por defecto
            timer_ticks: 0,
        }
    }

    /// Configurar interrupciones para userland
    pub fn setup_userland(&mut self) -> Result<(), &'static str> {
        // Configurar PIC
        self.setup_pic()?;
        
        // Configurar PIT
        self.setup_pit()?;
        
        // Habilitar interrupciones
        self.enable_interrupts();
        
        Ok(())
    }

    /// Configurar PIC (Programmable Interrupt Controller)
    fn setup_pic(&self) -> Result<(), &'static str> {
        unsafe {
            // Inicializar PIC Master
            asm!("out dx, al", in("dx") PIC_MASTER_COMMAND, in("al") PIC_ICW1_INIT | PIC_ICW1_ICW4);
            asm!("out dx, al", in("dx") PIC_MASTER_DATA, in("al") 0x20u8);  // Vector base 0x20
            asm!("out dx, al", in("dx") PIC_MASTER_DATA, in("al") 0x04u8);  // Slave en IRQ2
            asm!("out dx, al", in("dx") PIC_MASTER_DATA, in("al") PIC_ICW4_8086);

            // Inicializar PIC Slave
            asm!("out dx, al", in("dx") PIC_SLAVE_COMMAND, in("al") PIC_ICW1_INIT | PIC_ICW1_ICW4);
            asm!("out dx, al", in("dx") PIC_SLAVE_DATA, in("al") 0x28u8);  // Vector base 0x28
            asm!("out dx, al", in("dx") PIC_SLAVE_DATA, in("al") 0x02u8);  // Slave ID
            asm!("out dx, al", in("dx") PIC_SLAVE_DATA, in("al") PIC_ICW4_8086);

            // Configurar máscaras de interrupciones
            asm!("out dx, al", in("dx") PIC_MASTER_DATA, in("al") 0xFCu8);  // Habilitar timer y keyboard
            asm!("out dx, al", in("dx") PIC_SLAVE_DATA, in("al") 0xFFu8);   // Deshabilitar todas las interrupciones del slave
        }
        
        Ok(())
    }

    /// Configurar PIT (Programmable Interval Timer)
    fn setup_pit(&self) -> Result<(), &'static str> {
        let divisor = PIT_FREQUENCY / self.timer_frequency;
        
        if divisor > 65535 {
            return Err("Frecuencia de timer demasiado baja");
        }

        unsafe {
            // Configurar canal 0 del PIT
            asm!("out dx, al", in("dx") PIT_COMMAND, in("al") PIT_BOTH | PIT_MODE3 | PIT_BCD);
            
            // Establecer divisor
            asm!("out dx, al", in("dx") PIT_CHANNEL0, in("al") (divisor & 0xFF) as u8);
            asm!("out dx, al", in("dx") PIT_CHANNEL0, in("al") ((divisor >> 8) & 0xFF) as u8);
        }
        
        Ok(())
    }

    /// Habilitar interrupciones
    fn enable_interrupts(&self) {
        unsafe {
            asm!("sti", options(nomem, nostack));
        }
    }

    /// Deshabilitar interrupciones
    pub fn disable_interrupts(&self) {
        unsafe {
            asm!("cli", options(nomem, nostack));
        }
    }

    /// Enviar EOI (End of Interrupt)
    pub fn send_eoi(&self, irq: u8) {
        unsafe {
            if irq >= 8 {
                // EOI para slave
                asm!("out dx, al", in("dx") PIC_SLAVE_COMMAND, in("al") PIC_EOI);
            }
            // EOI para master
            asm!("out dx, al", in("dx") PIC_MASTER_COMMAND, in("al") PIC_EOI);
        }
    }

    /// Obtener frecuencia del timer
    pub fn get_timer_frequency(&self) -> u32 {
        self.timer_frequency
    }

    /// Establecer frecuencia del timer
    pub fn set_timer_frequency(&mut self, frequency: u32) -> Result<(), &'static str> {
        if frequency == 0 || frequency > PIT_FREQUENCY {
            return Err("Frecuencia de timer inválida");
        }

        self.timer_frequency = frequency;
        self.setup_pit()?;
        
        Ok(())
    }

    /// Obtener número de ticks del timer
    pub fn get_timer_ticks(&self) -> u64 {
        self.timer_ticks
    }

    /// Incrementar ticks del timer
    pub fn increment_timer_ticks(&mut self) {
        self.timer_ticks += 1;
    }

    /// Obtener tiempo en milisegundos
    pub fn get_time_ms(&self) -> u64 {
        (self.timer_ticks * 1000) / (self.timer_frequency as u64)
    }

    /// Obtener tiempo en segundos
    pub fn get_time_s(&self) -> u64 {
        self.timer_ticks / (self.timer_frequency as u64)
    }
}

impl Default for InterruptManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Gestor de timers
pub struct TimerManager {
    interrupt_manager: InterruptManager,
    start_time: u64,
}

impl TimerManager {
    /// Crear nuevo gestor de timers
    pub fn new() -> Self {
        Self {
            interrupt_manager: InterruptManager::new(),
            start_time: 0,
        }
    }

    /// Configurar timers para userland
    pub fn setup_userland(&mut self) -> Result<(), &'static str> {
        self.interrupt_manager.setup_userland()?;
        self.start_time = self.interrupt_manager.get_timer_ticks();
        Ok(())
    }

    /// Obtener tiempo transcurrido en milisegundos
    pub fn get_elapsed_ms(&self) -> u64 {
        let current_time = self.interrupt_manager.get_timer_ticks();
        if current_time >= self.start_time {
            (current_time - self.start_time) * 1000 / (self.interrupt_manager.get_timer_frequency() as u64)
        } else {
            0
        }
    }

    /// Obtener tiempo transcurrido en segundos
    pub fn get_elapsed_s(&self) -> u64 {
        self.get_elapsed_ms() / 1000
    }

    /// Obtener gestor de interrupciones
    pub fn get_interrupt_manager(&self) -> &InterruptManager {
        &self.interrupt_manager
    }

    /// Obtener gestor de interrupciones mutable
    pub fn get_interrupt_manager_mut(&mut self) -> &mut InterruptManager {
        &mut self.interrupt_manager
    }
}

impl Default for TimerManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Función de utilidad para configurar interrupciones
pub fn setup_userland_interrupts() -> Result<(), &'static str> {
    let mut interrupt_manager = InterruptManager::new();
    interrupt_manager.setup_userland()
}

/// Función de utilidad para configurar timers
pub fn setup_userland_timers() -> Result<(), &'static str> {
    let mut timer_manager = TimerManager::new();
    timer_manager.setup_userland()
}
