//! Superficies Wayland para Eclipse OS
//! 
//! Implementa la gestión de superficies (ventanas) en Wayland.

use super::protocol::*;
use core::sync::atomic::{AtomicBool, Ordering};
use alloc::vec::Vec;
use alloc::vec;

/// Superficie Wayland
pub struct WaylandSurface {
    pub id: ObjectId,
    pub client_id: ObjectId,
    pub width: u32,
    pub height: u32,
    pub buffer: Vec<u8>,
    pub is_damaged: AtomicBool,
    pub is_committed: AtomicBool,
    pub position: (i32, i32),
    pub z_order: i32,
}

impl WaylandSurface {
    pub fn new(id: ObjectId, client_id: ObjectId) -> Self {
        Self {
            id,
            client_id,
            width: 0,
            height: 0,
            buffer: Vec::new(),
            is_damaged: AtomicBool::new(false),
            is_committed: AtomicBool::new(false),
            position: (0, 0),
            z_order: 0,
        }
    }
    
    /// Actualizar buffer de la superficie
    pub fn update_buffer(&mut self, buffer: &[u8], width: u32, height: u32) -> Result<(), &'static str> {
        self.width = width;
        self.height = height;
        self.buffer.clear();
        self.buffer.extend_from_slice(buffer);
        self.is_damaged.store(true, Ordering::Release);
        Ok(())
    }
    
    /// Marcar superficie como comprometida
    pub fn commit(&mut self) {
        self.is_committed.store(true, Ordering::Release);
    }
    
    /// Verificar si la superficie está dañada
    pub fn is_damaged(&self) -> bool {
        self.is_damaged.load(Ordering::Acquire)
    }
    
    /// Verificar si la superficie está comprometida
    pub fn is_committed(&self) -> bool {
        self.is_committed.load(Ordering::Acquire)
    }
    
    /// Obtener buffer de la superficie
    pub fn get_buffer(&self) -> &[u8] {
        &self.buffer
    }
    
    /// Obtener dimensiones
    pub fn get_dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    
    /// Obtener posición
    pub fn get_position(&self) -> (i32, i32) {
        self.position
    }
    
    /// Establecer posición
    pub fn set_position(&mut self, x: i32, y: i32) {
        self.position = (x, y);
    }
    
    /// Obtener z-order
    pub fn get_z_order(&self) -> i32 {
        self.z_order
    }
    
    /// Establecer z-order
    pub fn set_z_order(&mut self, z: i32) {
        self.z_order = z;
    }
    
    /// Limpiar daño
    pub fn clear_damage(&mut self) {
        self.is_damaged.store(false, Ordering::Release);
    }
    
    /// Limpiar commit
    pub fn clear_commit(&mut self) {
        self.is_committed.store(false, Ordering::Release);
    }
}

/// Buffer de superficie
pub struct SurfaceBuffer {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub format: BufferFormat,
    pub stride: u32,
}

impl SurfaceBuffer {
    pub fn new(width: u32, height: u32, format: BufferFormat) -> Self {
        let stride = width * format.bytes_per_pixel();
        let size = (stride * height) as usize;
        
        Self {
            data: vec![0; size],
            width,
            height,
            format,
            stride,
        }
    }
    
    pub fn get_pixel(&self, x: u32, y: u32) -> Option<u32> {
        if x >= self.width || y >= self.height {
            return None;
        }
        
        let offset = ((y * self.stride) + (x * self.format.bytes_per_pixel())) as usize;
        if offset + 4 <= self.data.len() {
            Some(u32::from_le_bytes([
                self.data[offset],
                self.data[offset + 1],
                self.data[offset + 2],
                self.data[offset + 3],
            ]))
        } else {
            None
        }
    }
    
    pub fn set_pixel(&mut self, x: u32, y: u32, color: u32) -> Result<(), &'static str> {
        if x >= self.width || y >= self.height {
            return Err("Pixel coordinates out of bounds");
        }
        
        let offset = ((y * self.stride) + (x * self.format.bytes_per_pixel())) as usize;
        if offset + 4 <= self.data.len() {
            let bytes = color.to_le_bytes();
            self.data[offset] = bytes[0];
            self.data[offset + 1] = bytes[1];
            self.data[offset + 2] = bytes[2];
            self.data[offset + 3] = bytes[3];
            Ok(())
        } else {
            Err("Buffer overflow")
        }
    }
}

/// Formato de buffer
#[derive(Debug, Clone, Copy)]
pub enum BufferFormat {
    ARGB8888,
    XRGB8888,
    RGB565,
    RGBA8888,
}

impl BufferFormat {
    pub fn bytes_per_pixel(&self) -> u32 {
        match self {
            BufferFormat::ARGB8888 => 4,
            BufferFormat::XRGB8888 => 4,
            BufferFormat::RGB565 => 2,
            BufferFormat::RGBA8888 => 4,
        }
    }
    
    pub fn has_alpha(&self) -> bool {
        match self {
            BufferFormat::ARGB8888 => true,
            BufferFormat::XRGB8888 => false,
            BufferFormat::RGB565 => false,
            BufferFormat::RGBA8888 => true,
        }
    }
}
