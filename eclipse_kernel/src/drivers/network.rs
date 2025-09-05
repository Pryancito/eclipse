//! Drivers de red para Eclipse OS
//! 
//! Basado en los drivers de red de Redox OS

use crate::drivers::{
    device::{Device, DeviceInfo, DeviceType},
    manager::{Driver, DriverInfo, DriverResult, DriverError},
    MAX_DEVICES,
};

// Información de dispositivo de red
#[derive(Debug, Clone)]
pub struct NetworkDeviceInfo {
    pub device_id: u32,
    pub name: [u8; 32],
    pub mac_address: [u8; 6],
    pub ip_address: [u8; 4],
    pub subnet_mask: [u8; 4],
    pub gateway: [u8; 4],
    pub mtu: u16,
    pub speed: u32,
    pub duplex: bool,
    pub is_up: bool,
    pub interface_type: NetworkInterface,
    pub vendor_id: u16,
    pub device_id_pci: u16,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NetworkInterface {
    Ethernet,
    WiFi,
    Loopback,
    Unknown,
}

impl NetworkInterface {
    pub fn as_str(&self) -> &'static str {
        match self {
            NetworkInterface::Ethernet => "Ethernet",
            NetworkInterface::WiFi => "WiFi",
            NetworkInterface::Loopback => "Loopback",
            NetworkInterface::Unknown => "Unknown",
        }
    }
}

// Driver de red base
pub struct NetworkDriver {
    pub info: DriverInfo,
    pub devices: [Option<NetworkDeviceInfo>; MAX_DEVICES],
    pub device_count: u32,
}

impl NetworkDriver {
    pub fn new() -> Self {
        let mut info = DriverInfo::new();
        info.set_name("network");
        info.device_type = DeviceType::Network;
        info.version = 1;

        Self {
            info,
            devices: [(); MAX_DEVICES].map(|_| None),
            device_count: 0,
        }
    }

    pub fn add_device(&mut self, device_info: NetworkDeviceInfo) -> DriverResult<()> {
        if self.device_count >= MAX_DEVICES as u32 {
            return Err(DriverError::OutOfMemory);
        }

        for i in 0..MAX_DEVICES {
            if self.devices[i].is_none() {
                self.devices[i] = Some(device_info);
                self.device_count += 1;
                return Ok(());
            }
        }

        Err(DriverError::OutOfMemory)
    }
}

impl Driver for NetworkDriver {
    fn get_info(&self) -> &DriverInfo {
        &self.info
    }

    fn initialize(&mut self) -> DriverResult<()> {
        self.info.is_loaded = true;
        Ok(())
    }

    fn cleanup(&mut self) -> DriverResult<()> {
        for i in 0..MAX_DEVICES {
            self.devices[i] = None;
        }
        self.device_count = 0;
        self.info.is_loaded = false;
        Ok(())
    }

    fn probe_device(&mut self, device_info: &DeviceInfo) -> bool {
        device_info.device_type == DeviceType::Network
    }

    fn attach_device(&mut self, device: &mut Device) -> DriverResult<()> {
        let mut network_info = NetworkDeviceInfo {
            device_id: device.info.id,
            name: [0; 32],
            mac_address: [0; 6],
            ip_address: [0; 4],
            subnet_mask: [255, 255, 255, 0],
            gateway: [0; 4],
            mtu: 1500,
            speed: 1000,
            duplex: true,
            is_up: false,
            interface_type: NetworkInterface::Ethernet,
            vendor_id: device.info.vendor_id,
            device_id_pci: device.info.device_id,
        };

        // Configurar nombre
        network_info.name[..device.info.name.len()].copy_from_slice(&device.info.name);
        
        // Generar MAC address basada en vendor/device ID
        network_info.mac_address[0] = 0x02; // Local bit
        network_info.mac_address[1] = (device.info.vendor_id >> 8) as u8;
        network_info.mac_address[2] = device.info.vendor_id as u8;
        network_info.mac_address[3] = (device.info.device_id >> 8) as u8;
        network_info.mac_address[4] = device.info.device_id as u8;
        network_info.mac_address[5] = (device.info.id & 0xFF) as u8;

        self.add_device(network_info)?;
        device.driver_id = Some(self.info.id);
        
        Ok(())
    }

    fn detach_device(&mut self, _device_id: u32) -> DriverResult<()> {
        Ok(())
    }

    fn handle_interrupt(&mut self, _device_id: u32) -> DriverResult<()> {
        Ok(())
    }
}

// Funciones de inicialización
pub fn init_network_drivers() -> DriverResult<()> {
    // Inicializar drivers de red
    Ok(())
}