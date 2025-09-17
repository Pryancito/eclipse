//! Drivers de almacenamiento para Eclipse OS

use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone)]
pub enum StorageType {
    Sata,
    Nvme,
    Usb,
    SdCard,
    Network,
}

#[derive(Debug, Clone)]
pub struct StorageDevice {
    pub device_name: String,
    pub storage_type: StorageType,
    pub capacity: u64,
    pub block_size: u32,
    pub is_removable: bool,
    pub is_read_only: bool,
}

pub struct StorageManager {
    devices: Vec<StorageDevice>,
    initialized: bool,
}

impl StorageManager {
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
            initialized: false,
        }
    }

    pub fn initialize(&mut self) -> Result<(), &'static str> {
        if self.initialized {
            return Err("Storage manager already initialized");
        }
        self.initialized = true;
        Ok(())
    }

    pub fn add_device(&mut self, device: StorageDevice) {
        self.devices.push(device);
    }

    pub fn get_devices(&self) -> &[StorageDevice] {
        &self.devices
    }

    pub fn find_device(&self, name: &str) -> Option<&StorageDevice> {
        self.devices.iter().find(|d| d.device_name == name)
    }
}
