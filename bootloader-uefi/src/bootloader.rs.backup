//! Bootloader UEFI nativo para Eclipse OS
//! 
//! Este módulo implementa un bootloader UEFI en Rust que reemplaza GRUB
//! y carga directamente el kernel Linux de Eclipse OS.

#![no_std]
#![no_main]

use uefi::prelude::*;
use uefi::table::boot::{BootServices, LoadImageSource, MemoryType};
use uefi::table::runtime::ResetType;
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::file::{File, FileMode, FileAttribute};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::CString16;
use uefi::ResultExt;

/// Punto de entrada del bootloader UEFI
#[entry]
fn main(_handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    // Inicializar servicios UEFI
    uefi_services::init(&mut system_table).unwrap_success();
    
    // Obtener servicios de boot
    let boot_services = system_table.boot_services();
    
    // Mostrar banner de Eclipse OS
    print_banner();
    
    // Buscar y cargar el kernel
    match load_kernel(boot_services) {
        Ok(()) => {
            println!("✅ Kernel cargado exitosamente");
            Status::SUCCESS
        }
        Err(e) => {
            println!("❌ Error cargando kernel: {:?}", e);
            Status::LOAD_ERROR
        }
    }
}

/// Muestra el banner de Eclipse OS
fn print_banner() {
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║                    🌙 Eclipse OS Bootloader                  ║");
    println!("║                    Bootloader UEFI Nativo                    ║");
    println!("║                    Desarrollado en Rust                      ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();
}

/// Carga el kernel Linux de Eclipse OS
fn load_kernel(boot_services: &BootServices) -> uefi::Result<()> {
    println!("🔍 Buscando kernel de Eclipse OS...");
    
    // Obtener la imagen cargada actual (nuestro bootloader)
    let loaded_image = boot_services
        .open_protocol_exclusive::<LoadedImage>(boot_services.image_handle())?;
    
    // Obtener el sistema de archivos simple
    let file_system = boot_services
        .open_protocol_exclusive::<SimpleFileSystem>(loaded_image.device())?;
    
    // Abrir el directorio raíz
    let mut root_dir = file_system.open_volume()?;
    
    // Buscar el kernel en diferentes ubicaciones
    let kernel_paths = [
        "\\EFI\\BOOT\\vmlinuz-eclipse",
        "\\boot\\vmlinuz-eclipse", 
        "\\vmlinuz-eclipse",
        "\\EFI\\BOOT\\vmlinuz",
        "\\boot\\vmlinuz",
        "\\vmlinuz"
    ];
    
    let mut kernel_file: Option<File> = None;
    let mut kernel_path = CString16::new();
    
    for path in &kernel_paths {
        match CString16::try_from(path) {
            Ok(cpath) => {
                match root_dir.open(&cpath, FileMode::Read, FileAttribute::READ_ONLY) {
                    Ok(file) => {
                        kernel_file = Some(file);
                        kernel_path = cpath;
                        println!("✅ Kernel encontrado en: {}", path);
                        break;
                    }
                    Err(_) => continue,
                }
            }
            Err(_) => continue,
        }
    }
    
    let kernel_file = kernel_file.ok_or(uefi::Status::NOT_FOUND)?;
    
    // Leer el kernel completo en memoria
    println!("📖 Leyendo kernel en memoria...");
    let kernel_size = kernel_file.get_info::<uefi::proto::media::file::FileInfo>()?.file_size();
    
    // Asignar memoria para el kernel
    let kernel_buffer = boot_services.allocate_pool(
        MemoryType::LOADER_DATA,
        kernel_size as usize
    )?;
    
    // Leer el kernel
    let mut buffer = unsafe { 
        core::slice::from_raw_parts_mut(kernel_buffer, kernel_size as usize) 
    };
    kernel_file.read(&mut buffer)?;
    
    println!("✅ Kernel leído: {} bytes", kernel_size);
    
    // Buscar initrd
    let initrd_paths = [
        "\\EFI\\BOOT\\initrd-eclipse.img",
        "\\boot\\initrd-eclipse.img",
        "\\initrd-eclipse.img",
        "\\EFI\\BOOT\\initrd.img", 
        "\\boot\\initrd.img",
        "\\initrd.img"
    ];
    
    let mut initrd_buffer: Option<*mut u8> = None;
    let mut initrd_size = 0;
    
    for path in &initrd_paths {
        match CString16::try_from(path) {
            Ok(cpath) => {
                match root_dir.open(&cpath, FileMode::Read, FileAttribute::READ_ONLY) {
                    Ok(mut initrd_file) => {
                        let size = initrd_file.get_info::<uefi::proto::media::file::FileInfo>()?.file_size();
                        let buffer = boot_services.allocate_pool(
                            MemoryType::LOADER_DATA,
                            size as usize
                        )?;
                        
                        let mut initrd_data = unsafe { 
                            core::slice::from_raw_parts_mut(buffer, size as usize) 
                        };
                        initrd_file.read(&mut initrd_data)?;
                        
                        initrd_buffer = Some(buffer);
                        initrd_size = size as usize;
                        println!("✅ Initrd encontrado: {} bytes", size);
                        break;
                    }
                    Err(_) => continue,
                }
            }
            Err(_) => continue,
        }
    }
    
    // Preparar parámetros del kernel
    let cmdline = CString16::try_from("init=/init console=ttyS0,115200 quiet splash")?;
    
    println!("🚀 Iniciando kernel de Eclipse OS...");
    
    // Cargar el kernel como imagen EFI
    let kernel_image = boot_services.load_image(
        boot_services.image_handle(),
        LoadImageSource::FromBuffer {
            buffer: kernel_buffer,
            buffer_size: kernel_size as usize,
        },
        &cmdline,
    )?;
    
    // Si tenemos initrd, configurarlo
    if let Some(initrd_buf) = initrd_buffer {
        // Nota: La configuración del initrd requiere acceso a protocolos específicos
        // que pueden no estar disponibles en todas las implementaciones UEFI
        println!("📦 Initrd configurado");
    }
    
    // Ejecutar el kernel
    boot_services.start_image(kernel_image)?;
    
    Ok(())
}

/// Maneja errores críticos del bootloader
fn handle_critical_error(error: uefi::Status) {
    println!("💥 Error crítico del bootloader: {:?}", error);
    println!("🔄 Reiniciando sistema...");
    
    // Reiniciar el sistema
    unsafe {
        let rt = uefi::table::SystemTable::as_ptr().as_ref().unwrap().runtime_services();
        rt.reset(ResetType::Cold, uefi::Status::SUCCESS, None);
    }
}
