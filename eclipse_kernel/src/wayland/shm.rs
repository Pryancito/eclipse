//! Memoria compartida Wayland para Eclipse OS
//! 
//! Implementa el protocolo de memoria compartida (SHM) de Wayland.

use super::protocol::*;
use super::buffer::*;
use super::surface::BufferFormat;
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use alloc::vec;

/// Gestor de memoria compartida
pub struct SharedMemoryManager {
    pub pools: BTreeMap<ObjectId, SharedMemoryPool>,
    pub next_pool_id: ObjectId,
}

impl SharedMemoryManager {
    pub fn new() -> Self {
        Self {
            pools: BTreeMap::new(),
            next_pool_id: 1,
        }
    }
    
    /// Crear pool de memoria compartida
    pub fn create_pool(&mut self, size: u32) -> ObjectId {
        let pool_id = self.next_pool_id;
        self.next_pool_id += 1;
        
        let pool = SharedMemoryPool::new(size);
        self.pools.insert(pool_id, pool);
        pool_id
    }
    
    /// Destruir pool
    pub fn destroy_pool(&mut self, pool_id: ObjectId) -> bool {
        self.pools.remove(&pool_id).is_some()
    }
    
    /// Obtener pool
    pub fn get_pool(&self, pool_id: ObjectId) -> Option<&SharedMemoryPool> {
        self.pools.get(&pool_id)
    }
    
    /// Obtener pool (mutable)
    pub fn get_pool_mut(&mut self, pool_id: ObjectId) -> Option<&mut SharedMemoryPool> {
        self.pools.get_mut(&pool_id)
    }
}

/// Pool de memoria compartida
pub struct SharedMemoryPool {
    pub size: u32,
    pub data: Vec<u8>,
    pub buffers: Vec<SharedMemoryBuffer>,
}

impl SharedMemoryPool {
    pub fn new(size: u32) -> Self {
        Self {
            size,
            data: vec![0; size as usize],
            buffers: Vec::new(),
        }
    }
    
    /// Crear buffer en el pool
    pub fn create_buffer(&mut self, offset: u32, width: u32, height: u32, stride: u32, format: BufferFormat) -> Result<ObjectId, &'static str> {
        if offset + (stride * height) > self.size {
            return Err("Buffer would exceed pool size");
        }
        
        let buffer_id = self.buffers.len() as ObjectId + 1;
        let buffer = SharedMemoryBuffer {
            data: self.data[offset as usize..(offset + stride * height) as usize].to_vec(),
            width,
            height,
            format,
            stride,
            offset,
        };
        
        self.buffers.push(buffer);
        Ok(buffer_id)
    }
    
    /// Obtener buffer
    pub fn get_buffer(&self, buffer_id: ObjectId) -> Option<&SharedMemoryBuffer> {
        self.buffers.get((buffer_id - 1) as usize)
    }
    
    /// Obtener buffer (mutable)
    pub fn get_buffer_mut(&mut self, buffer_id: ObjectId) -> Option<&mut SharedMemoryBuffer> {
        self.buffers.get_mut((buffer_id - 1) as usize)
    }
}
