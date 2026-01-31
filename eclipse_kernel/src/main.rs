//! Eclipse Microkernel - Punto de entrada principal
//! 
//! Este es el punto de entrada del microkernel compatible con el bootloader UEFI.

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

use core::panic::PanicInfo;
use alloc::boxed::Box;

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
mod elf_loader;
mod virtio;
mod filesystem;
mod binaries;

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
    
    // Inicializar dispositivos VirtIO
    serial::serial_print("Initializing VirtIO devices...\n");
    virtio::init();
    
    // Inicializar filesystem
    serial::serial_print("Initializing filesystem subsystem...\n");
    filesystem::init();
    
    serial::serial_print("Microkernel initialized successfully!\n");
    
    // Llamar a kernel_main
    kernel_main(framebuffer_info_ptr);
}

/// Init process binary embedded in kernel
/// This will be loaded instead of the test process
pub static INIT_BINARY: &[u8] = include_bytes!("../userspace/init/target/x86_64-unknown-none/release/eclipse-init");

/// Función principal del kernel
fn kernel_main(_framebuffer_info_ptr: u64) -> ! {
    serial::serial_print("Entering kernel main loop...\n");
    
    // Intentar montar el sistema de archivos
    serial::serial_print("[KERNEL] Attempting to mount root filesystem...\n");
    let mut init_loaded = false;
    
    match filesystem::mount_root() {
        Ok(_) => {
            serial::serial_print("[KERNEL] Root filesystem mounted successfully\n");
            
            // Try to load init from /sbin/init
            serial::serial_print("[KERNEL] Attempting to load init from /sbin/init...\n");
            
            // Allocate buffer on heap for reading init binary (max 512KB)
            const MAX_INIT_SIZE: usize = 512 * 1024;
            let mut init_buffer = Box::new([0u8; MAX_INIT_SIZE]);
            
            match filesystem::read_file("/sbin/init", &mut init_buffer[..]) {
                Ok(bytes_read) => {
                    serial::serial_print("[KERNEL] Read /sbin/init: ");
                    serial::serial_print_dec(bytes_read as u64);
                    serial::serial_print(" bytes\n");
                    
                    // Load the ELF binary
                    if let Some(pid) = elf_loader::load_elf(&init_buffer[..bytes_read]) {
                        serial::serial_print("[KERNEL] Init process loaded from /sbin/init with PID: ");
                        serial::serial_print_dec(pid as u64);
                        serial::serial_print("\n");
                        
                        // Add to scheduler queue
                        scheduler::enqueue_process(pid);
                        
                        serial::serial_print("[KERNEL] Init process scheduled for execution\n");
                        init_loaded = true;
                    } else {
                        serial::serial_print("[KERNEL] Failed to load ELF from /sbin/init\n");
                        serial::serial_print("[KERNEL] Falling back to embedded init...\n");
                    }
                }
                Err(e) => {
                    serial::serial_print("[KERNEL] Failed to read /sbin/init: ");
                    serial::serial_print(e);
                    serial::serial_print("\n");
                    serial::serial_print("[KERNEL] Falling back to embedded init...\n");
                }
            }
        }
        Err(e) => {
            serial::serial_print("[KERNEL] Failed to mount filesystem: ");
            serial::serial_print(e);
            serial::serial_print("\n");
            serial::serial_print("[KERNEL] Falling back to embedded init...\n");
        }
    }
    
    // If init was not loaded from /sbin/init, load embedded binary
    if !init_loaded {
        serial::serial_print("\n[KERNEL] Loading init process from embedded binary...\n");
        serial::serial_print("[KERNEL] Init binary size: ");
        serial::serial_print_dec(INIT_BINARY.len() as u64);
        serial::serial_print(" bytes\n");
        
        // Use the ELF loader to load the init binary
        if let Some(pid) = elf_loader::load_elf(INIT_BINARY) {
            serial::serial_print("[KERNEL] Init process loaded with PID: ");
            serial::serial_print_dec(pid as u64);
            serial::serial_print("\n");
            
            // Add to scheduler queue
            scheduler::enqueue_process(pid);
            
            serial::serial_print("[KERNEL] Init process scheduled for execution\n");
        } else {
            serial::serial_print("[KERNEL] ERROR: Failed to load init process!\n");
            serial::serial_print("[KERNEL] System cannot continue without init\n");
        }
    }
    
    serial::serial_print("\n[KERNEL] System initialization complete!\n\n");
    
    loop {
        // Main loop del microkernel
        // Procesar mensajes IPC
        ipc::process_messages();
        
        // Yield CPU
        unsafe { core::arch::asm!("hlt") };
    }
}
