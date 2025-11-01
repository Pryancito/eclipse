//! Sistema Básico de Configuración del Kernel para Eclipse OS
//!
//! Este módulo implementa un sistema completo de configuración que permite:
//! - Configuración centralizada de opciones del kernel
//! - Carga de configuración desde memoria o archivos
//! - Acceso seguro y eficiente a configuraciones
//! - Soporte para diferentes tipos de valores de configuración
//! - Configuraciones por defecto y override

#![no_std]
#![allow(unused_imports)]

extern crate alloc;
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use alloc::format;
use core::fmt;

// Importar macros de logging

/// Tipos de valores de configuración soportados
#[derive(Debug, Clone)]
pub enum ConfigValue {
    /// Valor booleano
    Bool(bool),
    /// Valor entero de 8 bits
    I8(i8),
    /// Valor entero de 16 bits
    I16(i16),
    /// Valor entero de 32 bits
    I32(i32),
    /// Valor entero de 64 bits
    I64(i64),
    /// Valor entero sin signo de 8 bits
    U8(u8),
    /// Valor entero sin signo de 16 bits
    U16(u16),
    /// Valor entero sin signo de 32 bits
    U32(u32),
    /// Valor entero sin signo de 64 bits
    U64(u64),
    /// Valor de punto flotante de 32 bits
    F32(f32),
    /// Valor de punto flotante de 64 bits
    F64(f64),
    /// Cadena de texto
    String(String),
    /// Lista de valores
    Array(Vec<ConfigValue>),
}

impl ConfigValue {
    /// Convierte el valor a booleano
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            ConfigValue::Bool(b) => Some(*b),
            ConfigValue::U8(0) => Some(false),
            ConfigValue::U8(1) => Some(true),
            ConfigValue::U32(0) => Some(false),
            ConfigValue::U32(1) => Some(true),
            _ => None,
        }
    }

    /// Convierte el valor a u32
    pub fn as_u32(&self) -> Option<u32> {
        match self {
            ConfigValue::U32(n) => Some(*n),
            ConfigValue::U64(n) if *n <= u32::MAX as u64 => Some(*n as u32),
            ConfigValue::I32(n) if *n >= 0 => Some(*n as u32),
            ConfigValue::I64(n) if *n >= 0 && *n <= u32::MAX as i64 => Some(*n as u32),
            _ => None,
        }
    }

    /// Convierte el valor a u64
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            ConfigValue::U64(n) => Some(*n),
            ConfigValue::U32(n) => Some(*n as u64),
            ConfigValue::I64(n) if *n >= 0 => Some(*n as u64),
            _ => None,
        }
    }

    /// Convierte el valor a string
    pub fn as_string(&self) -> Option<&str> {
        match self {
            ConfigValue::String(s) => Some(s),
            _ => None,
        }
    }

    /// Convierte el valor a array
    pub fn as_array(&self) -> Option<&[ConfigValue]> {
        match self {
            ConfigValue::Array(arr) => Some(arr),
            _ => None,
        }
    }
}

impl fmt::Display for ConfigValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigValue::Bool(b) => write!(f, "{}", b),
            ConfigValue::I8(n) => write!(f, "{}", n),
            ConfigValue::I16(n) => write!(f, "{}", n),
            ConfigValue::I32(n) => write!(f, "{}", n),
            ConfigValue::I64(n) => write!(f, "{}", n),
            ConfigValue::U8(n) => write!(f, "{}", n),
            ConfigValue::U16(n) => write!(f, "{}", n),
            ConfigValue::U32(n) => write!(f, "{}", n),
            ConfigValue::U64(n) => write!(f, "{}", n),
            ConfigValue::F32(n) => write!(f, "{}", n),
            ConfigValue::F64(n) => write!(f, "{}", n),
            ConfigValue::String(s) => write!(f, "\"{}\"", s),
            ConfigValue::Array(arr) => {
                write!(f, "[")?;
                for (i, item) in arr.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", item)?;
                }
                write!(f, "]")
            }
        }
    }
}

/// Categorías de configuración del kernel
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConfigCategory {
    /// Configuración general del kernel
    Kernel,
    /// Configuración de memoria
    Memory,
    /// Configuración del scheduler de procesos
    Scheduler,
    /// Configuración de dispositivos
    Devices,
    /// Configuración de logging
    Logging,
    /// Configuración de módulos
    Modules,
    /// Configuración de red
    Network,
    /// Configuración de seguridad
    Security,
    /// Configuración de debugging
    Debug,
}

/// Estructura principal de configuración del kernel
#[derive(Debug)]
pub struct KernelConfig {
    /// Configuraciones organizadas por categoría
    categories: BTreeMap<ConfigCategory, BTreeMap<String, ConfigValue>>,
    /// Configuraciones planas (para acceso rápido)
    flat_config: BTreeMap<String, ConfigValue>,
    /// Indica si la configuración está inicializada
    initialized: bool,
}

impl KernelConfig {
    /// Crea una nueva instancia de KernelConfig con valores por defecto
    pub fn new() -> Self {
        let mut config = Self {
            categories: BTreeMap::new(),
            flat_config: BTreeMap::new(),
            initialized: false,
        };

        // Inicializar con valores por defecto
        config.initialize_defaults();
        config
    }

    /// Inicializa la configuración con valores por defecto
    fn initialize_defaults(&mut self) {
        // Configuración del kernel
        self.set_default(ConfigCategory::Kernel, "version", ConfigValue::String(String::from("0.1.0")));
        self.set_default(ConfigCategory::Kernel, "hostname", ConfigValue::String(String::from("eclipse-os")));
        self.set_default(ConfigCategory::Kernel, "panic_on_error", ConfigValue::Bool(false));
        self.set_default(ConfigCategory::Kernel, "debug_mode", ConfigValue::Bool(false));

        // Configuración de memoria
        self.set_default(ConfigCategory::Memory, "heap_size", ConfigValue::U64(1024 * 1024)); // 1MB
        self.set_default(ConfigCategory::Memory, "stack_size", ConfigValue::U64(64 * 1024)); // 64KB
        self.set_default(ConfigCategory::Memory, "page_size", ConfigValue::U64(4096)); // 4KB
        self.set_default(ConfigCategory::Memory, "enable_paging", ConfigValue::Bool(true));

        // Configuración del scheduler
        self.set_default(ConfigCategory::Scheduler, "time_slice", ConfigValue::U32(10)); // 10ms
        self.set_default(ConfigCategory::Scheduler, "max_processes", ConfigValue::U32(256));
        self.set_default(ConfigCategory::Scheduler, "enable_preemption", ConfigValue::Bool(true));
        self.set_default(ConfigCategory::Scheduler, "priority_levels", ConfigValue::U8(32));

        // Configuración de dispositivos
        self.set_default(ConfigCategory::Devices, "max_devices", ConfigValue::U32(128));
        self.set_default(ConfigCategory::Devices, "auto_detect_devices", ConfigValue::Bool(true));
        self.set_default(ConfigCategory::Devices, "device_scan_interval", ConfigValue::U32(1000)); // 1s

        // Configuración de logging
        self.set_default(ConfigCategory::Logging, "log_level", ConfigValue::String(String::from("INFO")));
        self.set_default(ConfigCategory::Logging, "max_log_files", ConfigValue::U32(10));
        self.set_default(ConfigCategory::Logging, "log_file_size", ConfigValue::U64(1024 * 1024)); // 1MB
        self.set_default(ConfigCategory::Logging, "enable_syslog", ConfigValue::Bool(true));

        // Configuración de módulos
        self.set_default(ConfigCategory::Modules, "max_modules", ConfigValue::U32(64));
        self.set_default(ConfigCategory::Modules, "module_path", ConfigValue::String(String::from("/modules")));
        self.set_default(ConfigCategory::Modules, "auto_load_modules", ConfigValue::Bool(true));

        // Configuración de red
        self.set_default(ConfigCategory::Network, "enable_networking", ConfigValue::Bool(true));
        self.set_default(ConfigCategory::Network, "max_connections", ConfigValue::U32(1024));
        self.set_default(ConfigCategory::Network, "default_gateway", ConfigValue::String(String::from("192.168.1.1")));
        self.set_default(ConfigCategory::Network, "dhcp_enabled", ConfigValue::Bool(true));
        self.set_default(ConfigCategory::Network, "dns_server", ConfigValue::String(String::from("8.8.8.8")));
        self.set_default(ConfigCategory::Network, "hostname", ConfigValue::String(String::from("eclipse-os")));
        self.set_default(ConfigCategory::Network, "domain_name", ConfigValue::String(String::from("local")));
        self.set_default(ConfigCategory::Network, "mtu", ConfigValue::U32(1500));

        // Configuración de seguridad
        self.set_default(ConfigCategory::Security, "enable_kaslr", ConfigValue::Bool(true));
        self.set_default(ConfigCategory::Security, "enable_smap", ConfigValue::Bool(true));
        self.set_default(ConfigCategory::Security, "enable_smep", ConfigValue::Bool(true));

        // Configuración de debug
        self.set_default(ConfigCategory::Debug, "enable_kernel_debug", ConfigValue::Bool(false));
        self.set_default(ConfigCategory::Debug, "debug_port", ConfigValue::U16(1234));
        self.set_default(ConfigCategory::Debug, "enable_stack_trace", ConfigValue::Bool(true));
    }

    /// Establece un valor por defecto
    fn set_default(&mut self, category: ConfigCategory, key: &str, value: ConfigValue) {
        let cat_map = self.categories.entry(category).or_insert_with(BTreeMap::new);
        cat_map.insert(key.to_string(), value.clone());

        // También agregar a la configuración plana
        let flat_key = format!("{}.{}", category_to_string(category), key);
        self.flat_config.insert(flat_key, value);
    }

    /// Obtiene un valor de configuración
    pub fn get(&self, category: ConfigCategory, key: &str) -> Option<&ConfigValue> {
        self.categories.get(&category)?.get(key)
    }

    /// Obtiene un valor de configuración usando clave plana
    pub fn get_flat(&self, key: &str) -> Option<&ConfigValue> {
        self.flat_config.get(key)
    }

    /// Establece un valor de configuración
    pub fn set(&mut self, category: ConfigCategory, key: &str, value: ConfigValue) {
        let cat_map = self.categories.entry(category).or_insert_with(BTreeMap::new);
        cat_map.insert(key.to_string(), value.clone());

        // Actualizar también la configuración plana
        let flat_key = format!("{}.{}", category_to_string(category), key);
        self.flat_config.insert(flat_key, value);
    }

    /// Verifica si la configuración está inicializada
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Marca la configuración como inicializada
    pub fn mark_initialized(&mut self) {
        self.initialized = true;
    }

    /// Lista todas las configuraciones por categoría
    pub fn list_by_category(&self, category: ConfigCategory) -> Vec<(&str, &ConfigValue)> {
        if let Some(cat_map) = self.categories.get(&category) {
            cat_map.iter().map(|(k, v)| (k.as_str(), v)).collect()
        } else {
            Vec::new()
        }
    }

    /// Lista todas las configuraciones
    pub fn list_all(&self) -> Vec<(String, &ConfigValue)> {
        self.flat_config.iter().map(|(k, v)| (k.clone(), v)).collect()
    }

    /// Carga configuración desde un array de bytes (formato simple)
    pub fn load_from_bytes(&mut self, data: &[u8]) -> Result<(), ConfigError> {
        // Implementación simple: parsea líneas de formato "categoria.clave=valor"
        let content = core::str::from_utf8(data).map_err(|_| ConfigError::ParseError)?;

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue; // Ignorar líneas vacías y comentarios
            }

            if let Some((key, value_str)) = line.split_once('=') {
                let key = key.trim();
                let value_str = value_str.trim();

                if let Some((cat_str, key_part)) = key.split_once('.') {
                    if let Some(category) = string_to_category(cat_str) {
                        if let Some(value) = parse_config_value(value_str) {
                            self.set(category, key_part, value);
                        }
                    }
                }
            }
        }

        self.mark_initialized();
        Ok(())
    }

    /// Obtiene estadísticas de la configuración
    pub fn get_stats(&self) -> ConfigStats {
        let total_configs = self.flat_config.len();
        let categories_count = self.categories.len();

        ConfigStats {
            total_configs,
            categories_count,
            initialized: self.initialized,
        }
    }
}

/// Estadísticas de la configuración
#[derive(Debug, Clone)]
pub struct ConfigStats {
    /// Número total de configuraciones
    pub total_configs: usize,
    /// Número de categorías
    pub categories_count: usize,
    /// Indica si está inicializada
    pub initialized: bool,
}

/// Errores de configuración
#[derive(Debug, Clone)]
pub enum ConfigError {
    /// Error de parsing
    ParseError,
    /// Configuración no encontrada
    NotFound,
    /// Tipo incorrecto
    WrongType,
    /// Error genérico
    Other(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::ParseError => write!(f, "Error de parsing de configuración"),
            ConfigError::NotFound => write!(f, "Configuración no encontrada"),
            ConfigError::WrongType => write!(f, "Tipo de configuración incorrecto"),
            ConfigError::Other(msg) => write!(f, "Error: {}", msg),
        }
    }
}

/// Convierte una categoría a string
fn category_to_string(category: ConfigCategory) -> &'static str {
    match category {
        ConfigCategory::Kernel => "kernel",
        ConfigCategory::Memory => "memory",
        ConfigCategory::Scheduler => "scheduler",
        ConfigCategory::Devices => "devices",
        ConfigCategory::Logging => "logging",
        ConfigCategory::Modules => "modules",
        ConfigCategory::Network => "network",
        ConfigCategory::Security => "security",
        ConfigCategory::Debug => "debug",
    }
}

/// Convierte un string a categoría
fn string_to_category(s: &str) -> Option<ConfigCategory> {
    match s {
        "kernel" => Some(ConfigCategory::Kernel),
        "memory" => Some(ConfigCategory::Memory),
        "scheduler" => Some(ConfigCategory::Scheduler),
        "devices" => Some(ConfigCategory::Devices),
        "logging" => Some(ConfigCategory::Logging),
        "modules" => Some(ConfigCategory::Modules),
        "network" => Some(ConfigCategory::Network),
        "security" => Some(ConfigCategory::Security),
        "debug" => Some(ConfigCategory::Debug),
        _ => None,
    }
}

/// Parsea un valor de configuración desde string
fn parse_config_value(s: &str) -> Option<ConfigValue> {
    // Remover comillas si existen
    let s = s.trim_matches('"');

    // Intentar parsear como booleano
    if s.eq_ignore_ascii_case("true") {
        return Some(ConfigValue::Bool(true));
    }
    if s.eq_ignore_ascii_case("false") {
        return Some(ConfigValue::Bool(false));
    }

    // Intentar parsear como número
    if let Ok(n) = s.parse::<u64>() {
        return Some(ConfigValue::U64(n));
    }
    if let Ok(n) = s.parse::<i64>() {
        return Some(ConfigValue::I64(n));
    }
    if let Ok(n) = s.parse::<f64>() {
        return Some(ConfigValue::F64(n));
    }

    // Por defecto, tratar como string
    Some(ConfigValue::String(String::from(s)))
}

/// Instancia global de configuración del kernel
static mut KERNEL_CONFIG: Option<KernelConfig> = None;

/// Inicializa el sistema de configuración del kernel
pub fn init_kernel_config() -> Result<(), ConfigError> {
    unsafe {
        KERNEL_CONFIG = Some(KernelConfig::new());
    }

    // Log removido
    Ok(())
}

/// Obtiene una referencia a la configuración del kernel
pub fn get_kernel_config() -> Option<&'static mut KernelConfig> {
    unsafe {
        KERNEL_CONFIG.as_mut()
    }
}

/// Carga configuración desde bytes
pub fn load_config_from_bytes(data: &[u8]) -> Result<(), ConfigError> {
    if let Some(config) = get_kernel_config() {
        config.load_from_bytes(data)?;
        // Log removido
        Ok(())
    } else {
        Err(ConfigError::Other("Sistema de configuración no inicializado".to_string()))
    }
}

/// Funciones helper para acceder a configuraciones comunes

/// Obtiene el hostname configurado
pub fn get_hostname() -> Option<String> {
    get_kernel_config()?
        .get(ConfigCategory::Kernel, "hostname")
        .and_then(|v| v.as_string())
        .map(|s| s.to_string())
}

/// Obtiene el tamaño del heap configurado
pub fn get_heap_size() -> Option<u64> {
    get_kernel_config()?
        .get(ConfigCategory::Memory, "heap_size")
        .and_then(|v| v.as_u64())
}

/// Obtiene el slice de tiempo del scheduler
pub fn get_scheduler_time_slice() -> Option<u32> {
    get_kernel_config()?
        .get(ConfigCategory::Scheduler, "time_slice")
        .and_then(|v| v.as_u32())
}

/// Verifica si el modo debug está habilitado
pub fn is_debug_mode() -> bool {
    get_kernel_config()
        .and_then(|c| c.get(ConfigCategory::Kernel, "debug_mode"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Verifica si el networking está habilitado
pub fn is_networking_enabled() -> bool {
    get_kernel_config()
        .and_then(|c| c.get(ConfigCategory::Network, "enable_networking"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Genera un reporte de la configuración actual
pub fn generate_config_report() -> Result<String, ConfigError> {
    let config = get_kernel_config().ok_or(ConfigError::Other("Configuración no inicializada".to_string()))?;

    let mut report = String::from("=== REPORTE DE CONFIGURACIÓN DEL KERNEL ===\n\n");

    // Estadísticas generales
    let stats = config.get_stats();
    report.push_str(&format!("Configuraciones totales: {}\n", stats.total_configs));
    report.push_str(&format!("Categorías: {}\n", stats.categories_count));
    report.push_str(&format!("Inicializada: {}\n\n", stats.initialized));

    // Configuraciones por categoría
    for category in &[
        ConfigCategory::Kernel,
        ConfigCategory::Memory,
        ConfigCategory::Scheduler,
        ConfigCategory::Devices,
        ConfigCategory::Logging,
        ConfigCategory::Modules,
        ConfigCategory::Network,
        ConfigCategory::Security,
        ConfigCategory::Debug,
    ] {
        report.push_str(&format!("{}:\n", category_to_string(*category).to_uppercase()));

        let configs = config.list_by_category(*category);
        if configs.is_empty() {
            report.push_str("  (sin configuraciones)\n");
        } else {
            for (key, value) in configs {
                report.push_str(&format!("  {} = {}\n", key, value));
            }
        }
        report.push_str("\n");
    }

    report.push_str("=== FIN DEL REPORTE ===\n");

    Ok(report)
}

// Macros para facilitar el acceso a configuraciones

#[macro_export]
macro_rules! config_get {
    ($category:expr, $key:expr) => {
        $crate::config::get_kernel_config()
            .and_then(|c| c.get($category, $key))
    };
}

#[macro_export]
macro_rules! config_get_bool {
    ($category:expr, $key:expr) => {
        $crate::config::get_kernel_config()
            .and_then(|c| c.get($category, $key))
            .and_then(|v| v.as_bool())
    };
}

#[macro_export]
macro_rules! config_get_u32 {
    ($category:expr, $key:expr) => {
        $crate::config::get_kernel_config()
            .and_then(|c| c.get($category, $key))
            .and_then(|v| v.as_u32())
    };
}

#[macro_export]
macro_rules! config_get_string {
    ($category:expr, $key:expr) => {
        $crate::config::get_kernel_config()
            .and_then(|c| c.get($category, $key))
            .and_then(|v| v.as_string())
            .map(|s| s.to_string())
    };
}