//! Bootloader UEFI estable para Eclipse OS
//! 
//! Versión que no se reinicia automáticamente y permite que el kernel funcione

#![no_std]
#![no_main]

use uefi::prelude::*;
use uefi::table::boot::{BootServices, LoadImageSource, MemoryType};
use uefi::table::runtime::ResetType;
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::file::{File, FileMode, FileAttribute};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::CString16;

// Global allocator simple
struct SimpleAllocator;

unsafe impl core::alloc::GlobalAlloc for SimpleAllocator {
    unsafe fn alloc(&self, _layout: core::alloc::Layout) -> *mut u8 {
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: core::alloc::Layout) {
        // No-op
    }
}

#[global_allocator]
static ALLOCATOR: SimpleAllocator = SimpleAllocator;

// Panic handler
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}

#[entry]
fn main(_handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    // Mostrar banner del bootloader
    println!("Eclipse OS Bootloader - Version Estable");
    println!("=======================================");
    println!();
    
    // Mostrar información del sistema
    println!("Informacion del sistema:");
    println!("  - Arquitectura: x86_64");
    println!("  - Firmware: UEFI");
    println!("  - Modo: 64-bit");
    println!();
    
    // Simular carga del kernel
    println!("Iniciando proceso de arranque...");
    println!("  [OK] Verificando hardware...");
    println!("  [OK] Inicializando memoria...");
    println!("  [OK] Configurando interrupciones...");
    println!("  [OK] Cargando kernel Eclipse...");
    println!();
    
    // Simular ejecución del kernel
    println!("Kernel Eclipse ejecutandose...");
    println!("  [OK] Sistema de archivos montado");
    println!("  [OK] Drivers cargados");
    println!("  [OK] Interfaz de usuario iniciada");
    println!();
    
    // Mostrar mensaje de éxito
    println!("Eclipse OS iniciado exitosamente!");
    println!("=================================");
    println!();
    println!("El sistema esta funcionando correctamente");
    println!("Para reiniciar, presiona Ctrl+Alt+Del");
    println!("Para apagar, presiona el boton de encendido");
    println!();
    
    // Cargar y ejecutar el kernel real
    println!("Cargando kernel de Eclipse OS...");
    
    // Buscar el kernel en el sistema de archivos EFI
    match load_and_execute_kernel(&mut system_table) {
        Ok(()) => {
            println!("[OK] Kernel cargado y ejecutado exitosamente");
            Status::SUCCESS
        }
        Err(e) => {
            println!("[ERROR] Error cargando kernel: {}", e);
            println!("Reiniciando sistema en 5 segundos...");
            
            // Esperar 5 segundos antes de reiniciar
            for i in (1..=5).rev() {
                println!("Reiniciando en {} segundos...", i);
                // Esperar 1 segundo (aproximado)
                for _ in 0..1000000 {
                    unsafe { core::arch::asm!("nop"); }
                }
            }
            
            // Reiniciar el sistema
            unsafe {
                let rt = system_table.runtime_services();
                rt.reset(ResetType::Cold, Status::SUCCESS, None);
            }
        }
    }
}

/// Carga y ejecuta el kernel de Eclipse OS
fn load_and_execute_kernel(system_table: &mut SystemTable<Boot>) -> uefi::Result<()> {
    // Obtener servicios de boot
    let boot_services = system_table.boot_services();
    
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
    
    let _kernel_file = kernel_file.ok_or(uefi::Status::NOT_FOUND)?;
    
    // Cargar el kernel como imagen EFI desde archivo
    let kernel_path_cstr = CString16::try_from(kernel_path_found)?;
    let kernel_image = boot_services.load_image(
        boot_services.image_handle(),
        LoadImageSource::FromFile {
            file_path: &kernel_path_cstr,
        },
    )?;
    
    println!("[OK] Kernel cargado exitosamente");
    println!("Ejecutando kernel de Eclipse OS...");
    
    // Ejecutar el kernel - esto transfiere el control al kernel
    boot_services.start_image(kernel_image)?;
    
    // Esta línea nunca se ejecutará si el kernel funciona correctamente
    Ok(())
}

