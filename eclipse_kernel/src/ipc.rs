//! Sistema IPC (Inter-Process Communication) para Eclipse OS
//!
//! Este módulo implementa la comunicación entre el kernel y el userland
//! para el sistema de drivers dinámicos.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

/// ID único para cada mensaje IPC
pub type IpcMessageId = u32;

/// Contador global para generar IDs únicos de mensajes
static NEXT_MESSAGE_ID: AtomicU32 = AtomicU32::new(1);

/// Tipos de mensajes IPC entre kernel y userland
#[derive(Debug, Clone)]
pub enum IpcMessage {
    /// Solicitud de inicialización de módulo
    InitModule {
        module_type: ModuleType,
        name: String,
        config: ModuleConfig,
    },
    /// Respuesta de inicialización
    InitResponse {
        success: bool,
        error: Option<String>,
        module_id: Option<u32>,
    },
    /// Comando para el módulo
    Command {
        module_id: u32,
        command: String,
        args: Vec<String>,
    },
    /// Respuesta del comando
    CommandResponse {
        module_id: u32,
        success: bool,
        result: Option<String>,
    },
    /// Cargar driver dinámicamente
    LoadDriver {
        driver_type: DriverType,
        driver_name: String,
        driver_data: Vec<u8>,
        config: DriverConfig,
    },
    /// Respuesta de carga de driver
    LoadDriverResponse {
        success: bool,
        driver_id: Option<u32>,
        error: Option<String>,
    },
    /// Comando específico para driver
    DriverCommand {
        driver_id: u32,
        command: DriverCommandType,
        args: Vec<u8>,
    },
    /// Respuesta de comando de driver
    DriverCommandResponse {
        driver_id: u32,
        success: bool,
        result: Option<Vec<u8>>,
        error: Option<String>,
    },
    /// Desregistrar driver
    UnloadDriver {
        driver_id: u32,
    },
    /// Listar drivers disponibles
    ListDrivers,
    /// Respuesta de lista de drivers
    ListDriversResponse {
        drivers: Vec<DriverInfo>,
    },
    /// Ping/Pong para mantener conexión
    Ping,
    Pong,
    /// Notificación de cierre
    Shutdown,
    /// Solicitud FS (VFS over IPC)
    FsRequest(FsRequest),
    /// Respuesta FS (VFS over IPC)
    FsResponse(FsResponse),
}

/// Tipos de módulos disponibles
#[derive(Debug, Clone)]
pub enum ModuleType {
    Graphics,
    Audio,
    Network,
    Storage,
    Driver(DriverType),
    Custom(String),
}

/// Tipos de drivers específicos
#[derive(Debug, Clone)]
pub enum DriverType {
    PCI,
    NVIDIA,
    AMD,
    Intel,
    USB,
    Network,
    Storage,
    Audio,
    Input,
    Custom(String),
}

/// Configuración de módulo
#[derive(Debug, Clone)]
pub struct ModuleConfig {
    pub name: String,
    pub module_type: ModuleType,
    pub priority: u8,
    pub auto_start: bool,
    pub memory_limit: u64,
    pub cpu_limit: f32,
}

/// Configuración de driver
#[derive(Debug, Clone)]
pub struct DriverConfig {
    pub name: String,
    pub version: String,
    pub author: String,
    pub description: String,
    pub priority: u8,
    pub auto_load: bool,
    pub memory_limit: u64,
    pub dependencies: Vec<String>,
    pub capabilities: Vec<DriverCapability>,
}

/// Información de driver
#[derive(Debug, Clone)]
pub struct DriverInfo {
    pub id: u32,
    pub config: DriverConfig,
    pub status: DriverStatus,
    pub pid: Option<u32>,
    pub memory_usage: u64,
    pub uptime: u64,
}

/// Estado del driver
#[derive(Debug, Clone)]
pub enum DriverStatus {
    Unloaded,
    Loading,
    Loaded,
    Initializing,
    Ready,
    Error(String),
    Unloading,
}

/// Capacidades del driver
#[derive(Debug, Clone)]
pub enum DriverCapability {
    Graphics,
    Network,
    Storage,
    Audio,
    Input,
    Power,
    Security,
    Custom(String),
}

/// Tipos de comandos de driver
#[derive(Debug, Clone)]
pub enum DriverCommandType {
    Initialize,
    Shutdown,
    Suspend,
    Resume,
    GetStatus,
    GetCapabilities,
    ExecuteCommand { command: String },
    GetDeviceInfo { device_id: u32 },
    ScanDevices,
    EnableDevice { device_id: u32 },
    DisableDevice { device_id: u32 },
    Custom { command: String },
}

/// Manager IPC para comunicación entre kernel y userland
pub struct IpcManager {
    message_queue: Vec<(IpcMessageId, IpcMessage)>,
    response_queue: Vec<(IpcMessageId, IpcMessage)>,
    registered_drivers: BTreeMap<u32, DriverInfo>,
    next_driver_id: u32,
}

impl IpcManager {
    pub fn new() -> Self {
        Self {
            message_queue: Vec::new(),
            response_queue: Vec::new(),
            registered_drivers: BTreeMap::new(),
            next_driver_id: 1,
        }
    }

    /// Enviar mensaje al userland
    pub fn send_message(&mut self, message: IpcMessage) -> IpcMessageId {
        let message_id = NEXT_MESSAGE_ID.fetch_add(1, Ordering::SeqCst);
        self.message_queue.push((message_id, message));
        message_id
    }

    /// Obtener mensaje del userland
    pub fn receive_message(&mut self) -> Option<(IpcMessageId, IpcMessage)> {
        self.message_queue.pop()
    }

    /// Enviar respuesta al userland
    pub fn send_response(&mut self, message_id: IpcMessageId, response: IpcMessage) {
        self.response_queue.push((message_id, response));
    }

    /// Obtener respuesta para el userland
    pub fn get_response(&mut self) -> Option<(IpcMessageId, IpcMessage)> {
        self.response_queue.pop()
    }

    /// Procesar mensaje IPC
    pub fn process_message(&mut self, message_id: IpcMessageId, message: IpcMessage) -> IpcMessage {
        match message {
            IpcMessage::LoadDriver {
                driver_type,
                driver_name,
                driver_data: _,
                config,
            } => {
                // Registrar driver en el kernel
                let driver_id = self.next_driver_id;
                self.next_driver_id += 1;

                let driver_info = DriverInfo {
                    id: driver_id,
                    config: config.clone(),
                    status: DriverStatus::Loading,
                    pid: None,
                    memory_usage: 0,
                    uptime: 0,
                };

                self.registered_drivers.insert(driver_id, driver_info);

                // Driver registrado via IPC

                IpcMessage::LoadDriverResponse {
                    success: true,
                    driver_id: Some(driver_id),
                    error: None,
                }
            }
            IpcMessage::DriverCommand {
                driver_id,
                command,
                args,
            } => {
                if let Some(driver_info) = self.registered_drivers.get_mut(&driver_id) {
                    match command {
                        DriverCommandType::Initialize => {
                            driver_info.status = DriverStatus::Initializing;
                            // Aquí se inicializaría el driver real
                            driver_info.status = DriverStatus::Ready;
                            IpcMessage::DriverCommandResponse {
                                driver_id,
                                success: true,
                                result: Some(b"Driver inicializado".to_vec()),
                                error: None,
                            }
                        }
                        DriverCommandType::GetStatus => {
                            let status_str = format!("{:?}", driver_info.status);
                            IpcMessage::DriverCommandResponse {
                                driver_id,
                                success: true,
                                result: Some(status_str.into_bytes()),
                                error: None,
                            }
                        }
                        DriverCommandType::GetCapabilities => {
                            let caps: Vec<String> = driver_info
                                .config
                                .capabilities
                                .iter()
                                .map(|c| format!("{:?}", c))
                                .collect();
                            let caps_str = caps.join(",");
                            IpcMessage::DriverCommandResponse {
                                driver_id,
                                success: true,
                                result: Some(caps_str.into_bytes()),
                                error: None,
                            }
                        }
                        DriverCommandType::ExecuteCommand { command: cmd } => {
                            // Ejecutar comando específico del driver
                            let result = match cmd.as_str() {
                                "get_gpu_count" => {
                                    // Simular conteo de GPUs
                                    2u32.to_le_bytes().to_vec()
                                }
                                "get_memory_info" => {
                                    // Simular información de memoria
                                    b"8GB VRAM detectada".to_vec()
                                }
                                _ => b"Comando ejecutado".to_vec(),
                            };
                            IpcMessage::DriverCommandResponse {
                                driver_id,
                                success: true,
                                result: Some(result),
                                error: None,
                            }
                        }
                        _ => IpcMessage::DriverCommandResponse {
                            driver_id,
                            success: false,
                            result: None,
                            error: Some("Comando no implementado".to_string()),
                        },
                    }
                } else {
                    IpcMessage::DriverCommandResponse {
                        driver_id,
                        success: false,
                        result: None,
                        error: Some("Driver no encontrado".to_string()),
                    }
                }
            }
            IpcMessage::ListDrivers => {
                let drivers: Vec<DriverInfo> = self.registered_drivers.values().cloned().collect();
                IpcMessage::ListDriversResponse { drivers }
            }
            IpcMessage::UnloadDriver { driver_id } => {
                if self.registered_drivers.remove(&driver_id).is_some() {
                    // Driver desregistrado via IPC
                    IpcMessage::LoadDriverResponse {
                        success: true,
                        driver_id: Some(driver_id),
                        error: None,
                    }
                } else {
                    IpcMessage::LoadDriverResponse {
                        success: false,
                        driver_id: None,
                        error: Some("Driver no encontrado".to_string()),
                    }
                }
            }
            IpcMessage::Ping => IpcMessage::Pong,
            IpcMessage::FsRequest(_req) => {
                // En esta capa solo enrutaríamos hacia el servidor adecuado.
                // La lógica real de VFS/FS userland manejará estas solicitudes.
                FsResponse::err_simple(_req.request_id, FsError::ENOTSUP)
            }
            _ => IpcMessage::CommandResponse {
                module_id: 0,
                success: false,
                result: Some("Mensaje no soportado".to_string()),
            },
        }
    }

    /// Obtener información de driver
    pub fn get_driver_info(&self, driver_id: u32) -> Option<&DriverInfo> {
        self.registered_drivers.get(&driver_id)
    }

    /// Listar todos los drivers
    pub fn list_drivers(&self) -> Vec<&DriverInfo> {
        self.registered_drivers.values().collect()
    }

    /// Procesar cola de mensajes
    pub fn process_message_queue(&mut self) {
        while let Some((message_id, message)) = self.receive_message() {
            let response = self.process_message(message_id, message);
            self.send_response(message_id, response);
        }
    }
}

impl Default for IpcManager {
    fn default() -> Self {
        Self::new()
    }
}

// ==========================
//  VFS over IPC: Tipos base
// ==========================

/// Operaciones de FS soportadas por IPC
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsOp {
    Open,
    Read,
    Write,
    Close,
    Stat,
    Readdir,
    Lseek,
    Mkdir,
    Unlink,
    Rmdir,
    Rename,
    MountHandshake,
}

/// Códigos de error de FS
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    OK,
    ENOENT,
    EACCES,
    EINVAL,
    EISDIR,
    ENOTDIR,
    EROFS,
    EIO,
    ETIMEDOUT,
    ENOTSUP,
}

/// Estado de respuesta
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsStatus {
    OK,
    ERR,
}

/// Entrada de directorio (simplificada)
#[derive(Debug, Clone)]
pub struct FsDirEnt {
    pub name: String,
    pub inode: u64,
    pub is_dir: bool,
}

/// Stat de archivo (simplificado)
#[derive(Debug, Clone)]
pub struct FsStat {
    pub inode: u64,
    pub size: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

/// Solicitud FS por IPC
#[derive(Debug, Clone)]
pub struct FsRequest {
    pub request_id: IpcMessageId,
    pub proto_ver: u16,
    pub mount_id: u32,
    pub op: FsOp,
    pub path: Option<String>,
    pub new_path: Option<String>,
    pub fd_remote: Option<u32>,
    pub offset: u64,
    pub len: u32,
    pub flags: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub cookie: u64,
}

impl FsRequest {
    pub fn new(mount_id: u32, op: FsOp) -> Self {
        Self {
            request_id: NEXT_MESSAGE_ID.fetch_add(1, Ordering::SeqCst),
            proto_ver: 1,
            mount_id,
            op,
            path: None,
            new_path: None,
            fd_remote: None,
            offset: 0,
            len: 0,
            flags: 0,
            mode: 0,
            uid: 0,
            gid: 0,
            cookie: 0,
        }
    }
}

/// Respuesta FS por IPC
#[derive(Debug, Clone)]
pub struct FsResponse {
    pub request_id: IpcMessageId,
    pub status: FsStatus,
    pub error: Option<FsError>,
    pub bytes: Option<Vec<u8>>, // READ/READDIR payload (metadatos pueden ir serializados)
    pub fd_remote: Option<u32>, // OPEN
    pub written: Option<u32>,   // WRITE
    pub stat: Option<FsStat>,   // STAT
    pub entries: Option<Vec<FsDirEnt>>, // READDIR
    pub next_cookie: Option<u64>, // READDIR paginación
}

impl FsResponse {
    pub fn ok(request_id: IpcMessageId) -> Self {
        Self {
            request_id,
            status: FsStatus::OK,
            error: None,
            bytes: None,
            fd_remote: None,
            written: None,
            stat: None,
            entries: None,
            next_cookie: None,
        }
    }
    pub fn err(request_id: IpcMessageId, err: FsError) -> Self {
        Self {
            request_id,
            status: FsStatus::ERR,
            error: Some(err),
            bytes: None,
            fd_remote: None,
            written: None,
            stat: None,
            entries: None,
            next_cookie: None,
        }
    }
    pub fn err_simple(request_id: IpcMessageId, err: FsError) -> IpcMessage {
        IpcMessage::FsResponse(Self::err(request_id, err))
    }
}

/// Cliente IPC para FS userland
pub struct IpcFsClient<'a> {
    pub ipc: &'a mut IpcManager,
    pub mount_id: u32,
}

impl<'a> IpcFsClient<'a> {
    pub fn new(ipc: &'a mut IpcManager, mount_id: u32) -> Self {
        Self { ipc, mount_id }
    }

    /// Enviar solicitud y obtener respuesta (bloqueante simple)
    pub fn call(&mut self, mut req: FsRequest) -> Option<FsResponse> {
        req.request_id = NEXT_MESSAGE_ID.fetch_add(1, Ordering::SeqCst);
        let id = self.ipc.send_message(IpcMessage::FsRequest(req.clone()));

        // Bucle simple: en una implementación real, habría colas por ID y espera con timeout
        for _ in 0..1024 {
            if let Some((_rid, IpcMessage::FsResponse(resp))) = self.ipc.get_response() {
                if resp.request_id == id {
                    return Some(resp);
                }
            }
        }
        None
    }
}
