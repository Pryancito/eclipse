//! Definición de errores para EclipseFS

#[cfg(feature = "std")]
use std::fmt;

#[cfg(not(feature = "std"))]
use core::fmt;

/// Resultado de operaciones EclipseFS
pub type EclipseFSResult<T> = Result<T, EclipseFSError>;

/// Errores específicos de EclipseFS
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EclipseFSError {
    /// Formato de archivo inválido
    InvalidFormat,
    /// Versión no soportada
    UnsupportedVersion,
    /// Archivo no encontrado
    NotFound,
    /// Entrada duplicada
    DuplicateEntry,
    /// Operación inválida
    InvalidOperation,
    /// Operación no soportada
    UnsupportedOperation,
    /// Error de I/O
    IoError,
    /// Permisos insuficientes
    PermissionDenied,
    /// Dispositivo lleno
    DeviceFull,
    /// Archivo demasiado grande
    FileTooLarge,
    /// Nombre de archivo inválido
    InvalidFileName,
    /// Sistema de archivos corrupto
    CorruptedFilesystem,
    /// Error de memoria
    OutOfMemory,
    /// Error de cifrado
    EncryptionError,
    /// Error de compresión
    CompressionError,
    /// Error de snapshot
    SnapshotError,
    /// Error de ACL
    AclError,
}

impl From<&str> for EclipseFSError {
    fn from(msg: &str) -> Self {
        // Mapear strings a errores específicos
        match msg {
            msg if msg.contains("deshabilitada") => EclipseFSError::InvalidOperation,
            msg if msg.contains("no encontrada") => EclipseFSError::NotFound,
            msg if msg.contains("no es para firmas") => EclipseFSError::InvalidOperation,
            msg if msg.contains("no coincide") => EclipseFSError::InvalidOperation,
            msg if msg.contains("deshabilitada") => EclipseFSError::InvalidOperation,
            _ => EclipseFSError::InvalidOperation,
        }
    }
}

impl fmt::Display for EclipseFSError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EclipseFSError::InvalidFormat => write!(f, "Formato de archivo inválido"),
            EclipseFSError::UnsupportedVersion => write!(f, "Versión no soportada"),
            EclipseFSError::NotFound => write!(f, "Archivo no encontrado"),
            EclipseFSError::DuplicateEntry => write!(f, "Entrada duplicada"),
            EclipseFSError::InvalidOperation => write!(f, "Operación inválida"),
            EclipseFSError::UnsupportedOperation => write!(f, "Operación no soportada"),
            EclipseFSError::IoError => write!(f, "Error de I/O"),
            EclipseFSError::PermissionDenied => write!(f, "Permisos insuficientes"),
            EclipseFSError::DeviceFull => write!(f, "Dispositivo lleno"),
            EclipseFSError::FileTooLarge => write!(f, "Archivo demasiado grande"),
            EclipseFSError::InvalidFileName => write!(f, "Nombre de archivo inválido"),
            EclipseFSError::CorruptedFilesystem => write!(f, "Sistema de archivos corrupto"),
            EclipseFSError::OutOfMemory => write!(f, "Error de memoria"),
            EclipseFSError::EncryptionError => write!(f, "Error de cifrado"),
            EclipseFSError::CompressionError => write!(f, "Error de compresión"),
            EclipseFSError::SnapshotError => write!(f, "Error de snapshot"),
            EclipseFSError::AclError => write!(f, "Error de ACL"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for EclipseFSError {}

#[cfg(feature = "std")]
impl From<std::io::Error> for EclipseFSError {
    fn from(_: std::io::Error) -> Self {
        EclipseFSError::IoError
    }
}
