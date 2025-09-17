//! Sistema de interrupciones para Eclipse OS
//! 
//! Implementa manejo de interrupciones, excepciones y timers

use alloc::vec::Vec;

/// Número de interrupción
pub type InterruptNumber = u8;

/// Tipo de interrupción
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InterruptType {
    Timer,
    Keyboard,
    Mouse,
    Network,
    Storage,
    Audio,
    Graphics,
    System,
    Error,
}

/// Prioridad de interrupción
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum InterruptPriority {
    Low = 0,
    Normal = 1,
    High = 2,
    Critical = 3,
}

/// Handler de interrupción
pub type InterruptHandler = fn(InterruptNumber) -> ();

/// Información de interrupción
#[derive(Debug, Clone)]
pub struct InterruptInfo {
    pub number: InterruptNumber,
    pub interrupt_type: InterruptType,
    pub priority: InterruptPriority,
    pub handler: Option<InterruptHandler>,
    pub is_enabled: bool,
    pub is_pending: bool,
}

/// Gestor de interrupciones
pub struct InterruptManager {
    interrupts: Vec<InterruptInfo>,
    initialized: bool,
}

impl InterruptManager {
    pub fn new() -> Self {
        Self {
            interrupts: Vec::new(),
            initialized: false,
        }
    }

    pub fn initialize(&mut self) -> Result<(), &'static str> {
        if self.initialized {
            return Err("Interrupt manager already initialized");
        }

        // Inicializar tabla de interrupciones
        for i in 0..256 {
            let interrupt_type = match i {
                0..=31 => InterruptType::System,
                32..=47 => InterruptType::Timer,
                48..=63 => InterruptType::Keyboard,
                64..=79 => InterruptType::Mouse,
                80..=95 => InterruptType::Network,
                96..=111 => InterruptType::Storage,
                112..=127 => InterruptType::Audio,
                128..=143 => InterruptType::Graphics,
                _ => InterruptType::Error,
            };

            let priority = match interrupt_type {
                InterruptType::System => InterruptPriority::Critical,
                InterruptType::Timer => InterruptPriority::High,
                InterruptType::Keyboard => InterruptPriority::High,
                InterruptType::Mouse => InterruptPriority::Normal,
                InterruptType::Network => InterruptPriority::Normal,
                InterruptType::Storage => InterruptPriority::Normal,
                InterruptType::Audio => InterruptPriority::Low,
                InterruptType::Graphics => InterruptPriority::Low,
                InterruptType::Error => InterruptPriority::Critical,
            };

            let interrupt_info = InterruptInfo {
                number: i as InterruptNumber,
                interrupt_type,
                priority,
                handler: None,
                is_enabled: false,
                is_pending: false,
            };

            self.interrupts.push(interrupt_info);
        }

        self.initialized = true;
        Ok(())
    }

    pub fn register_handler(&mut self, interrupt_number: InterruptNumber, handler: InterruptHandler) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("Interrupt manager not initialized");
        }

        if interrupt_number as usize >= self.interrupts.len() {
            return Err("Invalid interrupt number");
        }

        self.interrupts[interrupt_number as usize].handler = Some(handler);
        Ok(())
    }

    pub fn enable_interrupt(&mut self, interrupt_number: InterruptNumber) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("Interrupt manager not initialized");
        }

        if interrupt_number as usize >= self.interrupts.len() {
            return Err("Invalid interrupt number");
        }

        self.interrupts[interrupt_number as usize].is_enabled = true;
        Ok(())
    }

    pub fn disable_interrupt(&mut self, interrupt_number: InterruptNumber) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("Interrupt manager not initialized");
        }

        if interrupt_number as usize >= self.interrupts.len() {
            return Err("Invalid interrupt number");
        }

        self.interrupts[interrupt_number as usize].is_enabled = false;
        Ok(())
    }

    pub fn handle_interrupt(&mut self, interrupt_number: InterruptNumber) -> Result<(), &'static str> {
        if !self.initialized {
            return Err("Interrupt manager not initialized");
        }

        if interrupt_number as usize >= self.interrupts.len() {
            return Err("Invalid interrupt number");
        }

        let interrupt_info = &self.interrupts[interrupt_number as usize];
        
        if !interrupt_info.is_enabled {
            return Err("Interrupt not enabled");
        }

        if let Some(handler) = interrupt_info.handler {
            handler(interrupt_number);
            Ok(())
        } else {
            Err("No handler registered for interrupt")
        }
    }

    pub fn get_interrupt_info(&self, interrupt_number: InterruptNumber) -> Option<&InterruptInfo> {
        if (interrupt_number as usize) < self.interrupts.len() {
            Some(&self.interrupts[interrupt_number as usize])
        } else {
            None
        }
    }

    pub fn get_pending_interrupts(&self) -> Vec<InterruptNumber> {
        self.interrupts.iter()
            .filter(|info| info.is_pending && info.is_enabled)
            .map(|info| info.number)
            .collect()
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
}
