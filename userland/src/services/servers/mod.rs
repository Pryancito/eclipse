//! Servidores Microkernel para Eclipse OS
//! 
//! Este módulo implementa los servidores del sistema que se ejecutan en userspace
//! y se comunican con el kernel a través del sistema de mensajes del microkernel.

pub mod filesystem_server;
pub mod graphics_server;
pub mod network_server;
pub mod input_server;
pub mod audio_server;
pub mod ai_server;
pub mod security_server;

use anyhow::Result;

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

/// Trait común para todos los servidores del microkernel
pub trait MicrokernelServer {
    /// Obtener el nombre del servidor
    fn name(&self) -> &str;
    
    /// Obtener el tipo de mensaje que maneja este servidor
    fn message_type(&self) -> MessageType;
    
    /// Obtener la prioridad del servidor
    fn priority(&self) -> u8;
    
    /// Inicializar el servidor
    fn initialize(&mut self) -> Result<()>;
    
    /// Procesar un mensaje
    fn process_message(&mut self, message: &Message) -> Result<Vec<u8>>;
    
    /// Detener el servidor
    fn shutdown(&mut self) -> Result<()>;
    
    /// Obtener estadísticas del servidor
    fn get_stats(&self) -> ServerStats;
}

/// Estadísticas de un servidor
#[derive(Debug, Clone)]
pub struct ServerStats {
    pub messages_processed: u64,
    pub messages_failed: u64,
    pub uptime_seconds: u64,
    pub last_error: Option<String>,
}

impl Default for ServerStats {
    fn default() -> Self {
        Self {
            messages_processed: 0,
            messages_failed: 0,
            uptime_seconds: 0,
            last_error: None,
        }
    }
}

/// Gestor de servidores del microkernel
pub struct MicrokernelServerManager {
    servers: Vec<Box<dyn MicrokernelServer>>,
    running: bool,
}

impl MicrokernelServerManager {
    /// Crear un nuevo gestor de servidores
    pub fn new() -> Self {
        Self {
            servers: Vec::new(),
            running: false,
        }
    }
    
    /// Registrar un servidor
    pub fn register_server(&mut self, server: Box<dyn MicrokernelServer>) -> Result<()> {
        println!("   ✓ Registrando servidor: {}", server.name());
        self.servers.push(server);
        Ok(())
    }
    
    /// Inicializar todos los servidores
    pub fn initialize_all(&mut self) -> Result<()> {
        println!("Inicializando servidores del microkernel...");
        for server in &mut self.servers {
            server.initialize()?;
            println!("   ✓ Servidor '{}' inicializado", server.name());
        }
        self.running = true;
        println!("✅ Todos los servidores del microkernel inicializados");
        Ok(())
    }
    
    /// Detener todos los servidores
    pub fn shutdown_all(&mut self) -> Result<()> {
        println!("Deteniendo servidores del microkernel...");
        for server in &mut self.servers {
            server.shutdown()?;
            println!("   ✓ Servidor '{}' detenido", server.name());
        }
        self.running = false;
        Ok(())
    }
    
    /// Procesar un mensaje en el servidor apropiado
    pub fn route_message(&mut self, message: &Message) -> Result<Vec<u8>> {
        for server in &mut self.servers {
            if server.message_type() == message.message_type {
                return server.process_message(message);
            }
        }
        Err(anyhow::anyhow!("No se encontró servidor para tipo de mensaje {:?}", message.message_type))
    }
    
    /// Obtener estadísticas de todos los servidores
    pub fn get_all_stats(&self) -> Vec<(String, ServerStats)> {
        self.servers.iter()
            .map(|s| (s.name().to_string(), s.get_stats()))
            .collect()
    }
}

impl Default for MicrokernelServerManager {
    fn default() -> Self {
        Self::new()
    }
}
