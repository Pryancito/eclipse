//! APIs de User Mode para USB en Eclipse OS
//! 
//! Proporciona interfaces de alto nivel para que las aplicaciones
//! puedan interactuar con dispositivos USB desde user mode.

use crate::debug::serial_write_str;
use crate::drivers::usb_audio::{UsbAudioDriver, UsbAudioConfig, AudioFormat};
use crate::drivers::usb_video::{UsbVideoDriver, UsbVideoConfig, VideoFormat};
use crate::drivers::usb_network::{UsbNetworkDriver, UsbNetworkConfig, NetworkInterfaceType};
use crate::drivers::usb_hotplug::{UsbHotPlugManager, UsbHotPlugConfig};
use crate::drivers::usb_events::{UsbDeviceInfo, UsbControllerType, UsbDeviceSpeed};
use alloc::vec::Vec;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Identificador de dispositivo USB para user mode
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsbDeviceHandle {
    pub device_id: u32,
    pub device_type: UsbDeviceType,
}

/// Tipos de dispositivos USB
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UsbDeviceType {
    HID,        // Human Interface Device
    Audio,      // Audio Device
    Video,      // Video Device
    Network,    // Network Device
    MassStorage,// Mass Storage Device
    Hub,        // USB Hub
    Unknown,
}

/// Configuración de audio para user mode
#[derive(Debug, Clone)]
pub struct UserAudioConfig {
    pub sample_rate: u32,
    pub channels: u8,
    pub bit_depth: u8,
    pub format: AudioFormat,
}

/// Configuración de video para user mode
#[derive(Debug, Clone)]
pub struct UserVideoConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub format: VideoFormat,
}

/// Configuración de red para user mode
#[derive(Debug, Clone)]
pub struct UserNetworkConfig {
    pub interface_type: NetworkInterfaceType,
    pub mtu: u32,
    pub ip_address: [u8; 4],
    pub subnet_mask: [u8; 4],
    pub gateway: [u8; 4],
}

/// Información de dispositivo para user mode
#[derive(Debug, Clone)]
pub struct UserUsbDeviceInfo {
    pub handle: UsbDeviceHandle,
    pub vendor_name: String,
    pub product_name: String,
    pub device_class: String,
    pub port_number: u8,
    pub speed: String,
    pub is_connected: bool,
}

/// Manager de APIs USB para user mode
pub struct UsbUserApiManager {
    audio_driver: UsbAudioDriver,
    video_driver: UsbVideoDriver,
    network_driver: UsbNetworkDriver,
    hotplug_manager: UsbHotPlugManager,
    next_handle_id: AtomicU32,
    api_initialized: AtomicBool,
}

impl UsbUserApiManager {
    /// Crear nuevo manager de APIs USB
    pub fn new() -> Self {
        serial_write_str("USB_USER_API: Inicializando manager de APIs USB para user mode\n");
        
        Self {
            audio_driver: UsbAudioDriver::new(),
            video_driver: UsbVideoDriver::new(),
            network_driver: UsbNetworkDriver::new(),
            hotplug_manager: UsbHotPlugManager::new(),
            next_handle_id: AtomicU32::new(1),
            api_initialized: AtomicBool::new(false),
        }
    }

    /// Inicializar todas las APIs USB
    pub fn initialize(&mut self) -> Result<(), &'static str> {
        serial_write_str("USB_USER_API: Inicializando APIs USB...\n");
        
        // Inicializar drivers
        self.audio_driver.initialize()?;
        self.video_driver.initialize()?;
        self.network_driver.initialize()?;
        
        // Inicializar hot-plug con configuración para user mode
        let hotplug_config = UsbHotPlugConfig {
            check_interval_ms: 500,  // Más frecuente para user mode
            max_events_in_queue: 512,
            enable_logging: true,
            auto_power_management: true,
        };
        if let Err(e) = self.hotplug_manager.initialize(hotplug_config) {
            return Err("Error al inicializar hot-plug manager");
        }
        
        self.api_initialized.store(true, Ordering::SeqCst);
        serial_write_str("USB_USER_API: APIs USB inicializadas correctamente\n");
        
        Ok(())
    }

    /// Obtener siguiente ID de handle
    fn get_next_handle_id(&self) -> u32 {
        self.next_handle_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Enumerar dispositivos USB disponibles
    pub fn enumerate_devices(&self) -> Vec<UserUsbDeviceInfo> {
        let mut devices = Vec::new();
        
        // Agregar dispositivos de audio
        for audio_device in self.audio_driver.get_audio_devices() {
            devices.push(UserUsbDeviceInfo {
                handle: UsbDeviceHandle {
                    device_id: audio_device.device_id,
                    device_type: UsbDeviceType::Audio,
                },
                vendor_name: String::from(audio_device.device_info.get_vendor_name()),
                product_name: String::from("USB Audio Device"),
                device_class: String::from("Audio"),
                port_number: audio_device.device_info.port_number,
                speed: String::from(audio_device.device_info.get_speed_string()),
                is_connected: true,
            });
        }
        
        // Agregar dispositivos de video
        for video_device in self.video_driver.get_video_devices() {
            devices.push(UserUsbDeviceInfo {
                handle: UsbDeviceHandle {
                    device_id: video_device.device_id,
                    device_type: UsbDeviceType::Video,
                },
                vendor_name: String::from(video_device.device_info.get_vendor_name()),
                product_name: String::from("USB Video Device"),
                device_class: String::from("Video"),
                port_number: video_device.device_info.port_number,
                speed: String::from(video_device.device_info.get_speed_string()),
                is_connected: true,
            });
        }
        
        // Agregar dispositivos de red
        for network_device in self.network_driver.get_network_devices() {
            let interface_name = match network_device.config.interface_type {
                NetworkInterfaceType::Ethernet => "Ethernet",
                NetworkInterfaceType::WiFi => "WiFi",
                NetworkInterfaceType::Cellular => "Cellular",
                _ => "Unknown",
            };
            
            devices.push(UserUsbDeviceInfo {
                handle: UsbDeviceHandle {
                    device_id: network_device.device_id,
                    device_type: UsbDeviceType::Network,
                },
                vendor_name: String::from(network_device.device_info.get_vendor_name()),
                product_name: String::from(interface_name),
                device_class: String::from("Network"),
                port_number: network_device.device_info.port_number,
                speed: String::from(network_device.device_info.get_speed_string()),
                is_connected: network_device.link_up,
            });
        }
        
        serial_write_str(&alloc::format!(
            "USB_USER_API: Enumerados {} dispositivos USB\n",
            devices.len()
        ));
        
        devices
    }

    /// Abrir dispositivo USB
    pub fn open_device(&mut self, handle: UsbDeviceHandle) -> Result<(), &'static str> {
        match handle.device_type {
            UsbDeviceType::Audio => {
                self.audio_driver.initialize_device(handle.device_id)?;
                serial_write_str(&alloc::format!("USB_USER_API: Dispositivo de audio {} abierto\n", handle.device_id));
                Ok(())
            }
            UsbDeviceType::Video => {
                self.video_driver.initialize_device(handle.device_id)?;
                serial_write_str(&alloc::format!("USB_USER_API: Dispositivo de video {} abierto\n", handle.device_id));
                Ok(())
            }
            UsbDeviceType::Network => {
                self.network_driver.initialize_device(handle.device_id)?;
                serial_write_str(&alloc::format!("USB_USER_API: Dispositivo de red {} abierto\n", handle.device_id));
                Ok(())
            }
            _ => Err("Tipo de dispositivo no soportado por la API")
        }
    }

    /// Cerrar dispositivo USB
    pub fn close_device(&mut self, handle: UsbDeviceHandle) -> Result<(), &'static str> {
        match handle.device_type {
            UsbDeviceType::Audio => {
                serial_write_str(&alloc::format!("USB_USER_API: Dispositivo de audio {} cerrado\n", handle.device_id));
                Ok(())
            }
            UsbDeviceType::Video => {
                // Detener streaming si está activo
                let _ = self.video_driver.stop_streaming(handle.device_id);
                serial_write_str(&alloc::format!("USB_USER_API: Dispositivo de video {} cerrado\n", handle.device_id));
                Ok(())
            }
            UsbDeviceType::Network => {
                // Desactivar interfaz si está activa
                let _ = self.network_driver.bring_down_interface(handle.device_id);
                serial_write_str(&alloc::format!("USB_USER_API: Dispositivo de red {} cerrado\n", handle.device_id));
                Ok(())
            }
            _ => Err("Tipo de dispositivo no soportado por la API")
        }
    }

    // === APIs de Audio ===

    /// Configurar dispositivo de audio
    pub fn configure_audio_device(&mut self, handle: UsbDeviceHandle, config: UserAudioConfig) -> Result<(), &'static str> {
        if handle.device_type != UsbDeviceType::Audio {
            return Err("No es un dispositivo de audio");
        }

        serial_write_str(&alloc::format!(
            "USB_USER_API: Configurando audio {} - {}Hz, {}ch, {}bit, {:?}\n",
            handle.device_id,
            config.sample_rate,
            config.channels,
            config.bit_depth,
            config.format
        ));

        // En un sistema real, esto aplicaría la configuración al dispositivo
        Ok(())
    }

    /// Reproducir audio
    pub fn play_audio(&mut self, handle: UsbDeviceHandle, audio_data: &[u8]) -> Result<(), &'static str> {
        if handle.device_type != UsbDeviceType::Audio {
            return Err("No es un dispositivo de audio");
        }

        self.audio_driver.play_audio(handle.device_id, audio_data)?;
        Ok(())
    }

    /// Capturar audio
    pub fn capture_audio(&mut self, handle: UsbDeviceHandle, buffer: &mut [u8]) -> Result<usize, &'static str> {
        if handle.device_type != UsbDeviceType::Audio {
            return Err("No es un dispositivo de audio");
        }

        self.audio_driver.capture_audio(handle.device_id, buffer)
    }

    /// Controlar volumen
    pub fn set_audio_volume(&mut self, handle: UsbDeviceHandle, volume: u8) -> Result<(), &'static str> {
        if handle.device_type != UsbDeviceType::Audio {
            return Err("No es un dispositivo de audio");
        }

        self.audio_driver.set_volume(handle.device_id, volume)?;
        Ok(())
    }

    /// Silenciar/desilenciar audio
    pub fn set_audio_mute(&mut self, handle: UsbDeviceHandle, mute: bool) -> Result<(), &'static str> {
        if handle.device_type != UsbDeviceType::Audio {
            return Err("No es un dispositivo de audio");
        }

        self.audio_driver.set_mute(handle.device_id, mute)?;
        Ok(())
    }

    // === APIs de Video ===

    /// Configurar dispositivo de video
    pub fn configure_video_device(&mut self, handle: UsbDeviceHandle, config: UserVideoConfig) -> Result<(), &'static str> {
        if handle.device_type != UsbDeviceType::Video {
            return Err("No es un dispositivo de video");
        }

        serial_write_str(&alloc::format!(
            "USB_USER_API: Configurando video {} - {}x{}@{}fps, {:?}\n",
            handle.device_id,
            config.width,
            config.height,
            config.fps,
            config.format
        ));

        // Configurar resolución y FPS
        self.video_driver.set_resolution(handle.device_id, config.width, config.height)?;
        self.video_driver.set_fps(handle.device_id, config.fps)?;
        
        Ok(())
    }

    /// Iniciar streaming de video
    pub fn start_video_streaming(&mut self, handle: UsbDeviceHandle) -> Result<(), &'static str> {
        if handle.device_type != UsbDeviceType::Video {
            return Err("No es un dispositivo de video");
        }

        self.video_driver.start_streaming(handle.device_id)?;
        Ok(())
    }

    /// Detener streaming de video
    pub fn stop_video_streaming(&mut self, handle: UsbDeviceHandle) -> Result<(), &'static str> {
        if handle.device_type != UsbDeviceType::Video {
            return Err("No es un dispositivo de video");
        }

        self.video_driver.stop_streaming(handle.device_id)?;
        Ok(())
    }

    /// Capturar frame de video
    pub fn capture_video_frame(&mut self, handle: UsbDeviceHandle, buffer: &mut [u8]) -> Result<usize, &'static str> {
        if handle.device_type != UsbDeviceType::Video {
            return Err("No es un dispositivo de video");
        }

        self.video_driver.capture_frame(handle.device_id, buffer)
    }

    /// Controlar brillo de cámara
    pub fn set_video_brightness(&mut self, handle: UsbDeviceHandle, brightness: u8) -> Result<(), &'static str> {
        if handle.device_type != UsbDeviceType::Video {
            return Err("No es un dispositivo de video");
        }

        self.video_driver.set_brightness(handle.device_id, brightness)?;
        Ok(())
    }

    /// Controlar contraste de cámara
    pub fn set_video_contrast(&mut self, handle: UsbDeviceHandle, contrast: u8) -> Result<(), &'static str> {
        if handle.device_type != UsbDeviceType::Video {
            return Err("No es un dispositivo de video");
        }

        self.video_driver.set_contrast(handle.device_id, contrast)?;
        Ok(())
    }

    // === APIs de Red ===

    /// Configurar dispositivo de red
    pub fn configure_network_device(&mut self, handle: UsbDeviceHandle, config: UserNetworkConfig) -> Result<(), &'static str> {
        if handle.device_type != UsbDeviceType::Network {
            return Err("No es un dispositivo de red");
        }

        serial_write_str(&alloc::format!(
            "USB_USER_API: Configurando red {} - MTU: {}, IP: {}.{}.{}.{}\n",
            handle.device_id,
            config.mtu,
            config.ip_address[0],
            config.ip_address[1],
            config.ip_address[2],
            config.ip_address[3]
        ));

        self.network_driver.set_ip_address(handle.device_id, config.ip_address, config.subnet_mask, config.gateway)?;
        Ok(())
    }

    /// Activar interfaz de red
    pub fn bring_up_network_interface(&mut self, handle: UsbDeviceHandle) -> Result<(), &'static str> {
        if handle.device_type != UsbDeviceType::Network {
            return Err("No es un dispositivo de red");
        }

        self.network_driver.bring_up_interface(handle.device_id)?;
        Ok(())
    }

    /// Desactivar interfaz de red
    pub fn bring_down_network_interface(&mut self, handle: UsbDeviceHandle) -> Result<(), &'static str> {
        if handle.device_type != UsbDeviceType::Network {
            return Err("No es un dispositivo de red");
        }

        self.network_driver.bring_down_interface(handle.device_id)?;
        Ok(())
    }

    /// Enviar paquete de red
    pub fn send_network_packet(&mut self, handle: UsbDeviceHandle, packet: &[u8]) -> Result<(), &'static str> {
        if handle.device_type != UsbDeviceType::Network {
            return Err("No es un dispositivo de red");
        }

        self.network_driver.send_packet(handle.device_id, packet)?;
        Ok(())
    }

    /// Recibir paquete de red
    pub fn receive_network_packet(&mut self, handle: UsbDeviceHandle, buffer: &mut [u8]) -> Result<usize, &'static str> {
        if handle.device_type != UsbDeviceType::Network {
            return Err("No es un dispositivo de red");
        }

        self.network_driver.receive_packet(handle.device_id, buffer)
    }

    /// Obtener estadísticas de dispositivo de red
    pub fn get_network_device_stats(&self, handle: UsbDeviceHandle) -> Result<String, &'static str> {
        if handle.device_type != UsbDeviceType::Network {
            return Err("No es un dispositivo de red");
        }

        let device = self.network_driver.get_device_stats(handle.device_id)?;
        let stats = alloc::format!(
            "Dispositivo {}: Enviados: {} bytes ({} paquetes), Recibidos: {} bytes ({} paquetes), Velocidad: {} Mbps",
            handle.device_id,
            device.bytes_sent,
            device.packets_sent,
            device.bytes_received,
            device.packets_received,
            device.link_speed
        );
        
        Ok(stats)
    }

    // === APIs de Hot-Plug ===

    /// Registrar callback para eventos de hot-plug
    pub fn register_hotplug_callback(&mut self, callback: fn(UsbDeviceHandle, bool)) -> Result<(), &'static str> {
        serial_write_str("USB_USER_API: Callback de hot-plug registrado\n");
        // En un sistema real, esto registraría el callback en el manager de hot-plug
        Ok(())
    }

    /// Procesar eventos de hot-plug pendientes
    pub fn process_hotplug_events(&mut self) -> Result<(), &'static str> {
        if let Err(_) = self.hotplug_manager.process_hotplug_events() {
            return Err("Error al procesar eventos de hot-plug");
        }
        Ok(())
    }

    /// Obtener estadísticas completas del sistema USB
    pub fn get_system_stats(&self) -> UsbSystemStats {
        let audio_stats = self.audio_driver.get_driver_stats();
        let video_stats = self.video_driver.get_driver_stats();
        let network_stats = self.network_driver.get_driver_stats();
        let hotplug_stats = self.hotplug_manager.get_system_stats();

        UsbSystemStats {
            audio_devices: audio_stats.total_devices,
            video_devices: video_stats.total_devices,
            network_devices: network_stats.total_devices,
            total_controllers: hotplug_stats.total_controllers as u32,
            monitoring_enabled: hotplug_stats.monitoring_enabled,
            api_initialized: self.api_initialized.load(Ordering::SeqCst),
        }
    }
}

/// Estadísticas del sistema USB
#[derive(Debug, Clone)]
pub struct UsbSystemStats {
    pub audio_devices: u32,
    pub video_devices: u32,
    pub network_devices: u32,
    pub total_controllers: u32,
    pub monitoring_enabled: bool,
    pub api_initialized: bool,
}

/// Función principal de las APIs USB para user mode
pub fn usb_user_api_main() {
    serial_write_str("USB_USER_API: Iniciando APIs USB para user mode\n");
    
    let mut api_manager = UsbUserApiManager::new();
    
    if let Err(e) = api_manager.initialize() {
        serial_write_str(&alloc::format!("USB_USER_API: Error al inicializar: {}\n", e));
        return;
    }

    // Enumerar dispositivos disponibles
    let devices = api_manager.enumerate_devices();
    serial_write_str(&alloc::format!(
        "USB_USER_API: {} dispositivos USB disponibles para user mode\n",
        devices.len()
    ));

    // Mostrar información de cada dispositivo
    for device in devices {
        let device_type_name = match device.handle.device_type {
            UsbDeviceType::Audio => "Audio",
            UsbDeviceType::Video => "Video",
            UsbDeviceType::Network => "Network",
            UsbDeviceType::HID => "HID",
            UsbDeviceType::MassStorage => "Mass Storage",
            UsbDeviceType::Hub => "Hub",
            _ => "Unknown",
        };

        serial_write_str(&alloc::format!(
            "USB_USER_API: Dispositivo {} - {} {} {} (Puerto: {}, Velocidad: {})\n",
            device.handle.device_id,
            device.vendor_name,
            device.product_name,
            device_type_name,
            device.port_number,
            device.speed
        ));
    }

    // Mostrar estadísticas del sistema
    let stats = api_manager.get_system_stats();
    serial_write_str(&alloc::format!(
        "USB_USER_API: Sistema USB - Audio: {}, Video: {}, Red: {}, Controladores: {}\n",
        stats.audio_devices,
        stats.video_devices,
        stats.network_devices,
        stats.total_controllers
    ));

    serial_write_str("USB_USER_API: APIs USB para user mode listas\n");
}
