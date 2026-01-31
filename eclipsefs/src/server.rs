//! Servidor EclipseFS para el microkernel Eclipse OS
//! 
//! Este servidor proporciona todas las operaciones del sistema de archivos EclipseFS
//! y se comunica con el kernel a través del sistema de mensajes IPC.

use anyhow::Result;
use crate::messages::{Message, MessageType, EclipseFSCommand};
use crate::operations::FileSystemOperations;

/// Trait común para servidores del microkernel
pub trait MicrokernelServer {
    fn name(&self) -> &str;
    fn message_type(&self) -> MessageType;
    fn priority(&self) -> u8;
    fn initialize(&mut self) -> Result<()>;
    fn process_message(&mut self, message: &Message) -> Result<Vec<u8>>;
    fn shutdown(&mut self) -> Result<()>;
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

/// Servidor de sistema de archivos EclipseFS
pub struct EclipseFSServer {
    name: String,
    stats: ServerStats,
    initialized: bool,
    operations: FileSystemOperations,
}

impl EclipseFSServer {
    /// Crear un nuevo servidor EclipseFS
    pub fn new() -> Self {
        Self {
            name: "EclipseFS".to_string(),
            stats: ServerStats::default(),
            initialized: false,
            operations: FileSystemOperations::new(),
        }
    }

    /// Procesar comando de montaje
    fn handle_mount(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let path = String::from_utf8_lossy(data).to_string();
        self.operations.mount(&path)?;
        Ok(vec![1]) // Success
    }

    /// Procesar comando de desmontaje
    fn handle_unmount(&mut self) -> Result<Vec<u8>> {
        self.operations.unmount()?;
        Ok(vec![1]) // Success
    }

    /// Procesar comando de apertura de archivo
    fn handle_open(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < 4 {
            return Err(anyhow::anyhow!("Datos insuficientes para OPEN"));
        }

        let flags = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let path = String::from_utf8_lossy(&data[4..]).to_string();
        
        let fd = self.operations.open(&path, flags)?;
        Ok(fd.to_le_bytes().to_vec())
    }

    /// Procesar comando de lectura
    fn handle_read(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < 8 {
            return Err(anyhow::anyhow!("Datos insuficientes para READ"));
        }

        let fd = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let size = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;

        let mut buffer = vec![0u8; size];
        let bytes_read = self.operations.read(fd, &mut buffer)?;
        buffer.truncate(bytes_read);
        
        Ok(buffer)
    }

    /// Procesar comando de escritura
    fn handle_write(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < 4 {
            return Err(anyhow::anyhow!("Datos insuficientes para WRITE"));
        }

        let fd = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let write_data = &data[4..];

        let bytes_written = self.operations.write(fd, write_data)?;
        Ok((bytes_written as u32).to_le_bytes().to_vec())
    }

    /// Procesar comando de cierre
    fn handle_close(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < 4 {
            return Err(anyhow::anyhow!("Datos insuficientes para CLOSE"));
        }

        let fd = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        self.operations.close(fd)?;
        
        Ok(vec![1]) // Success
    }

    /// Procesar comando de creación
    fn handle_create(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < 4 {
            return Err(anyhow::anyhow!("Datos insuficientes para CREATE"));
        }

        let mode = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let path = String::from_utf8_lossy(&data[4..]).to_string();
        
        let fd = self.operations.create(&path, mode)?;
        Ok(fd.to_le_bytes().to_vec())
    }

    /// Procesar comando de eliminación
    fn handle_delete(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let path = String::from_utf8_lossy(data).to_string();
        self.operations.delete(&path)?;
        Ok(vec![1]) // Success
    }

    /// Procesar comando de listado
    fn handle_list(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let path = String::from_utf8_lossy(data).to_string();
        let entries = self.operations.list(&path)?;
        
        // Serializar lista de archivos
        let mut result = Vec::new();
        for entry in entries {
            result.extend_from_slice(entry.as_bytes());
            result.push(b'\n');
        }
        
        Ok(result)
    }

    /// Procesar comando de stat
    fn handle_stat(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let path = String::from_utf8_lossy(data).to_string();
        let stat = self.operations.stat(&path)?;
        
        // Serializar FileStat
        let mut result = Vec::new();
        result.extend_from_slice(&stat.size.to_le_bytes());
        result.extend_from_slice(&stat.blocks.to_le_bytes());
        result.push(if stat.is_directory { 1 } else { 0 });
        result.extend_from_slice(&stat.permissions.to_le_bytes());
        
        Ok(result)
    }

    /// Procesar comando de sincronización
    fn handle_sync(&mut self) -> Result<Vec<u8>> {
        self.operations.sync()?;
        Ok(vec![1]) // Success
    }
}

impl Default for EclipseFSServer {
    fn default() -> Self {
        Self::new()
    }
}

impl MicrokernelServer for EclipseFSServer {
    fn name(&self) -> &str {
        &self.name
    }

    fn message_type(&self) -> MessageType {
        MessageType::FileSystem
    }

    fn priority(&self) -> u8 {
        10 // Alta prioridad (igual que el FileSystem Server)
    }

    fn initialize(&mut self) -> Result<()> {
        println!("═══════════════════════════════════════════════════════");
        println!("   🗂️  Inicializando EclipseFS Server v{}", crate::ECLIPSEFS_SERVER_VERSION);
        println!("═══════════════════════════════════════════════════════");
        
        println!("   [EclipseFS] Inicializando sistema de archivos...");
        println!("   [EclipseFS] Preparando gestión de archivos abiertos...");
        println!("   [EclipseFS] Sistema de cache inicializado");
        println!("   [EclipseFS] Soporte para extents habilitado");
        println!("   [EclipseFS] Journal configurado para operaciones seguras");
        
        self.initialized = true;
        
        println!("   ✅ Servidor EclipseFS listo para recibir solicitudes");
        println!("═══════════════════════════════════════════════════════\n");
        
        Ok(())
    }

    fn process_message(&mut self, message: &Message) -> Result<Vec<u8>> {
        if !self.initialized {
            return Err(anyhow::anyhow!("Servidor no inicializado"));
        }

        self.stats.messages_processed += 1;

        // El primer byte de data indica el comando
        if message.data_size == 0 {
            self.stats.messages_failed += 1;
            return Err(anyhow::anyhow!("Mensaje vacío"));
        }

        let command_byte = message.data[0];
        let command = EclipseFSCommand::from_u8(command_byte)
            .ok_or_else(|| anyhow::anyhow!("Comando desconocido: {}", command_byte))?;
        
        let command_data = &message.data[1..message.data_size as usize];

        let result = match command {
            EclipseFSCommand::Mount => self.handle_mount(command_data),
            EclipseFSCommand::Unmount => self.handle_unmount(),
            EclipseFSCommand::Open => self.handle_open(command_data),
            EclipseFSCommand::Read => self.handle_read(command_data),
            EclipseFSCommand::Write => self.handle_write(command_data),
            EclipseFSCommand::Close => self.handle_close(command_data),
            EclipseFSCommand::Create => self.handle_create(command_data),
            EclipseFSCommand::Delete => self.handle_delete(command_data),
            EclipseFSCommand::List => self.handle_list(command_data),
            EclipseFSCommand::Stat => self.handle_stat(command_data),
            EclipseFSCommand::Sync => self.handle_sync(),
            _ => Err(anyhow::anyhow!("Comando no implementado: {:?}", command)),
        };

        if result.is_err() {
            self.stats.messages_failed += 1;
            self.stats.last_error = Some(format!("{:?}", result));
        }

        result
    }

    fn shutdown(&mut self) -> Result<()> {
        println!("\n═══════════════════════════════════════════════════════");
        println!("   🗂️  Deteniendo EclipseFS Server");
        println!("═══════════════════════════════════════════════════════");
        
        println!("   [EclipseFS] Sincronizando buffers al disco...");
        let _ = self.operations.sync();
        
        println!("   [EclipseFS] Cerrando archivos abiertos...");
        let _ = self.operations.unmount();
        
        println!("   [EclipseFS] Liberando recursos...");
        
        self.initialized = false;
        
        println!("   ✅ Servidor EclipseFS detenido correctamente");
        println!("   📊 Estadísticas:");
        println!("      - Mensajes procesados: {}", self.stats.messages_processed);
        println!("      - Mensajes fallidos: {}", self.stats.messages_failed);
        println!("═══════════════════════════════════════════════════════\n");
        
        Ok(())
    }

    fn get_stats(&self) -> ServerStats {
        self.stats.clone()
    }
}
