//! Journald para Eclipse SystemD
//! 
//! Este módulo implementa el sistema de logging de systemd
//! para registrar eventos del sistema y servicios.

use anyhow::Result;
use log::{info, warn, error, debug};
use serde::{Serialize, Deserialize};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Write, BufWriter, BufReader, Read};
use std::path::Path;
use std::sync::{Arc, Mutex};
use chrono::{DateTime, Utc};
use flate2::Compression;
use flate2::write::GzEncoder;

/// Entrada del journal
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub timestamp: DateTime<Utc>,
    pub priority: Priority,
    pub facility: Facility,
    pub service: String,
    pub message: String,
    pub pid: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub fields: HashMap<String, String>,
}

/// Prioridad del mensaje
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Priority {
    Emergency = 0,
    Alert = 1,
    Critical = 2,
    Error = 3,
    Warning = 4,
    Notice = 5,
    Info = 6,
    Debug = 7,
}

/// Facilidad del mensaje
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Facility {
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

/// Manager del journal
pub struct JournalManager {
    /// Archivo del journal
    journal_file: String,
    /// Buffer de escritura
    writer: Arc<Mutex<BufWriter<std::fs::File>>>,
    /// Ruta de log plano adicional
    plain_log_file: String,
    /// Configuración
    config: JournalConfig,
}

/// Configuración del journal
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct JournalConfig {
    pub max_file_size: u64,
    pub max_files: u32,
    pub compress_old: bool,
    pub sync_interval: u64,
    pub compression_level: i32,
    pub retention_days: u32,
}

impl Default for JournalConfig {
    fn default() -> Self {
        Self {
            max_file_size: 100 * 1024 * 1024, // 100MB
            max_files: 10,
            compress_old: true,
            sync_interval: 5, // 5 segundos
            compression_level: 6, // Nivel de compresión por defecto
            retention_days: 30, // Mantener logs por 30 días
        }
    }
}

impl JournalManager {
    /// Crea una nueva instancia del manager del journal
    pub fn new(journal_file: &str) -> Result<Self> {
        let config = JournalConfig::default();
        Self::with_config(journal_file, config)
    }

    /// Crea una nueva instancia con configuración personalizada
    pub fn with_config(journal_file: &str, config: JournalConfig) -> Result<Self> {
        // Crear directorio si no existe
        if let Some(parent) = Path::new(journal_file).parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Abrir archivo del journal con fallback a /run si /var está ro
        let writer = {
            match OpenOptions::new().create(true).append(true).open(journal_file) {
                Ok(file) => Arc::new(Mutex::new(BufWriter::new(file))),
                Err(e) => {
                    let fallback_json = "/run/systemd-journal.json";
                    if let Some(parent) = Path::new(fallback_json).parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let file = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(fallback_json)?;
                    eprintln!(
                        "[systemd] aviso: no se pudo abrir {} ({}), usando {}",
                        journal_file, e, fallback_json
                    );
                    Arc::new(Mutex::new(BufWriter::new(file)))
                }
            }
        };

        // Preparar log plano adicional en /var/log/systemd.log
        let plain_log_file = "/var/log/systemd.log".to_string();
        if let Some(parent) = Path::new(&plain_log_file).parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Crear si no existe y ajustar permisos básicos (0644)
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&plain_log_file)?;
        // Establecer permisos 0644 si es posible
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(metadata) = std::fs::metadata(&plain_log_file) {
                let mut perms = metadata.permissions();
                perms.set_mode(0o644);
                let _ = std::fs::set_permissions(&plain_log_file, perms);
            }
        }

        Ok(Self {
            journal_file: journal_file.to_string(),
            writer,
            plain_log_file,
            config,
        })
    }

    /// Registra una entrada en el journal
    pub fn log(&self, entry: JournalEntry) -> Result<()> {
        debug!("Registrando Registrando entrada en journal: {:?}", entry);

        // Serializar entrada como JSON
        let json = serde_json::to_string(&entry)?;
        
        // Escribir al archivo
        {
            let mut writer = match self.writer.lock() {
                Ok(guard) => guard,
                Err(poisoned) => {
                    error!("Lock envenenado en write_entry (journald), recuperando...");
                    poisoned.into_inner()
                }
            };
            writeln!(writer, "{}", json)?;
            writer.flush()?;
        }

        // Escribir también al log plano /var/log/systemd.log
        self.append_plain_log(&entry)?;

        // Verificar tamaño del archivo
        self.check_file_size()?;

        Ok(())
    }

    /// Añade una línea legible a /var/log/systemd.log
    fn append_plain_log(&self, entry: &JournalEntry) -> Result<()> {
        let ts = entry.timestamp.format("%Y-%m-%d %H:%M:%S%.3f");
        let prio = match entry.priority {
            Priority::Emergency => "EMERG",
            Priority::Alert => "ALERT",
            Priority::Critical => "CRIT",
            Priority::Error => "ERROR",
            Priority::Warning => "WARN",
            Priority::Notice => "NOTICE",
            Priority::Info => "INFO",
            Priority::Debug => "DEBUG",
        };
        let line = format!(
            "[{}] {} {}{}: {}\n",
            ts,
            prio,
            entry.service,
            entry.pid.map(|p| format!("[{}]", p)).unwrap_or_default(),
            entry.message
        );

        // Intentar escribir en /var/log/systemd.log
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.plain_log_file)
        {
            Ok(mut file) => {
                file.write_all(line.as_bytes())?;
                file.flush()?;
                Ok(())
            }
            Err(e) => {
                // Fallback: intentar /run/systemd.log (suele ser escribible en live/ISO)
                let fallback = "/run/systemd.log";
                if let Some(parent) = Path::new(fallback).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match OpenOptions::new().create(true).append(true).open(fallback) {
                    Ok(mut file) => {
                        let _ = file.write_all(line.as_bytes());
                        let _ = file.flush();
                        eprintln!("[systemd] aviso: no se pudo escribir en {} ({}), usando {}", self.plain_log_file, e, fallback);
                        Ok(())
                    }
                    Err(e2) => {
                        // Último recurso: stderr
                        eprintln!("[systemd] error: no se pudo escribir logs en {} ({}) ni en {} ({}). Mensaje: {}", self.plain_log_file, e, fallback, e2, line.trim());
                        Ok(())
                    }
                }
            }
        }
    }

    /// Registra un mensaje simple
    pub fn log_message(&self, service: &str, priority: Priority, message: &str) -> Result<()> {
        let entry = JournalEntry {
            timestamp: Utc::now(),
            priority,
            facility: Facility::Daemon,
            service: service.to_string(),
            message: message.to_string(),
            pid: std::process::id().into(),
            uid: Some(0), // root
            gid: Some(0), // root
            fields: HashMap::new(),
        };

        self.log(entry)
    }

    /// Registra un mensaje de error
    pub fn log_error(&self, service: &str, message: &str) -> Result<()> {
        self.log_message(service, Priority::Error, message)
    }

    /// Registra un mensaje de advertencia
    pub fn log_warning(&self, service: &str, message: &str) -> Result<()> {
        self.log_message(service, Priority::Warning, message)
    }

    /// Registra un mensaje informativo
    pub fn log_info(&self, service: &str, message: &str) -> Result<()> {
        self.log_message(service, Priority::Info, message)
    }

    /// Registra un mensaje de debug
    pub fn log_debug(&self, service: &str, message: &str) -> Result<()> {
        self.log_message(service, Priority::Debug, message)
    }

    /// Registra un mensaje estructurado con campos adicionales
    #[allow(dead_code)]
    pub fn log_structured(&self, service: &str, priority: Priority, message: &str, fields: HashMap<String, String>) -> Result<()> {
        let entry = JournalEntry {
            timestamp: Utc::now(),
            priority,
            facility: Facility::Daemon,
            service: service.to_string(),
            message: message.to_string(),
            pid: std::process::id().into(),
            uid: Some(0), // root
            gid: Some(0), // root
            fields,
        };

        self.log(entry)
    }

    /// Registra evento de inicio de servicio
    #[allow(dead_code)]
    pub fn log_service_start(&self, service_name: &str, pid: u32) -> Result<()> {
        let mut fields = HashMap::new();
        fields.insert("EVENT".to_string(), "SERVICE_START".to_string());
        fields.insert("PID".to_string(), pid.to_string());

        self.log_structured(service_name, Priority::Info, &format!("Servicio {} iniciado", service_name), fields)
    }

    /// Registra evento de parada de servicio
    #[allow(dead_code)]
    pub fn log_service_stop(&self, service_name: &str, pid: Option<u32>, exit_code: Option<i32>) -> Result<()> {
        let mut fields = HashMap::new();
        fields.insert("EVENT".to_string(), "SERVICE_STOP".to_string());

        if let Some(pid) = pid {
            fields.insert("PID".to_string(), pid.to_string());
        }

        if let Some(code) = exit_code {
            fields.insert("EXIT_CODE".to_string(), code.to_string());
        }

        let message = if let Some(code) = exit_code {
            format!("Servicio {} detenido (código: {})", service_name, code)
        } else {
            format!("Servicio {} detenido", service_name)
        };

        self.log_structured(service_name, Priority::Info, &message, fields)
    }

    /// Registra evento de fallo de servicio
    #[allow(dead_code)]
    pub fn log_service_failure(&self, service_name: &str, error: &str) -> Result<()> {
        let mut fields = HashMap::new();
        fields.insert("EVENT".to_string(), "SERVICE_FAILURE".to_string());
        fields.insert("ERROR".to_string(), error.to_string());

        self.log_structured(service_name, Priority::Error, &format!("Servicio {} falló: {}", service_name, error), fields)
    }

    /// Lee entradas del journal
    #[allow(dead_code)]
    pub fn read_entries(&self, service: Option<&str>, limit: Option<usize>) -> Result<Vec<JournalEntry>> {
        let content = std::fs::read_to_string(&self.journal_file)?;
        let mut entries = Vec::new();

        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }

            match serde_json::from_str::<JournalEntry>(line) {
                Ok(entry) => {
                    // Filtrar por servicio si se especifica
                    if let Some(service_filter) = service {
                        if entry.service != service_filter {
                            continue;
                        }
                    }

                    entries.push(entry);

                    // Limitar número de entradas
                    if let Some(limit) = limit {
                        if entries.len() >= limit {
                            break;
                        }
                    }
                }
                Err(e) => {
                    warn!("Advertencia  Error parseando entrada del journal: {}", e);
                }
            }
        }

        // Ordenar por timestamp (más recientes primero)
        entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

        Ok(entries)
    }

    /// Busca entradas en el journal
    #[allow(dead_code)]
    pub fn search_entries(&self, query: &str, service: Option<&str>) -> Result<Vec<JournalEntry>> {
        let entries = self.read_entries(service, None)?;
        let mut results = Vec::new();

        for entry in entries {
            if entry.message.contains(query) || entry.service.contains(query) {
                results.push(entry);
            }
        }

        Ok(results)
    }

    /// Obtiene estadísticas del journal
    #[allow(dead_code)]
    pub fn get_stats(&self) -> Result<JournalStats> {
        let entries = self.read_entries(None, None)?;
        
        let mut service_counts = HashMap::new();
        let mut priority_counts = HashMap::new();
        
        for entry in &entries {
            *service_counts.entry(entry.service.clone()).or_insert(0) += 1;
            *priority_counts.entry(entry.priority).or_insert(0) += 1;
        }

        Ok(JournalStats {
            total_entries: entries.len(),
            service_counts,
            priority_counts,
            oldest_entry: entries.last().map(|e| e.timestamp),
            newest_entry: entries.first().map(|e| e.timestamp),
        })
    }

    /// Verifica el tamaño del archivo y rota si es necesario
    fn check_file_size(&self) -> Result<()> {
        let metadata = std::fs::metadata(&self.journal_file)?;
        
        if metadata.len() > self.config.max_file_size {
            info!("Reiniciando Rotando archivo del journal (tamaño: {} bytes)", metadata.len());
            self.rotate_journal()?;
        }

        Ok(())
    }

    /// Rota el archivo del journal
    fn rotate_journal(&self) -> Result<()> {
        // Cerrar el writer actual
        drop(match self.writer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                error!("Lock envenenado al cerrar writer en rotate_journal, recuperando...");
                poisoned.into_inner()
            }
        });

        // Generar nombre del archivo rotado con timestamp
        let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
        let rotated_file = format!("{}.{}", self.journal_file, timestamp);

        // Renombrar archivo actual
        std::fs::rename(&self.journal_file, &rotated_file)?;

        // Crear nuevo archivo
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(&self.journal_file)?;

        // Actualizar writer
        match self.writer.lock() {
            Ok(mut writer) => *writer = BufWriter::new(file),
            Err(poisoned) => {
                error!("Lock envenenado al actualizar writer en rotate_journal, recuperando...");
                *poisoned.into_inner() = BufWriter::new(file);
            }
        }

        // Comprimir archivo rotado si está habilitado
        if self.config.compress_old {
            self.compress_rotated_file(&rotated_file)?;
        }

        // Limpiar archivos antiguos
        self.cleanup_old_files()?;

        info!("Servicio Journal rotado correctamente");
        Ok(())
    }

    /// Comprime un archivo rotado del journal
    fn compress_rotated_file(&self, file_path: &str) -> Result<()> {
        let compressed_path = format!("{}.gz", file_path);

        // Abrir archivo original
        let input_file = std::fs::File::open(file_path)?;
        let reader = BufReader::new(input_file);

        // Crear archivo comprimido
        let output_file = std::fs::File::create(&compressed_path)?;
        let mut encoder = GzEncoder::new(output_file, Compression::new(self.config.compression_level as u32));

        // Copiar contenido y comprimir
        std::io::copy(&mut reader.take(u64::MAX), &mut encoder)?;
        encoder.finish()?;

        // Eliminar archivo original
        std::fs::remove_file(file_path)?;

        debug!("📦 Archivo comprimido: {} -> {}", file_path, compressed_path);
        Ok(())
    }

    /// Limpia archivos antiguos del journal
    fn cleanup_old_files(&self) -> Result<()> {
        let journal_dir = Path::new(&self.journal_file).parent()
            .ok_or_else(|| anyhow::anyhow!("No se pudo obtener directorio padre del journal"))?;
        let journal_name = Path::new(&self.journal_file).file_name()
            .ok_or_else(|| anyhow::anyhow!("No se pudo obtener nombre del archivo journal"))?;

        let mut files = Vec::new();
        
        for entry in std::fs::read_dir(journal_dir)? {
            let entry = entry?;
            let path = entry.path();
            
            if let Some(file_name) = path.file_name() {
                if file_name.to_string_lossy().starts_with(&*journal_name.to_string_lossy()) {
                    if let Ok(metadata) = path.metadata() {
                        files.push((path, metadata.modified()?));
                    }
                }
            }
        }

        // Ordenar por fecha de modificación (más antiguos primero)
        files.sort_by(|a, b| a.1.cmp(&b.1));

        // Eliminar archivos excedentes
        if files.len() > self.config.max_files as usize {
            let to_remove = files.len() - self.config.max_files as usize;
            for (path, _) in files.iter().take(to_remove) {
                if let Err(e) = std::fs::remove_file(path) {
                    warn!("Advertencia  Error eliminando archivo antiguo {:?}: {}", path, e);
                }
            }
        }

        Ok(())
    }

    /// Sincroniza el journal
    pub fn sync(&self) -> Result<()> {
        match self.writer.lock() {
            Ok(mut writer) => writer.flush()?,
            Err(poisoned) => {
                error!("Lock envenenado en sync, recuperando...");
                poisoned.into_inner().flush()?;
            }
        }
        Ok(())
    }
}

/// Estadísticas del journal
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct JournalStats {
    pub total_entries: usize,
    pub service_counts: HashMap<String, usize>,
    pub priority_counts: HashMap<Priority, usize>,
    pub oldest_entry: Option<DateTime<Utc>>,
    pub newest_entry: Option<DateTime<Utc>>,
}

impl JournalStats {
    #[allow(dead_code)]
    pub fn get_summary(&self) -> String {
        format!(
            "Journal: {} entradas, {} servicios, {} errores, {} advertencias",
            self.total_entries,
            self.service_counts.len(),
            self.priority_counts.get(&Priority::Error).unwrap_or(&0),
            self.priority_counts.get(&Priority::Warning).unwrap_or(&0)
        )
    }
}

/// Macro para logging fácil
#[macro_export]
macro_rules! journal_log {
    ($journal:expr, $service:expr, $priority:expr, $($arg:tt)*) => {
        if let Err(e) = $journal.log_message($service, $priority, &format!($($arg)*)) {
            eprintln!("Error escribiendo al journal: {}", e);
        }
    };
}

/// Macro para logging de errores
#[macro_export]
macro_rules! journal_error {
    ($journal:expr, $service:expr, $($arg:tt)*) => {
        journal_log!($journal, $service, Priority::Error, $($arg)*);
    };
}

/// Macro para logging de advertencias
#[macro_export]
macro_rules! journal_warning {
    ($journal:expr, $service:expr, $($arg:tt)*) => {
        journal_log!($journal, $service, Priority::Warning, $($arg)*);
    };
}

/// Macro para logging informativo
#[macro_export]
macro_rules! journal_info {
    ($journal:expr, $service:expr, $($arg:tt)*) => {
        journal_log!($journal, $service, Priority::Info, $($arg)*);
    };
}

/// Macro para logging de debug
#[macro_export]
macro_rules! journal_debug {
    ($journal:expr, $service:expr, $($arg:tt)*) => {
        journal_log!($journal, $service, Priority::Debug, $($arg)*);
    };
}
