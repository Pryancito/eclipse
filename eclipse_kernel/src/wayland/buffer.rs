//! Buffers Wayland para Eclipse OS
//! 
//! Implementa la gestión de buffers de memoria compartida.

use super::surface::*;
use alloc::vec::Vec;
use alloc::vec;

/// Buffer de memoria compartida
#[derive(Debug, Clone)]
pub struct SharedMemoryBuffer {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub format: BufferFormat,
    pub stride: u32,
    pub offset: u32,
}

impl SharedMemoryBuffer {
    pub fn new(width: u32, height: u32, format: BufferFormat) -> Self {
        let stride = width * format.bytes_per_pixel();
        let size = (stride * height) as usize;
        
        Self {
            data: vec![0; size],
            width,
            height,
            format,
            stride,
            offset: 0,
        }
    }
    
    /// Obtener datos del buffer
    pub fn get_data(&self) -> &[u8] {
        &self.data
    }
    
    /// Obtener datos del buffer (mutable)
    pub fn get_data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
    
    /// Obtener stride
    pub fn get_stride(&self) -> u32 {
        self.stride
    }
    
    /// Obtener formato
    pub fn get_format(&self) -> BufferFormat {
        self.format
    }
}
