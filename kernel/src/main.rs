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
    
    // Inicializar IDT e interrupciones
    serial::serial_print("Initializing IDT and interrupts...\n");
    interrupts::init();
    
    // Inicializar sistema IPC
    serial::serial_print("Initializing IPC system...\n");
    ipc::init();
    
    serial::serial_print("Microkernel initialized successfully!\n");
    
    // Llamar a kernel_main
    kernel_main(framebuffer_info_ptr);
}

/// Función principal del kernel
fn kernel_main(_framebuffer_info_ptr: u64) -> ! {
    serial::serial_print("Entering kernel main loop...\n");
    
    // TODO: Iniciar servidores del sistema
    // TODO: Configurar scheduler
    // TODO: Cargar proceso init
    
    loop {
        // Main loop del microkernel
        // Procesar mensajes IPC
        ipc::process_messages();
        
        // Yield CPU
        unsafe { core::arch::asm!("hlt") };
    }
}
