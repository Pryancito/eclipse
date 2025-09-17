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
    FramebufferDriver, FramebufferInfo
};
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
use crate::hardware_detection::{GraphicsMode, detect_graphics_hardware};
use crate::drivers::ipc::{DriverManager, DriverMessage, DriverResponse};
use crate::drivers::pci_driver::PciDriver;
use crate::drivers::nvidia_pci_driver::NvidiaPciDriver;
use crate::drivers::binary_driver_manager::{BinaryDriverManager, BinaryDriverMetadata};
use crate::ipc::{IpcManager, IpcMessage, DriverType, DriverConfig, DriverCommandType};
use crate::hotplug::{HotplugManager, UsbDeviceType, UsbHotplugEvent};
use crate::hotplug::HotplugConfig;
use crate::graphics::{GraphicsManager, Position, Size, WidgetType};
use crate::graphics::graphics_manager::GraphicsConfig;

/// Función principal del kernel
pub fn kernel_main(fb: &mut FramebufferDriver) {
    // Configurar SSE/MMX antes de cualquier operación
    unsafe {
        // Asegurar que SSE esté habilitado
        core::arch::asm!(
            "mov rax, cr0",
            "and rax, ~(1 << 2)",        // CR0.EM = 0
            "or  rax,  (1 << 1)",        // CR0.MP = 1
            "mov cr0, rax",
            "mov rax, cr4",
            "or  rax,  (1 << 9)",        // CR4.OSFXSR = 1
            "or  rax,  (1 << 10)",       // CR4.OSXMMEXCPT = 1
            "mov cr4, rax"
        );
    }
    
    // Asegurar allocador inicializado antes de usar alloc en este main
    #[cfg(feature = "alloc")]
    {
        crate::allocator::init_allocator();
    }
    
    // Debug: Verificar estado del framebuffer
    let fb_initialized = fb.is_initialized();
    let fb_width = fb.info.width;
    let fb_height = fb.info.height;
    let fb_base = fb.info.base_address;
    
    if fb_initialized {
        // Usar la API del framebuffer si está disponible
        fb.clear_screen(Color::BLACK);
        // Intentar escribir texto simple
        fb.write_text_kernel("Bienvenido a Eclipse OS!", Color::WHITE);
    } else {
        // Si el framebuffer no está inicializado, intentar inicialización de emergencia
        panic!("Framebuffer no inicializado - base: 0x{:x}, width: {}, height: {}", 
               fb_base, fb_width, fb_height);
    }
    fb.write_text_kernel("[1/6] Detectando hardware...", Color::WHITE);
    // Detección de hardware
    let hw_result = detect_graphics_hardware();

    fb.write_text_kernel("[2/6] Hardware detectado correctamente", Color::GREEN);

    // Inicializar sistema IPC de drivers
    fb.write_text_kernel("Inicializando sistema IPC de drivers...", Color::YELLOW);
    let mut driver_manager = DriverManager::new();
    let mut ipc_manager = IpcManager::new();
    let mut binary_driver_manager = BinaryDriverManager::new();
    
    // Sistema de hot-plug removido para simplificar el kernel
    fb.write_text_kernel("Sistema de hot-plug removido", Color::YELLOW);
    
    // Registrar driver PCI base
    fb.write_text_kernel("[3/6] Registrando PCI driver...", Color::LIGHT_GRAY);
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
        let nvidia_driver = Box::new(NvidiaPciDriver::new());
        match driver_manager.register_driver(nvidia_driver) {
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
    
    // Inicializar sistema de gráficos avanzado
    // Modo texto temporal (Wayland más adelante): omitimos creación de ventanas/render
    fb.write_text_kernel("Modo texto: sistema gráfico omitido (Wayland pendiente)", Color::LIGHT_GRAY);
    
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

    fb.write_text_kernel("[3/6] Modo grafico: ", Color::WHITE);
    fb.write_text_kernel(modo_str, color_modo);

    // Selector de backend de vídeo: Virtio-GPU si es la GPU primaria, GOP como fallback
    if let Some(primary) = &hw_result.primary_gpu {
        match primary.gpu_type {
            GpuType::Virtio => {
                let mut virt = VirtioGpuDriver::new();
                match virt.initialize() {
                    Ok(_) => fb.write_text_kernel("Backend de vídeo: Virtio-GPU", Color::GREEN),
                    Err(e) => fb.write_text_kernel(&format!("Backend de vídeo: Virtio-GPU (falló init: {:?})", e), Color::YELLOW),
                }
            }
            GpuType::QemuBochs => {
                let mut bochs = BochsVbeDriver::new();
                match bochs.initialize() {
                    Ok(_) => fb.write_text_kernel("Backend de vídeo: Bochs VBE", Color::GREEN),
                    Err(_) => fb.write_text_kernel("Backend de vídeo: Bochs VBE (falló init)", Color::YELLOW),
                }
            }
            GpuType::Vmware => {
                let mut vmw = VmwareSvgaDriver::new();
                match vmw.initialize() {
                    Ok(_) => fb.write_text_kernel("Backend de vídeo: VMware SVGA II", Color::GREEN),
                    Err(_) => fb.write_text_kernel("Backend de vídeo: VMware SVGA II (falló init)", Color::YELLOW),
                }
            }
            _ => {
                fb.write_text_kernel("Backend de vídeo: GOP/UEFI (fallback)", Color::LIGHT_GRAY);
            }
        }
    } else {
        fb.write_text_kernel("Backend de vídeo: GOP/UEFI (sin GPU primaria)", Color::LIGHT_GRAY);
    }

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
    fb.write_text_kernel("[4/6] Iniciando sistema de AI...", Color::YELLOW);
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
    fb.write_text_kernel("[5/6] Inicializando drivers USB...", Color::YELLOW);
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
    } else {
        fb.write_text_kernel("Teclado USB: Error", Color::RED);
    }
    
    if mouse_init_result.is_ok() {
        fb.write_text_kernel("Mouse USB: Inicializado", Color::GREEN);
    } else {
        fb.write_text_kernel("Mouse USB: Error", Color::RED);
    }
    
    // Inicializar Wayland si está disponible
    fb.write_text_kernel("[6/8] Inicializando Wayland...", Color::CYAN);
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

    // Inicializar COSMIC Desktop Environment
    fb.write_text_kernel("[7/8] Inicializando COSMIC Desktop Environment...", Color::CYAN);

    let cosmic_config = CosmicConfig {
        enable_ai_features: true,
        enable_space_theme: true,
        enable_hardware_acceleration: true,
        window_manager_mode: WindowManagerMode::Hybrid,
        ai_assistant_enabled: true,
        performance_mode: PerformanceMode::Balanced,
    };

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

    // BUCLE PRINCIPAL SIMPLIFICADO: Evitar operaciones complejas que causan cuelgues
    fb.write_text_kernel("[8/8] Sistema listo - Bucle principal iniciado", Color::GREEN);
    
    loop {
        // Pausa optimizada para el loop
        for _ in 0..100000 {
            core::hint::spin_loop();
        }
    }
}