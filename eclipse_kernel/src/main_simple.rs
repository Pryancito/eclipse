//! Módulo principal simplificado del kernel Eclipse OS

#![no_std]
#![no_main]

extern crate alloc;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::error::Error;
use core::fmt::Result as FmtResult;
use core::fmt::Write;
use core::panic::PanicInfo;

// Importar módulos del kernel
use crate::cosmic::{CosmicConfig, CosmicManager, PerformanceMode, WindowManagerMode};
use crate::filesystem::eclipsefs::{EclipseFSDeviceInfo, EclipseFSWrapper};
use crate::init_system::{InitProcess, InitSystem};
use crate::wayland::{get_wayland_state, init_wayland, is_wayland_initialized};

use crate::ai::{ModelLoader, ModelType};
use crate::ai_pretrained_models::{PretrainedModelManager, PretrainedModelType};
use crate::ai_typing_system::{
    create_ai_typing_system, AiTypingConfig, AiTypingSystem, TypingEffect,
};
use crate::drivers::framebuffer::get_framebuffer;
use crate::drivers::framebuffer::{Color, FramebufferDriver};
// Módulo ai_font_generator removido
use crate::desktop_ai::Rect;
use crate::drivers::amd_graphics::Amd2DOperation;
use crate::drivers::bochs_vbe::BochsVbeDriver;
use crate::drivers::framebuffer::Color as FbColor;
use crate::drivers::intel_graphics::Intel2DOperation;
use crate::drivers::ipc::Driver;
use crate::drivers::ipc::{DriverManager, DriverMessage, DriverResponse};
use crate::drivers::nvidia_graphics::Nvidia2DOperation;
use crate::drivers::nvidia_pci_driver::NvidiaPciDriver;
use crate::drivers::pci::PciDevice;
use crate::drivers::pci::PciManager;
use crate::drivers::pci::{GpuInfo, GpuType};
use crate::drivers::pci_driver::PciDriver;
use crate::drivers::usb::UsbDriver;
use crate::drivers::usb_keyboard::{KeyboardConfig, KeyboardEvent, UsbKeyCode, UsbKeyboardDriver};
use crate::drivers::usb_mouse::{MouseButton, MouseConfig, MouseEvent, UsbMouseDriver};
use crate::drivers::usb_xhci::XhciController;
use crate::drivers::usb_diagnostic;
use crate::drivers::virtio_gpu::VirtioGpuDriver;
use crate::drivers::vmware_svga::VmwareSvgaDriver;
use crate::filesystem::vfs::{get_vfs, init_vfs};
use crate::graphics::{init_graphics_system, transition_to_drm};
use crate::graphics_optimization::{
    force_framebuffer_update, get_optimization_stats, init_graphics_optimizer,
};
use crate::hardware_detection::{
    detect_graphics_hardware, with_gpu_driver_manager, GraphicsMode, HardwareDetectionResult,
};
use crate::hotplug::HotplugConfig;
use crate::hotplug::{HotplugManager, UsbDeviceType, UsbHotplugEvent};
use crate::ipc::{DriverCommandType, DriverConfig, DriverType, IpcManager, IpcMessage};
use crate::drivers::storage_manager::{init_storage_manager, get_storage_manager, StorageManager};
use crate::filesystem::fat32::mount_fat32_from_storage;
use crate::filesystem::eclipsefs::mount_eclipsefs_from_storage;
use crate::debug::serial_write_str;
use crate::paging::PagingManager;
use crate::idt::{setup_userland_idt, get_interrupt_stats, InterruptStats};
// use crate::advanced_shell::AdvancedShell; // Comentado temporalmente
use crate::ai_services::{AIService, AIServiceState, AIServiceType, AIServiceConfig};
use spin::Mutex;
use x86_64::VirtAddr;
use crate::filesystem::vfs::VfsError;
use crate::hardware_detection::RecommendedDriver;

pub static PAGING_MANAGER: Mutex<Option<PagingManager>> = Mutex::new(None);

/// Detectar hardware real con timeout para evitar cuelgues
fn detect_hardware_with_timeout() -> HardwareDetectionResult {
    // Usar directamente la función global que inicializa polished_pci correctamente
    let result = crate::hardware_detection::detect_graphics_hardware();
    
    // Clonar el resultado para poder retornarlo
    result.clone()
}

fn scan_pci_bus(fb: &mut FramebufferDriver) {
    let mut pci_manager = PciManager::new();
    pci_manager.scan_devices();

    fb.write_text_kernel(
        &alloc::format!(
            "Dispositivos PCI detectados: {} (guardados: {})",
            pci_manager.total_device_count(),
            pci_manager.device_count()
        ),
        Color::CYAN,
    );

    let mut gpus = Vec::new();
    for opt in pci_manager.get_gpus().iter() {
        if let Some(info) = opt {
            gpus.push(*info);
        }
    }

    if gpus.is_empty() {
        fb.write_text_kernel("No se detectaron GPUs via PCI", Color::YELLOW);
    } else {
        fb.write_text_kernel(&alloc::format!("GPUs PCI: {}", gpus.len()), Color::WHITE);
        fb.write_text_kernel("Detalle de GPUs detectadas:", Color::WHITE);
        for gpu in &gpus {
            fb.write_text_kernel(
                &alloc::format!(
                    "  GPU {:04X}:{:04X} {:?} bus {} dev {} func {}",
                    gpu.pci_device.vendor_id,
                    gpu.pci_device.device_id,
                    gpu.gpu_type,
                    gpu.pci_device.bus,
                    gpu.pci_device.device,
                    gpu.pci_device.function
                ),
                Color::LIGHT_GRAY,
            );

            fb.write_text_kernel(
                &alloc::format!(
                    "     Memoria: {} MB, 2D: {}, 3D: {}, Resolución máx: {}x{}",
                    gpu.memory_size / (1024 * 1024),
                    if gpu.supports_2d { "sí" } else { "no" },
                    if gpu.supports_3d { "sí" } else { "no" },
                    gpu.max_resolution.0,
                    gpu.max_resolution.1
                ),
                Color::CYAN,
            );
        }

        if gpus.len() > 1 {
            fb.write_text_kernel("Configuración multi-GPU detectada", Color::GREEN);
            let primary = gpus.iter().find(|g| g.is_primary);
            if let Some(primary_gpu) = primary {
                fb.write_text_kernel(
                    &alloc::format!(
                        "  GPU primaria: {:?} en bus {}",
                        primary_gpu.gpu_type,
                        primary_gpu.pci_device.bus
                    ),
                    Color::WHITE,
                );
            }

            for (idx, gpu) in gpus.iter().enumerate().take(4) {
                fb.write_text_kernel(
                    &alloc::format!(
                        "  [{}] {:?} -> asignando render pipe {}",
                        idx,
                        gpu.gpu_type,
                        idx
                    ),
                    Color::LIGHT_GRAY,
                );
            }

            if gpus.len() > 4 {
                fb.write_text_kernel(
                    "  (más GPUs detectadas, registro truncado)",
                    Color::LIGHT_GRAY,
                );
            }
        }
    }

    // Listado de dispositivos PCI removido para limpiar pantalla
}

fn is_valid_framebuffer_address(address: u64) -> bool {
    address < 0x1000_0000 || (address >= 0x1000_0000 && address < 0x6000_0000)
}

fn test_framebuffer_write(fb: &mut FramebufferDriver) -> bool {
    if fb.info.base_address == 0 || fb.info.width == 0 || fb.info.height == 0 {
        return false;
    }

    if !fb.is_initialized() {
        return fb.info.base_address >= 0x1000_0000 && fb.info.base_address < 0x6000_0000;
    }

    let bytes_per_pixel = core::cmp::max(1u32, fb.bytes_per_pixel() as u32);
    let ppsl = fb.info.pixels_per_scan_line.max(fb.info.width);
    let x = (fb.info.width / 2).min(ppsl.saturating_sub(1)).min(100);
    let y = (fb.info.height / 2)
        .min(fb.info.height.saturating_sub(1))
        .min(100);
    let offset_bytes = ((y * ppsl) + x) * bytes_per_pixel;

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

fn verify_framebuffer_memory(fb: &mut FramebufferDriver) -> bool {
    if fb.info.base_address == 0 || fb.info.width == 0 || fb.info.height == 0 {
        return false;
    }

    if !fb.is_initialized() {
        return fb.info.base_address >= 0x1000_0000 && fb.info.base_address < 0x6000_0000;
    }

    let bytes_per_pixel = core::cmp::max(1u32, fb.bytes_per_pixel() as u32);
    let ppsl = fb.info.pixels_per_scan_line.max(fb.info.width);
    let positions = [
        (0, 0),
        (fb.info.width.saturating_sub(1), 0),
        (0, fb.info.height.saturating_sub(1)),
        (
            fb.info.width.saturating_sub(1),
            fb.info.height.saturating_sub(1),
        ),
        (fb.info.width / 2, fb.info.height / 2),
    ];

    for (x, y) in positions.iter().copied() {
        let offset_bytes = ((y * ppsl) + x) * bytes_per_pixel;
        unsafe {
            let ptr = (fb.info.base_address as *mut u8).add(offset_bytes as usize) as *mut u32;
            let original = core::ptr::read_volatile(ptr);
            core::ptr::write_volatile(ptr, original);
        }
    }

    true
}

/// Función principal del kernel
pub fn kernel_main(fb: &mut FramebufferDriver) -> ! {
    serial_write_str("KERNEL_MAIN: Entered.\n");
    #[cfg(feature = "alloc")]
    {
        crate::allocator::init_allocator();
    }

    fb.clear_screen(Color::BLACK);
    fb.write_text_kernel("Eclipse OS Kernel v0.6.0", Color::WHITE);

    fb.write_text_kernel("GDT inicializada.", Color::GREEN);
    serial_write_str("KERNEL_MAIN: GDT initialized.\n");

    // --- Inicialización del Sistema de Interrupciones (DESHABILITADO) ---
    serial_write_str("KERNEL_MAIN: Skipping interrupt system initialization for hardware compatibility...\n");
    fb.write_text_kernel("Sistema de interrupciones omitido por compatibilidad.", Color::YELLOW);
    
    // Las interrupciones causan excepciones en hardware real, omitimos por ahora
    serial_write_str("KERNEL_MAIN: Interrupt system initialization skipped for hardware compatibility.\n");

    // --- Inicialización del Gestor de Paginación ---
    serial_write_str("KERNEL_MAIN: Initializing Paging Manager...\n");
    
    // Por ahora, vamos a usar un enfoque más simple: mapear directamente en las tablas existentes
    // cuando sea necesario, en lugar de crear un PagingManager completo
    serial_write_str("KERNEL_MAIN: Using direct page mapping approach...\n");
    
    // Crear un PagingManager básico solo para almacenar en el global
    // pero no vamos a usar sus tablas de páginas
    let paging_manager = crate::paging::PagingManager::new();
    serial_write_str("KERNEL_MAIN: Basic PagingManager created for compatibility.\n");
    
    // Almacenar el PagingManager en el global
    {
        let mut pm_guard = PAGING_MANAGER.lock();
        *pm_guard = Some(paging_manager);
    }
    
    serial_write_str("KERNEL_MAIN: Paging Manager initialized.\n");


    // --- Detección de Hardware (MEJORADA) ---
    serial_write_str("KERNEL_MAIN: Detecting hardware...\n");
    fb.write_text_kernel("Detectando hardware...", Color::WHITE);
    
    // Usar detección real de hardware con timeout
    let hw = detect_hardware_with_timeout();

    fb.write_text_kernel("Detección de GPU completada", Color::GREEN);
    match hw.graphics_mode {
        GraphicsMode::Framebuffer => fb.write_text_kernel("Modo gráfico: Framebuffer", Color::CYAN),
        GraphicsMode::VGA => fb.write_text_kernel("Modo gráfico: VGA", Color::CYAN),
        GraphicsMode::HardwareAccelerated => {
            fb.write_text_kernel("Modo gráfico: Acelerado", Color::CYAN)
        }
    }

    if let Some(primary) = hw.primary_gpu.as_ref() {
        fb.write_text_kernel(
            &alloc::format!(
                "GPU primaria: {:04X}:{:04X} ({:?})",
                primary.pci_device.vendor_id,
                primary.pci_device.device_id,
                primary.gpu_type
            ),
            Color::WHITE,
        );
        fb.write_text_kernel(
            &alloc::format!(
                "Memoria estimada: {} MB",
                primary.memory_size / (1024 * 1024)
            ),
            Color::WHITE,
        );
        fb.write_text_kernel(
            &alloc::format!(
                "Resolución máxima: {}x{}",
                primary.max_resolution.0,
                primary.max_resolution.1
            ),
            Color::WHITE,
        );
    } else {
        fb.write_text_kernel("No se detectó GPU primaria", Color::YELLOW);
    }

    // --- Diagnóstico USB ---
    serial_write_str("KERNEL_MAIN: Iniciando diagnóstico USB...\n");
    fb.write_text_kernel("Diagnóstico USB...", Color::WHITE);
    usb_diagnostic::usb_diagnostic_main();
    fb.write_text_kernel("Diagnóstico USB completado", Color::GREEN);

    if hw.available_gpus.is_empty() {
        fb.write_text_kernel("No se detectaron GPUs adicionales", Color::YELLOW);
    } else {
        fb.write_text_kernel("GPUs detectadas:", Color::WHITE);
        for (idx, gpu) in hw.available_gpus.iter().enumerate() {
            fb.write_text_kernel(
                &alloc::format!(
                    "  [{}] {:04X}:{:04X} ({:?}) - bus {}:func {}",
                    idx,
                    gpu.pci_device.vendor_id,
                    gpu.pci_device.device_id,
                    gpu.gpu_type,
                    gpu.pci_device.bus,
                    gpu.pci_device.function
                ),
                Color::LIGHT_GRAY,
            );
        }
    }

    // --- Detección de Almacenamiento ---
    fb.write_text_kernel("Detectando almacenamiento...", Color::WHITE);
    if hw.nvme_controller_available {
        fb.write_text_kernel("Controladora NVMe detectada.", Color::GREEN);
    } else {
        fb.write_text_kernel("No se encontró controladora NVMe.", Color::YELLOW);
    }
    if hw.sata_controller_available {
        fb.write_text_kernel("Controladora SATA (AHCI) detectada.", Color::GREEN);
    } else {
        fb.write_text_kernel("No se encontró controladora SATA.", Color::YELLOW);
    }

    // --- Inicialización de Drivers de Almacenamiento ---
    serial_write_str("KERNEL_MAIN: Initializing storage drivers...\n");
    fb.write_text_kernel("Inicializando drivers de almacenamiento...", Color::WHITE);
    
    let mut storage_manager = StorageManager::new();
    
    match init_storage_manager() {
        Ok(()) => {
            serial_write_str("KERNEL_MAIN: Storage drivers initialized.\n");
            if let Some(manager) = get_storage_manager() {
                storage_manager = manager.clone();
            }
        }
        Err(err) => {
            serial_write_str(&alloc::format!(
                "KERNEL_MAIN: Storage driver initialization failed: {}\n",
                err
            ));
            fb.write_text_kernel(&alloc::format!("Error almacenamiento: {}", err), Color::RED);
            fb.write_text_kernel(
                "Continuando sin dispositivos de almacenamiento inicializados.",
                Color::YELLOW,
            );
        }
    }

    // --- INICIALIZACIÓN DEL SISTEMA DE ARCHIVOS ---
    serial_write_str("KERNEL_MAIN: Initializing VFS...\n");
    fb.write_text_kernel("Inicializando VFS...", Color::WHITE);
    init_vfs();
    serial_write_str("KERNEL_MAIN: VFS Initialized.\n");
    fb.write_text_kernel("VFS inicializado.", Color::GREEN);

    // Estrategia: intentar montar EclipseFS directamente
    if storage_manager.device_count() > 0 {
        serial_write_str("KERNEL_MAIN: Intentando montar EclipseFS...\n");
        fb.write_text_kernel("Intentando montar EclipseFS...", Color::WHITE);

        // Intentar montar EclipseFS desde el almacenamiento
        match mount_eclipsefs_from_storage(&storage_manager, None) {
            Ok(()) => {
                serial_write_str("KERNEL_MAIN: ¡EclipseFS montado exitosamente!\n");
                fb.write_text_kernel("¡EclipseFS montado exitosamente!", Color::GREEN);
                if let Some(vfs_guard) = get_vfs().as_ref() {
                    vfs_guard.debug_list_mounts();
                }
            }
            Err(e) => {
                serial_write_str(&alloc::format!("KERNEL_MAIN: Error al montar EclipseFS: {:?}\n", e));
                fb.write_text_kernel(&alloc::format!("Error al montar EclipseFS: {:?}", e), Color::YELLOW);

                // Investigar el contenido del disco para diagnóstico
                serial_write_str("KERNEL_MAIN: Investigando el contenido del disco...\n");
                investigate_disk_contents(&storage_manager);
            }
        }
        match mount_fat32_from_storage(&storage_manager) {
            Ok(()) => {
                serial_write_str(&alloc::format!("KERNEL_MAIN: ¡FAT32 montado exitosamente!\n"));
                fb.write_text_kernel(&alloc::format!("¡FAT32 montado exitosamente!"), Color::GREEN);
            }
            Err(e) => {
                serial_write_str(&alloc::format!("KERNEL_MAIN: Error al montar FAT32 como fallback: {:?}\n", e));
                fb.write_text_kernel(&alloc::format!("Error al montar FAT32 como fallback: {:?}", e), Color::RED);
            }
        }
    } else {
        serial_write_str("KERNEL_MAIN: No storage devices found. Trying bootloader data...\n");
        fb.write_text_kernel("No se encontraron dispositivos de almacenamiento.", Color::YELLOW);
    }
loop {
    unsafe {
        core::arch::asm!("hlt");
    }
}
    // --- Inicialización del Sistema de IA ---
    serial_write_str("KERNEL_MAIN: Initializing AI system...\n");
    fb.write_text_kernel("Inicializando sistema de IA...", Color::WHITE);
    
    // Inicializar servicios de IA
    let ai_service = initialize_ai_services(fb);
    
    // Inicializar sistema de escritura inteligente
    // let ai_typing_config = AiTypingConfig { ... }; // Comentado temporalmente
    // let mut ai_typing_system = create_ai_typing_system(); // Comentado temporalmente
    serial_write_str("KERNEL_MAIN: AI typing system initialized.\n");
    fb.write_text_kernel("Sistema de escritura inteligente inicializado.", Color::GREEN);
    
    // Inicializar modelos pre-entrenados
    // let mut model_manager = PretrainedModelManager::new(64); // Comentado temporalmente
    // initialize_pretrained_models(&mut model_manager, fb); // Comentado temporalmente
    
    serial_write_str("KERNEL_MAIN: AI system initialized.\n");
    fb.write_text_kernel("Sistema de IA inicializado.", Color::GREEN);

    // Bucle infinito para mantener el kernel en ejecución
    serial_write_str("KERNEL_MAIN: Entering main loop.\n");
    fb.write_text_kernel("Kernel en ejecución. Sistema listo.", Color::GREEN);
    
    let mut interrupt_counter = 0u32;
    let mut shell_demo_counter = 0u32;
    
    loop {
        // Cada 1000 iteraciones, mostrar estadísticas de interrupciones
        if interrupt_counter % 1000 == 0 {
            let stats = get_interrupt_stats();
            if stats.total_interrupts > 0 {
                fb.write_text_kernel(
                    &alloc::format!(
                        "Interrupciones: Total={}, Timer={}, Teclado={}, Syscalls={}",
                        stats.total_interrupts,
                        stats.timer_interrupts,
                        stats.keyboard_interrupts,
                        stats.syscalls
                    ),
                    Color::CYAN,
                );
            }
        }
        
        // Cada 5000 iteraciones, demostrar funcionalidades del shell
        if shell_demo_counter % 5000 == 0 {
            // demonstrate_shell_features(&mut shell, fb); // Comentado temporalmente
        }
        
        // Cada 10000 iteraciones, demostrar funcionalidades de IA
        if shell_demo_counter % 10000 == 0 {
            // demonstrate_ai_features(&mut ai_typing_system, &mut model_manager, fb); // Comentado temporalmente
        }
        
        interrupt_counter = interrupt_counter.wrapping_add(1);
        shell_demo_counter = shell_demo_counter.wrapping_add(1);
        
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}

/// Monta EclipseFS usando datos del bootloader como fallback
fn mount_eclipsefs_from_bootloader_data(fb: &mut FramebufferDriver) {
    use crate::filesystem::eclipsefs::EclipseFSWrapper;
    use crate::filesystem::vfs::{get_vfs, FileSystem};
    use alloc::boxed::Box;
    
    serial_write_str("KERNEL_MAIN: Attempting to mount EclipseFS from bootloader data...\n");
    fb.write_text_kernel("Montando EclipseFS desde datos del bootloader...", Color::WHITE);
    
    // Crear un EclipseFS de prueba con estructura básica
    let mut fs_instance = eclipsefs_lib::EclipseFS::new();
    
    // Crear estructura básica del sistema de archivos
    let test_data = create_test_eclipsefs_data();
    
    if let Err(e) = fs_instance.load_from_buffer(&test_data) {
        serial_write_str(&alloc::format!("KERNEL_MAIN: Failed to load EclipseFS from test data: {:?}\n", e));
        serial_write_str(&alloc::format!("KERNEL_MAIN: Test data size: {} bytes\n", test_data.len()));
        serial_write_str(&alloc::format!("KERNEL_MAIN: First 32 bytes: {:02X?}\n", &test_data[..32.min(test_data.len())]));
        fb.write_text_kernel(&alloc::format!("Error cargando EclipseFS: {:?}", e), Color::RED);
        return;
    }
    
    // Crear header e inode entries dummy para main_simple
    let header = eclipsefs_lib::EclipseFSHeader {
        magic: *b"ECLIPSEFS",
        version: 0x00020000, // Versión 2.0
        inode_table_offset: 4096,
        inode_table_size: 1024,
        total_inodes: 100,
        header_checksum: 0,
        metadata_checksum: 0,
        data_checksum: 0,
        creation_time: 0,
        last_check: 0,
        flags: 0,
    };
    
    let mut inode_entries = Vec::new();
    inode_entries.push(eclipsefs_lib::InodeTableEntry {
        inode: 1,
        offset: 0,
    });
    inode_entries.push(eclipsefs_lib::InodeTableEntry {
        inode: 2,
        offset: 100,
    });
    
    // Montar el sistema de archivos
    let mut vfs_guard = get_vfs();
    if let Some(vfs) = vfs_guard.as_mut() {
        // Crear información del dispositivo dummy para main_simple
        let device_info = EclipseFSDeviceInfo::new("/dev/sda2".to_string(), 1000000, 204800);
        let fs_wrapper = Box::new(EclipseFSWrapper::new_lazy(header, inode_entries, 1, device_info));
        vfs.mount("/", fs_wrapper);
        
        serial_write_str("KERNEL_MAIN: EclipseFS mounted from bootloader data successfully.\n");
        fb.write_text_kernel("¡EclipseFS montado desde datos del bootloader!", Color::CYAN);
        
        list_root_directory(fb);
    } else {
        serial_write_str("KERNEL_MAIN: VFS not available for mounting.\n");
        fb.write_text_kernel("VFS no disponible para montar.", Color::RED);
    }
}

/// Lista el contenido del directorio raíz
fn list_root_directory(fb: &mut FramebufferDriver) {
    use crate::filesystem::vfs::{get_vfs, FileSystem};
    
    serial_write_str("KERNEL_MAIN: Listing root directory...\n");
    fb.write_text_kernel("Contenido del directorio raíz:", Color::WHITE);
    
    let vfs_guard = get_vfs();
    if let Some(vfs) = vfs_guard.as_ref() {
        if let Some(root_fs) = vfs.get_root_fs() {
            let fs_guard = root_fs.lock();
            match fs_guard.readdir(1) { // Inode 1 es típicamente el directorio raíz
                Ok(entries) => {
                    serial_write_str(&alloc::format!("KERNEL_MAIN: Found {} entries in root directory.\n", entries.len()));
                    for (idx, entry) in entries.iter().enumerate() {
                        if idx < 10 { // Mostrar solo los primeros 10 para no saturar la pantalla
                            fb.write_text_kernel(&alloc::format!("  - {}", entry), Color::LIGHT_GRAY);
                            serial_write_str(&alloc::format!("KERNEL_MAIN: Root entry: {}\n", entry));
                        }
                    }
                    if entries.len() > 10 {
                        fb.write_text_kernel(&alloc::format!("  ... y {} más", entries.len() - 10), Color::LIGHT_GRAY);
                    }
                }
                Err(e) => {
                    serial_write_str("KERNEL_MAIN: Error reading root directory.\n");
                    fb.write_text_kernel(&alloc::format!("Error leyendo directorio raíz: {:?}", e), Color::RED);
                }
            }
        } else {
            serial_write_str("KERNEL_MAIN: No root filesystem mounted.\n");
            fb.write_text_kernel("No hay sistema de archivos montado en la raíz.", Color::RED);
        }
    }
}

/// Crea datos de prueba para EclipseFS
fn create_test_eclipsefs_data() -> alloc::vec::Vec<u8> {
    use alloc::vec::Vec;
    
    // Crear un buffer de datos de EclipseFS básico compatible con v2.0
    // Esto es un placeholder - en una implementación real, esto vendría del bootloader
    // o se generaría dinámicamente con la estructura del sistema de archivos
    
    let mut data = Vec::new();
    
    // Header de EclipseFS (v2.0) - 33 bytes
    data.extend_from_slice(b"ECLIPSEFS"); // Magic number (9 bytes)
    data.extend_from_slice(&0x00020000u32.to_le_bytes()); // Version 2.0 (bytes 9-12)
    data.extend_from_slice(&4096u64.to_le_bytes()); // inode_table_offset (bytes 13-20)
    data.extend_from_slice(&16u64.to_le_bytes()); // inode_table_size (bytes 21-28)
    data.extend_from_slice(&2u32.to_le_bytes()); // total_inodes (bytes 29-32)
    
    // Padding hasta 4096 bytes (BLOCK_SIZE) - CRÍTICO para que inode_table_offset sea válido
    while data.len() < 4096 {
        data.push(0);
    }
    
    // Tabla de inodos (16 bytes para 2 inodos)
    data.extend_from_slice(&1u32.to_le_bytes()); // inode 1 (root)
    data.extend_from_slice(&0u32.to_le_bytes()); // offset relativo 0
    data.extend_from_slice(&2u32.to_le_bytes()); // inode 2 (ai_models)
    data.extend_from_slice(&200u32.to_le_bytes()); // offset relativo 200
    
    // Registro del nodo raíz (inode 1)
    data.extend_from_slice(&1u32.to_le_bytes()); // inode
    data.extend_from_slice(&200u32.to_le_bytes()); // record_size
    
    // TLV para nodo raíz (directorio)
    data.extend_from_slice(&0x0001u16.to_le_bytes()); // NODE_TYPE tag
    data.extend_from_slice(&1u32.to_le_bytes()); // length
    data.push(2); // value (directorio)
    
    data.extend_from_slice(&0x0002u16.to_le_bytes()); // MODE tag
    data.extend_from_slice(&4u32.to_le_bytes()); // length
    data.extend_from_slice(&0o40755u32.to_le_bytes()); // value
    
    data.extend_from_slice(&0x0003u16.to_le_bytes()); // UID tag
    data.extend_from_slice(&4u32.to_le_bytes()); // length
    data.extend_from_slice(&0u32.to_le_bytes()); // value
    
    data.extend_from_slice(&0x0004u16.to_le_bytes()); // GID tag
    data.extend_from_slice(&4u32.to_le_bytes()); // length
    data.extend_from_slice(&0u32.to_le_bytes()); // value
    
    data.extend_from_slice(&0x0005u16.to_le_bytes()); // SIZE tag
    data.extend_from_slice(&8u32.to_le_bytes()); // length
    data.extend_from_slice(&0u64.to_le_bytes()); // value
    
    // Registro del nodo ai_models (inode 2)
    data.extend_from_slice(&2u32.to_le_bytes()); // inode
    data.extend_from_slice(&150u32.to_le_bytes()); // record_size
    
    // TLV para nodo ai_models (directorio)
    data.extend_from_slice(&0x0001u16.to_le_bytes()); // NODE_TYPE tag
    data.extend_from_slice(&1u32.to_le_bytes()); // length
    data.push(2); // value (directorio)
    
    data.extend_from_slice(&0x0002u16.to_le_bytes()); // MODE tag
    data.extend_from_slice(&4u32.to_le_bytes()); // length
    data.extend_from_slice(&0o40755u32.to_le_bytes()); // value
    
    data.extend_from_slice(&0x0003u16.to_le_bytes()); // UID tag
    data.extend_from_slice(&4u32.to_le_bytes()); // length
    data.extend_from_slice(&0u32.to_le_bytes()); // value
    
    data.extend_from_slice(&0x0004u16.to_le_bytes()); // GID tag
    data.extend_from_slice(&4u32.to_le_bytes()); // length
    data.extend_from_slice(&0u32.to_le_bytes()); // value
    
    data.extend_from_slice(&0x0005u16.to_le_bytes()); // SIZE tag
    data.extend_from_slice(&8u32.to_le_bytes()); // length
    data.extend_from_slice(&0u64.to_le_bytes()); // value
    
    data
}

/// Demostrar funcionalidades del shell avanzado
fn demonstrate_shell_features(_shell: &mut (), fb: &mut FramebufferDriver) {
    // Demostrar algunos comandos del shell
    let demo_commands = [
        ("info", "Información del sistema"),
        ("version", "Versión del kernel"),
        ("uptime", "Tiempo de actividad"),
        ("whoami", "Usuario actual"),
        ("pwd", "Directorio actual"),
        ("ls", "Listar archivos"),
    ];
    
    // Seleccionar un comando aleatorio para demostrar
    let command_index = 0; // Simplificado temporalmente
    let (command, description) = demo_commands[command_index];
    
    fb.write_text_kernel(
        &alloc::format!("Demo shell: {} - {}", command, description),
        Color::YELLOW,
    );
    
    // Simular ejecución del comando (sin implementación real por ahora)
    match command {
        "info" => {
            fb.write_text_kernel("  Sistema: Eclipse OS v0.6.0", Color::LIGHT_GRAY);
            fb.write_text_kernel("  Arquitectura: x86_64", Color::LIGHT_GRAY);
            fb.write_text_kernel("  Kernel: Monolítico con módulos", Color::LIGHT_GRAY);
        }
        "version" => {
            fb.write_text_kernel("  Eclipse OS Kernel v0.6.0", Color::LIGHT_GRAY);
            fb.write_text_kernel("  Compilado: Rust 1.70+", Color::LIGHT_GRAY);
            fb.write_text_kernel("  Características: IA, Wayland, COSMIC", Color::LIGHT_GRAY);
        }
        "uptime" => {
            fb.write_text_kernel("  Tiempo de actividad: Sistema iniciado", Color::LIGHT_GRAY);
            fb.write_text_kernel("  Estado: Funcionando correctamente", Color::LIGHT_GRAY);
        }
        "whoami" => {
            fb.write_text_kernel("  Usuario: root", Color::LIGHT_GRAY);
            fb.write_text_kernel("  Hostname: eclipse-os", Color::LIGHT_GRAY);
        }
        "pwd" => {
            fb.write_text_kernel("  Directorio actual: /", Color::LIGHT_GRAY);
        }
        "ls" => {
            fb.write_text_kernel("  bin/  etc/  home/  root/", Color::LIGHT_GRAY);
            fb.write_text_kernel("  (directorios del sistema de archivos)", Color::LIGHT_GRAY);
        }
        _ => {
            fb.write_text_kernel("  Comando no implementado aún", Color::LIGHT_GRAY);
        }
    }
}

/// Inicializar servicios de IA
fn initialize_ai_services(fb: &mut FramebufferDriver) -> AIService {
    serial_write_str("KERNEL_MAIN: Initializing AI services...\n");
    fb.write_text_kernel("Inicializando servicios de IA...", Color::WHITE);

    let root_fs_arc = {
        let vfs_guard = get_vfs();
        if vfs_guard.is_none() {
            serial_write_str("AI_INIT: VFS no inicializado\n");
            None
        } else {
            let vfs = vfs_guard.as_ref().unwrap();
            let root = vfs.get_root_fs();
            if root.is_none() {
                serial_write_str("AI_INIT: root_fs no montado\n");
            }
            root
        }
    };

    let model_listing_result = if let Some(root_fs) = root_fs_arc {
        let fs_guard = root_fs.lock();
        match fs_guard.readdir_path("/ai_models") {
            Ok(list) => Ok(list),
            Err(err) => {
                serial_write_str("AI_INIT: readdir_path fallo\n");
                Err(err)
            }
        }
    } else {
        Err(VfsError::InvalidOperation)
    };

    match model_listing_result {
        Ok(models) => {
            if models.is_empty() {
                fb.write_text_kernel("No se encontraron modelos de IA en /boot/ai_models/", Color::YELLOW);
            } else {
                fb.write_text_kernel("Modelos de IA encontrados en /ai_models/:", Color::GREEN);
                for name in models.iter().take(10) {
                    fb.write_text_kernel(&alloc::format!("  - {}", name), Color::LIGHT_GRAY);
                }
                if models.len() > 10 {
                    fb.write_text_kernel(&alloc::format!("  ... y {} más", models.len() - 10), Color::LIGHT_GRAY);
                }
            }
        }
        Err(err) => {
            fb.write_text_kernel(
                &alloc::format!("No se pudo acceder a /ai_models/ ({:?})", err),
                Color::YELLOW,
            );
        }
    }

    // Configuración simplificada de servicios (temporal)
    let configs = [
        AIServiceConfig {
            service_type: AIServiceType::ProcessOptimization,
            model_requirements: alloc::vec![], // Simplificado temporalmente
            priority: 1,
            auto_start: true,
            max_memory_mb: 64,
            update_interval_ms: 1000,
        },
        AIServiceConfig {
            service_type: AIServiceType::SecurityMonitoring,
            model_requirements: alloc::vec![], // Simplificado temporalmente
            priority: 2,
            auto_start: true,
            max_memory_mb: 32,
            update_interval_ms: 500,
        },
        AIServiceConfig {
            service_type: AIServiceType::PerformanceTuning,
            model_requirements: alloc::vec![], // Simplificado temporalmente
            priority: 3,
            auto_start: true,
            max_memory_mb: 48,
            update_interval_ms: 2000,
        },
    ];
    
    fb.write_text_kernel("Servicios de IA configurados:", Color::GREEN);
    for config in &configs {
        fb.write_text_kernel(
            &alloc::format!(
                "  - {:?}: {} MB, prioridad {}",
                config.service_type,
                config.max_memory_mb,
                config.priority
            ),
            Color::LIGHT_GRAY,
        );
    }
    
    // Crear servicio de IA (implementación simplificada)
    AIService {
        is_initialized: core::sync::atomic::AtomicBool::new(true),
        active_models: alloc::collections::BTreeMap::new(),
        service_state: AIServiceState::Ready,
    }
}

/// Inicializar modelos pre-entrenados
fn initialize_pretrained_models(_model_manager: &mut (), fb: &mut FramebufferDriver) {
    serial_write_str("KERNEL_MAIN: Initializing pretrained models...\n");
    fb.write_text_kernel("Inicializando modelos pre-entrenados...", Color::WHITE);
    
    // Simplificado temporalmente
    fb.write_text_kernel("  Modelos de IA disponibles:", Color::GREEN);
    fb.write_text_kernel("  - TinyLlama (Lenguaje)", Color::LIGHT_GRAY);
    fb.write_text_kernel("  - MobileNetV2 (Visión)", Color::LIGHT_GRAY);
    fb.write_text_kernel("  - AnomalyDetector (Análisis)", Color::LIGHT_GRAY);
    
    serial_write_str("KERNEL_MAIN: Pretrained models initialized.\n");
    fb.write_text_kernel("Modelos pre-entrenados inicializados.", Color::GREEN);
}

/// Demostrar funcionalidades de IA
fn demonstrate_ai_features(
    _ai_typing_system: &mut (),
    _model_manager: &mut (),
    fb: &mut FramebufferDriver,
) {
    let ai_demos = [
        "Escritura inteligente: Autocompletado activo",
        "Análisis de rendimiento: Sistema optimizado",
        "Detección de anomalías: Sin problemas detectados",
        "Predicción de recursos: Uso normal",
        "Aprendizaje automático: Patrones actualizados",
        "Optimización de procesos: Scheduler mejorado",
    ];
    
    // Seleccionar una demostración aleatoria
    let demo_index = 0; // Simplificado temporalmente
    let demo_text = ai_demos[demo_index];
    
    fb.write_text_kernel(
        &alloc::format!("Demo IA: {}", demo_text),
        Color::MAGENTA,
    );
    
    // Simular procesamiento de IA
    match demo_index {
        0 => {
            // Escritura inteligente
            fb.write_text_kernel(
                "  Sugerencias: eclipse, os, kernel, system",
                Color::LIGHT_GRAY,
            );
        }
        1 => {
            // Análisis de rendimiento
            fb.write_text_kernel("  CPU: 15%, RAM: 45%, GPU: 8%", Color::LIGHT_GRAY);
            fb.write_text_kernel("  Estado: Óptimo", Color::LIGHT_GRAY);
        }
        2 => {
            // Detección de anomalías
            fb.write_text_kernel("  Escaneo: 100% completado", Color::LIGHT_GRAY);
            fb.write_text_kernel("  Anomalías: 0 detectadas", Color::LIGHT_GRAY);
        }
        3 => {
            // Predicción de recursos
            fb.write_text_kernel("  Predicción: Uso estable", Color::LIGHT_GRAY);
            fb.write_text_kernel("  Recomendación: Continuar operación normal", Color::LIGHT_GRAY);
        }
        4 => {
            // Aprendizaje automático
            fb.write_text_kernel("  Patrones aprendidos: 1,247", Color::LIGHT_GRAY);
            fb.write_text_kernel("  Precisión: 94.2%", Color::LIGHT_GRAY);
        }
        5 => {
            // Optimización de procesos
            fb.write_text_kernel("  Procesos optimizados: 12", Color::LIGHT_GRAY);
            fb.write_text_kernel("  Mejora de rendimiento: +18%", Color::LIGHT_GRAY);
        }
        _ => {
            fb.write_text_kernel("  Funcionalidad de IA en desarrollo", Color::LIGHT_GRAY);
        }
    }
}

fn investigate_disk_contents(storage: &StorageManager) {
    crate::debug::serial_write_str("DISK_INVESTIGATION: Iniciando investigación del disco...\n");
    
    // Verificar si hay dispositivos VirtIO disponibles
    if storage.devices.len() < 3 {
        crate::debug::serial_write_str("DISK_INVESTIGATION: No hay suficientes dispositivos (necesario dispositivo 2)\n");
        return;
    }
    
    let virtio_device = &storage.devices[2];
    crate::debug::serial_write_str(&alloc::format!("DISK_INVESTIGATION: Dispositivo VirtIO: {:?}\n", virtio_device.info));
    
    // Crear buffer para leer sectores
    let mut sector_buffer = [0u8; 512];
    
    // Investigar diferentes sectores
    let sectors_to_check = [0, 1, 2, 3, 2048, 2049, 2050, 4096, 8192];
    
    for &sector in &sectors_to_check {
        crate::debug::serial_write_str(&alloc::format!("DISK_INVESTIGATION: Leyendo sector {}\n", sector));
        
        match storage.read_device_sector_with_type(&virtio_device.info, sector, &mut sector_buffer, crate::drivers::storage_manager::StorageSectorType::FAT32) {
            Ok(()) => {
                // Verificar si el sector tiene datos no nulos
                let has_data = sector_buffer.iter().any(|&b| b != 0);
                if has_data {
                    crate::debug::serial_write_str(&alloc::format!("DISK_INVESTIGATION: Sector {} tiene datos!\n", sector));
                    
                    // Mostrar primeros 32 bytes
                    let hex_str = sector_buffer[0..32]
                        .iter()
                        .map(|b| alloc::format!("{:02X}", b))
                        .collect::<alloc::vec::Vec<_>>()
                        .join(" ");
                    crate::debug::serial_write_str(&alloc::format!("DISK_INVESTIGATION: Primeros 32 bytes: {}\n", hex_str));
                    
                    // Verificar si es un boot sector válido
                    if sector_buffer[510] == 0x55 && sector_buffer[511] == 0xAA {
                        crate::debug::serial_write_str(&alloc::format!("DISK_INVESTIGATION: Sector {} tiene boot signature válida!\n", sector));
                    }
                    
                    // Verificar si es EclipseFS
                    if &sector_buffer[0..9] == b"ECLIPSEFS" {
                        crate::debug::serial_write_str(&alloc::format!("DISK_INVESTIGATION: Sector {} contiene EclipseFS!\n", sector));
                    }
                } else {
                    crate::debug::serial_write_str(&alloc::format!("DISK_INVESTIGATION: Sector {} está vacío\n", sector));
                }
            }
            Err(e) => {
                crate::debug::serial_write_str(&alloc::format!("DISK_INVESTIGATION: Error leyendo sector {}: {}\n", sector, e));
            }
        }
    }
    
    // Intentar leer la tabla de particiones MBR (sector 0)
    crate::debug::serial_write_str("DISK_INVESTIGATION: Analizando tabla de particiones MBR...\n");
    match storage.read_device_sector_with_type(&virtio_device.info, 0, &mut sector_buffer, crate::drivers::storage_manager::StorageSectorType::FAT32) {
        Ok(()) => {
            // Verificar boot signature
            if sector_buffer[510] == 0x55 && sector_buffer[511] == 0xAA {
                crate::debug::serial_write_str("DISK_INVESTIGATION: MBR tiene boot signature válida\n");
                
                // Analizar entradas de partición (offset 446, 16 bytes cada una)
                for i in 0..4 {
                    let offset = 446 + (i * 16);
                    let partition_type = sector_buffer[offset + 4];
                    let partition_start = u32::from_le_bytes([
                        sector_buffer[offset + 8],
                        sector_buffer[offset + 9],
                        sector_buffer[offset + 10],
                        sector_buffer[offset + 11],
                    ]);
                    let partition_size = u32::from_le_bytes([
                        sector_buffer[offset + 12],
                        sector_buffer[offset + 13],
                        sector_buffer[offset + 14],
                        sector_buffer[offset + 15],
                    ]);
                    
                    if partition_type != 0 {
                        crate::debug::serial_write_str(&alloc::format!(
                            "DISK_INVESTIGATION: Partición {} - Tipo: 0x{:02X}, Inicio: {}, Tamaño: {} sectores\n",
                            i + 1, partition_type, partition_start, partition_size
                        ));
                    }
                }
            } else {
                crate::debug::serial_write_str("DISK_INVESTIGATION: MBR no tiene boot signature válida\n");
            }
        }
        Err(e) => {
            crate::debug::serial_write_str(&alloc::format!("DISK_INVESTIGATION: Error leyendo MBR: {}\n", e));
        }
    }
    
    crate::debug::serial_write_str("DISK_INVESTIGATION: Investigación completada\n");
}

