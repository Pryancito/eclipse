//! Syscalls USB para Eclipse OS
//! 
//! Implementa syscalls específicos para interactuar con dispositivos USB
//! desde aplicaciones de user mode.

use crate::debug::serial_write_str;
use crate::drivers::usb_user_api::{
    UsbUserApiManager, UsbDeviceHandle, UsbDeviceType, 
    UserAudioConfig, UserVideoConfig, UserNetworkConfig,
    UserUsbDeviceInfo, UsbSystemStats
};
use crate::drivers::usb_audio::AudioFormat;
use crate::drivers::usb_video::VideoFormat;
use crate::drivers::usb_network::NetworkInterfaceType;
use crate::drivers::usb_power_management::PowerManagementPolicy;
use alloc::vec::Vec;
use crate::synchronization::Mutex;

/// Manager global de APIs USB para syscalls
static USB_API_MANAGER: Mutex<Option<UsbUserApiManager>> = Mutex::new(None);

/// Inicializar manager USB para syscalls
pub fn init_usb_syscalls() -> Result<(), &'static str> {
    serial_write_str("USB_SYSCALLS: Inicializando syscalls USB\n");
    
    let mut manager = UsbUserApiManager::new();
    manager.initialize()?;
    
    let mut global_manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    *global_manager = Some(manager);
    
    serial_write_str("USB_SYSCALLS: Syscalls USB inicializados\n");
    Ok(())
}

/// Syscall: Enumerar dispositivos USB
pub fn sys_usb_enumerate_devices() -> Result<Vec<UserUsbDeviceInfo>, &'static str> {
    let manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    if let Some(ref api_manager) = manager.as_ref() {
        let devices = api_manager.enumerate_devices();
        serial_write_str(&alloc::format!("USB_SYSCALLS: Enumerados {} dispositivos USB\n", devices.len()));
        Ok(devices)
    } else {
        Err("USB API manager not initialized")
    }
}

/// Syscall: Abrir dispositivo USB
pub fn sys_usb_open_device(device_id: u32, device_type: UsbDeviceType) -> Result<UsbDeviceHandle, &'static str> {
    let mut manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    if let Some(ref mut api_manager) = manager.as_mut() {
        let handle = UsbDeviceHandle { device_id, device_type };
        api_manager.open_device(handle)?;
        serial_write_str(&alloc::format!("USB_SYSCALLS: Dispositivo {} abierto\n", device_id));
        Ok(handle)
    } else {
        Err("USB API manager not initialized")
    }
}

/// Syscall: Cerrar dispositivo USB
pub fn sys_usb_close_device(handle: UsbDeviceHandle) -> Result<(), &'static str> {
    let mut manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    if let Some(ref mut api_manager) = manager.as_mut() {
        api_manager.close_device(handle)?;
        serial_write_str(&alloc::format!("USB_SYSCALLS: Dispositivo {} cerrado\n", handle.device_id));
        Ok(())
    } else {
        Err("USB API manager not initialized")
    }
}

// === Syscalls de Audio ===

/// Syscall: Configurar dispositivo de audio
pub fn sys_usb_audio_configure(handle: UsbDeviceHandle, sample_rate: u32, channels: u8, bit_depth: u8, format: AudioFormat) -> Result<(), &'static str> {
    let mut manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    if let Some(ref mut api_manager) = manager.as_mut() {
        let config = UserAudioConfig {
            sample_rate,
            channels,
            bit_depth,
            format,
        };
        api_manager.configure_audio_device(handle, config)?;
        serial_write_str(&alloc::format!("USB_SYSCALLS: Audio {} configurado - {}Hz, {}ch, {}bit\n", handle.device_id, sample_rate, channels, bit_depth));
        Ok(())
    } else {
        Err("USB API manager not initialized")
    }
}

/// Syscall: Reproducir audio
pub fn sys_usb_audio_play(handle: UsbDeviceHandle, audio_data: &[u8]) -> Result<(), &'static str> {
    let mut manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    if let Some(ref mut api_manager) = manager.as_mut() {
        api_manager.play_audio(handle, audio_data)?;
        serial_write_str(&alloc::format!("USB_SYSCALLS: Reproduciendo {} bytes en audio {}\n", audio_data.len(), handle.device_id));
        Ok(())
    } else {
        Err("USB API manager not initialized")
    }
}

/// Syscall: Capturar audio
pub fn sys_usb_audio_capture(handle: UsbDeviceHandle, buffer: &mut [u8]) -> Result<usize, &'static str> {
    let mut manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    if let Some(ref mut api_manager) = manager.as_mut() {
        let captured = api_manager.capture_audio(handle, buffer)?;
        serial_write_str(&alloc::format!("USB_SYSCALLS: Capturados {} bytes de audio {}\n", captured, handle.device_id));
        Ok(captured)
    } else {
        Err("USB API manager not initialized")
    }
}

/// Syscall: Controlar volumen de audio
pub fn sys_usb_audio_set_volume(handle: UsbDeviceHandle, volume: u8) -> Result<(), &'static str> {
    let mut manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    if let Some(ref mut api_manager) = manager.as_mut() {
        api_manager.set_audio_volume(handle, volume)?;
        serial_write_str(&alloc::format!("USB_SYSCALLS: Volumen de audio {} establecido a {}%\n", handle.device_id, volume));
        Ok(())
    } else {
        Err("USB API manager not initialized")
    }
}

/// Syscall: Silenciar/desilenciar audio
pub fn sys_usb_audio_set_mute(handle: UsbDeviceHandle, mute: bool) -> Result<(), &'static str> {
    let mut manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    if let Some(ref mut api_manager) = manager.as_mut() {
        api_manager.set_audio_mute(handle, mute)?;
        serial_write_str(&alloc::format!("USB_SYSCALLS: Audio {} {} silenciado\n", handle.device_id, if mute { "" } else { "no" }));
        Ok(())
    } else {
        Err("USB API manager not initialized")
    }
}

// === Syscalls de Video ===

/// Syscall: Configurar dispositivo de video
pub fn sys_usb_video_configure(handle: UsbDeviceHandle, width: u32, height: u32, fps: u32, format: VideoFormat) -> Result<(), &'static str> {
    let mut manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    if let Some(ref mut api_manager) = manager.as_mut() {
        let config = UserVideoConfig {
            width,
            height,
            fps,
            format,
        };
        api_manager.configure_video_device(handle, config)?;
        serial_write_str(&alloc::format!("USB_SYSCALLS: Video {} configurado - {}x{}@{}fps\n", handle.device_id, width, height, fps));
        Ok(())
    } else {
        Err("USB API manager not initialized")
    }
}

/// Syscall: Iniciar streaming de video
pub fn sys_usb_video_start_streaming(handle: UsbDeviceHandle) -> Result<(), &'static str> {
    let mut manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    if let Some(ref mut api_manager) = manager.as_mut() {
        api_manager.start_video_streaming(handle)?;
        serial_write_str(&alloc::format!("USB_SYSCALLS: Streaming de video {} iniciado\n", handle.device_id));
        Ok(())
    } else {
        Err("USB API manager not initialized")
    }
}

/// Syscall: Detener streaming de video
pub fn sys_usb_video_stop_streaming(handle: UsbDeviceHandle) -> Result<(), &'static str> {
    let mut manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    if let Some(ref mut api_manager) = manager.as_mut() {
        api_manager.stop_video_streaming(handle)?;
        serial_write_str(&alloc::format!("USB_SYSCALLS: Streaming de video {} detenido\n", handle.device_id));
        Ok(())
    } else {
        Err("USB API manager not initialized")
    }
}

/// Syscall: Capturar frame de video
pub fn sys_usb_video_capture_frame(handle: UsbDeviceHandle, buffer: &mut [u8]) -> Result<usize, &'static str> {
    let mut manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    if let Some(ref mut api_manager) = manager.as_mut() {
        let captured = api_manager.capture_video_frame(handle, buffer)?;
        serial_write_str(&alloc::format!("USB_SYSCALLS: Frame de video {} capturado ({} bytes)\n", handle.device_id, captured));
        Ok(captured)
    } else {
        Err("USB API manager not initialized")
    }
}

/// Syscall: Controlar brillo de video
pub fn sys_usb_video_set_brightness(handle: UsbDeviceHandle, brightness: u8) -> Result<(), &'static str> {
    let mut manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    if let Some(ref mut api_manager) = manager.as_mut() {
        api_manager.set_video_brightness(handle, brightness)?;
        serial_write_str(&alloc::format!("USB_SYSCALLS: Brillo de video {} establecido a {}%\n", handle.device_id, brightness));
        Ok(())
    } else {
        Err("USB API manager not initialized")
    }
}

/// Syscall: Controlar contraste de video
pub fn sys_usb_video_set_contrast(handle: UsbDeviceHandle, contrast: u8) -> Result<(), &'static str> {
    let mut manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    if let Some(ref mut api_manager) = manager.as_mut() {
        api_manager.set_video_contrast(handle, contrast)?;
        serial_write_str(&alloc::format!("USB_SYSCALLS: Contraste de video {} establecido a {}%\n", handle.device_id, contrast));
        Ok(())
    } else {
        Err("USB API manager not initialized")
    }
}

// === Syscalls de Red ===

/// Syscall: Configurar dispositivo de red
pub fn sys_usb_network_configure(handle: UsbDeviceHandle, interface_type: NetworkInterfaceType, mtu: u32, ip: [u8; 4], subnet: [u8; 4], gateway: [u8; 4]) -> Result<(), &'static str> {
    let mut manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    if let Some(ref mut api_manager) = manager.as_mut() {
        let config = UserNetworkConfig {
            interface_type,
            mtu,
            ip_address: ip,
            subnet_mask: subnet,
            gateway,
        };
        api_manager.configure_network_device(handle, config)?;
        serial_write_str(&alloc::format!("USB_SYSCALLS: Red {} configurada - {}.{}.{}.{}\n", handle.device_id, ip[0], ip[1], ip[2], ip[3]));
        Ok(())
    } else {
        Err("USB API manager not initialized")
    }
}

/// Syscall: Activar interfaz de red
pub fn sys_usb_network_bring_up(handle: UsbDeviceHandle) -> Result<(), &'static str> {
    let mut manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    if let Some(ref mut api_manager) = manager.as_mut() {
        api_manager.bring_up_network_interface(handle)?;
        serial_write_str(&alloc::format!("USB_SYSCALLS: Interfaz de red {} activada\n", handle.device_id));
        Ok(())
    } else {
        Err("USB API manager not initialized")
    }
}

/// Syscall: Desactivar interfaz de red
pub fn sys_usb_network_bring_down(handle: UsbDeviceHandle) -> Result<(), &'static str> {
    let mut manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    if let Some(ref mut api_manager) = manager.as_mut() {
        api_manager.bring_down_network_interface(handle)?;
        serial_write_str(&alloc::format!("USB_SYSCALLS: Interfaz de red {} desactivada\n", handle.device_id));
        Ok(())
    } else {
        Err("USB API manager not initialized")
    }
}

/// Syscall: Enviar paquete de red
pub fn sys_usb_network_send_packet(handle: UsbDeviceHandle, packet: &[u8]) -> Result<(), &'static str> {
    let mut manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    if let Some(ref mut api_manager) = manager.as_mut() {
        api_manager.send_network_packet(handle, packet)?;
        serial_write_str(&alloc::format!("USB_SYSCALLS: Paquete de {} bytes enviado por red {}\n", packet.len(), handle.device_id));
        Ok(())
    } else {
        Err("USB API manager not initialized")
    }
}

/// Syscall: Recibir paquete de red
pub fn sys_usb_network_receive_packet(handle: UsbDeviceHandle, buffer: &mut [u8]) -> Result<usize, &'static str> {
    let mut manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    if let Some(ref mut api_manager) = manager.as_mut() {
        let received = api_manager.receive_network_packet(handle, buffer)?;
        serial_write_str(&alloc::format!("USB_SYSCALLS: Paquete de {} bytes recibido de red {}\n", received, handle.device_id));
        Ok(received)
    } else {
        Err("USB API manager not initialized")
    }
}

// === Syscalls de Sistema ===

/// Syscall: Obtener estadísticas del sistema USB
pub fn sys_usb_get_system_stats() -> Result<UsbSystemStats, &'static str> {
    let manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    if let Some(ref api_manager) = manager.as_ref() {
        let stats = api_manager.get_system_stats();
        serial_write_str(&alloc::format!("USB_SYSCALLS: Estadísticas obtenidas - Audio: {}, Video: {}, Red: {}\n", 
                                       stats.audio_devices, stats.video_devices, stats.network_devices));
        Ok(stats)
    } else {
        Err("USB API manager not initialized")
    }
}

/// Syscall: Procesar eventos de hot-plug
pub fn sys_usb_process_hotplug_events() -> Result<(), &'static str> {
    let mut manager = USB_API_MANAGER.lock().map_err(|_| "Failed to lock USB manager")?;
    if let Some(ref mut api_manager) = manager.as_mut() {
        api_manager.process_hotplug_events()?;
        serial_write_str("USB_SYSCALLS: Eventos de hot-plug procesados\n");
        Ok(())
    } else {
        Err("USB API manager not initialized")
    }
}

/// Función principal de syscalls USB
pub fn usb_syscalls_main() {
    serial_write_str("USB_SYSCALLS: Iniciando syscalls USB\n");
    
    if let Err(e) = init_usb_syscalls() {
        serial_write_str(&alloc::format!("USB_SYSCALLS: Error al inicializar: {}\n", e));
        return;
    }
    
    // Procesar eventos iniciales
    if let Err(e) = sys_usb_process_hotplug_events() {
        serial_write_str(&alloc::format!("USB_SYSCALLS: Error al procesar eventos: {}\n", e));
    }
    
    // Mostrar estadísticas iniciales
    if let Ok(stats) = sys_usb_get_system_stats() {
        serial_write_str(&alloc::format!(
            "USB_SYSCALLS: Sistema USB listo - {} audio, {} video, {} red\n",
            stats.audio_devices,
            stats.video_devices,
            stats.network_devices
        ));
    }
    
    serial_write_str("USB_SYSCALLS: Syscalls USB inicializados\n");
}
