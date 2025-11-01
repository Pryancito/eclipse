//! Timer del Sistema para Eclipse OS
//!
//! Este módulo configura y maneja el PIT (Programmable Interval Timer)
//! para generar interrupciones periódicas usadas en scheduling.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use crate::debug::serial_write_str;

/// Frecuencia del PIT (1.193182 MHz)
const PIT_FREQUENCY: u32 = 1193182;

/// Puertos del PIT
const PIT_CHANNEL_0: u16 = 0x40;
const PIT_CHANNEL_1: u16 = 0x41;
const PIT_CHANNEL_2: u16 = 0x42;
const PIT_COMMAND: u16 = 0x43;

/// Comandos del PIT
const PIT_CMD_BINARY: u8 = 0x00;
const PIT_CMD_BCD: u8 = 0x01;
const PIT_CMD_MODE_0: u8 = 0x00;
const PIT_CMD_MODE_2: u8 = 0x04; // Rate generator
const PIT_CMD_MODE_3: u8 = 0x06; // Square wave
const PIT_CMD_RW_LSB: u8 = 0x10;
const PIT_CMD_RW_MSB: u8 = 0x20;
const PIT_CMD_RW_BOTH: u8 = 0x30;
const PIT_CMD_CHANNEL_0: u8 = 0x00;

/// Configuración del timer
pub struct TimerConfig {
    /// Frecuencia deseada en Hz
    pub frequency_hz: u32,
    /// Quantum de tiempo en ms (para scheduling)
    pub quantum_ms: u64,
    /// Habilitar preemption automática
    pub enable_preemption: bool,
}

impl Default for TimerConfig {
    fn default() -> Self {
        Self {
            frequency_hz: 100,      // 100 Hz = 10ms por tick
            quantum_ms: 10,         // 10ms quantum
            enable_preemption: true,
        }
    }
}

/// Gestor del timer del sistema
pub struct SystemTimer {
    /// Configuración
    config: TimerConfig,
    /// Contador de ticks
    ticks: AtomicU64,
    /// Tiempo del sistema en ms
    system_time_ms: AtomicU64,
    /// Timer inicializado
    initialized: AtomicBool,
    /// Frecuencia real configurada
    actual_frequency: u32,
}

impl SystemTimer {
    /// Crear nuevo gestor de timer
    pub const fn new() -> Self {
        Self {
            config: TimerConfig {
                frequency_hz: 100,
                quantum_ms: 10,
                enable_preemption: true,
            },
            ticks: AtomicU64::new(0),
            system_time_ms: AtomicU64::new(0),
            initialized: AtomicBool::new(false),
            actual_frequency: 100,
        }
    }

    /// Inicializar el timer con configuración
    pub fn init(&mut self, config: TimerConfig) -> Result<(), &'static str> {
        if self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }

        serial_write_str(&alloc::format!(
            "TIMER: Inicializando con frecuencia {}Hz ({}ms por tick)\n",
            config.frequency_hz,
            1000 / config.frequency_hz
        ));

        self.config = config;

        // Configurar el PIT
        self.configure_pit()?;

        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    /// Configurar el PIT (Programmable Interval Timer)
    fn configure_pit(&mut self) -> Result<(), &'static str> {
        // Calcular el divisor para la frecuencia deseada
        let divisor = (PIT_FREQUENCY / self.config.frequency_hz) as u16;
        
        serial_write_str(&alloc::format!(
            "TIMER: Configurando PIT con divisor {} para {}Hz\n",
            divisor, self.config.frequency_hz
        ));

        self.actual_frequency = PIT_FREQUENCY / divisor as u32;

        unsafe {
            // Enviar comando al PIT
            // Channel 0, access mode: lobyte/hibyte, mode 2 (rate generator), binary
            let command = PIT_CMD_CHANNEL_0 | PIT_CMD_RW_BOTH | PIT_CMD_MODE_2 | PIT_CMD_BINARY;
            
            asm!(
                "out dx, al",
                in("dx") PIT_COMMAND,
                in("al") command,
                options(nostack, nomem)
            );

            // Enviar byte bajo del divisor
            asm!(
                "out dx, al",
                in("dx") PIT_CHANNEL_0,
                in("al") (divisor & 0xFF) as u8,
                options(nostack, nomem)
            );

            // Enviar byte alto del divisor
            asm!(
                "out dx, al",
                in("dx") PIT_CHANNEL_0,
                in("al") ((divisor >> 8) & 0xFF) as u8,
                options(nostack, nomem)
            );
        }

        serial_write_str("TIMER: PIT configurado exitosamente\n");
        Ok(())
    }

    /// Incrementar contador de ticks (llamado desde el interrupt handler)
    pub fn tick(&self) {
        let ticks = self.ticks.fetch_add(1, Ordering::Relaxed);
        
        // Actualizar tiempo del sistema
        // Si frecuencia es 100Hz, cada tick = 10ms
        let ms_per_tick = 1000 / self.actual_frequency as u64;
        self.system_time_ms.fetch_add(ms_per_tick, Ordering::Relaxed);

        // Verificar si es momento de hacer context switch
        if self.config.enable_preemption {
            let quantum_ticks = (self.config.quantum_ms * self.actual_frequency as u64) / 1000;
            
            if ticks % quantum_ticks == 0 {
                // Momento de hacer context switch
                self.do_context_switch();
            }
        }
    }

    /// Realizar context switch
    fn do_context_switch(&self) {
        use crate::process::context_switch::switch_to_next_process;
        
        // Cambiar al siguiente proceso
        // NOTA: Esto puede no retornar inmediatamente si cambia a otro proceso
        switch_to_next_process();
    }

    /// Obtener número de ticks
    pub fn get_ticks(&self) -> u64 {
        self.ticks.load(Ordering::Relaxed)
    }

    /// Obtener tiempo del sistema en ms
    pub fn get_system_time_ms(&self) -> u64 {
        self.system_time_ms.load(Ordering::Relaxed)
    }

    /// Verificar si está inicializado
    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// Obtener frecuencia actual
    pub fn get_frequency(&self) -> u32 {
        self.actual_frequency
    }
}

/// Timer global del sistema
static SYSTEM_TIMER: spin::Mutex<SystemTimer> = spin::Mutex::new(SystemTimer::new());

/// Inicializar el timer del sistema
pub fn init_system_timer(config: TimerConfig) -> Result<(), &'static str> {
    let mut timer = SYSTEM_TIMER.lock();
    timer.init(config)
}

/// Obtener el timer del sistema
pub fn get_system_timer() -> &'static spin::Mutex<SystemTimer> {
    &SYSTEM_TIMER
}

/// Handler de interrupción de timer (llamado desde interrupts/handlers.rs)
pub fn on_timer_interrupt() {
    let timer = SYSTEM_TIMER.lock();
    timer.tick();
}

/// Obtener tiempo del sistema
pub fn get_uptime_ms() -> u64 {
    let timer = SYSTEM_TIMER.lock();
    timer.get_system_time_ms()
}

/// Obtener ticks del sistema
pub fn get_ticks() -> u64 {
    let timer = SYSTEM_TIMER.lock();
    timer.get_ticks()
}

