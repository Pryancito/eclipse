use serde::{Deserialize, Serialize};

/// Tipos de mensajes IPC entre kernel y userland
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcMessage {
    /// Solicitud de inicialización de módulo
    InitModule { 
        module_type: ModuleType, 
        name: String,
        config: ModuleConfig 
    },
    /// Respuesta de inicialización
    InitResponse { 
        success: bool, 
        error: Option<String>,
        module_id: Option<u32>
    },
    /// Comando para el módulo
    Command { 
        module_id: u32,
        command: String, 
        args: Vec<String> 
    },
    /// Respuesta del comando
    CommandResponse { 
        module_id: u32,
        success: bool, 
        result: Option<String> 
    },
    /// Datos de gráficos
    GraphicsData {
        module_id: u32,
        operation: GraphicsOperation,
        data: Vec<u8>
    },
    /// Datos de audio
    AudioData {
        module_id: u32,
        operation: AudioOperation,
        data: Vec<u8>
    },
    /// Datos de red
    NetworkData {
        module_id: u32,
        operation: NetworkOperation,
        data: Vec<u8>
    },
    /// Ping/Pong para mantener conexión
    Ping,
    Pong,
    /// Notificación de cierre
    Shutdown,
}

/// Tipos de módulos disponibles
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ModuleType {
    Graphics,
    Audio,
    Network,
    Storage,
    Custom(String),
}

/// Configuración de módulo
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleConfig {
    pub name: String,
    pub module_type: ModuleType,
    pub priority: u8,
    pub auto_start: bool,
    pub memory_limit: u64,
    pub cpu_limit: f32,
}

/// Estado del módulo
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ModuleStatus {
    Stopped,
    Starting,
    Running,
    Stopping,
    Error(String),
}

/// Información del módulo
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleInfo {
    pub id: u32,
    pub config: ModuleConfig,
    pub status: ModuleStatus,
    pub pid: Option<u32>,
    pub memory_usage: u64,
    pub cpu_usage: f32,
    pub uptime: u64,
}

/// Operaciones gráficas
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GraphicsOperation {
    SetMode { width: u32, height: u32, bpp: u8 },
    DrawPixel { x: u32, y: u32, color: u32 },
    DrawLine { x1: u32, y1: u32, x2: u32, y2: u32, color: u32 },
    DrawRect { x: u32, y: u32, width: u32, height: u32, color: u32 },
    DrawText { x: u32, y: u32, text: String, color: u32 },
    ClearScreen { color: u32 },
    SwapBuffers,
}

/// Operaciones de audio
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AudioOperation {
    SetSampleRate { rate: u32 },
    SetChannels { channels: u8 },
    PlayBuffer { data: Vec<u8> },
    Stop,
    Pause,
    Resume,
}

/// Operaciones de red
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NetworkOperation {
    Connect { host: String, port: u16 },
    Disconnect,
    Send { data: Vec<u8> },
    Receive,
    Listen { port: u16 },
    Accept,
}

/// Trait para serialización de mensajes IPC
pub trait IpcSerializable: Serialize + for<'de> Deserialize<'de> {
    fn serialize(&self) -> Result<Vec<u8>, bincode::Error> {
        bincode::serialize(self)
    }
    
    fn deserialize(data: &[u8]) -> Result<Self, bincode::Error> {
        bincode::deserialize(data)
    }
}

impl IpcSerializable for IpcMessage {}



