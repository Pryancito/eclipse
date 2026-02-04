//! Servidor de Gráficos en Userspace
//! 
//! Implementa el servidor de gráficos que maneja todas las operaciones de display,
//! renderizado y aceleración por hardware desde el espacio de usuario.
//!
//! **STATUS**: STUB IMPLEMENTATION
//! - Display initialization: STUB (no framebuffer access)
//! - Pixel/Rect/Line drawing: STUB (no actual rendering)
//! - Buffer swapping: STUB (no double buffering)
//! TODO: Integrate with kernel framebuffer or DRM/KMS
//! TODO: Implement actual rendering via framebuffer writes
//! TODO: Add hardware acceleration support

use super::{Message, MessageType, MicrokernelServer, ServerStats};
use anyhow::Result;

/// Syscall number for getting framebuffer info
const SYS_GET_FRAMEBUFFER_INFO: u64 = 15;
/// Syscall number for mapping framebuffer into virtual memory
const SYS_MAP_FRAMEBUFFER: u64 = 16;

/// Framebuffer information from kernel/bootloader
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    pub address: u64,      // Physical address of framebuffer
    pub width: u32,        // Width in pixels
    pub height: u32,       // Height in pixels
    pub pitch: u32,        // Bytes per scanline
    pub bpp: u16,          // Bits per pixel
    pub red_mask_size: u8,
    pub red_mask_shift: u8,
    pub green_mask_size: u8,
    pub green_mask_shift: u8,
    pub blue_mask_size: u8,
    pub blue_mask_shift: u8,
}

/// Get framebuffer info from kernel via syscall
fn get_framebuffer_info() -> Option<FramebufferInfo> {
    let ptr: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") SYS_GET_FRAMEBUFFER_INFO => ptr,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    
    if ptr == 0 {
        None
    } else {
        unsafe { Some(*(ptr as *const FramebufferInfo)) }
    }
}

/// Map framebuffer into process virtual memory
/// Returns virtual address where framebuffer is mapped, or None on failure
fn map_framebuffer() -> Option<*mut u8> {
    let addr: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") SYS_MAP_FRAMEBUFFER => addr,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    
    if addr == 0 {
        None
    } else {
        Some(addr as *mut u8)
    }
}

/// Comandos de gráficos
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GraphicsCommand {
    InitDisplay = 1,
    DrawPixel = 2,
    DrawRect = 3,
    DrawLine = 4,
    Clear = 5,
    Swap = 6,
}

impl TryFrom<u8> for GraphicsCommand {
    type Error = ();
    
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(GraphicsCommand::InitDisplay),
            2 => Ok(GraphicsCommand::DrawPixel),
            3 => Ok(GraphicsCommand::DrawRect),
            4 => Ok(GraphicsCommand::DrawLine),
            5 => Ok(GraphicsCommand::Clear),
            6 => Ok(GraphicsCommand::Swap),
            _ => Err(()),
        }
    }
}

/// Servidor de gráficos
pub struct GraphicsServer {
    name: String,
    stats: ServerStats,
    initialized: bool,
    width: u32,
    height: u32,
    framebuffer_info: Option<FramebufferInfo>,
    framebuffer_ptr: Option<*mut u8>,  // Mapped virtual address
}

impl GraphicsServer {
    /// Crear un nuevo servidor de gráficos
    pub fn new() -> Self {
        // Get framebuffer info from kernel
        let fb_info = get_framebuffer_info();
        
        let (width, height) = if let Some(ref info) = fb_info {
            (info.width, info.height)
        } else {
            (1920, 1080) // Default fallback
        };
        
        Self {
            name: "Graphics".to_string(),
            stats: ServerStats::default(),
            initialized: false,
            width,
            height,
            framebuffer_info: fb_info,
            framebuffer_ptr: None,
        }
    }
    
    /// Procesar comando de inicialización de display
    fn handle_init_display(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() >= 8 {
            self.width = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
            self.height = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        }
        
        println!("   [GFX] Inicializando display {}x{}", self.width, self.height);
        Ok(vec![1])
    }
    
    /// Procesar comando de dibujo de pixel
    fn handle_draw_pixel(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < 12 {
            return Err(anyhow::anyhow!("Datos insuficientes para DRAW_PIXEL"));
        }
        
        let x = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let y = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let color = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
        
        // TODO: Access actual framebuffer and draw pixel at (x, y) with color
        // For now, stub implementation (no actual rendering)
        Ok(vec![1])
    }
    
    /// Procesar comando de dibujo de rectángulo
    fn handle_draw_rect(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < 20 {
            return Err(anyhow::anyhow!("Datos insuficientes para DRAW_RECT"));
        }
        
        println!("   [GFX] Dibujando rectángulo");
        Ok(vec![1])
    }
    
    /// Procesar comando de dibujo de línea
    fn handle_draw_line(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < 20 {
            return Err(anyhow::anyhow!("Datos insuficientes para DRAW_LINE"));
        }
        
        let x1 = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let y1 = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let x2 = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
        let y2 = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
        let color = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);
        
        println!("   [GFX] Dibujando línea de ({},{}) a ({},{}) con color 0x{:06X}", 
                 x1, y1, x2, y2, color);
        Ok(vec![1])
    }
    
    /// Procesar comando de limpieza de pantalla
    fn handle_clear(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let color = if data.len() >= 4 {
            u32::from_le_bytes([data[0], data[1], data[2], data[3]])
        } else {
            0x000000 // Negro por defecto
        };
        
        println!("   [GFX] Limpiando pantalla con color 0x{:06X}", color);
        Ok(vec![1])
    }
    
    /// Procesar comando de swap de buffers
    fn handle_swap(&mut self, _data: &[u8]) -> Result<Vec<u8>> {
        // Simular swap de buffers
        Ok(vec![1])
    }
}

impl Default for GraphicsServer {
    fn default() -> Self {
        Self::new()
    }
}

impl MicrokernelServer for GraphicsServer {
    fn name(&self) -> &str {
        &self.name
    }
    
    fn message_type(&self) -> MessageType {
        MessageType::Graphics
    }
    
    fn priority(&self) -> u8 {
        9 // Alta prioridad
    }
    
    fn initialize(&mut self) -> Result<()> {
        println!("   [GFX] Inicializando servidor de gráficos...");
        
        if let Some(ref fb_info) = self.framebuffer_info {
            println!("   [GFX] Framebuffer detectado:");
            println!("   [GFX]   Dirección: 0x{:016X}", fb_info.address);
            println!("   [GFX]   Resolución: {}x{}", fb_info.width, fb_info.height);
            println!("   [GFX]   Pitch: {} bytes", fb_info.pitch);
            println!("   [GFX]   BPP: {} bits", fb_info.bpp);
            
            self.width = fb_info.width;
            self.height = fb_info.height;
            
            // Map framebuffer into our virtual address space
            println!("   [GFX] Mapeando framebuffer...");
            if let Some(fb_ptr) = map_framebuffer() {
                self.framebuffer_ptr = Some(fb_ptr);
                println!("   [GFX] Framebuffer mapeado en: 0x{:016X}", fb_ptr as u64);
                
                // Clear screen to black
                println!("   [GFX] Limpiando pantalla (negro)...");
                unsafe {
                    core::ptr::write_bytes(
                        fb_ptr,
                        0,  // Black
                        (fb_info.pitch * fb_info.height) as usize,
                    );
                }
                println!("   [GFX] Pantalla limpiada");
            } else {
                println!("   [GFX] ERROR: No se pudo mapear el framebuffer");
            }
        } else {
            println!("   [GFX] ADVERTENCIA: No se pudo obtener info del framebuffer");
            println!("   [GFX] Usando configuración por defecto: {}x{}", self.width, self.height);
        }
        
        println!("   [GFX] Configurando modo de video {}x{}", self.width, self.height);
        println!("   [GFX] Inicializando aceleración por hardware...");
        
        self.initialized = true;
        println!("   [GFX] Servidor de gráficos listo");
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
        
        let command_byte = message.data[0];
        let command_data = &message.data[1..message.data_size as usize];
        
        let command = GraphicsCommand::try_from(command_byte)
            .map_err(|_| anyhow::anyhow!("Comando desconocido: {}", command_byte))?;
        
        let result = match command {
            GraphicsCommand::InitDisplay => self.handle_init_display(command_data),
            GraphicsCommand::DrawPixel => self.handle_draw_pixel(command_data),
            GraphicsCommand::DrawRect => self.handle_draw_rect(command_data),
            GraphicsCommand::DrawLine => self.handle_draw_line(command_data),
            GraphicsCommand::Clear => self.handle_clear(command_data),
            GraphicsCommand::Swap => self.handle_swap(command_data),
        };
        
        if result.is_err() {
            self.stats.messages_failed += 1;
            self.stats.last_error = Some(format!("{:?}", result));
        }
        
        result
    }
    
    fn shutdown(&mut self) -> Result<()> {
        println!("   [GFX] Deteniendo servidor de gráficos...");
        println!("   [GFX] Liberando recursos de GPU...");
        self.initialized = false;
        println!("   [GFX] Servidor de gráficos detenido");
        Ok(())
    }
    
    fn get_stats(&self) -> ServerStats {
        self.stats.clone()
    }
}
