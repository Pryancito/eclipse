//! Sistema de logging similar a syslog para Eclipse OS
//! 
//! Implementa un sistema de logging robusto y eficiente
//! compatible con el estándar syslog.

use core::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, AtomicBool, Ordering};
use core::fmt::Write;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::format;

/// Facilidades syslog estándar
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyslogFacility {
    Kernel = 0,
    User = 1,
    Mail = 2,
    Daemon = 3,
    Auth = 4,
    Syslog = 5,
    Lpr = 6,
    News = 7,
    Uucp = 8,
    Cron = 9,
    AuthPriv = 10,
    Ftp = 11,
    Local0 = 16,
    Local1 = 17,
    Local2 = 18,
    Local3 = 19,
    Local4 = 20,
    Local5 = 21,
    Local6 = 22,
    Local7 = 23,
}

/// Niveles de severidad syslog
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SyslogSeverity {
    Emergency = 0,  // system is unusable
    Alert = 1,      // action must be taken immediately
    Critical = 2,   // critical conditions
    Error = 3,      // error conditions
    Warning = 4,    // warning conditions
    Notice = 5,     // normal but significant condition
    Info = 6,       // informational messages
    Debug = 7,      // debug-level messages
}

/// Entrada de log syslog
#[derive(Debug, Clone)]
pub struct SyslogEntry {
    pub timestamp: u64,
    pub facility: SyslogFacility,
    pub severity: SyslogSeverity,
    pub hostname: String,
    pub tag: String,
    pub message: String,
    pub pid: Option<u32>,
}

impl SyslogEntry {
    pub fn new(facility: SyslogFacility, severity: SyslogSeverity, tag: &str, message: &str) -> Self {
        Self {
            timestamp: get_current_timestamp(),
            facility,
            severity,
            hostname: "eclipse-os".to_string(),
            tag: tag.to_string(),
            message: message.to_string(),
            pid: None,
        }
    }

    /// Formatear entrada en formato syslog estándar
    pub fn format_syslog(&self) -> String {
        let priority = (self.facility as u8) * 8 + (self.severity as u8);
        let timestamp_str = format_timestamp(self.timestamp);
        
        if let Some(pid) = self.pid {
            format!("<{}>{} {} {}[{}]: {}", 
                priority, timestamp_str, self.hostname, self.tag, pid, self.message)
        } else {
            format!("<{}>{} {} {}: {}", 
                priority, timestamp_str, self.hostname, self.tag, self.message)
        }
    }

    /// Formatear entrada en formato RFC 3164
    pub fn format_rfc3164(&self) -> String {
        let priority = (self.facility as u8) * 8 + (self.severity as u8);
        let timestamp_str = format_timestamp_rfc3164(self.timestamp);
        
        if let Some(pid) = self.pid {
            format!("<{}>{} {} {}[{}]: {}", 
                priority, timestamp_str, self.hostname, self.tag, pid, self.message)
        } else {
            format!("<{}>{} {} {}: {}", 
                priority, timestamp_str, self.hostname, self.tag, self.message)
        }
    }
}

/// Logger syslog principal
pub struct SyslogLogger {
    serial_port: u16,
    max_entries: usize,
    entries: Vec<SyslogEntry>,
    current_entry: AtomicUsize,
    enabled: AtomicBool,
    min_severity: AtomicU8,
}

static SYSLOG_LOGGER: SyslogLogger = SyslogLogger {
    serial_port: 0x3F8, // COM1
    max_entries: 1000,
    entries: Vec::new(),
    current_entry: AtomicUsize::new(0),
    enabled: AtomicBool::new(true),
    min_severity: AtomicU8::new(SyslogSeverity::Info as u8),
};

impl SyslogLogger {
    /// Inicializar el logger syslog
    pub fn init() -> Result<(), &'static str> {
        // Inicializar puerto serial
        init_serial_port(SYSLOG_LOGGER.serial_port)?;
        
        // Log de inicio
        let entry = SyslogEntry::new(
            SyslogFacility::Kernel,
            SyslogSeverity::Info,
            "syslog",
            "Sistema de logging syslog inicializado"
        );
        
        SYSLOG_LOGGER.log_entry(&entry);
        Ok(())
    }

    /// Registrar una entrada de log
    pub fn log_entry(&self, entry: &SyslogEntry) {
        if !self.enabled.load(Ordering::SeqCst) {
            return;
        }

        if (entry.severity as u8) > self.min_severity.load(Ordering::SeqCst) {
            return;
        }

        // Formatear y enviar a puerto serial
        let formatted = entry.format_syslog();
        self.write_to_serial(&formatted);

        // Almacenar en buffer circular (si hay espacio)
        // En un kernel real, esto sería más complejo
    }

    /// Escribir a puerto serial
    fn write_to_serial(&self, data: &str) {
        for byte in data.bytes() {
            self.write_serial_byte(byte);
        }
    }

    /// Escribir un byte al puerto serial
    fn write_serial_byte(&self, byte: u8) {
        // Esperar a que el puerto esté listo
        while !self.is_serial_ready() {
            core::hint::spin_loop();
        }
        
        // Escribir byte
        unsafe {
            core::ptr::write_volatile(self.serial_port as *mut u8, byte);
        }
    }

    /// Verificar si el puerto serial está listo
    fn is_serial_ready(&self) -> bool {
        unsafe {
            let status_port = self.serial_port + 5;
            let status = core::ptr::read_volatile(status_port as *const u8);
            (status & 0x20) != 0 // Bit 5 = Transmit Holding Register Empty
        }
    }

    /// Configurar nivel mínimo de severidad
    pub fn set_min_severity(&self, severity: SyslogSeverity) {
        self.min_severity.store(severity as u8, Ordering::SeqCst);
    }

    /// Habilitar/deshabilitar logging
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }

    /// Obtener estadísticas del logger
    pub fn get_stats(&self) -> SyslogStats {
        SyslogStats {
            total_entries: self.current_entry.load(Ordering::SeqCst),
            enabled: self.enabled.load(Ordering::SeqCst),
            min_severity: self.min_severity.load(Ordering::SeqCst),
            serial_port: self.serial_port,
        }
    }
}

/// Estadísticas del logger syslog
#[derive(Debug, Clone)]
pub struct SyslogStats {
    pub total_entries: usize,
    pub enabled: bool,
    pub min_severity: u8,
    pub serial_port: u16,
}

/// Funciones de conveniencia para logging
pub fn log_kernel(severity: SyslogSeverity, tag: &str, message: &str) {
    let entry = SyslogEntry::new(SyslogFacility::Kernel, severity, tag, message);
    SYSLOG_LOGGER.log_entry(&entry);
}

pub fn log_daemon(severity: SyslogSeverity, tag: &str, message: &str) {
    let entry = SyslogEntry::new(SyslogFacility::Daemon, severity, tag, message);
    SYSLOG_LOGGER.log_entry(&entry);
}

pub fn log_auth(severity: SyslogSeverity, tag: &str, message: &str) {
    let entry = SyslogEntry::new(SyslogFacility::Auth, severity, tag, message);
    SYSLOG_LOGGER.log_entry(&entry);
}

pub fn log_mail(severity: SyslogSeverity, tag: &str, message: &str) {
    let entry = SyslogEntry::new(SyslogFacility::Mail, severity, tag, message);
    SYSLOG_LOGGER.log_entry(&entry);
}

/// Macros de conveniencia
#[macro_export]
macro_rules! syslog_emerg {
    ($tag:expr, $msg:expr) => {
        crate::syslog::log_kernel(crate::syslog::SyslogSeverity::Emergency, $tag, $msg);
    };
}

#[macro_export]
macro_rules! syslog_alert {
    ($tag:expr, $msg:expr) => {
        crate::syslog::log_kernel(crate::syslog::SyslogSeverity::Alert, $tag, $msg);
    };
}

#[macro_export]
macro_rules! syslog_crit {
    ($tag:expr, $msg:expr) => {
        crate::syslog::log_kernel(crate::syslog::SyslogSeverity::Critical, $tag, $msg);
    };
}

#[macro_export]
macro_rules! syslog_err {
    ($tag:expr, $msg:expr) => {
        crate::syslog::log_kernel(crate::syslog::SyslogSeverity::Error, $tag, $msg);
    };
}

#[macro_export]
macro_rules! syslog_warn {
    ($tag:expr, $msg:expr) => {
        crate::syslog::log_kernel(crate::syslog::SyslogSeverity::Warning, $tag, $msg);
    };
}

#[macro_export]
macro_rules! syslog_notice {
    ($tag:expr, $msg:expr) => {
        crate::syslog::log_kernel(crate::syslog::SyslogSeverity::Notice, $tag, $msg);
    };
}

#[macro_export]
macro_rules! syslog_info {
    ($tag:expr, $msg:expr) => {
        crate::syslog::log_kernel(crate::syslog::SyslogSeverity::Info, $tag, $msg);
    };
}

#[macro_export]
macro_rules! syslog_debug {
    ($tag:expr, $msg:expr) => {
        crate::syslog::log_kernel(crate::syslog::SyslogSeverity::Debug, $tag, $msg);
    };
}

#[macro_export]
macro_rules! syslog_trace {
    ($tag:expr, $msg:expr) => {
        crate::syslog::log_kernel(crate::syslog::SyslogSeverity::Debug, $tag, $msg);
    };
}

/// Funciones auxiliares
fn get_current_timestamp() -> u64 {
    // En una implementación real, esto obtendría el timestamp real
    // Por ahora, simulamos con un contador
    static COUNTER: AtomicU64 = AtomicU64::new(1640995200);
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

fn format_timestamp(timestamp: u64) -> String {
    // Formato: YYYY-MM-DD HH:MM:SS
    // Simplificado para el ejemplo
    format!("2024-01-01 12:00:{}", timestamp % 60)
}

fn format_timestamp_rfc3164(timestamp: u64) -> String {
    // Formato RFC 3164: MMM DD HH:MM:SS
    // Simplificado para el ejemplo
    format!("Jan  1 12:00:{}", timestamp % 60)
}

fn init_serial_port(port: u16) -> Result<(), &'static str> {
    // Inicializar puerto serial COM1
    // Configurar baud rate, bits de datos, paridad, etc.
    
    // Configurar divisor de baud rate (115200 bps)
    unsafe {
        // Habilitar DLAB
        core::ptr::write_volatile((port + 3) as *mut u8, 0x80);
        
        // Configurar divisor (115200 bps = 1)
        core::ptr::write_volatile(port as *mut u8, 0x01);
        core::ptr::write_volatile((port + 1) as *mut u8, 0x00);
        
        // Configurar línea de control (8 bits, sin paridad, 1 stop bit)
        core::ptr::write_volatile((port + 3) as *mut u8, 0x03);
        
        // Habilitar FIFO
        core::ptr::write_volatile((port + 2) as *mut u8, 0xC7);
        
        // Habilitar interrupciones
        core::ptr::write_volatile((port + 1) as *mut u8, 0x01);
    }
    
    Ok(())
}

/// Funciones públicas para el kernel
pub fn init_syslog() -> Result<(), &'static str> {
    SyslogLogger::init()
}

pub fn get_syslog_stats() -> SyslogStats {
    SYSLOG_LOGGER.get_stats()
}

pub fn set_syslog_level(severity: SyslogSeverity) {
    SYSLOG_LOGGER.set_min_severity(severity);
}

pub fn enable_syslog(enabled: bool) {
    SYSLOG_LOGGER.set_enabled(enabled);
}
