//! Pipes (IPC) para Eclipse OS
//!
//! Este módulo implementa pipes (tuberías) para comunicación
//! entre procesos, permitiendo redirección I/O en shells.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use spin::Mutex;
use crate::debug::serial_write_str;

/// Tamaño del buffer de pipe (4KB)
pub const PIPE_BUFFER_SIZE: usize = 4096;

/// Estado de un extremo del pipe
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PipeEndState {
    Open,
    Closed,
}

/// Buffer compartido de un pipe
#[derive(Debug)]
pub struct PipeBuffer {
    /// Buffer de datos
    data: VecDeque<u8>,
    /// Capacidad máxima
    capacity: usize,
    /// Estado del extremo de lectura
    read_end_state: PipeEndState,
    /// Estado del extremo de escritura
    write_end_state: PipeEndState,
    /// Contador de lectores
    readers: usize,
    /// Contador de escritores
    writers: usize,
}

impl PipeBuffer {
    /// Crear nuevo buffer de pipe
    pub fn new() -> Self {
        Self {
            data: VecDeque::with_capacity(PIPE_BUFFER_SIZE),
            capacity: PIPE_BUFFER_SIZE,
            read_end_state: PipeEndState::Open,
            write_end_state: PipeEndState::Open,
            readers: 1,
            writers: 1,
        }
    }

    /// Escribir datos al pipe
    pub fn write(&mut self, data: &[u8]) -> Result<usize, &'static str> {
        if self.write_end_state == PipeEndState::Closed {
            return Err("Write end of pipe is closed");
        }

        // Calcular cuántos bytes podemos escribir
        let available_space = self.capacity - self.data.len();
        let bytes_to_write = core::cmp::min(data.len(), available_space);

        if bytes_to_write == 0 {
            // Pipe lleno - en un sistema real, esto bloquearía al escritor
            return Ok(0);
        }

        // Escribir al buffer
        for i in 0..bytes_to_write {
            self.data.push_back(data[i]);
        }

        Ok(bytes_to_write)
    }

    /// Leer datos del pipe
    pub fn read(&mut self, buffer: &mut [u8]) -> Result<usize, &'static str> {
        if self.read_end_state == PipeEndState::Closed {
            return Err("Read end of pipe is closed");
        }

        // Si no hay datos y el write end está cerrado, EOF
        if self.data.is_empty() {
            if self.write_end_state == PipeEndState::Closed || self.writers == 0 {
                return Ok(0); // EOF
            }
            // En un sistema real, esto bloquearía al lector
            return Ok(0);
        }

        // Leer datos disponibles
        let bytes_to_read = core::cmp::min(buffer.len(), self.data.len());
        
        for i in 0..bytes_to_read {
            buffer[i] = self.data.pop_front().unwrap();
        }

        Ok(bytes_to_read)
    }

    /// Cerrar extremo de lectura
    pub fn close_read_end(&mut self) {
        self.read_end_state = PipeEndState::Closed;
        if self.readers > 0 {
            self.readers -= 1;
        }
    }

    /// Cerrar extremo de escritura
    pub fn close_write_end(&mut self) {
        self.write_end_state = PipeEndState::Closed;
        if self.writers > 0 {
            self.writers -= 1;
        }
    }

    /// Verificar si hay datos disponibles
    pub fn has_data(&self) -> bool {
        !self.data.is_empty()
    }

    /// Obtener bytes disponibles
    pub fn available(&self) -> usize {
        self.data.len()
    }

    /// Obtener espacio libre
    pub fn free_space(&self) -> usize {
        self.capacity - self.data.len()
    }

    /// Incrementar contador de lectores
    pub fn add_reader(&mut self) {
        self.readers += 1;
    }

    /// Incrementar contador de escritores
    pub fn add_writer(&mut self) {
        self.writers += 1;
    }
}

/// Extremo de pipe (read o write)
#[derive(Clone)]
pub struct PipeEnd {
    /// Buffer compartido
    buffer: Arc<Mutex<PipeBuffer>>,
    /// Tipo de extremo
    end_type: PipeEndType,
}

/// Tipo de extremo de pipe
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PipeEndType {
    Read,
    Write,
}

impl PipeEnd {
    /// Crear nuevo extremo de pipe
    pub fn new(buffer: Arc<Mutex<PipeBuffer>>, end_type: PipeEndType) -> Self {
        Self { buffer, end_type }
    }

    /// Escribir al pipe (solo write end)
    pub fn write(&self, data: &[u8]) -> Result<usize, &'static str> {
        if self.end_type != PipeEndType::Write {
            return Err("Cannot write to read end of pipe");
        }

        let mut buf = self.buffer.lock();
        buf.write(data)
    }

    /// Leer del pipe (solo read end)
    pub fn read(&self, buffer: &mut [u8]) -> Result<usize, &'static str> {
        if self.end_type != PipeEndType::Read {
            return Err("Cannot read from write end of pipe");
        }

        let mut buf = self.buffer.lock();
        buf.read(buffer)
    }

    /// Cerrar este extremo
    pub fn close(&self) {
        let mut buf = self.buffer.lock();
        match self.end_type {
            PipeEndType::Read => buf.close_read_end(),
            PipeEndType::Write => buf.close_write_end(),
        }
    }

    /// Obtener tipo de extremo
    pub fn get_type(&self) -> PipeEndType {
        self.end_type
    }

    /// Verificar si hay datos disponibles (solo read end)
    pub fn has_data(&self) -> bool {
        let buf = self.buffer.lock();
        buf.has_data()
    }

    /// Obtener bytes disponibles
    pub fn available(&self) -> usize {
        let buf = self.buffer.lock();
        buf.available()
    }
}

/// Crear un nuevo pipe
pub fn create_pipe() -> (PipeEnd, PipeEnd) {
    serial_write_str("PIPE: Creando nuevo pipe\n");
    
    let buffer = Arc::new(Mutex::new(PipeBuffer::new()));
    
    let read_end = PipeEnd::new(buffer.clone(), PipeEndType::Read);
    let write_end = PipeEnd::new(buffer, PipeEndType::Write);
    
    serial_write_str("PIPE: Pipe creado exitosamente\n");
    
    (read_end, write_end)
}

/// Estadísticas de un pipe
#[derive(Debug, Clone, Copy)]
pub struct PipeStats {
    pub available_bytes: usize,
    pub free_space: usize,
    pub capacity: usize,
    pub readers: usize,
    pub writers: usize,
}

/// Obtener estadísticas de un pipe
pub fn get_pipe_stats(pipe: &PipeEnd) -> PipeStats {
    let buf = pipe.buffer.lock();
    PipeStats {
        available_bytes: buf.available(),
        free_space: buf.free_space(),
        capacity: buf.capacity,
        readers: buf.readers,
        writers: buf.writers,
    }
}

