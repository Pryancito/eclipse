//! Sistema de Logging para Eclipse OS Kernel
//!
//! Este módulo implementa un logger que usa el puerto serie como backend
//! y proporciona las macros estándar de logging de Rust (info!, warn!, error!, etc.)

use log::{Level, LevelFilter, Log, Metadata, Record};
use spin::Mutex;
use core::fmt::Write;

/// Logger que escribe al puerto serie
pub struct SerialLogger {
    port: Mutex<&'static mut crate::serial::SerialPort>,
}

impl SerialLogger {
    /// Crear un nuevo logger con el puerto serie especificado
    pub fn new(port: &'static mut crate::serial::SerialPort) -> Self {
        Self {
            port: Mutex::new(port),
        }
    }

    /// Escribir un mensaje formateado al puerto serie
    fn write_message(&self, level: Level, target: &str, args: &core::fmt::Arguments) {
        let mut port = self.port.lock();

        // Escribir timestamp simulado (número de segundos desde el arranque)
        port.write_str("[");
        port.write_hex(crate::logger::get_timestamp());
        port.write_str("] ");

        // Escribir nivel de log
        let level_str = match level {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        };
        port.write_str(level_str);
        port.write_str(" ");

        // Escribir target (si no es vacío y no es el default)
        if !target.is_empty() && target != "eclipse_kernel" {
            port.write_str("[");
            port.write_str(target);
            port.write_str("] ");
        }

        // Escribir el mensaje usando write! directamente al puerto serie
        // Esto evita el problema de borrowing con el buffer
        if let Err(_) = write!(&mut *port, "{}", args) {
            port.write_str("FORMATTING ERROR");
        }

        // Nueva línea
        port.write_str("\r\n");
    }
}

impl Log for SerialLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        // Habilitar todos los niveles de log
        // En producción podrías filtrar basado en configuración
        true
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            self.write_message(
                record.level(),
                record.target(),
                record.args(),
            );
        }
    }

    fn flush(&self) {
        // El puerto serie no necesita flush explícito
        // Los datos se envían inmediatamente
    }
}


/// Instancia global del logger
static mut LOGGER: Option<SerialLogger> = None;

/// Obtener timestamp simulado (ticks desde arranque)
/// En un kernel real, esto debería usar el timer del sistema
pub fn get_timestamp() -> u64 {
    // Timestamp simple basado en un contador estático
    // En producción, usar el timer del sistema
    static mut TIMESTAMP: u64 = 0;
    unsafe {
        TIMESTAMP = TIMESTAMP.wrapping_add(1);
        TIMESTAMP
    }
}

/// Inicializar el sistema de logging
/// NOTA: El allocador debe estar inicializado ANTES de llamar esta función
pub fn init() {
    // Inicializar el puerto serie primero
    crate::serial::init();

    // Crear el logger con el puerto serie
    let port = crate::serial::get_serial_port();
    let logger = SerialLogger::new(port);

    // Registrar el logger globalmente
    unsafe {
        LOGGER = Some(logger);
        if let Some(ref logger) = LOGGER {
            log::set_logger(logger).expect("Logger ya inicializado");
        }
    }

    // Establecer el nivel máximo de logging
    log::set_max_level(LevelFilter::Info);

    // Log de inicialización exitosa - usando early logging para evitar problemas
    early_log("[EARLY INFO] Sistema de logging inicializado correctamente");
    early_log("[EARLY INFO] Eclipse OS Kernel Logger v1.0");
}

/// Obtener referencia al logger actual (para uso interno)
pub fn get_logger() -> Option<&'static SerialLogger> {
    unsafe { LOGGER.as_ref() }
}

/// Función de utilidad para escribir directamente al puerto serie
/// Útil para logs muy tempranos antes de que el logger esté inicializado
pub fn early_log(message: &str) {
    // Logging removido temporalmente para evitar breakpoint
    // Logging removido temporalmente para evitar breakpoint
}

// Las macros early_* se definen al final del archivo para uso externo

/// Configuración del logger para diferentes entornos
#[derive(Debug, Clone, Copy)]
pub enum LogConfig {
    /// Logging básico: solo errores y warnings
    Basic,
    /// Logging completo: todos los niveles
    Full,
    /// Logging de desarrollo: con debug y trace
    Development,
    /// Logging silencioso: solo errores críticos
    Silent,
}

impl LogConfig {
    /// Obtener el LevelFilter correspondiente
    pub fn to_level_filter(self) -> LevelFilter {
        match self {
            LogConfig::Basic => LevelFilter::Warn,
            LogConfig::Full => LevelFilter::Info,
            LogConfig::Development => LevelFilter::Trace,
            LogConfig::Silent => LevelFilter::Error,
        }
    }
}

/// Configurar el logger con una configuración específica
pub fn configure(config: LogConfig) {
    log::set_max_level(config.to_level_filter());
    log::info!("Logger configurado con nivel: {:?}", config);
}

/// Función de utilidad para dumpear el estado del puerto serie
pub fn dump_serial_status() {
    let port = crate::serial::get_serial_port();
    let status = port.get_status();
    log::debug!("Estado del puerto serie: {}", status);
}

/// Macros de conveniencia para logging temprano - VERSION SEGURA
/// Estas macros evitan usar alloc::format! que requiere allocador inicializado
/// Se definen fuera del módulo para poder ser exportadas
#[macro_export]
macro_rules! early_info {
    ($msg:expr) => {
        {
        }
    };
    ($msg:expr, $($arg:tt)*) => {
        {
        }
    };
}

#[macro_export]
macro_rules! early_error {
    ($msg:expr) => {
        {
        }
    };
    ($msg:expr, $($arg:tt)*) => {
        {
        }
    };
}

#[macro_export]
macro_rules! early_warn {
    ($msg:expr) => {
        {
        }
    };
    ($msg:expr, $($arg:tt)*) => {
        {
        }
    };
}

#[macro_export]
macro_rules! early_debug {
    ($msg:expr) => {
        {
        }
    };
    ($msg:expr, $($arg:tt)*) => {
        {
        }
    };
}
