//! Definiciones de mensajes para el servidor EclipseFS

/// Tipos de mensaje del microkernel (debe coincidir con el kernel)
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    System = 0x00000001,
    Memory = 0x00000002,
    FileSystem = 0x00000004,
    Network = 0x00000008,
    Graphics = 0x00000010,
    Audio = 0x00000020,
    Input = 0x00000040,
    AI = 0x00000080,
    Security = 0x00000100,
    User = 0x00000200,
}

/// Mensaje del microkernel (estructura compatible con el kernel)
#[repr(C)]
#[derive(Debug, Clone)]
pub struct Message {
    pub id: u64,
    pub from: u32,
    pub to: u32,
    pub message_type: MessageType,
    pub data: [u8; 256],
    pub data_size: u32,
    pub priority: u8,
    pub flags: u8,
    pub reserved: [u8; 2],
}

/// Comandos específicos de EclipseFS
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EclipseFSCommand {
    /// Abrir archivo
    Open = 1,
    /// Leer datos de archivo
    Read = 2,
    /// Escribir datos a archivo
    Write = 3,
    /// Cerrar archivo
    Close = 4,
    /// Crear archivo nuevo
    Create = 5,
    /// Eliminar archivo
    Delete = 6,
    /// Listar directorio
    List = 7,
    /// Obtener información de archivo
    Stat = 8,
    /// Crear directorio
    Mkdir = 9,
    /// Eliminar directorio
    Rmdir = 10,
    /// Renombrar archivo/directorio
    Rename = 11,
    /// Cambiar permisos
    Chmod = 12,
    /// Sincronizar cambios al disco
    Sync = 13,
    /// Obtener información del filesystem
    StatFS = 14,
    /// Montar filesystem
    Mount = 15,
    /// Desmontar filesystem
    Unmount = 16,
}

impl EclipseFSCommand {
    /// Convertir desde u8
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Open),
            2 => Some(Self::Read),
            3 => Some(Self::Write),
            4 => Some(Self::Close),
            5 => Some(Self::Create),
            6 => Some(Self::Delete),
            7 => Some(Self::List),
            8 => Some(Self::Stat),
            9 => Some(Self::Mkdir),
            10 => Some(Self::Rmdir),
            11 => Some(Self::Rename),
            12 => Some(Self::Chmod),
            13 => Some(Self::Sync),
            14 => Some(Self::StatFS),
            15 => Some(Self::Mount),
            16 => Some(Self::Unmount),
            _ => None,
        }
    }
}
