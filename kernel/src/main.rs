//! Eclipse Microkernel - Punto de entrada principal
//! 
//! Este es el punto de entrada del microkernel compatible con el bootloader UEFI.

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

use core::panic::PanicInfo;

// Módulos del microkernel
mod boot;
mod memory;
mod interrupts;
mod ipc;
mod serial;
mod process;
mod scheduler;
mod syscalls;
mod servers;

/// Información del framebuffer recibida del bootloader UEFI
#[repr(C)]
pub struct FramebufferInfo {
    pub base_address: u64,
    pub width: u32,
    pub height: u32,
    pub pixels_per_scan_line: u32,
    pub pixel_format: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
}

/// Panic handler del kernel
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    serial::serial_print("KERNEL PANIC: ");
    if let Some(location) = info.location() {
        serial::serial_print("at ");
        serial::serial_print(location.file());
        serial::serial_print(":");
        // Note: Can't easily print numbers without format! macro
    }
    serial::serial_print("\n");
    loop {
        unsafe { core::arch::asm!("hlt") };
    }
}

/// Punto de entrada del kernel, llamado desde el bootloader UEFI
#[no_mangle]
#[link_section = ".init"]
pub extern "C" fn _start(framebuffer_info_ptr: u64) -> ! {
    // Inicializar serial para debugging
    serial::init();
    serial::serial_print("Eclipse Microkernel v0.1.0 starting...\n");
    
    // Cargar GDT
    serial::serial_print("Loading GDT...\n");
    boot::load_gdt();
    
    // Inicializar memoria
    serial::serial_print("Initializing memory system...\n");
    memory::init();
    
    // Configurar paginación
    serial::serial_print("Enabling paging...\n");
    memory::init_paging();
    
    // Inicializar IDT e interrupciones
    serial::serial_print("Initializing IDT and interrupts...\n");
    interrupts::init();
    
    // Inicializar sistema IPC
    serial::serial_print("Initializing IPC system...\n");
    ipc::init();
    
    // Inicializar scheduler
    serial::serial_print("Initializing scheduler...\n");
    scheduler::init();
    
    // Inicializar syscalls
    serial::serial_print("Initializing syscalls...\n");
    syscalls::init();
    
    // Inicializar servidores del sistema
    serial::serial_print("Initializing system servers...\n");
    servers::init_servers();
    
    serial::serial_print("Microkernel initialized successfully!\n");
    
    // Llamar a kernel_main
    kernel_main(framebuffer_info_ptr);
}

/// Función principal del kernel
fn kernel_main(_framebuffer_info_ptr: u64) -> ! {
    serial::serial_print("Entering kernel main loop...\n");
    
    // Crear un proceso de prueba
    serial::serial_print("Creating test process...\n");
    let stack_base = 0x400000; // 4MB mark
    let stack_size = 0x10000;  // 64KB
    
    if let Some(pid) = process::create_process(test_process as u64, stack_base, stack_size) {
        serial::serial_print("Test process created with PID: ");
        serial::serial_print_dec(pid as u64);
        serial::serial_print("\n");
        
        // Agregar a la cola del scheduler
        scheduler::enqueue_process(pid);
    }
    
    loop {
        // Main loop del microkernel
        // Procesar mensajes IPC
        ipc::process_messages();
        
        // Yield CPU
        unsafe { core::arch::asm!("hlt") };
    }
}

/// Proceso de prueba simple
extern "C" fn test_process() -> ! {
    loop {
        // Proceso de prueba - simplemente yield
        scheduler::yield_cpu();
    }
}
