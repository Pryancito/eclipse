//! Drivers de audio para Eclipse OS

use alloc::vec::Vec;
use alloc::string::String;
use super::{AudioFormat, AudioChannel, AudioDeviceType};

#[derive(Debug, Clone)]
pub struct AudioDriver {
    pub name: String,
    pub version: String,
    pub supported_devices: Vec<AudioDeviceType>,
    pub initialized: bool,
}

impl AudioDriver {
    pub fn new(name: String, version: String) -> Self {
        Self {
            name,
            version,
            supported_devices: Vec::new(),
            initialized: false,
        }
    }

    pub fn initialize(&mut self) -> Result<(), &'static str> {
        if self.initialized {
            return Err("Audio driver already initialized");
        }
        self.initialized = true;
        Ok(())
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
}
