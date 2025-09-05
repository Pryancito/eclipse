//! Bootloader UEFI simplificado para Eclipse OS
//! 
//! Versión simplificada que evita problemas de compatibilidad

#![no_std]
#![no_main]

use uefi::prelude::*;
use uefi::table::boot::{BootServices, LoadImageSource};
use uefi::table::runtime::ResetType;
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::file::{File, FileMode, FileAttribute};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::CString16;
use uefi::helpers::init;

#[entry]
fn main(_handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    // Inicializar servicios UEFI
    init(&mut system_table).unwrap();
    
    // Obtener servicios de boot
    let boot_services = system_table.boot_services();
    
    // Mostrar banner simple
    println!("🌙 Eclipse OS Bootloader");
    println!("   Cargando kernel Eclipse...");
    
    // Cargar y ejecutar el kernel
    match load_and_run_kernel(boot_services) {
        Ok(()) => {
            println!("✅ Kernel Eclipse iniciado");
            Status::SUCCESS
        }
        Err(e) => {
            println!("❌ Error: {:?}", e);
            handle_error();
            Status::LOAD_ERROR
        }
    }
}

/// Carga y ejecuta el kernel Eclipse
fn load_and_run_kernel(boot_services: &BootServices) -> uefi::Result<()> {
    // Obtener sistema de archivos
    let loaded_image = boot_services
        .open_protocol_exclusive::<LoadedImage>(boot_services.image_handle())?;
    let file_system = boot_services
        .open_protocol_exclusive::<SimpleFileSystem>(loaded_image.device())?;
    let mut root_dir = file_system.open_volume()?;
    
    // Buscar kernel Eclipse
    let kernel_paths = [
        "\\vmlinuz-eclipse",
        "\\boot\\vmlinuz-eclipse", 
        "\\EFI\\BOOT\\vmlinuz-eclipse",
        "\\eclipse_kernel"
    ];
    
    let mut kernel_file: Option<File> = None;
    let mut kernel_path_found = "";
    
    for path in &kernel_paths {
        if let Ok(cpath) = CString16::try_from(*path) {
            if let Ok(file) = root_dir.open(&cpath, FileMode::Read, FileAttribute::READ_ONLY) {
                kernel_file = Some(file);
                kernel_path_found = path;
                println!("   Kernel encontrado: {}", path);
                break;
            }
        }
    }
    
    let kernel_file = kernel_file.ok_or(uefi::Status::NOT_FOUND)?;
    
    // Leer kernel en memoria
    let mut file_info_buffer = [0u8; 1024];
    let file_info = kernel_file.get_info::<uefi::proto::media::file::FileInfo>(&mut file_info_buffer)?.file_size();
    
    println!("   Kernel cargado: {} bytes", file_info);
    
    // Preparar línea de comandos simple
    let cmdline = CString16::try_from("console=ttyS0 quiet").map_err(|_| uefi::Status::INVALID_PARAMETER)?;
    
    // Cargar como imagen EFI desde archivo
    let kernel_path_cstr = CString16::try_from(kernel_path_found).map_err(|_| uefi::Status::INVALID_PARAMETER)?;
    let kernel_image = boot_services.load_image(
        boot_services.image_handle(),
        LoadImageSource::FromFile {
            file_path: &kernel_path_cstr,
        },
    )?;
    
    // Ejecutar kernel
    boot_services.start_image(kernel_image)?;
    
    Ok(())
}

/// Maneja errores del bootloader
fn handle_error() {
    println!("🔄 Reiniciando en 5 segundos...");
    
    // Esperar un poco
    for i in (1..=5).rev() {
        println!("   {}...", i);
        // En un bootloader real, aquí habría una espera
    }
    
    // Reiniciar sistema
    unsafe {
        let rt = uefi::table::SystemTable::current().runtime_services();
        rt.reset(ResetType::Cold, uefi::Status::SUCCESS, None);
    }
}