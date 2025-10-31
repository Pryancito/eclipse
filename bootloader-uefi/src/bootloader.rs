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
use uefi_services::println;

#[entry]
fn main(_handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    // Inicializar servicios UEFI
    uefi_services::init(&mut system_table).unwrap();
    
    // Obtener servicios de boot
    let boot_services = system_table.boot_services();
    
    // Mostrar banner de Eclipse OS
    print_banner();
    
    // Buscar y cargar el kernel
    match load_kernel(boot_services) {
        Ok(()) => {
            println!("[OK] Kernel cargado exitosamente");
            Status::SUCCESS
        }
        Err(e) => {
            println!("[ERROR] Error cargando kernel: {:?}", e);
            Status::LOAD_ERROR
        }
    }
}

/// Muestra el banner de Eclipse OS
fn print_banner() {
    println!("================================================================");
    println!("                    Eclipse OS Bootloader                      ");
    println!("                    Bootloader UEFI Nativo                    ");
    println!("                    Desarrollado en Rust                      ");
    println!("================================================================");
    println!();
}

/// Carga el kernel Linux de Eclipse OS
fn load_kernel(boot_services: &BootServices) -> uefi::Result<()> {
    println!("Buscando kernel de Eclipse OS...");
    
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
        "\\boot\\eclipse_kernel",
        "\\EFI\\BOOT\\eclipse_kernel",
        "\\eclipse_kernel",
        "\\boot\\vmlinuz-eclipse",
        "\\EFI\\BOOT\\vmlinuz-eclipse", 
        "\\vmlinuz-eclipse",
        "\\boot\\vmlinuz",
        "\\EFI\\BOOT\\vmlinuz",
        "\\vmlinuz"
    ];
    
    let mut kernel_file: Option<File> = None;
    let mut kernel_path_found = "";
    
    for path in &kernel_paths {
        match CString16::try_from(path) {
            Ok(cpath) => {
                match root_dir.open(&cpath, FileMode::Read, FileAttribute::READ_ONLY) {
                    Ok(file) => {
                        kernel_file = Some(file);
                        kernel_path_found = path;
                        println!("[OK] Kernel encontrado en: {}", path);
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
    println!("Leyendo kernel en memoria...");
    
    // Crear buffer para obtener información del archivo
    let mut file_info_buffer = [0u8; 1024];
    let file_info = kernel_file.get_info::<uefi::proto::media::file::FileInfo>(&mut file_info_buffer)?.file_size();
    
    // Asignar memoria para el kernel
    let kernel_buffer = boot_services.allocate_pool(
        MemoryType::LOADER_DATA,
        file_info as usize
    )?;
    
    // Leer el kernel
    let mut buffer = unsafe { 
        core::slice::from_raw_parts_mut(kernel_buffer, file_info as usize) 
    };
    kernel_file.read(&mut buffer)?;
    
    println!("[OK] Kernel leido: {} bytes", file_info);
    
    // Preparar parámetros del kernel
    let cmdline = CString16::try_from("console=ttyS0 quiet")?;
    
    println!("Iniciando kernel de Eclipse OS...");
    
    // Cargar el kernel como imagen EFI desde archivo
    let kernel_path_cstr = CString16::try_from(kernel_path_found)?;
    let kernel_image = boot_services.load_image(
        boot_services.image_handle(),
        LoadImageSource::FromFile {
            file_path: &kernel_path_cstr,
        },
    )?;
    
    // Ejecutar el kernel
    boot_services.start_image(kernel_image)?;
    
    Ok(())
}

/// Maneja errores críticos del bootloader
fn handle_critical_error(error: uefi::Status) {
    println!("[CRITICAL] Error critico del bootloader: {:?}", error);
    println!("Reiniciando sistema...");
    
    // Reiniciar el sistema
    unsafe {
        let rt = uefi::table::SystemTable::current().runtime_services();
        rt.reset(ResetType::Cold, uefi::Status::SUCCESS, None);
    }
}