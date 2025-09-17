//! Buffer de audio para Eclipse OS

use alloc::vec::Vec;
use super::{AudioFormat, AudioChannel};

#[derive(Debug, Clone)]
pub struct AudioBuffer {
    pub data: Vec<u8>,
    pub format: AudioFormat,
    pub channels: AudioChannel,
    pub sample_rate: u32,
    pub length: usize,
    pub position: usize,
}

impl AudioBuffer {
    pub fn new(format: AudioFormat, channels: AudioChannel, sample_rate: u32, capacity: usize) -> Self {
        Self {
            data: {
                let mut data = Vec::with_capacity(capacity);
                for _ in 0..capacity {
                    data.push(0);
                }
                data
            },
            format,
            channels,
            sample_rate,
            length: 0,
            position: 0,
        }
    }

    pub fn write(&mut self, data: &[u8]) -> Result<usize, &'static str> {
        let bytes_to_write = data.len().min(self.data.len() - self.position);
        self.data[self.position..self.position + bytes_to_write].copy_from_slice(&data[..bytes_to_write]);
        self.position += bytes_to_write;
        if self.position > self.length {
            self.length = self.position;
        }
        Ok(bytes_to_write)
    }

    pub fn read(&mut self, buffer: &mut [u8]) -> Result<usize, &'static str> {
        let bytes_to_read = buffer.len().min(self.length - self.position);
        buffer[..bytes_to_read].copy_from_slice(&self.data[self.position..self.position + bytes_to_read]);
        self.position += bytes_to_read;
        Ok(bytes_to_read)
    }

    pub fn seek(&mut self, position: usize) -> Result<(), &'static str> {
        if position > self.length {
            return Err("Position out of bounds");
        }
        self.position = position;
        Ok(())
    }

    pub fn get_length(&self) -> usize {
        self.length
    }

    pub fn get_position(&self) -> usize {
        self.position
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn is_full(&self) -> bool {
        self.length == self.data.len()
    }
}
