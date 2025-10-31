//! Gestión de buffers de red
//!
//! Pool de buffers para paquetes de red y gestión de memoria

#![allow(dead_code)] // Permitir código no utilizado - API completa del kernel

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use super::NetworkError;

/// Buffer de red
pub struct NetworkBuffer {
    pub data: Vec<u8>,
    pub length: usize,
    pub capacity: usize,
    pub timestamp: u64,
    pub interface_index: u32,
    pub protocol: u8,
}

impl NetworkBuffer {
    /// Crear nuevo buffer
    pub fn new(capacity: usize) -> Self {
        Self {
            data: Vec::with_capacity(capacity),
            length: 0,
            capacity,
            timestamp: 0,
            interface_index: 0,
            protocol: 0,
        }
    }

    /// Crear buffer con datos
    pub fn with_data(data: Vec<u8>, interface_index: u32, protocol: u8) -> Self {
        let length = data.len();
        let capacity = data.capacity();

        Self {
            data,
            length,
            capacity,
            timestamp: 0,
            interface_index,
            protocol,
        }
    }

    /// Establecer timestamp
    pub fn set_timestamp(&mut self, timestamp: u64) {
        self.timestamp = timestamp;
    }

    /// Establecer interfaz
    pub fn set_interface(&mut self, interface_index: u32) {
        self.interface_index = interface_index;
    }

    /// Establecer protocolo
    pub fn set_protocol(&mut self, protocol: u8) {
        self.protocol = protocol;
    }

    /// Obtener datos
    pub fn get_data(&self) -> &[u8] {
        &self.data[..self.length]
    }

    /// Obtener datos mutables
    pub fn get_data_mut(&mut self) -> &mut [u8] {
        &mut self.data[..self.length]
    }

    /// Establecer longitud
    pub fn set_length(&mut self, length: usize) {
        if length <= self.capacity {
            self.length = length;
        }
    }

    /// Agregar datos
    pub fn append_data(&mut self, data: &[u8]) -> Result<(), NetworkError> {
        if self.length + data.len() > self.capacity {
            return Err(NetworkError::BufferFull);
        }

        self.data.extend_from_slice(data);
        self.length += data.len();
        Ok(())
    }

    /// Limpiar buffer
    pub fn clear(&mut self) {
        self.data.clear();
        self.length = 0;
        self.timestamp = 0;
        self.interface_index = 0;
        self.protocol = 0;
    }

    /// Verificar si el buffer está vacío
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Verificar si el buffer está lleno
    pub fn is_full(&self) -> bool {
        self.length >= self.capacity
    }

    /// Obtener espacio disponible
    pub fn get_available_space(&self) -> usize {
        self.capacity - self.length
    }

    /// Clonar buffer
    pub fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
            length: self.length,
            capacity: self.capacity,
            timestamp: self.timestamp,
            interface_index: self.interface_index,
            protocol: self.protocol,
        }
    }
}

/// Pool de buffers
pub struct BufferPool {
    pub buffers: VecDeque<NetworkBuffer>,
    pub max_buffers: usize,
    pub buffer_size: usize,
    pub allocated_count: AtomicUsize,
    pub free_count: AtomicUsize,
}

impl BufferPool {
    /// Crear nuevo pool de buffers
    pub fn new(max_buffers: usize, buffer_size: usize) -> Self {
        let mut pool = Self {
            buffers: VecDeque::new(),
            max_buffers,
            buffer_size,
            allocated_count: AtomicUsize::new(0),
            free_count: AtomicUsize::new(0),
        };

        // Pre-allocar algunos buffers
        for _ in 0..core::cmp::min(max_buffers / 2, 64) {
            pool.buffers.push_back(NetworkBuffer::new(buffer_size));
        }

        pool.free_count.store(pool.buffers.len(), Ordering::Relaxed);
        pool
    }

    /// Obtener buffer del pool
    pub fn get_buffer(&mut self) -> Option<NetworkBuffer> {
        if let Some(mut buffer) = self.buffers.pop_front() {
            buffer.clear();
            self.allocated_count.fetch_add(1, Ordering::Relaxed);
            self.free_count.fetch_sub(1, Ordering::Relaxed);
            Some(buffer)
        } else if self.allocated_count.load(Ordering::Relaxed) < self.max_buffers {
            // Crear nuevo buffer si no hemos alcanzado el límite
            let buffer = NetworkBuffer::new(self.buffer_size);
            self.allocated_count.fetch_add(1, Ordering::Relaxed);
            Some(buffer)
        } else {
            None
        }
    }

    /// Devolver buffer al pool
    pub fn return_buffer(&mut self, mut buffer: NetworkBuffer) {
        if self.buffers.len() < self.max_buffers {
            buffer.clear();
            self.buffers.push_back(buffer);
            self.allocated_count.fetch_sub(1, Ordering::Relaxed);
            self.free_count.fetch_add(1, Ordering::Relaxed);
        } else {
            // Pool lleno, descartar buffer
            self.allocated_count.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Obtener estadísticas del pool
    pub fn get_stats(&self) -> BufferPoolStats {
        BufferPoolStats {
            max_buffers: self.max_buffers,
            buffer_size: self.buffer_size,
            allocated: self.allocated_count.load(Ordering::Relaxed),
            free: self.free_count.load(Ordering::Relaxed),
            pool_size: self.buffers.len(),
        }
    }

    /// Verificar si hay buffers disponibles
    pub fn has_available(&self) -> bool {
        !self.buffers.is_empty() || self.allocated_count.load(Ordering::Relaxed) < self.max_buffers
    }

    /// Obtener número de buffers libres
    pub fn get_free_count(&self) -> usize {
        self.free_count.load(Ordering::Relaxed)
    }

    /// Obtener número de buffers asignados
    pub fn get_allocated_count(&self) -> usize {
        self.allocated_count.load(Ordering::Relaxed)
    }
}

/// Estadísticas del pool de buffers
#[derive(Debug, Clone)]
pub struct BufferPoolStats {
    pub max_buffers: usize,
    pub buffer_size: usize,
    pub allocated: usize,
    pub free: usize,
    pub pool_size: usize,
}

/// Cola de paquetes
pub struct PacketQueue {
    pub packets: VecDeque<NetworkBuffer>,
    pub max_size: usize,
    pub dropped_packets: u64,
}

impl PacketQueue {
    /// Crear nueva cola de paquetes
    pub fn new(max_size: usize) -> Self {
        Self {
            packets: VecDeque::new(),
            max_size,
            dropped_packets: 0,
        }
    }

    /// Agregar paquete a la cola
    pub fn enqueue(&mut self, packet: NetworkBuffer) -> Result<(), NetworkError> {
        if self.packets.len() >= self.max_size {
            self.dropped_packets += 1;
            return Err(NetworkError::BufferFull);
        }

        self.packets.push_back(packet);
        Ok(())
    }

    /// Extraer paquete de la cola
    pub fn dequeue(&mut self) -> Option<NetworkBuffer> {
        self.packets.pop_front()
    }

    /// Verificar si la cola está vacía
    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    /// Verificar si la cola está llena
    pub fn is_full(&self) -> bool {
        self.packets.len() >= self.max_size
    }

    /// Obtener tamaño de la cola
    pub fn size(&self) -> usize {
        self.packets.len()
    }

    /// Limpiar cola
    pub fn clear(&mut self) {
        self.packets.clear();
    }

    /// Obtener estadísticas de la cola
    pub fn get_stats(&self) -> PacketQueueStats {
        PacketQueueStats {
            current_size: self.packets.len(),
            max_size: self.max_size,
            dropped_packets: self.dropped_packets,
        }
    }
}

/// Estadísticas de cola de paquetes
#[derive(Debug, Clone)]
pub struct PacketQueueStats {
    pub current_size: usize,
    pub max_size: usize,
    pub dropped_packets: u64,
}

/// Gestor de buffers global
pub struct BufferManager {
    pub rx_pool: BufferPool,
    pub tx_pool: BufferPool,
    pub rx_queue: PacketQueue,
    pub tx_queue: PacketQueue,
}

impl BufferManager {
    /// Crear nuevo gestor de buffers
    pub fn new() -> Self {
        Self {
            rx_pool: BufferPool::new(super::BUFFER_POOL_SIZE / 2, super::MAX_PACKET_SIZE),
            tx_pool: BufferPool::new(super::BUFFER_POOL_SIZE / 2, super::MAX_PACKET_SIZE),
            rx_queue: PacketQueue::new(1024),
            tx_queue: PacketQueue::new(1024),
        }
    }

    /// Obtener buffer de recepción
    pub fn get_rx_buffer(&mut self) -> Option<NetworkBuffer> {
        self.rx_pool.get_buffer()
    }

    /// Obtener buffer de envío
    pub fn get_tx_buffer(&mut self) -> Option<NetworkBuffer> {
        self.tx_pool.get_buffer()
    }

    /// Devolver buffer de recepción
    pub fn return_rx_buffer(&mut self, buffer: NetworkBuffer) {
        self.rx_pool.return_buffer(buffer);
    }

    /// Devolver buffer de envío
    pub fn return_tx_buffer(&mut self, buffer: NetworkBuffer) {
        self.tx_pool.return_buffer(buffer);
    }

    /// Agregar paquete a cola de recepción
    pub fn enqueue_rx(&mut self, packet: NetworkBuffer) -> Result<(), NetworkError> {
        self.rx_queue.enqueue(packet)
    }

    /// Agregar paquete a cola de envío
    pub fn enqueue_tx(&mut self, packet: NetworkBuffer) -> Result<(), NetworkError> {
        self.tx_queue.enqueue(packet)
    }

    /// Extraer paquete de cola de recepción
    pub fn dequeue_rx(&mut self) -> Option<NetworkBuffer> {
        self.rx_queue.dequeue()
    }

    /// Extraer paquete de cola de envío
    pub fn dequeue_tx(&mut self) -> Option<NetworkBuffer> {
        self.tx_queue.dequeue()
    }

    /// Verificar si hay paquetes en cola de recepción
    pub fn has_rx_packets(&self) -> bool {
        !self.rx_queue.is_empty()
    }

    /// Verificar si hay paquetes en cola de envío
    pub fn has_tx_packets(&self) -> bool {
        !self.tx_queue.is_empty()
    }

    /// Obtener estadísticas del gestor
    pub fn get_stats(&self) -> BufferManagerStats {
        BufferManagerStats {
            rx_pool: self.rx_pool.get_stats(),
            tx_pool: self.tx_pool.get_stats(),
            rx_queue: self.rx_queue.get_stats(),
            tx_queue: self.tx_queue.get_stats(),
        }
    }
}

/// Estadísticas del gestor de buffers
#[derive(Debug, Clone)]
pub struct BufferManagerStats {
    pub rx_pool: BufferPoolStats,
    pub tx_pool: BufferPoolStats,
    pub rx_queue: PacketQueueStats,
    pub tx_queue: PacketQueueStats,
}

/// Instancia global del gestor de buffers
static mut BUFFER_MANAGER: Option<BufferManager> = None;

/// Inicializar gestor de buffers
pub fn init_buffer_pool() -> Result<(), NetworkError> {
    unsafe {
        if BUFFER_MANAGER.is_some() {
            return Err(NetworkError::ProtocolError);
        }

        BUFFER_MANAGER = Some(BufferManager::new());
        Ok(())
    }
}

/// Obtener gestor de buffers
pub fn get_buffer_manager() -> Option<&'static mut BufferManager> {
    unsafe { BUFFER_MANAGER.as_mut() }
}

/// Obtener estadísticas de buffers
pub fn get_buffer_stats() -> Option<BufferManagerStats> {
    unsafe { BUFFER_MANAGER.as_ref().map(|bm| bm.get_stats()) }
}
