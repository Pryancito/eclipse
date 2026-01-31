//! Servidor de Sistema de Archivos en Userspace
//! 
//! Implementa el servidor de archivos que maneja todas las operaciones de I/O de archivos
//! desde el espacio de usuario, comunicándose con el kernel vía IPC.

use super::{Message, MessageType, MicrokernelServer, ServerStats};
use anyhow::Result;

/// Comandos de sistema de archivos
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSystemCommand {
    Open = 1,
    Read = 2,
    Write = 3,
    Close = 4,
    Delete = 5,
    Create = 6,
    List = 7,
    Stat = 8,
}

/// Servidor de sistema de archivos
pub struct FileSystemServer {
    name: String,
    stats: ServerStats,
    initialized: bool,
}

impl FileSystemServer {
    /// Crear un nuevo servidor de sistema de archivos
    pub fn new() -> Self {
        Self {
            name: "FileSystem".to_string(),
            stats: ServerStats::default(),
            initialized: false,
        }
    }
    
    /// Procesar comando de apertura de archivo
    fn handle_open(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        // Extraer nombre de archivo de los datos
        let filename = String::from_utf8_lossy(data);
        println!("   [FS] Abriendo archivo: {}", filename);
        
        // Simular apertura exitosa - retornar file descriptor
        let fd: u32 = 42;
        Ok(fd.to_le_bytes().to_vec())
    }
    
    /// Procesar comando de lectura de archivo
    fn handle_read(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < 8 {
            return Err(anyhow::anyhow!("Datos insuficientes para READ"));
        }
        
        let fd = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let size = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        
        println!("   [FS] Leyendo {} bytes del FD {}", size, fd);
        
        // Simular lectura - retornar datos de ejemplo
        let mut result = Vec::new();
        result.extend_from_slice(b"Hello from FileSystem Server!");
        Ok(result)
    }
    
    /// Procesar comando de escritura de archivo
    fn handle_write(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < 4 {
            return Err(anyhow::anyhow!("Datos insuficientes para WRITE"));
        }
        
        let fd = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let write_data = &data[4..];
        
        println!("   [FS] Escribiendo {} bytes al FD {}", write_data.len(), fd);
        
        // Simular escritura exitosa - retornar bytes escritos
        let bytes_written = write_data.len() as u32;
        Ok(bytes_written.to_le_bytes().to_vec())
    }
    
    /// Procesar comando de cierre de archivo
    fn handle_close(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < 4 {
            return Err(anyhow::anyhow!("Datos insuficientes para CLOSE"));
        }
        
        let fd = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        println!("   [FS] Cerrando FD {}", fd);
        
        // Simular cierre exitoso
        Ok(vec![1])
    }
    
    /// Procesar comando de listado de directorio
    fn handle_list(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let path = String::from_utf8_lossy(data);
        println!("   [FS] Listando directorio: {}", path);
        
        // Simular listado de archivos
        let listing = "file1.txt\nfile2.txt\ndir1/\n";
        Ok(listing.as_bytes().to_vec())
    }
}

impl Default for FileSystemServer {
    fn default() -> Self {
        Self::new()
    }
}

impl MicrokernelServer for FileSystemServer {
    fn name(&self) -> &str {
        &self.name
    }
    
    fn message_type(&self) -> MessageType {
        MessageType::FileSystem
    }
    
    fn priority(&self) -> u8 {
        10 // Alta prioridad
    }
    
    fn initialize(&mut self) -> Result<()> {
        println!("   [FS] Inicializando servidor de sistema de archivos...");
        
        // Inicializar sistemas de archivos
        println!("   [FS] Montando sistemas de archivos...");
        println!("   [FS] EclipseFS montado en /");
        println!("   [FS] FAT32 montado en /boot");
        
        self.initialized = true;
        println!("   [FS] Servidor de sistema de archivos listo");
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
        
        let command = message.data[0];
        let command_data = &message.data[1..message.data_size as usize];
        
        let result = match command {
            1 => self.handle_open(command_data),
            2 => self.handle_read(command_data),
            3 => self.handle_write(command_data),
            4 => self.handle_close(command_data),
            7 => self.handle_list(command_data),
            _ => {
                self.stats.messages_failed += 1;
                Err(anyhow::anyhow!("Comando desconocido: {}", command))
            }
        };
        
        if result.is_err() {
            self.stats.messages_failed += 1;
            self.stats.last_error = Some(format!("{:?}", result));
        }
        
        result
    }
    
    fn shutdown(&mut self) -> Result<()> {
        println!("   [FS] Desmontando sistemas de archivos...");
        println!("   [FS] Sincronizando buffers...");
        self.initialized = false;
        println!("   [FS] Servidor de sistema de archivos detenido");
        Ok(())
    }
    
    fn get_stats(&self) -> ServerStats {
        self.stats.clone()
    }
}
