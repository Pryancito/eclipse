//! Drivers de entrada para Eclipse OS
//!
//! Basado en los drivers de entrada de Redox OS

use crate::drivers::{
    device::{Device, DeviceInfo, DeviceType},
    manager::{Driver, DriverError, DriverInfo, DriverResult},
    MAX_DEVICES,
};

// Información de dispositivo de entrada
#[derive(Debug, Clone)]
pub struct InputDeviceInfo {
    pub device_id: u32,
    pub name: [u8; 32],
    pub device_type: InputDeviceType,
    pub is_initialized: bool,
    pub key_count: u32,
    pub button_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputDeviceType {
    Keyboard,
    Mouse,
    Gamepad,
    Touchpad,
    Unknown,
}

impl InputDeviceType {
    pub fn as_str(&self) -> &'static str {
        match self {
            InputDeviceType::Keyboard => "Keyboard",
            InputDeviceType::Mouse => "Mouse",
            InputDeviceType::Gamepad => "Gamepad",
            InputDeviceType::Touchpad => "Touchpad",
            InputDeviceType::Unknown => "Unknown",
        }
    }
}

// Driver de entrada base
pub struct InputDriver {
    pub info: DriverInfo,
    pub devices: [Option<InputDeviceInfo>; MAX_DEVICES],
    pub device_count: u32,
}

impl InputDriver {
    pub fn new() -> Self {
        let mut info = DriverInfo::new();
        info.set_name("input");
        info.device_type = DeviceType::Input;
        info.version = 1;

        Self {
            info,
            devices: [(); MAX_DEVICES].map(|_| None),
            device_count: 0,
        }
    }

    pub fn add_device(&mut self, device_info: InputDeviceInfo) -> DriverResult<()> {
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

impl Driver for InputDriver {
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
        device_info.device_type == DeviceType::Input
    }

    fn attach_device(&mut self, device: &mut Device) -> DriverResult<()> {
        let mut input_info = InputDeviceInfo {
            device_id: device.info.id,
            name: [0; 32],
            device_type: InputDeviceType::Keyboard,
            is_initialized: false,
            key_count: 104,
            button_count: 0,
        };

        // Configurar nombre
        input_info.name[..device.info.name.len()].copy_from_slice(&device.info.name);

        self.add_device(input_info)?;
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
pub fn init_input_drivers() -> DriverResult<()> {
    // Inicializar drivers de entrada
    Ok(())
}
