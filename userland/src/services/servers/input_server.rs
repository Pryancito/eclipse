//! Servidor de Entrada en Userspace
//! 
//! Implementa el servidor de entrada que maneja todos los eventos de teclado, mouse
//! y otros dispositivos de entrada.

use super::{Message, MessageType, MicrokernelServer, ServerStats};
use anyhow::Result;

/// Servidor de entrada
pub struct InputServer {
    name: String,
    stats: ServerStats,
    initialized: bool,
}

impl InputServer {
    /// Crear un nuevo servidor de entrada
    pub fn new() -> Self {
        Self {
            name: "Input".to_string(),
            stats: ServerStats::default(),
            initialized: false,
        }
    }
    
    /// Procesar evento de teclado
    fn handle_keyboard_event(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < 4 {
            return Err(anyhow::anyhow!("Datos insuficientes para evento de teclado"));
        }
        
        let key_code = data[0];
        let pressed = data[1] != 0;
        let modifiers = data[2];
        
        println!("   [INPUT] Evento de teclado: código={}, presionado={}, modificadores=0x{:02X}", 
                 key_code, pressed, modifiers);
        
        Ok(vec![1])
    }
    
    /// Procesar evento de mouse
    fn handle_mouse_event(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < 9 {
            return Err(anyhow::anyhow!("Datos insuficientes para evento de mouse"));
        }
        
        let x = i32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let y = i32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let buttons = data[8];
        
        // Simular procesamiento de evento de mouse
        Ok(vec![1])
    }
    
    /// Obtener estado del teclado
    fn handle_get_keyboard_state(&mut self, _data: &[u8]) -> Result<Vec<u8>> {
        println!("   [INPUT] Obteniendo estado del teclado");
        
        // Simular estado del teclado (256 bytes, uno por tecla)
        let state = vec![0u8; 256];
        Ok(state)
    }
    
    /// Obtener estado del mouse
    fn handle_get_mouse_state(&mut self, _data: &[u8]) -> Result<Vec<u8>> {
        println!("   [INPUT] Obteniendo estado del mouse");
        
        // Simular estado del mouse: x (4 bytes), y (4 bytes), botones (1 byte)
        let mut state = Vec::new();
        state.extend_from_slice(&0i32.to_le_bytes()); // x
        state.extend_from_slice(&0i32.to_le_bytes()); // y
        state.push(0); // botones
        Ok(state)
    }
}

impl Default for InputServer {
    fn default() -> Self {
        Self::new()
    }
}

impl MicrokernelServer for InputServer {
    fn name(&self) -> &str {
        &self.name
    }
    
    fn message_type(&self) -> MessageType {
        MessageType::Input
    }
    
    fn priority(&self) -> u8 {
        9 // Alta prioridad
    }
    
    fn initialize(&mut self) -> Result<()> {
        println!("   [INPUT] Inicializando servidor de entrada...");
        println!("   [INPUT] Inicializando driver de teclado USB...");
        println!("   [INPUT] Inicializando driver de mouse USB...");
        println!("   [INPUT] Configurando eventos de entrada...");
        
        self.initialized = true;
        println!("   [INPUT] Servidor de entrada listo");
        Ok(())
    }
    
    fn process_message(&mut self, message: &Message) -> Result<Vec<u8>> {
        if !self.initialized {
            return Err(anyhow::anyhow!("Servidor no inicializado"));
        }
        
        self.stats.messages_processed += 1;
        
        if message.data_size == 0 {
            self.stats.messages_failed += 1;
            return Err(anyhow::anyhow!("Mensaje vacío"));
        }
        
        let command = message.data[0];
        let command_data = &message.data[1..message.data_size as usize];
        
        let result = match command {
            1 => self.handle_keyboard_event(command_data),
            2 => self.handle_mouse_event(command_data),
            3 => self.handle_get_keyboard_state(command_data),
            4 => self.handle_get_mouse_state(command_data),
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
        println!("   [INPUT] Deteniendo drivers de entrada...");
        println!("   [INPUT] Desconectando dispositivos de entrada...");
        self.initialized = false;
        println!("   [INPUT] Servidor de entrada detenido");
        Ok(())
    }
    
    fn get_stats(&self) -> ServerStats {
        self.stats.clone()
    }
}
