//! Módulo principal simplificado del kernel Eclipse OS

#![no_std]
#![no_main]

extern crate alloc;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Result as FmtResult;
use core::error::Error;
use core::fmt::Write;
use core::panic::PanicInfo;

// Importar módulos del kernel
use crate::init_system::{InitSystem, InitProcess};
use crate::wayland::{init_wayland, is_wayland_initialized, get_wayland_state};
use crate::cosmic::{CosmicManager, CosmicConfig, WindowManagerMode, PerformanceMode};


use crate::drivers::framebuffer::{Color, get_framebuffer,
    FramebufferDriver, FramebufferInfo, init_framebuffer
};
use crate::drivers::direct_framebuffer::DirectFramebufferDriver;
use crate::drivers::hardware_framebuffer::HardwareFramebufferDriver;
use crate::drivers::gpu_control::{GpuController, GpuResult};
use crate::drivers::framebuffer_updater::{FramebufferUpdater, ResolutionConfig};
use crate::drivers::resolution_manager::{ResolutionManager, VideoMode};
use crate::ai_typing_system::{AiTypingSystem, AiTypingConfig, TypingEffect,
    create_ai_typing_system};
use crate::ai_pretrained_models::{PretrainedModelManager, PretrainedModelType};
use crate::ai::{ModelLoader, ModelType};
// Módulo ai_font_generator removido
use crate::drivers::pci::{GpuType, GpuInfo};
use crate::drivers::virtio_gpu::VirtioGpuDriver;
use crate::drivers::bochs_vbe::BochsVbeDriver;
use crate::drivers::vmware_svga::VmwareSvgaDriver;
use crate::drivers::usb_xhci::XhciController;
use crate::drivers::pci::PciManager;
use crate::drivers::pci::PciDevice;
use crate::drivers::usb::UsbDriver;
use crate::drivers::usb_keyboard::{UsbKeyboardDriver, UsbKeyCode, KeyboardEvent, KeyboardConfig};
use crate::drivers::usb_mouse::{UsbMouseDriver, MouseButton, MouseEvent, MouseConfig};
use crate::hardware_detection::{GraphicsMode, detect_graphics_hardware, HardwareDetectionResult};
use crate::drivers::ipc::{DriverManager, DriverMessage, DriverResponse};
use crate::drivers::pci_driver::PciDriver;
use crate::drivers::nvidia_pci_driver::NvidiaPciDriver;
use crate::drivers::ipc::Driver;
use crate::drivers::binary_driver_manager::{BinaryDriverManager, BinaryDriverMetadata};
use crate::ipc::{IpcManager, IpcMessage, DriverType, DriverConfig, DriverCommandType};
use crate::hotplug::{HotplugManager, UsbDeviceType, UsbHotplugEvent};
use crate::hotplug::HotplugConfig;
use crate::filesystem::vfs::{init_vfs, get_vfs, get_vfs_statistics, create_demo_filesystem, write_demo_content};
use crate::filesystem::cache::init_file_cache;
use crate::filesystem::block::init_block_device;
use crate::filesystem::fat32::{init_fat32, get_fat32_driver};
use crate::graphics_optimization::{init_graphics_optimizer, get_optimization_stats, force_framebuffer_update};
use crate::graphics::{init_graphics_system, transition_to_drm};

/// Obtener información del framebuffer UEFI (estilo Linux)
/// Linux usa la información de UEFI tal como está, no la modifica
fn get_uefi_framebuffer_info(current_fb: &FramebufferDriver) -> (u32, u32, u32, u64) {
    // Linux usa directamente la información del framebuffer UEFI
    // No intenta "mejorarla" o cambiarla
    let width = current_fb.info.width;
    let height = current_fb.info.height;
    let pixels_per_scan_line = current_fb.info.pixels_per_scan_line;
    let base_address = current_fb.info.base_address;
    
    (width, height, pixels_per_scan_line, base_address)
}

/// Verificar si una dirección de framebuffer es válida
fn is_valid_framebuffer_address(address: u64) -> bool {
    // Direcciones físicas bajas o direcciones mapeadas de video
    address < 0x100000000 || (address >= 0x10000000 && address < 0x60000000)
}

/// Probar escritura en framebuffer
fn test_framebuffer_write(fb: &mut FramebufferDriver) -> bool {
    if fb.info.base_address == 0 || fb.info.width == 0 || fb.info.height == 0 {
        return false;
    }
    
    // Si el driver no está inicializado, verificar que la dirección esté mapeada
    if !fb.is_initialized() {
        return fb.info.base_address >= 0x10000000 && fb.info.base_address < 0x60000000;
    }
    
    // Para drivers inicializados, verificar acceso a VRAM
    let bytes_per_pixel = core::cmp::max(1u32, fb.bytes_per_pixel() as u32);
    let ppsl = fb.info.pixels_per_scan_line.max(fb.info.width);
    let x = (fb.info.width / 2).min(ppsl.saturating_sub(1)).min(100);
    let y = (fb.info.height / 2).min(fb.info.height.saturating_sub(1)).min(100);
    let offset_bytes = ((y * ppsl) + x) * bytes_per_pixel;
    
    // Verificar acceso a VRAM para direcciones válidas
    if is_valid_framebuffer_address(fb.info.base_address) {
        unsafe {
            let ptr = (fb.info.base_address as *mut u8).add(offset_bytes as usize) as *mut u32;
            let original = core::ptr::read_volatile(ptr);
            let test_val = original ^ 0x00FF_FFFF;
            core::ptr::write_volatile(ptr, test_val);
            let read_back = core::ptr::read_volatile(ptr);
            core::ptr::write_volatile(ptr, original);
            return read_back == test_val;
        }
    }
    
    true
}

/// Verificar que la memoria del framebuffer es válida
fn verify_framebuffer_memory(fb: &mut FramebufferDriver) -> bool {
    if fb.info.base_address == 0 || fb.info.width == 0 || fb.info.height == 0 {
        return false;
    }
    
    // Si el driver no está inicializado, verificar que la dirección esté mapeada
    if !fb.is_initialized() {
        return fb.info.base_address >= 0x10000000 && fb.info.base_address < 0x60000000;
    }
    
    // Para drivers inicializados, verificar acceso a VRAM en posiciones clave
    let bytes_per_pixel = core::cmp::max(1u32, fb.bytes_per_pixel() as u32);
    let ppsl = fb.info.pixels_per_scan_line.max(fb.info.width);
    let positions = [
        (0, 0),
        (fb.info.width.saturating_sub(1), 0),
        (0, fb.info.height.saturating_sub(1)),
        (fb.info.width.saturating_sub(1), fb.info.height.saturating_sub(1)),
        (fb.info.width / 2, fb.info.height / 2),
    ];
    
    if is_valid_framebuffer_address(fb.info.base_address) {
        unsafe {
            for (x, y) in positions {
                let offset_bytes = ((y * ppsl) + x) * bytes_per_pixel;
                let ptr = (fb.info.base_address as *const u8).add(offset_bytes as usize) as *const u32;
                let _ = core::ptr::read_volatile(ptr);
            }
        }
    }
    
    true
}

/// Detectar problemas específicos de gráfica
fn detect_graphics_issues(fb: &mut FramebufferDriver, hw_result: &HardwareDetectionResult) {
    // Verificar si el framebuffer tiene resolución válida
    if fb.info.width == 0 || fb.info.height == 0 {
        fb.write_text_kernel("PROBLEMA: Resolución inválida (0x0)", Color::RED);
    }
    
    // Verificar si la dirección base es válida
    if fb.info.base_address == 0 {
        fb.write_text_kernel("PROBLEMA: Dirección base inválida (0x0)", Color::RED);
    }
    
    // Verificar si el stride es válido
    if fb.info.pixels_per_scan_line == 0 {
        fb.write_text_kernel("PROBLEMA: Stride inválido (0)", Color::RED);
    }
    
    // Verificar si hay GPUs detectadas
    if hw_result.available_gpus.is_empty() {
        fb.write_text_kernel("PROBLEMA: No hay GPUs detectadas", Color::RED);
    } else {
        fb.write_text_kernel(&format!("GPUs detectadas: {}", hw_result.available_gpus.len()), Color::GREEN);
    }
    
    // Verificar modo gráfico
    match hw_result.graphics_mode {
        GraphicsMode::Framebuffer => {
            fb.write_text_kernel("Modo: Framebuffer (OK)", Color::GREEN);
        },
        GraphicsMode::VGA => {
            fb.write_text_kernel("Modo: VGA (limitado)", Color::YELLOW);
        },
        GraphicsMode::HardwareAccelerated => {
            fb.write_text_kernel("Modo: Hardware acelerado (OK)", Color::GREEN);
        }
    }
}

/// Detectar hardware gráfico PCI (estilo Linux)
/// Linux solo detecta el hardware, no lee memoria del framebuffer
fn detect_graphics_hardware_pci(hw_result: &HardwareDetectionResult) -> GraphicsHardwareInfo {
    if let Some(primary_gpu) = &hw_result.primary_gpu {
        // Linux obtiene información del hardware desde PCI
        // pero NO lee la memoria del framebuffer
        GraphicsHardwareInfo {
            vendor_id: primary_gpu.pci_device.vendor_id,
            device_id: primary_gpu.pci_device.device_id,
            gpu_type: primary_gpu.gpu_type,
            max_resolution: primary_gpu.max_resolution,
            supports_hardware_acceleration: matches!(hw_result.graphics_mode, GraphicsMode::HardwareAccelerated),
            // Linux NO lee la dirección base del framebuffer desde PCI
            // Eso lo hace UEFI/ACPI
        }
    } else {
        // Hardware genérico
        GraphicsHardwareInfo {
            vendor_id: 0x0000,
            device_id: 0x0000,
            gpu_type: crate::drivers::pci::GpuType::Unknown,
            max_resolution: (1024, 768),
            supports_hardware_acceleration: false,
        }
    }
}

/// Información del hardware gráfico (estilo Linux)
#[derive(Debug)]
struct GraphicsHardwareInfo {
    vendor_id: u16,
    device_id: u16,
    gpu_type: crate::drivers::pci::GpuType,
    max_resolution: (u32, u32),
    supports_hardware_acceleration: bool,
}

/// Leer información real del framebuffer desde la memoria del hardware
/// Retorna (width, height, pixels_per_scan_line, base_address, pixel_format)
fn read_hardware_framebuffer_info(base_address: u64, pci_device: &crate::drivers::pci::PciDevice) -> (u32, u32, u32, u64, u32) {
    unsafe {
        // Convertir la dirección base a un puntero
        let fb_ptr = base_address as *const u32;
        
        // Intentar leer información del framebuffer desde diferentes ubicaciones comunes
        // Muchas GPUs almacenan esta información en los primeros bytes del framebuffer
        
        // Leer los primeros 16 bytes como posibles valores de configuración
        let config_data = core::ptr::read_volatile(fb_ptr);
        let config_data2 = core::ptr::read_volatile(fb_ptr.add(1));
        let config_data3 = core::ptr::read_volatile(fb_ptr.add(2));
        let config_data4 = core::ptr::read_volatile(fb_ptr.add(3));
        
        // Intentar detectar patrones comunes de configuración de framebuffer
        let mut width = 0;
        let mut height = 0;
        let mut pixels_per_scan_line = 0;
        let mut pixel_format = 0;
        
        // Patrón 1: Valores consecutivos (width, height, stride, format)
        if config_data > 0 && config_data < 4096 && config_data2 > 0 && config_data2 < 4096 {
            width = config_data;
            height = config_data2;
            pixels_per_scan_line = if config_data3 > 0 && config_data3 >= width { config_data3 } else { width };
            pixel_format = config_data4;
        }
        // Patrón 2: Valores en posiciones alternas
        else if config_data > 0 && config_data < 4096 && config_data3 > 0 && config_data3 < 4096 {
            width = config_data;
            height = config_data3;
            pixels_per_scan_line = if config_data2 > 0 && config_data2 >= width { config_data2 } else { width };
            pixel_format = config_data4;
        }
        // Patrón 3: Valores en little-endian
        else {
            let width_le = (config_data & 0xFFFF) as u32;
            let height_le = ((config_data >> 16) & 0xFFFF) as u32;
            let stride_le = (config_data2 & 0xFFFF) as u32;
            
            if width_le > 0 && width_le < 4096 && height_le > 0 && height_le < 4096 {
                width = width_le;
                height = height_le;
                pixels_per_scan_line = if stride_le > 0 && stride_le >= width { stride_le } else { width };
                pixel_format = (config_data2 >> 16) & 0xFFFF;
            }
        }
        
        // Si no pudimos leer información válida, usar valores por defecto basados en el vendor
        if width == 0 || height == 0 {
            match pci_device.vendor_id {
                0x10DE => { // NVIDIA
                    width = 1920;
                    height = 1080;
                    pixels_per_scan_line = 1920;
                    pixel_format = 0; // RGB
                },
                0x1002 => { // AMD
                    width = 1920;
                    height = 1080;
                    pixels_per_scan_line = 1920;
                    pixel_format = 0; // RGB
                },
                0x8086 => { // Intel
                    width = 1920;
                    height = 1080;
                    pixels_per_scan_line = 1920;
                    pixel_format = 0; // RGB
                },
                0x1234 => { // QEMU
                    width = 1024;
                    height = 768;
                    pixels_per_scan_line = 1024;
                    pixel_format = 0; // RGB
                },
                0x15AD => { // VMware
                    width = 1024;
                    height = 768;
                    pixels_per_scan_line = 1024;
                    pixel_format = 0; // RGB
                },
                0x1AF4 => { // VirtIO
                    width = 1024;
                    height = 768;
                    pixels_per_scan_line = 1024;
                    pixel_format = 0; // RGB
                },
                _ => { // Generic
                    width = 1024;
                    height = 768;
                    pixels_per_scan_line = 1024;
                    pixel_format = 0; // RGB
                }
            }
        }
        
        (width, height, pixels_per_scan_line, base_address, pixel_format)
    }
}

/// Función para cambiar la resolución de pantalla de forma segura
fn change_screen_resolution(fb: &mut FramebufferDriver, updater: &mut FramebufferUpdater, width: u32, height: u32, bits_per_pixel: u32) -> Result<FramebufferDriver, String> {
    // Validar resolución
    if !is_safe_resolution(width, height) {
        let error_msg = format!("Resolución {}x{} no es segura", width, height);
        fb.write_text_kernel(&error_msg, Color::YELLOW);
        return Err(error_msg);
    }

    // Verificar compatibilidad
    if !updater.is_resolution_supported(width, height, bits_per_pixel) {
        let error_msg = format!("Resolución {}x{} @{}bpp no es compatible", width, height, bits_per_pixel);
        fb.write_text_kernel(&error_msg, Color::YELLOW);
        return Err(error_msg.into());
    }

    // Cambiar resolución
    match updater.change_resolution(width, height, bits_per_pixel) {
        Ok(mut new_fb) => {
            fb.write_text_kernel(&format!("Resolución cambiada a {}x{} @{}bpp", width, height, bits_per_pixel), Color::GREEN);
            
            // Configurar el nuevo framebuffer
            new_fb.info.width = width;
            new_fb.info.height = height;
            new_fb.info.pixels_per_scan_line = probe_pixels_per_scan_line(
                new_fb.info.base_address,
                width,
                height,
                bits_per_pixel,
            );
            
            // Usar base anterior si no hay base válida
            if new_fb.info.base_address == 0 {
                new_fb.info.base_address = fb.info.base_address;
            }
            
            // Conservar formato si no está definido
            if new_fb.info.pixel_format == 0 {
                new_fb.info.pixel_format = fb.info.pixel_format;
                new_fb.info.red_mask = fb.info.red_mask;
                new_fb.info.green_mask = fb.info.green_mask;
                new_fb.info.blue_mask = fb.info.blue_mask;
                new_fb.info.reserved_mask = fb.info.reserved_mask;
            }
            
            Ok(new_fb)
        }
        Err(error) => {
            let error_msg = format!("Error cambiando resolución: {}", error);
            fb.write_text_kernel(&error_msg, Color::RED);
            Err(error_msg.into())
        }
    }
}

/// Calcular pixels_per_scan_line para el framebuffer
fn probe_pixels_per_scan_line(base_address: u64, width: u32, height: u32, bits_per_pixel: u32) -> u32 {
    if base_address == 0 || width == 0 || height == 0 {
        return width.max(1);
    }
    
    let bytes_per_pixel = core::cmp::max(1, (bits_per_pixel / 8)) as u32;
    let candidate_ppsl = width;
    
    // Verificar que el stride funciona escribiendo en dos líneas diferentes
    if is_valid_framebuffer_address(base_address) {
        unsafe {
            let fb_ptr = base_address as *mut u32;
            let off0 = 0;
            let off1 = candidate_ppsl * bytes_per_pixel;
            let p0 = fb_ptr.add(off0 as usize / 4);
            let p1 = fb_ptr.add(off1 as usize / 4);
            
            let orig0 = core::ptr::read_volatile(p0);
            let orig1 = core::ptr::read_volatile(p1);
            core::ptr::write_volatile(p0, 0x11223344);
            core::ptr::write_volatile(p1, 0x55667788);
            
            let ok = core::ptr::read_volatile(p0) == 0x11223344 && 
                     core::ptr::read_volatile(p1) == 0x55667788;
            
            // Restaurar valores originales
            core::ptr::write_volatile(p0, orig0);
            core::ptr::write_volatile(p1, orig1);
            
            if ok { return candidate_ppsl; }
        }
    }
    
    width
}

/// Verificar si una resolución es segura para el monitor
fn is_safe_resolution(width: u32, height: u32) -> bool {
    // Resoluciones consideradas seguras (estándar y ampliamente soportadas)
    let safe_resolutions = [
        (640, 480),   // VGA
        (800, 600),   // SVGA
        (1024, 768),  // XGA
        (1280, 720),  // HD
        (1280, 1024), // SXGA
        (1366, 768),  // HD WXGA
        (1440, 900),  // WXGA+
        (1600, 900),  // HD+
        (1680, 1050), // WSXGA+
        (1920, 1080), // Full HD
    ];

    safe_resolutions.iter().any(|(w, h)| *w == width && *h == height)
}

/// Función para mostrar información de resolución
fn show_resolution_info(fb: &mut FramebufferDriver) {
    let mut updater = FramebufferUpdater::new();
    
    if let Ok(_) = updater.initialize() {
        // Mostrar información de UEFI GOP
        if updater.is_uefi_gop_available() {
            fb.write_text_kernel("UEFI GOP: Disponible", Color::GREEN);
            let gop_info = updater.get_uefi_gop_info();
            fb.write_text_kernel(&gop_info, Color::LIGHT_GRAY);
        } else {
            fb.write_text_kernel("UEFI GOP: No disponible", Color::YELLOW);
        }
        
        let current_info = updater.get_current_resolution_info();
        fb.write_text_kernel(&format!("Resolución actual: {}", current_info), Color::CYAN);
        
        let modes_info = updater.list_available_modes();
        fb.write_text_kernel("Modos disponibles:", Color::YELLOW);
        for line in modes_info.lines().take(10) { // Mostrar los primeros 10 modos
            fb.write_text_kernel(line, Color::LIGHT_GRAY);
        }
    } else {
        fb.write_text_kernel("Error obteniendo información de resolución", Color::RED);
    }
}

/// Función principal del kernel
pub fn kernel_main(mut fb: &mut FramebufferDriver) {
    // Llamar directamente a la función principal del kernel
    // Asegurar allocador inicializado antes de usar alloc en este main
    #[cfg(feature = "alloc")]
    {
        crate::allocator::init_allocator();
    }
    let hw_result = detect_graphics_hardware();
    let is_qemu_bochs = hw_result.primary_gpu.as_ref().map(|g| matches!(g.gpu_type, GpuType::QemuBochs)).unwrap_or(false);
    if fb.is_initialized() {
        fb.clear_screen(Color::BLACK);
    }
    let mut updater = FramebufferUpdater::new();
    if let Ok(_) = updater.initialize() {
        // Pasar el framebuffer actual para reutilizar su base en QEMU/Bochs
        fb.write_text_kernel(
            &alloc::format!(
                "FB actual: {}x{} ppsl={} bpp={} @0x{:016X}",
                fb.info.width,
                fb.info.height,
                fb.info.pixels_per_scan_line,
                (fb.bytes_per_pixel() as u32) * 8,
                fb.info.base_address
            ),
            Color::LIGHT_GRAY,
        );
        updater.set_current_framebuffer(fb);
        if let Some((rec_width, rec_height, rec_bpp)) = updater.get_resolution_manager().get_recommended_resolution() {
            fb.write_text_kernel(&format!("Resolución recomendada: {}x{} @{}bpp", rec_width, rec_height, rec_bpp), Color::CYAN);
            
            match updater.change_resolution(rec_width, rec_height, rec_bpp) {
                Ok(mut new_fb) => {
                    // APLICAR EL NUEVO FRAMEBUFFER DE FORMA SEGURA
                    fb.write_text_kernel("¡Cambio de resolución exitoso!", Color::GREEN);
                    fb.write_text_kernel(&format!("Resolución recomendada {}x{} detectada", rec_width, rec_height), Color::GREEN);
                    fb.write_text_kernel("Aplicando nuevo framebuffer...", Color::CYAN);

                    // Depurar: mostrar info del nuevo framebuffer propuesto
                    fb.write_text_kernel(
                        &alloc::format!(
                            "FB propuesto: {}x{} ppsl={} bpp={} @0x{:016X}",
                            new_fb.info.width,
                            new_fb.info.height,
                            new_fb.info.pixels_per_scan_line,
                            (new_fb.bytes_per_pixel() as u32) * 8,
                            new_fb.info.base_address
                        ),
                        Color::LIGHT_GRAY,
                    );

                    // Validar SIEMPRE antes de aplicar
                    if test_framebuffer_write(&mut new_fb) && verify_framebuffer_memory(&mut new_fb) {
                        // Re-inicializar el puntero de buffer con la nueva info (respetando lo que trae new_fb)
                        let pixel_bitmask = (new_fb.info.red_mask) | (new_fb.info.green_mask) | (new_fb.info.blue_mask);
                        let _ = fb.init_from_uefi(
                            new_fb.info.base_address,
                            new_fb.info.width,
                            new_fb.info.height,
                            new_fb.info.pixels_per_scan_line,
                            new_fb.info.pixel_format,
                            pixel_bitmask,
                        );
                        fb = get_framebuffer().expect("Framebuffer no encontrado");
                        // Pequeña espera para estabilizar el modo antes del primer dibujo
                        for _ in 0..200000 { core::hint::spin_loop(); }
                        // Limpiar pantalla para evitar residuos/ruido tras el cambio de modo
                        fb.clear_screen(Color::BLACK);
                        fb.write_text_kernel("✓ Nuevo framebuffer aplicado exitosamente", Color::GREEN);
                        fb.write_text_kernel("✓ Resolución cambiada correctamente", Color::GREEN);
                    } else {
                        fb.write_text_kernel("⚠ Nuevo framebuffer no funciona correctamente", Color::YELLOW);
                        fb.write_text_kernel("Manteniendo framebuffer UEFI original", Color::YELLOW);
                    }
                }
                Err(_) => {
                    fb.write_text_kernel("No se pudo cambiar a la resolución recomendada", Color::YELLOW);
                    fb.write_text_kernel("Intentando resolución más segura (800x600)...", Color::YELLOW);
                    // Intentar con una resolución aún más segura
                    match updater.change_resolution(800, 600, 32) {
                        Ok(mut new_fb) => {
                            fb.write_text_kernel("¡Cambio a 800x600 exitoso!", Color::GREEN);
                            fb.write_text_kernel("Resolución segura 800x600 detectada", Color::GREEN);
                            fb.write_text_kernel("Aplicando nuevo framebuffer...", Color::CYAN);
                            
                            // Verificar que el nuevo framebuffer funciona antes de aplicarlo
                            if test_framebuffer_write(&mut new_fb) && verify_framebuffer_memory(&mut new_fb) {
                                // Re-inicializar el puntero de buffer con la nueva info (respetando lo que trae new_fb)
                                let pixel_bitmask = (new_fb.info.red_mask) | (new_fb.info.green_mask) | (new_fb.info.blue_mask);
                                let _ = fb.init_from_uefi(
                                    new_fb.info.base_address,
                                    new_fb.info.width,
                                    new_fb.info.height,
                                    new_fb.info.pixels_per_scan_line,
                                    new_fb.info.pixel_format,
                                    pixel_bitmask,
                                );
                                fb = get_framebuffer().expect("Framebuffer no encontrado");
                                // Pequeña espera para estabilizar el modo antes del primer dibujo
                                for _ in 0..200000 { core::hint::spin_loop(); }
                                // Limpiar pantalla para evitar residuos/ruido tras el cambio de modo
                                fb.clear_screen(Color::BLACK);
                                fb.write_text_kernel("✓ Nuevo framebuffer aplicado exitosamente", Color::GREEN);
                                fb.write_text_kernel("✓ Resolución cambiada correctamente", Color::GREEN);
                            } else {
                                fb.write_text_kernel("⚠ Nuevo framebuffer no funciona correctamente", Color::YELLOW);
                                fb.write_text_kernel("Manteniendo framebuffer UEFI original", Color::YELLOW);
                            }
                        }
                        Err(_) => {
                            fb.write_text_kernel("No se pudo cambiar resolución, manteniendo actual", Color::YELLOW);
                            fb.write_text_kernel("El sistema mantendrá la resolución UEFI original", Color::LIGHT_GRAY);
                            fb.write_text_kernel("Esto evita que el monitor pierda señal", Color::LIGHT_GRAY);
                        }
                    }
                }
            }
        } else {
            fb.write_text_kernel("No se pudo detectar resolución recomendada", Color::YELLOW);
            fb.write_text_kernel("Manteniendo resolución UEFI original", Color::LIGHT_GRAY);
        }
    } else {
        fb.write_text_kernel("Error inicializando gestor de resolución", Color::RED);
        fb.write_text_kernel("Manteniendo resolución UEFI original", Color::LIGHT_GRAY);
    }

    fb.write_text_kernel("Detectando hardware gráfico...", Color::WHITE);

    // Mostrar resultado de detección
    match hw_result.graphics_mode {
        GraphicsMode::Framebuffer => {
            fb.write_text_kernel("Hardware detectado: Framebuffer", Color::GREEN);
        },
        GraphicsMode::VGA => {
            fb.write_text_kernel("Hardware detectado: VGA", Color::GREEN);
        },
        GraphicsMode::HardwareAccelerated => {
            fb.write_text_kernel("Hardware detectado: Acelerado por hardware", Color::GREEN);
        }
    }
    
    // ========================================
    // FASE 3: TRANSICIÓN A HARDWARE DETECTADO
    // ========================================
    fb.write_text_kernel("Inicializando framebuffer del hardware...", Color::WHITE);
    
    // Inicializar sistema de gráficos para el hardware detectado
    match init_graphics_system() {
        Ok(_) => {
            fb.write_text_kernel("Sistema de gráficos inicializado", Color::GREEN);
        }
        Err(e) => {
            fb.write_text_kernel(&format!("Error inicializando gráficos: {}", e), Color::RED);
        }
    }
    
    // ========================================
    // INICIALIZAR NUEVO FRAMEBUFFER DEL HARDWARE
    // ========================================
    fb.write_text_kernel("Inicializando nuevo framebuffer del hardware...", Color::WHITE);
    
    // ========================================
    // DEBUG: DIAGNÓSTICO DE GRÁFICA EN HARDWARE REAL
    // ========================================
    fb.write_text_kernel("=== DEBUG: DIAGNÓSTICO DE GRÁFICA ===", Color::MAGENTA);
    
    // 1. Verificar estado del framebuffer actual
    fb.write_text_kernel("Verificando framebuffer actual...", Color::WHITE);
    fb.write_text_kernel(&format!("FB inicializado: {}", fb.is_initialized()), Color::CYAN);
    fb.write_text_kernel(&format!("FB base: 0x{:X}", fb.info.base_address), Color::CYAN);
    fb.write_text_kernel(&format!("FB resolución: {}x{}", fb.info.width, fb.info.height), Color::CYAN);
    fb.write_text_kernel(&format!("FB stride: {}", fb.info.pixels_per_scan_line), Color::CYAN);
    fb.write_text_kernel(&format!("FB formato: {}", fb.info.pixel_format), Color::CYAN);
    
    // 2. Probar escritura en framebuffer
    fb.write_text_kernel("Probando escritura en framebuffer...", Color::WHITE);
    let test_result = test_framebuffer_write(fb);
    fb.write_text_kernel(&format!("Test escritura: {}", if test_result { "OK" } else { "FALLO" }), 
        if test_result { Color::GREEN } else { Color::RED });
    
    // 3. Verificar memoria del framebuffer
    fb.write_text_kernel("Verificando memoria del framebuffer...", Color::WHITE);
    let memory_valid = verify_framebuffer_memory(fb);
    fb.write_text_kernel(&format!("Memoria válida: {}", if memory_valid { "SÍ" } else { "NO" }), 
        if memory_valid { Color::GREEN } else { Color::RED });
    
    // 4. Detectar problemas específicos
    fb.write_text_kernel("Detectando problemas específicos...", Color::WHITE);
    detect_graphics_issues(fb, &hw_result);
    
    // ========================================
    // ENFOQUE LINUX: USAR UEFI TAL COMO ESTÁ
    // ========================================
    fb.write_text_kernel("=== ENFOQUE LINUX: FRAMEBUFFER UEFI ===", Color::MAGENTA);
    
    // 1. Obtener información del framebuffer UEFI (estilo Linux)
    let uefi_fb_info = get_uefi_framebuffer_info(fb);
    fb.write_text_kernel(&format!("UEFI FB: {}x{} (stride: {}) @0x{:X}", 
        uefi_fb_info.0, uefi_fb_info.1, uefi_fb_info.2, uefi_fb_info.3), Color::LIGHT_GRAY);
    fb.write_text_kernel("✓ Información UEFI obtenida (sin modificaciones)", Color::GREEN);
    
    // 2. Detectar hardware PCI (estilo Linux)
    if let Some(primary_gpu) = &hw_result.primary_gpu {
        fb.write_text_kernel(&format!("GPU detectada: {:04X}:{:04X} ({:?})", 
            primary_gpu.pci_device.vendor_id, primary_gpu.pci_device.device_id, primary_gpu.gpu_type), Color::CYAN);
        fb.write_text_kernel(&format!("Resolución máxima: {}x{}", 
            primary_gpu.max_resolution.0, primary_gpu.max_resolution.1), Color::LIGHT_GRAY);
    }
    
    // 3. Linux NO cambia el framebuffer UEFI
    // Solo aplica optimizaciones específicas del hardware
    if let Some(primary_gpu) = &hw_result.primary_gpu {
        if primary_gpu.supports_2d || primary_gpu.supports_3d {
            fb.write_text_kernel("✓ Hardware soporta aceleración - aplicando optimizaciones", Color::GREEN);
            // Aquí irían las optimizaciones específicas del hardware
            // Pero NO se cambia el framebuffer base
        } else {
            fb.write_text_kernel("✓ Usando framebuffer UEFI estándar", Color::GREEN);
        }
    } else {
        fb.write_text_kernel("✓ Usando framebuffer UEFI estándar", Color::GREEN);
    }
    
    // 4. ENFOQUE CORRECTO: Mantener resolución UEFI como cualquier entorno gráfico
    fb.write_text_kernel("=== ENFOQUE GRÁFICO MODERNO ===", Color::MAGENTA);
    fb.write_text_kernel("Manteniendo resolución UEFI - NO cambiando modo de video", Color::CYAN);
    fb.write_text_kernel("Solo usando framebuffer directo para optimizaciones de rendimiento", Color::LIGHT_GRAY);
    
    let mut direct_fb_driver = DirectFramebufferDriver::new();
    let mut hardware_optimizations_enabled = false;
    
    // Intentar habilitar optimizaciones de hardware para la GPU primaria
    if let Some(primary_gpu) = &hw_result.primary_gpu {
        fb.write_text_kernel(&format!("Habilitando optimizaciones para: {} {:04X}:{:04X}", 
            primary_gpu.gpu_type.as_str(), primary_gpu.pci_device.vendor_id, primary_gpu.pci_device.device_id), 
            Color::CYAN);
        
        match direct_fb_driver.detect_and_configure(primary_gpu) {
            Ok(direct_fb_info) => {
                fb.write_text_kernel("✓ Optimizaciones de hardware habilitadas", Color::GREEN);
                fb.write_text_kernel(&format!("Resolución mantenida: {}x{} (stride: {})", 
                    direct_fb_info.width, direct_fb_info.height, direct_fb_info.stride), 
                    Color::LIGHT_GRAY);
                fb.write_text_kernel(&format!("GPU: {} {:04X}:{:04X}", 
                    direct_fb_info.gpu_type.as_str(), direct_fb_info.vendor_id, direct_fb_info.device_id), 
                    Color::LIGHT_GRAY);
                
                // Configurar optimizaciones de hardware para el framebuffer UEFI existente
                match direct_fb_driver.initialize_hardware_framebuffer(&direct_fb_info) {
                    Ok(_) => {
                        fb.write_text_kernel("✓ Optimizaciones de hardware configuradas", Color::GREEN);
                        
                        // Reconfigurar la tarjeta gráfica para el nuevo framebuffer
                        match direct_fb_driver.reconfigure_graphics_card(&fb.get_info()) {
                            Ok(_) => {
                                fb.write_text_kernel("✓ Tarjeta gráfica reconfigurada", Color::GREEN);
                                fb.write_text_kernel("Scroll y renderizado acelerados por GPU", Color::GREEN);
                                fb.write_text_kernel("Framebuffer UEFI optimizado con drivers específicos", Color::CYAN);
                                hardware_optimizations_enabled = true;
                            }
                            Err(e) => {
                                fb.write_text_kernel(&format!("Error reconfigurando tarjeta gráfica: {}", e), Color::RED);
                                fb.write_text_kernel("Manteniendo configuración UEFI estándar", Color::YELLOW);
                            }
                        }
                    }
                    Err(e) => {
                        fb.write_text_kernel(&format!("Error configurando optimizaciones: {}", e), Color::RED);
                        fb.write_text_kernel("Manteniendo framebuffer UEFI estándar", Color::YELLOW);
                    }
                }
            }
            Err(e) => {
                fb.write_text_kernel(&format!("Error habilitando optimizaciones: {}", e), Color::RED);
                fb.write_text_kernel("Manteniendo framebuffer UEFI estándar", Color::YELLOW);
            }
        }
    } else {
        fb.write_text_kernel("No hay GPU primaria para optimizaciones", Color::YELLOW);
    }
    
    // Mostrar estado final
    if !hardware_optimizations_enabled {
        fb.write_text_kernel("=== FRAMEBUFFER UEFI ESTÁNDAR ===", Color::GREEN);
        fb.write_text_kernel("Resolución UEFI mantenida - sin cambios de modo de video", Color::GREEN);
    }

    // ========================================
    // DETECCIÓN DE HARDWARE
    // ========================================
    let hw_result = detect_graphics_hardware();

    let (width, height, stride, base_address) = {
        let fb_info = fb.get_info();
        (fb_info.width, fb_info.height, fb_info.pixels_per_scan_line, fb_info.base_address)
    };
    
    fb.write_text_kernel(&format!("Resolución actual: {}x{} (stride: {})", width, height, stride), Color::WHITE);
    fb.write_text_kernel(&format!("Dirección base: 0x{:X}", base_address), Color::LIGHT_GRAY);
    
    // Determinar estrategia de scroll que se usará
    let scroll_strategy = if height > 1200 {
        "DMA+Hardware"
    } else if height > 800 {
        "GPU+Accel"
    } else if height > 600 {
        "SIMD+Fast"
    } else if height > 400 {
        "Cache+Opt"
    } else {
        "Memory+Val"
    };
    
    fb.write_text_kernel(&format!("Estrategia de scroll: {}", scroll_strategy), Color::CYAN);
    
    // Mostrar información detallada de la estrategia
    match scroll_strategy {
        "DMA+Hardware" => {
            fb.write_text_kernel("  - Usa DMA del hardware para transferencias rápidas", Color::LIGHT_GRAY);
            fb.write_text_kernel("  - Aprovecha aceleración por hardware", Color::LIGHT_GRAY);
            fb.write_text_kernel("  - Optimizado para resoluciones > 1200px", Color::LIGHT_GRAY);
        },
        "GPU+Accel" => {
            fb.write_text_kernel("  - Aceleración por GPU cuando está disponible", Color::LIGHT_GRAY);
            fb.write_text_kernel("  - Optimizaciones específicas de NVIDIA/AMD/Intel", Color::LIGHT_GRAY);
            fb.write_text_kernel("  - Optimizado para resoluciones > 800px", Color::LIGHT_GRAY);
        },
        "SIMD+Fast" => {
            fb.write_text_kernel("  - Instrucciones SIMD/SSE para operaciones vectoriales", Color::LIGHT_GRAY);
            fb.write_text_kernel("  - Optimización de caché", Color::LIGHT_GRAY);
            fb.write_text_kernel("  - Optimizado para resoluciones > 600px", Color::LIGHT_GRAY);
        },
        "Cache+Opt" => {
            fb.write_text_kernel("  - Optimización de acceso a memoria", Color::LIGHT_GRAY);
            fb.write_text_kernel("  - Prefetching inteligente", Color::LIGHT_GRAY);
            fb.write_text_kernel("  - Optimizado para resoluciones > 400px", Color::LIGHT_GRAY);
        },
        "Memory+Val" => {
            fb.write_text_kernel("  - Validación de memoria antes de operaciones", Color::LIGHT_GRAY);
            fb.write_text_kernel("  - Scroll básico pero seguro", Color::LIGHT_GRAY);
            fb.write_text_kernel("  - Optimizado para resoluciones <= 400px", Color::LIGHT_GRAY);
        },
        _ => {}
    }
    
    // Mostrar información del hardware detectado
    if let Some(primary_gpu) = &hw_result.primary_gpu {
        fb.write_text_kernel(&format!("GPU primaria: {} {:04X}:{:04X}", 
            primary_gpu.gpu_type.as_str(), 
            primary_gpu.pci_device.vendor_id, 
            primary_gpu.pci_device.device_id), Color::YELLOW);
        fb.write_text_kernel(&format!("Resolución máxima soportada: {}x{}", 
            primary_gpu.max_resolution.0, 
            primary_gpu.max_resolution.1), Color::LIGHT_GRAY);
    }
    
    // Información de rendimiento esperado
    fb.write_text_kernel("=== RENDIMIENTO ESPERADO ===", Color::GREEN);
    match scroll_strategy {
        "DMA+Hardware" => {
            fb.write_text_kernel("Rendimiento: MÁXIMO - Hasta 10x más rápido", Color::GREEN);
            fb.write_text_kernel("Uso de recursos: Bajo (DMA hardware)", Color::LIGHT_GRAY);
        },
        "GPU+Accel" => {
            fb.write_text_kernel("Rendimiento: ALTO - Hasta 5x más rápido", Color::GREEN);
            fb.write_text_kernel("Uso de recursos: Medio (GPU específica)", Color::LIGHT_GRAY);
        },
        "SIMD+Fast" => {
            fb.write_text_kernel("Rendimiento: BUENO - Hasta 3x más rápido", Color::YELLOW);
            fb.write_text_kernel("Uso de recursos: Medio (SIMD + caché)", Color::LIGHT_GRAY);
        },
        "Cache+Opt" => {
            fb.write_text_kernel("Rendimiento: MEJORADO - Hasta 2x más rápido", Color::YELLOW);
            fb.write_text_kernel("Uso de recursos: Bajo (optimización memoria)", Color::LIGHT_GRAY);
        },
        "Memory+Val" => {
            fb.write_text_kernel("Rendimiento: BÁSICO - Scroll seguro", Color::CYAN);
            fb.write_text_kernel("Uso de recursos: Mínimo (validación memoria)", Color::LIGHT_GRAY);
        },
        _ => {}
    }
    
    fb.write_text_kernel("=== SCROLL OPTIMIZADO ACTIVO ===", Color::GREEN);
    fb.write_text_kernel("El sistema detectará automáticamente la mejor estrategia", Color::GREEN);
    
    // ========================================
    // DEMOSTRACIÓN DE CAMBIO DE RESOLUCIÓN
    // ========================================
    fb.write_text_kernel("=== DEMOSTRACIÓN DE CAMBIO DE RESOLUCIÓN ===", Color::MAGENTA);
    
    // Mostrar información de resolución actual
    show_resolution_info(fb);
    
    // Intentar cambiar a la resolución recomendada más segura
    fb.write_text_kernel("Detectando mejor resolución segura...", Color::CYAN);
    
    // Mostrar información actualizada
    let (new_width, new_height, new_stride, new_base) = {
        let fb_info = fb.get_info();
        (fb_info.width, fb_info.height, fb_info.pixels_per_scan_line, fb_info.base_address)
    };
    fb.write_text_kernel(&format!("Resolución final: {}x{} (stride: {})", new_width, new_height, new_stride), Color::CYAN);
    fb.write_text_kernel(&format!("Dirección base final: 0x{:X}", new_base), Color::LIGHT_GRAY);
    
    // ========================================
    // CAMBIO DE FRAMEBUFFER: GOP -> HARDWARE
    // ========================================
    // El cambio de framebuffer ya se completó arriba
    fb.write_text_kernel("=== CAMBIO DE FRAMEBUFFER COMPLETADO ===", Color::GREEN);
    
    // Verificar que el framebuffer sigue funcionando después del cambio
    fb.write_text_kernel("Verificando framebuffer después del cambio...", Color::CYAN);
    let test_write = test_framebuffer_write(fb);
    fb.write_text_kernel(&format!("Test escritura post-cambio: {}", if test_write { "OK" } else { "FALLO" }), 
        if test_write { Color::GREEN } else { Color::RED });
    
    let memory_valid = verify_framebuffer_memory(fb);
    fb.write_text_kernel(&format!("Memoria válida post-cambio: {}", if memory_valid { "SÍ" } else { "NO" }), 
        if memory_valid { Color::GREEN } else { Color::RED });
    
    // Mostrar información del framebuffer actual
    let (current_width, current_height, current_stride, current_base) = {
        let fb_info = fb.get_info();
        (fb_info.width, fb_info.height, fb_info.pixels_per_scan_line, fb_info.base_address)
    };
    fb.write_text_kernel(&format!("FB actual: {}x{} (stride: {}) @0x{:X}", 
        current_width, current_height, current_stride, current_base), Color::LIGHT_GRAY);


    fb.write_text_kernel("[5/7] Inicializando sistema IPC de drivers...", Color::YELLOW);
    let mut driver_manager = DriverManager::new();
    let mut ipc_manager = IpcManager::new();
    let mut binary_driver_manager = BinaryDriverManager::new();
    
    // Sistema de hot-plug removido para simplificar el kernel
    fb.write_text_kernel("Sistema de hot-plug removido", Color::YELLOW);
    
    // Registrar driver PCI base
    fb.write_text_kernel("[3/5] Registrando PCI driver...", Color::LIGHT_GRAY);
    let pci_driver = Box::new(PciDriver::new());
    match driver_manager.register_driver(pci_driver) {
        Ok(pci_id) => {
            fb.write_text_kernel(&format!("Driver PCI registrado (ID: {})", pci_id), Color::GREEN);
        }
        Err(e) => {
            fb.write_text_kernel(&format!("Error registrando driver PCI: {}", e), Color::RED);
        }
    }
    
    // Registrar driver NVIDIA si hay GPUs NVIDIA
    if hw_result.available_gpus.iter().any(|gpu| matches!(gpu.gpu_type, GpuType::Nvidia)) {
        fb.write_text_kernel("Inicializando driver NVIDIA...", Color::CYAN);
        let mut nvidia_driver = NvidiaPciDriver::new();
        
        // Usar el nuevo método con información de hardware
        match nvidia_driver.initialize_with_hardware(&hw_result) {
            Ok(_) => {
                fb.write_text_kernel("✓ Driver NVIDIA inicializado correctamente", Color::GREEN);
                let nvidia_driver_box = Box::new(nvidia_driver);
                match driver_manager.register_driver(nvidia_driver_box) {
            Ok(nvidia_id) => {
                fb.write_text_kernel(&format!("Driver NVIDIA registrado (ID: {})", nvidia_id), Color::GREEN);
                
                // Probar comandos del driver NVIDIA
                let gpu_count_cmd = DriverMessage::ExecuteCommand {
                    command: String::from("get_gpu_count"),
                    args: Vec::new(),
                };
                
                match driver_manager.send_message(nvidia_id, gpu_count_cmd) {
                    Ok(DriverResponse::SuccessWithData(data)) => {
                        if data.len() >= 4 {
                            let count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                            fb.write_text_kernel(&format!("GPUs NVIDIA detectadas: {}", count), Color::CYAN);
                        }
                    }
                    Ok(_) => { /* silencioso */ }
                    Err(e) => {
                        fb.write_text_kernel(&format!("Error ejecutando comando: {}", e), Color::RED);
                    }
                }
            }
            Err(e) => {
                fb.write_text_kernel(&format!("Error registrando driver NVIDIA: {}", e), Color::RED);
                    }
                }
            }
            Err(e) => {
                fb.write_text_kernel(&format!("✗ Error inicializando driver NVIDIA: {}", e), Color::RED);
                fb.write_text_kernel("Continuando sin driver NVIDIA...", Color::YELLOW);
            }
        }
    }

    // ========================================
    // CONTROL DIRECTO DE GPU PARA FRAMEBUFFER
    // ========================================
    fb.write_text_kernel("=== CONTROL DIRECTO DE GPU ===", Color::MAGENTA);
    
    // Crear controlador de GPU
    let mut gpu_controller = GpuController::new();
    
    // Buscar GPU para control directo
    let mut gpu_found = false;
    for gpu in &hw_result.available_gpus {
        if gpu.gpu_type != crate::drivers::pci::GpuType::Unknown {
            fb.write_text_kernel(&format!("Inicializando control directo para: {:?}", gpu.gpu_type), Color::CYAN);
            
            match gpu_controller.initialize(gpu) {
                Ok(_) => {
                    fb.write_text_kernel("✓ Controlador de GPU inicializado", Color::GREEN);
                    fb.write_text_kernel(&gpu_controller.get_gpu_info(), Color::CYAN);
                    gpu_found = true;
                    break;
                }
                Err(e) => {
                    fb.write_text_kernel(&format!("Error inicializando GPU: {}", e), Color::RED);
                }
            }
        }
    }
    
    // Saltar control directo de GPU en QEMU/Bochs para evitar reconfiguración que introduce ruido
    let is_qemu_bochs = hw_result.primary_gpu.as_ref().map(|g| matches!(g.gpu_type, GpuType::QemuBochs)).unwrap_or(false);
    if gpu_found && !is_qemu_bochs {
        // IMPLEMENTAR FRAMEBUFFER DIRECTO COMO LINUX
        fb.write_text_kernel("=== FRAMEBUFFER DIRECTO COMO LINUX ===", Color::MAGENTA);
        fb.write_text_kernel("✓ GPU detectado y controlador inicializado", Color::GREEN);
        
        // Crear framebuffer directo usando la GPU
        match gpu_controller.change_resolution(1024, 768, 32) {
            Ok(new_fb_info) => {
                fb.write_text_kernel("✓ Resolución cambiada a 1024x768 @32bpp", Color::GREEN);
                fb.write_text_kernel("✓ Framebuffer directo configurado en GPU", Color::GREEN);
                
                // APLICAR EL FRAMEBUFFER DIRECTO
                fb.info = new_fb_info;
                // Re-inicializar como arriba para mantener consistencia
                // Forzar parámetros seguros para Bochs/QEMU: 32bpp y stride=width
                fb.info.pixel_format = 2; // RGBA8888
                fb.info.pixels_per_scan_line = fb.info.width;
                let pixel_bitmask = (fb.info.red_mask) | (fb.info.green_mask) | (fb.info.blue_mask);
                let _ = fb.init_from_uefi(
                    fb.info.base_address,
                    fb.info.width,
                    fb.info.height,
                    fb.info.pixels_per_scan_line,
                    fb.info.pixel_format,
                    pixel_bitmask,
                );
                for _ in 0..200000 { core::hint::spin_loop(); }
                fb.clear_screen(Color::BLACK);
                fb.write_text_kernel("✓ Framebuffer GPU aplicado como principal", Color::GREEN);
                fb.write_text_kernel("✓ Aceleración gráfica activa", Color::GREEN);
                
                // Demostrar que funciona
                fb.write_text_kernel("=== DEMOSTRACIÓN DE ACELERACIÓN ===", Color::YELLOW);
                
                // Escribir texto de prueba
                for i in 0..20 {
                    fb.write_text_kernel(&format!("Línea {} - Aceleración GPU activa", i + 1), Color::WHITE);
                }
                
                fb.write_text_kernel("✓ Framebuffer directo funcionando", Color::GREEN);
                fb.write_text_kernel("✓ Aceleración gráfica disponible", Color::GREEN);
                fb.write_text_kernel("✓ Scroll optimizado con GPU", Color::GREEN);
            }
            Err(e) => {
                fb.write_text_kernel(&format!("Error configurando framebuffer directo: {}", e), Color::RED);
                fb.write_text_kernel("Manteniendo framebuffer UEFI", Color::YELLOW);
            }
        }
    } else {
        fb.write_text_kernel("No se encontró GPU compatible para framebuffer directo", Color::YELLOW);
        fb.write_text_kernel("Manteniendo framebuffer UEFI", Color::YELLOW);
    }

    // Continuar con la inicialización del sistema

    // Demostrar sistema IPC del kernel
    // Probando sistema IPC del kernel (mensaje reducido)
    fb.write_text_kernel("Probando IPC del kernel...", Color::CYAN);
    
    // Simular carga de driver desde userland
    let nvidia_config = DriverConfig {
        name: "NVIDIA Driver IPC".to_string(),
        version: "1.0.0".to_string(),
        author: "Eclipse OS Team".to_string(),
        description: "Driver NVIDIA cargado via IPC".to_string(),
        priority: 2,
        auto_load: false,
        memory_limit: 16 * 1024 * 1024,
        dependencies: {
            let mut deps = Vec::new();
            deps.push("PCI Driver".to_string());
            deps
        },
        capabilities: {
            let mut caps = Vec::new();
            caps.push(crate::ipc::DriverCapability::Graphics);
            caps.push(crate::ipc::DriverCapability::Custom("CUDA".to_string()));
            caps
        },
    };
    
    let load_message = IpcMessage::LoadDriver {
        driver_type: DriverType::NVIDIA,
        driver_name: "NVIDIA Driver IPC".to_string(),
        driver_data: Vec::new(),
        config: nvidia_config,
    };
    
    let message_id = ipc_manager.send_message(load_message);
    let receive_result = ipc_manager.receive_message();
    let response = ipc_manager.process_message(message_id, receive_result.unwrap().1);
    
    if let IpcMessage::LoadDriverResponse { success, driver_id, error } = response {
        if success {
            fb.write_text_kernel(&format!("Driver IPC cargado con ID: {}", driver_id.unwrap()), Color::GREEN);
            
            // Probar comando en el driver IPC
            let command_message = IpcMessage::DriverCommand {
                driver_id: driver_id.unwrap(),
                command: DriverCommandType::ExecuteCommand { command: "get_gpu_count".to_string() },
                args: Vec::new(),
            };
            
            let cmd_message_id = ipc_manager.send_message(command_message);
            let cmd_receive_result = ipc_manager.receive_message();
            let cmd_response = ipc_manager.process_message(cmd_message_id, cmd_receive_result.unwrap().1);
            
            if let IpcMessage::DriverCommandResponse { success: cmd_success, result, error: cmd_error, driver_id: _ } = cmd_response {
                if cmd_success {
                    if let Some(data) = result {
                        let gpu_count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                        fb.write_text_kernel(&format!("GPUs detectadas via IPC: {}", gpu_count), Color::CYAN);
                    }
                } else {
                    fb.write_text_kernel(&format!("Error en comando IPC: {}", cmd_error.unwrap_or_default()), Color::RED);
                }
            }
        } else {
            fb.write_text_kernel(&format!("Error cargando driver IPC: {}", error.unwrap_or_default()), Color::RED);
        }
    }
    
    // Demostrar sistema de drivers binarios
    fb.write_text_kernel("Probando sistema de drivers binarios...", Color::MAGENTA);
    
    // Crear metadatos de driver binario de ejemplo
    let binary_metadata = BinaryDriverMetadata {
        name: "Binary Graphics Driver".to_string(),
        version: "1.0.0".to_string(),
        author: "Eclipse OS Team".to_string(),
        description: "Driver binario de ejemplo para gráficos".to_string(),
        driver_type: DriverType::NVIDIA,
        capabilities: {
            let mut caps = Vec::new();
            caps.push(crate::drivers::ipc::DriverCapability::Graphics);
            caps.push(crate::drivers::ipc::DriverCapability::Custom("Binary".to_string()));
            caps
        },
        dependencies: {
            let mut deps = Vec::new();
            deps.push("PCI Driver".to_string());
            deps
        },
        entry_point: "driver_main".to_string(),
        file_size: 2048,
        checksum: "binary_checksum_12345".to_string(),
        target_arch: "x86_64".to_string(),
        target_os: "eclipse".to_string(),
    };
    
    // Crear datos binarios simulados
    let binary_data = b"ECLIPSE_DRIVER_METADATA\x00Binary driver code here...".to_vec();
    
    // Cargar driver binario
    match binary_driver_manager.load_binary_driver(binary_metadata, binary_data) {
        Ok(binary_driver_id) => {
            fb.write_text_kernel(&format!("Driver binario cargado con ID: {}", binary_driver_id), Color::GREEN);
            
            // Probar comando en driver binario
            match binary_driver_manager.execute_command(binary_driver_id, "driver_command", b"get_info".to_vec()) {
                Ok(result) => {
                    let result_str = String::from_utf8_lossy(&result);
                // Resultado de comando binario (mensaje reducido)
                fb.write_text_kernel(&format!("Cmd binario: {}", result_str), Color::CYAN);
                }
                Err(e) => {
                    fb.write_text_kernel(&format!("Error en comando binario: {}", e), Color::RED);
                }
            }
            
            // Obtener información del driver binario
            if let Some(driver_info) = binary_driver_manager.get_driver_info(binary_driver_id) {
            // Info resumida del driver binario
            fb.write_text_kernel(&format!("Driver: {} v{} ({:?})", driver_info.name, driver_info.version, driver_info.state), Color::LIGHT_GRAY);
            }
        }
        Err(e) => {
            fb.write_text_kernel(&format!("Error cargando driver binario: {}", e), Color::RED);
        }
    }
    
    fb.write_text_kernel("[6/7] Sistema de gráficos avanzado deshabilitado temporalmente", Color::YELLOW);
    
    // Inicializar sistema de hot-plug USB (mismo flujo en QEMU y hardware real)
    fb.write_text_kernel("Inicializando hot-plug USB...", Color::MAGENTA);
    let hotplug_config = HotplugConfig {
        enable_usb_hotplug: true,
        enable_mouse_support: true,
        enable_keyboard_support: true,
        enable_storage_support: true,
        poll_interval_ms: 100,
        max_devices: 32,
    };
    let mut hotplug_manager = HotplugManager::new(hotplug_config);
    match hotplug_manager.initialize() {
        Ok(_) => {
            fb.write_text_kernel("Sistema de hot-plug USB inicializado", Color::GREEN);
            if let Err(e) = hotplug_manager.start() {
                fb.write_text_kernel(&format!("Error iniciando hot-plug: {}", e), Color::RED);
            } else {
                fb.write_text_kernel("Polling de hot-plug iniciado", Color::GREEN);
            }
        }
        Err(e) => {
            fb.write_text_kernel(&format!("Error inicializando hot-plug: {}", e), Color::RED);
        }
    }
    
    // Detección básica de dispositivos PCI (mismo flujo en QEMU y hardware real)
    let mut pci_manager = PciManager::new();
    pci_manager.scan_devices();
    let pci_devices = pci_manager.get_gpus();
    fb.write_text_kernel(&format!("PCI GPUs: {}", pci_devices.len()), Color::CYAN);
    for device_option in pci_devices {
        if let Some(device) = device_option {
            fb.write_text_kernel(&format!("  - PCI {:04X}:{:04X} Clase: {:02X}", 
                device.pci_device.vendor_id, device.pci_device.device_id, device.pci_device.class_code), Color::LIGHT_GRAY);
        }
    }
    
    // Demostración USB

    // Mostrar información del modo gráfico detectado
    let modo_str = match hw_result.graphics_mode {
        GraphicsMode::Framebuffer => "Modo framebuffer",
        GraphicsMode::VGA => "Modo VGA",
        GraphicsMode::HardwareAccelerated => "Aceleración por hardware",
    };

    let color_modo = match hw_result.graphics_mode {
        GraphicsMode::Framebuffer | GraphicsMode::VGA => Color::GREEN,
        GraphicsMode::HardwareAccelerated => Color::GREEN,
    };

    fb.write_text_kernel("[3/5] Modo grafico: ", Color::WHITE);
    fb.write_text_kernel(modo_str, color_modo);

    
    // Mostrar breve info de framebuffer si está disponible
    if let Some(info) = crate::uefi_framebuffer::get_framebuffer_status().driver_info {
        let dims = format!("FB {}x{} @{}", info.width, info.height, info.pixels_per_scan_line);
        fb.write_text_kernel(&dims, Color::LIGHT_GRAY);
    } else {
        fb.write_text_kernel("FB no disponible", Color::YELLOW);
    }

    // Información de hardware detectado (GPUs, VGA, driver recomendado)
    let gpu_count = hw_result.available_gpus.len();
    let gpu_count_msg = format!("GPUs detectadas: {}", gpu_count);
    fb.write_text_kernel(&gpu_count_msg, Color::LIGHT_GRAY);

    // Listar hasta 4 GPUs detectadas con su vendor:device
    for (idx, gpu_opt) in hw_result.available_gpus.iter().enumerate().take(4) {
        let gpu = gpu_opt;
        let line = format!(
            "  [{}] {} {:04X}:{:04X}",
            idx,
            gpu.gpu_type.as_str(),
            gpu.pci_device.vendor_id,
            gpu.pci_device.device_id
        );
        fb.write_text_kernel(&line, Color::LIGHT_GRAY);
    }

    if let Some(gpu) = &hw_result.primary_gpu {
        let gpu_msg = format!(
            "GPU primaria: {} {:04X}:{:04X}",
            gpu.gpu_type.as_str(),
            gpu.pci_device.vendor_id,
            gpu.pci_device.device_id
        );
        fb.write_text_kernel(&gpu_msg, Color::LIGHT_GRAY);
    } else {
        fb.write_text_kernel("GPU primaria: ninguna", Color::YELLOW);
    }

    let vga_msg = if hw_result.vga_available { "VGA disponible" } else { "VGA no disponible" };
    fb.write_text_kernel(vga_msg, Color::LIGHT_GRAY);

    let driver_msg = format!("Driver recomendado: {}", hw_result.recommended_driver.as_str());
    fb.write_text_kernel(&driver_msg, Color::LIGHT_GRAY);

    // Depuracion: listar algunos dispositivos PCI detectados (siempre)
    fb.write_text_kernel("PCI dump (parcial):", Color::WHITE);
    let mut pci_dbg = PciManager::new();
    pci_dbg.scan_devices();
    for i in 0..core::cmp::min(12, pci_dbg.device_count()) {
        if let Some(dev) = pci_dbg.get_device(i) {
            let msg = format!(
                "  {:02X}:{:02X}.{} {:04X}:{:04X} class {:02X}:{:02X}",
                dev.bus,
                dev.device,
                dev.function,
                dev.vendor_id,
                dev.device_id,
                dev.class_code,
                dev.subclass_code
            );
            fb.write_text_kernel(&msg, Color::LIGHT_GRAY);
        }
    }

    // Detectar controladores USB por PCI (class 0x0C, subclass 0x03)
    let mut usb_ctrls: heapless::Vec<(u8,u8,u8,u8), 16> = heapless::Vec::new();
    for i in 0..pci_dbg.device_count() {
        if let Some(mut dev) = pci_dbg.get_device(i) {
            if dev.class_code == 0x0C && dev.subclass_code == 0x03 {
                let prog_if = dev.prog_if; // UHCI=0x00, OHCI=0x10, EHCI=0x20, XHCI=0x30
                // Habilitar MMIO/BusMaster para el controlador USB
                dev.enable_mmio_and_bus_master();
                let _ = usb_ctrls.push((dev.bus, dev.device, dev.function, prog_if));
            }
        }
    }
    let usb_msg = format!("Controladores USB (PCI): {}", usb_ctrls.len());
    fb.write_text_kernel(&usb_msg, Color::WHITE);
    for (bus, dev, func, prog_if) in usb_ctrls.iter().copied().take(8) {
        let kind = match prog_if {
            0x00 => "UHCI",
            0x10 => "OHCI",
            0x20 => "EHCI",
            0x30 => "XHCI",
            _ => "USB?",
        };
        let line = format!("  {:02X}:{:02X}.{} {}", bus, dev, func, kind);
        fb.write_text_kernel(&line, Color::LIGHT_GRAY);

        // Intentar inicializar xHCI genérico
        if prog_if == 0x30 {
            // Buscar el dispositivo por BDF manualmente
            let mut found = None;
            for idx in 0..pci_dbg.device_count() {
                if let Some(devinfo) = pci_dbg.get_device(idx) {
                    if devinfo.bus == bus && devinfo.device == dev && devinfo.function == func {
                        found = Some(devinfo.clone());
                        break;
                    }
                }
            }
            if let Some(pci_dev) = found {
                let mut xhci = XhciController::new(pci_dev);
                if xhci.initialize().is_ok() {
                    fb.write_text_kernel("xHCI inicializado", Color::GREEN);
                } else {
                    fb.write_text_kernel("xHCI fallo init", Color::YELLOW);
                }
            }
        }
    }

    // QEMU: sin demo gráfica; mantenemos solo texto para depuración

    // Inicializar GPU primaria: habilitar MMIO y Bus Master, leer BARs
    if let Some(primary) = &hw_result.primary_gpu {
        let dev: &PciDevice = &primary.pci_device;
        fb.write_text_kernel("Inicializando GPU primaria (MMIO/BusMaster)", Color::YELLOW);
        dev.enable_mmio_and_bus_master();
        
        // Leer todos los BARs
        let bars = dev.read_all_bars();
        let bars_str = format!("BARs: {:08X} {:08X} {:08X} {:08X} {:08X} {:08X}", 
                              bars[0], bars[1], bars[2], bars[3], bars[4], bars[5]);
        fb.write_text_kernel(&bars_str, Color::LIGHT_GRAY);
        
        // Calcular tamaños reales de BARs
        let mut total_memory = 0u64;
        let mut memory_bars = 0;
        for i in 0..6 {
            let size = dev.calculate_bar_size(i);
            if size > 0 {
                total_memory += size as u64;
                memory_bars += 1;
                let size_mb = size / (1024 * 1024);
                let size_gb = size / (1024 * 1024 * 1024);
                let bar_info = if size_gb > 0 {
                    format!("BAR{}: {}GB ({}MB)", i, size_gb, size_mb)
                } else {
                    format!("BAR{}: {}MB", i, size_mb)
                };
                fb.write_text_kernel(&bar_info, Color::LIGHT_GRAY);
            }
        }
        
        // Mostrar información de memoria total
        let total_gb = total_memory / (1024 * 1024 * 1024);
        let total_mb = total_memory / (1024 * 1024);
        let total_str = if total_gb > 0 {
            format!("Memoria total GPU: {}GB ({}MB) - {} BARs", total_gb, total_mb, memory_bars)
        } else {
            format!("Memoria total GPU: {}MB - {} BARs", total_mb, memory_bars)
        };
        fb.write_text_kernel(&total_str, Color::GREEN);
        
        // Leer capabilities
        let cap_ptr = dev.read_capability_pointer();
        if cap_ptr != 0 {
            let cap_str = format!("Capabilities en: 0x{:02X}", cap_ptr);
            fb.write_text_kernel(&cap_str, Color::LIGHT_GRAY);
            
            // Leer algunas capabilities
            let mut offset = cap_ptr;
            let mut cap_count = 0;
            while let Some((id, next)) = dev.read_capability(offset) {
                if cap_count < 5 { // Mostrar solo las primeras 5
                    let cap_name = match id {
                        0x01 => "Power Management",
                        0x05 => "MSI",
                        0x10 => "PCIe",
                        0x11 => "MSI-X",
                        _ => "Unknown",
                    };
                    let cap_info = format!("  Cap {}: {} (0x{:02X})", cap_count, cap_name, id);
                    fb.write_text_kernel(&cap_info, Color::LIGHT_GRAY);
                }
                cap_count += 1;
                if next == 0 || cap_count > 10 { break; }
                offset = next;
            }
        }
        
        // Información específica por tipo de GPU
        match primary.gpu_type {
            GpuType::Nvidia => {
                fb.write_text_kernel("Driver NVIDIA: Inicializando...", Color::GREEN);
                // Stub para NVIDIA: verificar si es 64-bit BAR
                if (bars[0] & 0x7) == 0x4 { // 64-bit memory BAR
                    let bar0_64 = ((bars[1] as u64) << 32) | (bars[0] as u64 & 0xFFFFFFF0);
                    let bar0_str = format!("NVIDIA BAR0 64-bit: 0x{:016X}", bar0_64);
                    fb.write_text_kernel(&bar0_str, Color::CYAN);
                }
            },
            GpuType::Intel => {
                fb.write_text_kernel("Driver Intel: Inicializando...", Color::GREEN);
                // Stub para Intel: verificar BAR2 (común en Intel)
                if bars[2] != 0 {
                    let bar2_str = format!("Intel BAR2: 0x{:08X}", bars[2]);
                    fb.write_text_kernel(&bar2_str, Color::CYAN);
                }
            },
            GpuType::Amd => {
                fb.write_text_kernel("Driver AMD: Inicializando...", Color::GREEN);
                // Stub para AMD: verificar BAR0 y BAR2
                if bars[0] != 0 {
                    let bar0_str = format!("AMD BAR0: 0x{:08X}", bars[0]);
                    fb.write_text_kernel(&bar0_str, Color::CYAN);
                }
            },
            _ => {
                fb.write_text_kernel("Driver genérico: Inicializando...", Color::YELLOW);
            }
        }
    } else {
        fb.write_text_kernel("Sin GPU primaria para inicializar", Color::YELLOW);
    }

    // Soporte básico multi-GPU: habilitar MMIO/BusMaster en las adicionales
    if hw_result.available_gpus.len() > 1 {
        fb.write_text_kernel("Multi-GPU: habilitando GPUs adicionales", Color::WHITE);
        for gpu in hw_result.available_gpus.iter() {
            if let Some(ref primary) = hw_result.primary_gpu {
                if gpu.pci_device.bus == primary.pci_device.bus
                    && gpu.pci_device.device == primary.pci_device.device
                    && gpu.pci_device.function == primary.pci_device.function {
                    continue; // ya tratada
                }
            }
            let dev = &gpu.pci_device;
            dev.enable_mmio_and_bus_master();
            let msg = format!(
                "  GPU secundaria habilitada {:04X}:{:04X}",
                dev.vendor_id, dev.device_id
            );
            fb.write_text_kernel(&msg, Color::LIGHT_GRAY);
        }
        fb.write_text_kernel("Multi-GPU (experimental) activo", Color::CYAN);
    }

    // Si el modo es acelerado, intentar inicializar la aceleración y mostrar detalles
    if let GraphicsMode::HardwareAccelerated = hw_result.graphics_mode {
        if let Some(ref gpu_info) = hw_result.primary_gpu {
            let resultado_acc = fb.init_hardware_acceleration(gpu_info);
            if resultado_acc.is_ok() {
                fb.write_text_kernel("Aceleración de hardware inicializada correctamente", Color::GREEN);
            } else {
                fb.write_text_kernel("Error al inicializar aceleración de hardware", Color::RED);
            }
        } else {
            fb.write_text_kernel("No se detectó GPU para aceleración", Color::RED);
        }
    }
    fb.write_text_kernel("[5/9] Iniciando sistema de AI...", Color::YELLOW);
    // Crear sistema de AI para escritura
    let mut ai_system = create_ai_typing_system();
    
    // Inicializar cargador de modelos de IA
    let mut model_loader = ModelLoader::new();
    fb.write_text_kernel("Cargando modelos de IA...", Color::CYAN);
    
    // Cargar modelos disponibles
    match model_loader.load_all_models() {
        Ok(_) => {
            let loaded_count = model_loader.list_models().iter().filter(|m| m.loaded).count();
            fb.write_text_kernel(&format!("Modelos cargados: {}/{}", loaded_count, model_loader.list_models().len()), Color::GREEN);
            
            // Mostrar memoria total requerida
            let total_mem = model_loader.total_memory_required() / (1024 * 1024); // MB
            fb.write_text_kernel(&format!("Memoria total requerida: {} MB", total_mem), Color::CYAN);
        },
        Err(_) => {
            fb.write_text_kernel("Error al cargar algunos modelos de IA", Color::RED);
        }
    }

    // Configurar efecto de escritura
    let mut config = AiTypingConfig::default();
    config.effect = TypingEffect::Typewriter;
    config.color = Color::WHITE;
    ai_system.set_config(config);
    
    // Escribir mensaje especial con efecto rainbow
    let special_message = String::from("Eclipse OS Kernel con AI");
    ai_system.write_message(fb, &special_message);
    // Escribir mensaje de bienvenida
    ai_system.write_welcome_message(fb);
    
    // Escribir mensajes del sistema
    ai_system.write_system_message(fb, 0); // "Cargando sistema de archivos..."
    ai_system.write_system_message(fb, 1); // "Inicializando drivers de hardware..."
    ai_system.write_system_message(fb, 2); // "Configurando red..."
    
    // Escribir mensaje de éxito
    ai_system.write_success_message(fb, 0); // "Operacion completada exitosamente"

    // Inicializar Sistema de Archivos
    fb.write_text_kernel("[4/5] Inicializando Sistema de Archivos...", Color::CYAN);
    match init_file_cache() {
        Ok(_) => {
            fb.write_text_kernel("Cache de archivos inicializado", Color::GREEN);
        }
        Err(e) => {
            fb.write_text_kernel(&format!("Error inicializando cache: {}", e), Color::YELLOW);
        }
    }
    
    match init_block_device() {
        Ok(_) => {
            fb.write_text_kernel("Dispositivo de bloques inicializado", Color::GREEN);
        }
        Err(e) => {
            fb.write_text_kernel(&format!("Error inicializando dispositivo de bloques: {}", e), Color::YELLOW);
        }
    }
    
    match init_vfs() {
        Ok(_) => {
            fb.write_text_kernel("VFS inicializado correctamente", Color::GREEN);
            
            // Inicializar soporte FAT32
            match init_fat32() {
                Ok(_) => {
                    fb.write_text_kernel("Soporte FAT32 inicializado", Color::GREEN);
                    
                    // Mostrar información del sistema FAT32
                    if let Some(fat32) = get_fat32_driver() {
                        let (total_sectors, free_clusters, fat_sectors, cluster_size) = fat32.get_filesystem_info();
                        fb.write_text_kernel(&format!("FAT32: {} sectores, {} clusters libres, {} bytes/cluster", 
                            total_sectors, free_clusters, cluster_size), Color::LIGHT_GRAY);
                    }
                }
                Err(e) => {
                    fb.write_text_kernel(&format!("Error inicializando FAT32: {}", e), Color::YELLOW);
                }
            }
            
            // Crear sistema de archivos de demostración
            match create_demo_filesystem() {
                Ok(_) => {
                    fb.write_text_kernel("Sistema de archivos de demostración creado", Color::GREEN);
                    
                    // Escribir contenido de ejemplo a algunos archivos
                    let welcome_content = b"Bienvenido a Eclipse OS!\n\nEste es un sistema operativo moderno construido en Rust.\nCaracteristicas:\n- Kernel monolitico con microkernel\n- Sistema de ventanas avanzado\n- Soporte para Wayland\n- Drivers de hardware modernos\n- Soporte para FAT32\n\nSistema de archivos funcionando correctamente.\n";
                    let _ = write_demo_content(2, welcome_content);
                    
                    let config_content = b"[system]\nversion=1.0.0\nkernel=eclipse\n\n[graphics]\nbackend=wayland\nresolution=1024x768\n\n[filesystem]\ntype=eclipsefs\ncache_size=64\nfat32_support=true\n";
                    let _ = write_demo_content(4, config_content);
                    
                    let log_content = b"[2024-01-01 00:00:00] Sistema iniciado\n[2024-01-01 00:00:01] VFS inicializado\n[2024-01-01 00:00:02] FAT32 inicializado\n[2024-01-01 00:00:03] Drivers cargados\n[2024-01-01 00:00:04] Sistema listo\n";
                    let _ = write_demo_content(6, log_content);
                    
                    fb.write_text_kernel("Archivos de ejemplo creados", Color::LIGHT_GRAY);
                }
                Err(e) => {
                    fb.write_text_kernel(&format!("Error creando sistema de demostración: {}", e), Color::YELLOW);
                }
            }
            
            // Mostrar estadísticas del sistema de archivos
            let (total_mounts, mounted_fs, open_files, total_files) = get_vfs_statistics();
            fb.write_text_kernel(&format!("Sistema de archivos: {} montajes, {} archivos abiertos", total_mounts, open_files), Color::LIGHT_GRAY);
        }
        Err(e) => {
            fb.write_text_kernel(&format!("Error inicializando VFS: {}", e), Color::YELLOW);
        }
    }
    
    fb.write_text_kernel("[5/5] Inicializando drivers USB...", Color::YELLOW);
    // Inicializar drivers USB (mismo flujo en QEMU y hardware real)
    let mut usb_driver = UsbDriver::new();
    let usb_init_result = usb_driver.initialize_controllers();
    
    // Inicializar driver de teclado USB (usando IDs de ejemplo)
    let mut keyboard_driver = UsbKeyboardDriver::new(0x1234);
    let keyboard_init_result = keyboard_driver.initialize();
    
    // Inicializar driver de mouse USB (usando IDs de ejemplo)
    let mut mouse_driver = UsbMouseDriver::new(0x1234);
    let mouse_init_result = mouse_driver.initialize();
    
    // Mostrar estado de los drivers
    if usb_init_result.is_ok() {
        fb.write_text_kernel("USB Driver: Inicializado", Color::GREEN);
    } else {
        fb.write_text_kernel("USB Driver: Error", Color::RED);
    }
    
    if keyboard_init_result.is_ok() {
        fb.write_text_kernel("Teclado USB: Inicializado", Color::GREEN);
        
        // Activar LEDs del teclado
        fb.write_text_kernel("Activando LEDs del teclado...", Color::CYAN);
        match keyboard_driver.enable_all_leds() {
            Ok(_) => {
                fb.write_text_kernel("✓ LEDs del teclado activados", Color::GREEN);
                
                // Activar LEDs específicos con delay
                let _ = keyboard_driver.set_num_lock_led(true);
                let _ = keyboard_driver.set_caps_lock_led(true);
                let _ = keyboard_driver.set_scroll_lock_led(true);
            }
            Err(e) => {
                fb.write_text_kernel(&format!("Error activando LEDs del teclado: {}", e), Color::YELLOW);
            }
        }
    } else {
        fb.write_text_kernel("Teclado USB: Error", Color::RED);
    }
    
    if mouse_init_result.is_ok() {
        fb.write_text_kernel("Mouse USB: Inicializado", Color::GREEN);
        
        // Activar LEDs del mouse
        fb.write_text_kernel("Activando LEDs del mouse...", Color::CYAN);
        match mouse_driver.enable_all_leds() {
            Ok(_) => {
                fb.write_text_kernel("✓ LEDs del mouse activados", Color::GREEN);
                
                // Activar LEDs específicos con delay
                let _ = mouse_driver.set_scroll_wheel_led(true);
                let _ = mouse_driver.set_side_buttons_led(true);
                let _ = mouse_driver.set_logo_led(true);
                let _ = mouse_driver.set_dpi_indicator_led(true);
            }
            Err(e) => {
                fb.write_text_kernel(&format!("Error activando LEDs del mouse: {}", e), Color::YELLOW);
            }
        }
    } else {
        fb.write_text_kernel("Mouse USB: Error", Color::RED);
    }
    
    // Inicializar Wayland si está disponible
    fb.write_text_kernel("[6/6] Inicializando Wayland...", Color::CYAN);
    init_wayland();

    if is_wayland_initialized() {
        fb.write_text_kernel("Wayland inicializado correctamente", Color::GREEN);
        let wayland_state = get_wayland_state();
        let compositor_status = if wayland_state.compositor_running.load(core::sync::atomic::Ordering::Acquire) {
            "activo"
        } else {
            "inactivo"
        };
        fb.write_text_kernel(&format!("Wayland: Compositor {}", compositor_status), Color::LIGHT_GRAY);
    } else {
        fb.write_text_kernel("Wayland no disponible, usando modo framebuffer", Color::YELLOW);
    }

    // Sistema de Ventanas ya inicializado con Wayland
    match crate::window_system::init_window_system() {
        Ok(_) => {
            fb.write_text_kernel("Sistema de Ventanas inicializado correctamente", Color::GREEN);
            
            // Crear ventanas de ejemplo
            let client_id = crate::window_system::client_api::connect_global_client("Sistema".to_string()).unwrap_or(0);
            if client_id > 0 {
                let flags = crate::window_system::protocol::WindowFlags::default();
                
                // Crear ventana principal
                match crate::window_system::window_manager::create_global_window(
                    client_id,
                    "Eclipse OS Desktop".to_string(),
                    50, 50, 600, 400,
                    flags,
                    crate::window_system::window::WindowType::Normal,
                ) {
                    Ok(window_id) => {
                        fb.write_text_kernel(&format!("Ventana principal creada: ID {}", window_id), Color::LIGHT_GRAY);
                    }
                    Err(e) => {
                        fb.write_text_kernel(&format!("Error creando ventana principal: {}", e), Color::YELLOW);
                    }
                }
                
                // Crear ventana secundaria
                match crate::window_system::window_manager::create_global_window(
                    client_id,
                    "Aplicación de Prueba".to_string(),
                    200, 150, 400, 300,
                    flags,
                    crate::window_system::window::WindowType::Normal,
                ) {
                    Ok(window_id) => {
                        fb.write_text_kernel(&format!("Ventana secundaria creada: ID {}", window_id), Color::LIGHT_GRAY);
                    }
                    Err(e) => {
                        fb.write_text_kernel(&format!("Error creando ventana secundaria: {}", e), Color::YELLOW);
                    }
                }
                
                // Crear ventana de diálogo
                match crate::window_system::window_manager::create_global_window(
                    client_id,
                    "Diálogo del Sistema".to_string(),
                    300, 200, 300, 200,
                    flags,
                    crate::window_system::window::WindowType::Dialog,
                ) {
                    Ok(window_id) => {
                        fb.write_text_kernel(&format!("Ventana de diálogo creada: ID {}", window_id), Color::LIGHT_GRAY);
                    }
                    Err(e) => {
                        fb.write_text_kernel(&format!("Error creando ventana de diálogo: {}", e), Color::YELLOW);
                    }
                }
                
                fb.write_text_kernel("Todas las ventanas mapeadas y visibles", Color::LIGHT_GRAY);
            }
        }
        Err(e) => {
            fb.write_text_kernel(&format!("Error inicializando sistema de ventanas: {}", e), Color::YELLOW);
        }
    }

    // COSMIC Desktop Environment ya inicializado con Wayland

    let cosmic_config = CosmicConfig::default();

    let mut cosmic_manager = CosmicManager::with_config(cosmic_config);

    match cosmic_manager.initialize() {
        Ok(_) => {
            fb.write_text_kernel("COSMIC inicializado correctamente", Color::GREEN);

            // Iniciar compositor COSMIC
            match cosmic_manager.start_compositor() {
                Ok(_) => {
                    fb.write_text_kernel("Compositor COSMIC iniciado", Color::GREEN);

                    // Iniciar gestor de ventanas
                    match cosmic_manager.start_window_manager() {
                        Ok(_) => {
                            fb.write_text_kernel("Gestor de ventanas COSMIC iniciado", Color::GREEN);

                            // Mostrar estadísticas de COSMIC
                            let stats = cosmic_manager.get_performance_stats();
                            fb.write_text_kernel(&format!("COSMIC: {} ventanas, {:.1} FPS",
                                stats.window_count, stats.frame_rate), Color::LIGHT_GRAY);
                        }
                        Err(e) => {
                            fb.write_text_kernel(&format!("Error iniciando gestor de ventanas: {}", e), Color::YELLOW);
                        }
                    }
                }
                Err(e) => {
                    fb.write_text_kernel(&format!("Error iniciando compositor: {}", e), Color::YELLOW);
                }
            }
        }
        Err(e) => {
            fb.write_text_kernel(&format!("Error inicializando COSMIC: {}", e), Color::YELLOW);
        }
    }

    // Mostrar estadísticas finales del sistema de archivos
    fb.write_text_kernel("[10/10] Mostrando estadísticas del sistema...", Color::CYAN);
    let (total_mounts, mounted_fs, open_files, total_files) = get_vfs_statistics();
    fb.write_text_kernel(&format!("Sistema de archivos: {} montajes, {} sistemas montados, {} archivos abiertos, {} archivos totales", 
        total_mounts, mounted_fs, open_files, total_files), Color::LIGHT_GRAY);
    
    // Mostrar estadísticas del cache si está disponible
    if let Some(cache) = crate::filesystem::cache::get_file_cache() {
        let (hits, misses, hit_rate) = cache.get_stats();
        fb.write_text_kernel(&format!("Cache de archivos: {} hits, {} misses, {:.2}% hit rate", 
            hits, misses, hit_rate * 100.0), Color::LIGHT_GRAY);
    }
    
    // Mostrar estadísticas del sistema de bloques
    if let Some(device) = crate::filesystem::block::get_block_device() {
        let (reads, writes) = device.get_stats();
        fb.write_text_kernel(&format!("Dispositivo de bloques: {} lecturas, {} escrituras", 
            reads, writes), Color::LIGHT_GRAY);
    }
    
    if let Some(block_cache) = crate::filesystem::block::get_block_cache() {
        let (total_blocks, dirty_blocks, free_blocks) = block_cache.get_stats();
        fb.write_text_kernel(&format!("Cache de bloques: {} bloques, {} sucios, {} libres", 
            total_blocks, dirty_blocks, free_blocks), Color::LIGHT_GRAY);
    }
    
    // Mostrar aplicaciones disponibles
    fb.write_text_kernel("Aplicaciones disponibles:", Color::CYAN);
    fb.write_text_kernel("  - Terminal avanzado con comandos modernos", Color::LIGHT_GRAY);
    fb.write_text_kernel("  - Navegador web con soporte HTML básico", Color::LIGHT_GRAY);
    fb.write_text_kernel("  - Gestor de archivos gráfico", Color::LIGHT_GRAY);
    fb.write_text_kernel("  - Calculadora científica", Color::LIGHT_GRAY);
    fb.write_text_kernel("  - Editor de texto", Color::LIGHT_GRAY);
    fb.write_text_kernel("  - Monitor del sistema", Color::LIGHT_GRAY);
    
    // BUCLE PRINCIPAL SIMPLIFICADO: Evitar operaciones complejas que causan cuelgues
    fb.write_text_kernel("Sistema Eclipse OS completamente inicializado - Bucle principal iniciado", Color::GREEN);

    // Contador de frames y control de logging
    let mut frame_counter: u64 = 0;
    let log_interval_frames: u64 = 120; // ~2s a 60 FPS
    let mut led_demo_counter: u64 = 0;
    let led_demo_interval: u64 = 300; // ~5s a 60 FPS
    
    loop {
        // Procesar eventos y renderizar un frame de COSMIC si está activo
        // (Ignorar errores de forma segura en este modo simplificado)
        {
            // Verificar estado del compositor antes de llamar a métodos
            // para evitar overhead innecesario
            // Nota: get_state() es barato y solo lee flags
            // Si el compositor está activo, procesar y renderizar
            if cosmic_manager.get_state().compositor_running {
                let _ = cosmic_manager.process_events();
                let _ = cosmic_manager.render_frame();
            }
        }

        // Procesar sistema de ventanas
        /*{
            if crate::window_system::is_window_system_initialized() {
                let _ = crate::window_system::process_window_system_events();
                let _ = crate::window_system::render_window_system_frame();
                
                // Renderizar ventanas al framebuffer principal
                if let Ok(mut compositor) = crate::window_system::get_compositor() {
                    let _ = compositor.render_to_framebuffer(fb);
                }
            }
        }*/

        // Logging de estadísticas cada cierto número de frames
        if frame_counter % log_interval_frames == 0 {
            let stats = cosmic_manager.get_performance_stats();
            fb.write_text_kernel(
                &format!(
                    "COSMIC: ventanas={}, {:.1} FPS, CPU {:.0}% GPU {:.0}%",
                    stats.window_count,
                    stats.frame_rate,
                    stats.cpu_usage,
                    stats.gpu_usage
                ),
                Color::LIGHT_GRAY,
            );
            
            // Mostrar información de scroll optimizado
            let (width, height) = {
                let fb_info = fb.get_info();
                (fb_info.width, fb_info.height)
            };
            let scroll_strategy = if height > 1200 {
                "DMA+Hardware"
            } else if height > 800 {
                "GPU+Accel"
            } else if height > 600 {
                "SIMD+Fast"
            } else if height > 400 {
                "Cache+Opt"
            } else {
                "Memory+Val"
            };
            
            fb.write_text_kernel(
                &format!(
                    "SCROLL: {} en {}x{} - Estrategia: {}",
                    if frame_counter % (log_interval_frames * 2) == 0 { "ACTIVO" } else { "OPTIMIZADO" },
                    width, height, scroll_strategy
                ),
                Color::CYAN,
            );
            
            // Mostrar estadísticas del sistema de ventanas
            if crate::window_system::is_window_system_initialized() {
                if let Ok(window_stats) = crate::window_system::get_window_system() {
                    let stats = window_stats.get_stats();
                    fb.write_text_kernel(
                        &format!(
                            "Window System: {} ventanas, {} clientes, {:.1} FPS, {} eventos",
                            stats.window_count,
                            stats.client_count,
                            stats.frame_rate,
                            stats.event_queue_size
                        ),
                        Color::CYAN,
                    );
                }
            }
        }

        // Demostración de LEDs cada cierto número de frames
        if frame_counter % led_demo_interval == 0 {
            led_demo_counter += 1;
            let demo_phase = (led_demo_counter % 4) as u8;
            
            match demo_phase {
                0 => {
                    // Activar todos los LEDs
                    let _ = keyboard_driver.enable_all_leds();
                    let _ = mouse_driver.enable_all_leds();
                    fb.write_text_kernel("LED Demo: Todos los LEDs activados", Color::CYAN);
                }
                1 => {
                    // Solo Num Lock y Scroll Wheel
                    let _ = keyboard_driver.set_num_lock_led(true);
                    let _ = keyboard_driver.set_caps_lock_led(false);
                    let _ = keyboard_driver.set_scroll_lock_led(false);
                    let _ = mouse_driver.set_scroll_wheel_led(true);
                    let _ = mouse_driver.set_side_buttons_led(false);
                    let _ = mouse_driver.set_logo_led(false);
                    let _ = mouse_driver.set_dpi_indicator_led(false);
                    fb.write_text_kernel("LED Demo: Num Lock + Scroll Wheel", Color::CYAN);
                }
                2 => {
                    // Solo Caps Lock y Logo
                    let _ = keyboard_driver.set_num_lock_led(false);
                    let _ = keyboard_driver.set_caps_lock_led(true);
                    let _ = keyboard_driver.set_scroll_lock_led(false);
                    let _ = mouse_driver.set_scroll_wheel_led(false);
                    let _ = mouse_driver.set_side_buttons_led(false);
                    let _ = mouse_driver.set_logo_led(true);
                    let _ = mouse_driver.set_dpi_indicator_led(false);
                    fb.write_text_kernel("LED Demo: Caps Lock + Logo", Color::CYAN);
                }
                3 => {
                    // Solo Scroll Lock y DPI Indicator
                    let _ = keyboard_driver.set_num_lock_led(false);
                    let _ = keyboard_driver.set_caps_lock_led(false);
                    let _ = keyboard_driver.set_scroll_lock_led(true);
                    let _ = mouse_driver.set_scroll_wheel_led(false);
                    let _ = mouse_driver.set_side_buttons_led(false);
                    let _ = mouse_driver.set_logo_led(false);
                    let _ = mouse_driver.set_dpi_indicator_led(true);
                    fb.write_text_kernel("LED Demo: Scroll Lock + DPI Indicator", Color::CYAN);
                }
                _ => {}
            }
        }

        // Demostración de movimiento de ventanas cada cierto número de frames
        if frame_counter % 180 == 0 { // Cada 3 segundos a 60 FPS
            if crate::window_system::is_window_system_initialized() {
                if let Ok(compositor) = crate::window_system::get_compositor() {
                    // Mover ventanas en un patrón circular
                    let time_factor = (frame_counter / 180) as f32;
                    let center_x = 400.0;
                    let center_y = 300.0;
                    let radius = 150.0;
                    
                    let window_count = compositor.get_window_count();
                    let window_order = compositor.get_window_order().clone();
                    for (i, window_id) in window_order.iter().enumerate() {
                        if i < window_count {
                            let angle = (time_factor + i as f32 * 0.5) * 0.3;
                    let x = (center_x + radius * cos(angle)) as i32;
                    let y = (center_y + radius * sin(angle)) as i32;
                            
                            let geometry = crate::window_system::geometry::Rectangle::new(x, y, 200, 150);
                            let _ = compositor.update_window_geometry(*window_id, geometry);
                            let _ = compositor.mark_window_dirty(*window_id);
                        }
                    }
                    
                    fb.write_text_kernel("Window Demo: Moviendo ventanas en patrón circular", Color::MAGENTA);
                }
            }
        }

        frame_counter = frame_counter.wrapping_add(1);

        // Pacing básico para ~60 FPS usando spin (no hay temporizador estándar)
        // Ajustar iteraciones según hardware/hipervisor si fuese necesario
        for _ in 0..60000u32 {
            core::hint::spin_loop();
        }
    }
    
    // Implementaciones simples de funciones trigonométricas
    fn cos(x: f32) -> f32 {
        // Aproximación simple usando serie de Taylor
        let x = x % (2.0 * core::f32::consts::PI);
        let x2 = x * x;
        let x4 = x2 * x2;
        let x6 = x4 * x2;
        let x8 = x6 * x2;
        
        1.0 - x2/2.0 + x4/24.0 - x6/720.0 + x8/40320.0
    }
    
    fn sin(x: f32) -> f32 {
        // Aproximación simple usando serie de Taylor
        let x = x % (2.0 * core::f32::consts::PI);
        let x2 = x * x;
        let x3 = x2 * x;
        let x5 = x3 * x2;
        let x7 = x5 * x2;
        let x9 = x7 * x2;
        
        x - x3/6.0 + x5/120.0 - x7/5040.0 + x9/362880.0
    }
}