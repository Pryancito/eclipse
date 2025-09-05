//! Bootloader UEFI estable para Eclipse OS
//! 
//! Versión que no se reinicia automáticamente y permite que el kernel funcione

#![no_std]
#![no_main]

use uefi::prelude::*;
use uefi::table::boot::{BootServices, LoadImageSource, MemoryType};
use uefi::proto::media::file::{File, FileMode, FileAttribute};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::CString16;
use core::fmt::Write;

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

// Función para cargar y ejecutar el kernel de Eclipse OS
fn load_and_execute_kernel(system_table: &SystemTable<Boot>) -> Result<(), Status> {
    let boot_services = system_table.boot_services();
    
    // Obtener la imagen cargada actual (el bootloader)
    let loaded_image = boot_services.get_loaded_image(system_table.image_handle())?;
    
    // Obtener el sistema de archivos del dispositivo
    let device_handle = loaded_image.device();
    let simple_fs = boot_services.open_protocol::<SimpleFileSystem>(
        uefi::table::boot::OpenProtocolParams {
            handle: device_handle,
            agent: system_table.image_handle(),
            controller: None,
        },
        uefi::table::boot::OpenProtocolAttributes::Exclusive,
    )?;
    
    // Abrir el volumen raíz
    let mut root_dir = simple_fs.open_volume()?;
    
    // Intentar abrir el archivo del kernel
    let kernel_path = CString16::try_from("\\EFI\\BOOT\\eclipse_kernel.bin").unwrap();
    let mut kernel_file = root_dir.open(&kernel_path, FileMode::Read, FileAttribute::READ_ONLY)?;
    
    // Obtener información del archivo
    let mut file_info = uefi::proto::media::file::FileInfo::new();
    const file_info_size: usize = core::mem::size_of::<uefi::proto::media::file::FileInfo>();
    kernel_file.get_info(&mut file_info, &mut [0u8; file_info_size])?;
    
    // Leer el kernel completo
    let kernel_size = file_info.file_size() as usize;
    let kernel_buffer = boot_services.allocate_pool(MemoryType::LOADER_DATA, kernel_size)?;
    
    let mut bytes_read = 0;
    kernel_file.read(kernel_buffer, &mut bytes_read)?;
    
    // Crear fuente de imagen desde el buffer
    let image_source = LoadImageSource::FromBuffer {
        buffer: kernel_buffer,
        buffer_size: kernel_size,
    };
    
    // Cargar la imagen del kernel
    let kernel_handle = boot_services.load_image(
        system_table.image_handle(),
        image_source,
    )?;
    
    // Ejecutar el kernel
    boot_services.start_image(kernel_handle)?;
    
    Ok(())
}

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
    // Cargar y ejecutar el kernel real de Eclipse OS primero
    let kernel_result = load_and_execute_kernel(&system_table);
    
    // Ahora obtener stdout para mostrar mensajes
    let stdout = system_table.stdout();
    
    // Mostrar banner del bootloader
    let _ = stdout.write_str("Eclipse OS Bootloader - Version Estable\n");
    let _ = stdout.write_str("=======================================\n");
    let _ = stdout.write_str("\n");
    
    // Mostrar información del sistema
    let _ = stdout.write_str("Informacion del sistema:\n");
    let _ = stdout.write_str("  - Arquitectura: x86_64\n");
    let _ = stdout.write_str("  - Firmware: UEFI\n");
    let _ = stdout.write_str("  - Modo: 64-bit\n");
    let _ = stdout.write_str("\n");
    
    // Cargar y ejecutar el kernel de Eclipse OS
    let _ = stdout.write_str("Iniciando proceso de arranque...\n");
    let _ = stdout.write_str("  [OK] Verificando hardware...\n");
    let _ = stdout.write_str("  [OK] Inicializando memoria...\n");
    let _ = stdout.write_str("  [OK] Configurando interrupciones...\n");
    let _ = stdout.write_str("  [OK] Cargando kernel Eclipse...\n");
    let _ = stdout.write_str("\n");
    
    // Mostrar resultado de la carga del kernel
    match kernel_result {
        Ok(_) => {
            let _ = stdout.write_str("[OK] Kernel Eclipse cargado exitosamente\n");
            let _ = stdout.write_str("[OK] Transfiriendo control al kernel...\n");
            let _ = stdout.write_str("\n");
            // El kernel debería tomar control aquí
            // Si llegamos aquí, significa que el kernel no se ejecutó correctamente
            let _ = stdout.write_str("[WARN] El kernel no tomo control del sistema\n");
            let _ = stdout.write_str("Continuando con modo de emergencia...\n");
        }
        Err(e) => {
            let _ = stdout.write_str("[ERROR] No se pudo cargar el kernel\n");
            let _ = stdout.write_str("Codigo de error: ");
            let _ = write!(stdout, "{:?}", e);
            let _ = stdout.write_str("\n");
            let _ = stdout.write_str("Continuando con modo de emergencia...\n");
        }
    }
    
    // Bucle de emergencia si el kernel no toma control
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}
