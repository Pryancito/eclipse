//! Journald para Eclipse SystemD
//! 
//! Este módulo implementa el sistema de logging de systemd
//! para registrar eventos del sistema y servicios.

use anyhow::Result;
use log::{info, warn, error, debug};
use serde::{Serialize, Deserialize};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Write, BufWriter};
use std::path::Path;
use std::sync::{Arc, Mutex};
use chrono::{DateTime, Utc};

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
    /// Configuración
    config: JournalConfig,
}

/// Configuración del journal
#[derive(Debug, Clone)]
pub struct JournalConfig {
    pub max_file_size: u64,
    pub max_files: u32,
    pub compress_old: bool,
    pub sync_interval: u64,
}

impl Default for JournalConfig {
    fn default() -> Self {
        Self {
            max_file_size: 100 * 1024 * 1024, // 100MB
            max_files: 10,
            compress_old: true,
            sync_interval: 5, // 5 segundos
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

        // Abrir archivo del journal
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(journal_file)?;

        let writer = Arc::new(Mutex::new(BufWriter::new(file)));

        Ok(Self {
            journal_file: journal_file.to_string(),
            writer,
            config,
        })
    }

    /// Registra una entrada en el journal
    pub fn log(&self, entry: JournalEntry) -> Result<()> {
        debug!("📝 Registrando entrada en journal: {:?}", entry);

        // Serializar entrada como JSON
        let json = serde_json::to_string(&entry)?;
        
        // Escribir al archivo
        {
            let mut writer = self.writer.lock().unwrap();
            writeln!(writer, "{}", json)?;
            writer.flush()?;
        }

        // Verificar tamaño del archivo
        self.check_file_size()?;

        Ok(())
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

    /// Lee entradas del journal
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
                    warn!("⚠️  Error parseando entrada del journal: {}", e);
                }
            }
        }

        // Ordenar por timestamp (más recientes primero)
        entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

        Ok(entries)
    }

    /// Busca entradas en el journal
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
            info!("🔄 Rotando archivo del journal (tamaño: {} bytes)", metadata.len());
            self.rotate_journal()?;
        }

        Ok(())
    }

    /// Rota el archivo del journal
    fn rotate_journal(&self) -> Result<()> {
        // Cerrar el writer actual
        drop(self.writer.lock().unwrap());

        // Renombrar archivo actual
        let rotated_file = format!("{}.1", self.journal_file);
        std::fs::rename(&self.journal_file, &rotated_file)?;

        // Crear nuevo archivo
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(&self.journal_file)?;

        // Actualizar writer
        *self.writer.lock().unwrap() = BufWriter::new(file);

        // Limpiar archivos antiguos
        self.cleanup_old_files()?;

        info!("✅ Journal rotado correctamente");
        Ok(())
    }

    /// Limpia archivos antiguos del journal
    fn cleanup_old_files(&self) -> Result<()> {
        let journal_dir = Path::new(&self.journal_file).parent().unwrap();
        let journal_name = Path::new(&self.journal_file).file_name().unwrap();

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
                    warn!("⚠️  Error eliminando archivo antiguo {:?}: {}", path, e);
                }
            }
        }

        Ok(())
    }

    /// Sincroniza el journal
    pub fn sync(&self) -> Result<()> {
        self.writer.lock().unwrap().flush()?;
        Ok(())
    }
}

/// Estadísticas del journal
#[derive(Debug, Clone)]
pub struct JournalStats {
    pub total_entries: usize,
    pub service_counts: HashMap<String, usize>,
    pub priority_counts: HashMap<Priority, usize>,
    pub oldest_entry: Option<DateTime<Utc>>,
    pub newest_entry: Option<DateTime<Utc>>,
}

impl JournalStats {
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
