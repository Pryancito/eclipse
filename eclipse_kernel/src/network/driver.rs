//! Drivers de red para Eclipse OS

use alloc::vec::Vec;
use alloc::string::String;
use super::{MacAddress, IpAddress};

#[derive(Debug, Clone)]
pub struct NetworkDevice {
    pub name: String,
    pub mac_address: MacAddress,
    pub ip_address: IpAddress,
    pub mtu: u16,
    pub is_up: bool,
}

pub struct NetworkDriver {
    devices: Vec<NetworkDevice>,
    initialized: bool,
}

impl NetworkDriver {
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
            initialized: false,
        }
    }

    pub fn initialize(&mut self) -> Result<(), &'static str> {
        if self.initialized {
            return Err("Network driver already initialized");
        }

        // Simular detección de dispositivos de red
        let device = NetworkDevice {
            name: "eth0"String::from(.to_string(),
            mac_address: MacAddress::new(0x00, 0x11, 0x22, 0x33, 0x44, 0x55),
            ip_address: IpAddress::new(192, 168, 1, 100),
            mtu: 1500,
            is_up: true,
        };

        self.devices.push(device);
        self.initialized = true;
        Ok(())
    }

    pub fn get_devices(&self) -> &[NetworkDevice] {
        &self.devices
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
}
