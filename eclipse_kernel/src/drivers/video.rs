//! Drivers de video para Eclipse OS
//! 
//! Basado en los drivers de video de Redox OS

use crate::drivers::{
    device::{Device, DeviceInfo, DeviceType},
    manager::{Driver, DriverInfo, DriverResult, DriverError},
    MAX_DEVICES,
};

// Información de dispositivo de video
#[derive(Debug, Clone)]
pub struct VideoDeviceInfo {
    pub device_id: u32,
    pub name: [u8; 32],
    pub width: u32,
    pub height: u32,
    pub bpp: u8,
    pub framebuffer_address: u64,
    pub framebuffer_size: u64,
    pub is_initialized: bool,
    pub interface_type: VideoInterface,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VideoInterface {
    VGA,
    VESA,
    PCI,
    Unknown,
}

impl VideoInterface {
    pub fn as_str(&self) -> &'static str {
        match self {
            VideoInterface::VGA => "VGA",
            VideoInterface::VESA => "VESA",
            VideoInterface::PCI => "PCI",
            VideoInterface::Unknown => "Unknown",
        }
    }
}

// Driver de video base
pub struct VideoDriver {
    pub info: DriverInfo,
    pub devices: [Option<VideoDeviceInfo>; MAX_DEVICES],
    pub device_count: u32,
}

impl VideoDriver {
    pub fn new() -> Self {
        let mut info = DriverInfo::new();
        info.set_name("video");
        info.device_type = DeviceType::Video;
        info.version = 1;

        Self {
            info,
            devices: [(); MAX_DEVICES].map(|_| None),
            device_count: 0,
        }
    }

    pub fn add_device(&mut self, device_info: VideoDeviceInfo) -> DriverResult<()> {
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

impl Driver for VideoDriver {
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
        device_info.device_type == DeviceType::Video
    }

    fn attach_device(&mut self, device: &mut Device) -> DriverResult<()> {
        let mut video_info = VideoDeviceInfo {
            device_id: device.info.id,
            name: [0; 32],
            width: 1024,
            height: 768,
            bpp: 32,
            framebuffer_address: 0,
            framebuffer_size: 0,
            is_initialized: false,
            interface_type: VideoInterface::VGA,
        };

        // Configurar nombre
        video_info.name[..device.info.name.len()].copy_from_slice(&device.info.name);
        
        // Calcular tamaño del framebuffer
        video_info.framebuffer_size = (video_info.width * video_info.height * video_info.bpp as u32 / 8) as u64;

        self.add_device(video_info)?;
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
pub fn init_video_drivers() -> DriverResult<()> {
    // Inicializar drivers de video
    Ok(())
}
