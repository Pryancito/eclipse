//! Eclipse Microkernel - Punto de entrada principal
//! 
//! Este es el punto de entrada del microkernel compatible con el bootloader UEFI.

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![feature(alloc_error_handler)]

#[macro_use]
extern crate alloc;

use core::panic::PanicInfo;
use alloc::boxed::Box;
use x86_64;

// Módulos del microkernel
mod boot;
mod memory;
mod memory_builtins;
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
mod ata;

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

#[alloc_error_handler]
fn alloc_error_handler(layout: alloc::alloc::Layout) -> ! {
    panic!("allocation error: {:?}", layout)
}

/// Punto de entrada del kernel, llamado desde el bootloader UEFI
#[no_mangle]
#[link_section = ".init"]
pub extern "C" fn _start(framebuffer_info_ptr: u64, kernel_phys_base: u64) -> ! {
    // Inicializar serial para debugging
    serial::init();
    serial::serial_print("DEBUG: Entered _start\n");
    serial::serial_print("Phys Base: 0x");
    serial::serial_print_hex(kernel_phys_base);
    serial::serial_print("\n");
    
    serial::serial_print("Eclipse Microkernel v0.1.0 starting...\n");
    
    // Cargar GDT
    serial::serial_print("Loading GDT...\n");
    boot::load_gdt();
    
    // Habilitar SSE
    serial::serial_print("Enabling SSE...\n");
    boot::enable_sse();

    // Configurar paginación
    serial::serial_print("Enabling paging...\n");
    memory::init_paging(kernel_phys_base);
    
    // Inicializar IDT e interrupciones
    serial::serial_print("Initializing IDT and interrupts...\n");
    interrupts::init();

    // TEST IDT
    serial::serial_print("Testing IDT with breakpoint...\n");
    x86_64::instructions::interrupts::int3();
    serial::serial_print("IDT test passed\n");
    
    // Inicializar memoria (Allocator)
    serial::serial_print("Initializing memory system...\n");
    memory::init();
    
    // Test heap allocation early to verify allocator
    serial::serial_print("Testing early heap allocation...\n");
    let test_vec = vec![0u8; 128];
    serial::serial_print("Early heap allocation successful, ptr: 0x");
    serial::serial_print_hex(test_vec.as_ptr() as u64);
    serial::serial_print("\n");
    core::mem::drop(test_vec); // Free it
     
    
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
    
    // Inicializar driver VirtIO (preferred for QEMU)
    serial::serial_print("Initializing VirtIO driver...\n");
    virtio::init();
    
    // Inicializar driver ATA (fallback for real hardware)
    serial::serial_print("Initializing ATA driver...\n");
    ata::init();
    
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
            
            // Try to load init from /sbin/eclipse-systemd
            serial::serial_print("[KERNEL] Attempting to load init from /sbin/eclipse-systemd...\n");
            
            // Allocate buffer on heap for reading init binary (max 512KB)
            // Use vec! to avoid stack overflow with Box::new([0u8; ...])
            const MAX_INIT_SIZE: usize = 8 * 1024 * 1024;
            let mut init_buffer = vec![0u8; MAX_INIT_SIZE];
            
            match filesystem::read_file("/sbin/eclipse-systemd", &mut init_buffer[..]) {
                Ok(bytes_read) => {
                    serial::serial_print("[KERNEL] Read /sbin/eclipse-systemd: ");
                    serial::serial_print_dec(bytes_read as u64);
                    serial::serial_print(" bytes\n");
                    
                    // Load the ELF binary
                    if let Some(pid) = elf_loader::load_elf(&init_buffer[..bytes_read]) {
                        serial::serial_print("[KERNEL] Init process loaded from /sbin/eclipse-systemd with PID: ");
                        serial::serial_print_dec(pid as u64);
                        serial::serial_print("\n");
                        
                        // Add to scheduler queue
                        scheduler::enqueue_process(pid);
                        
                        serial::serial_print("[KERNEL] Init process scheduled for execution\n");
                        init_loaded = true;
                        
                        // DEBUG: Run immediately to test userspace switch
                        if let Some(process) = process::get_process(pid) {
                             unsafe {
                                 serial::serial_print("[KERNEL] DEBUG: Jumping directly to userspace (bypassing scheduler)...\n");
                                 // Stack grows down, so base + size is top
                                 // Note: create_process subtracts 16 from rsp?
                                 // Let's use stack_base + stack_size
                                 let stack_top = process.stack_base + process.stack_size as u64;
                                 elf_loader::jump_to_userspace(process.context.rip, stack_top);
                             }
                        }
                    } else {
                        serial::serial_print("[KERNEL] Failed to load ELF from /sbin/eclipse-systemd\n");
                        serial::serial_print("[KERNEL] Falling back to embedded init...\n");
                    }
                }
                Err(e) => {
                    serial::serial_print("[KERNEL] Failed to read /sbin/eclipse-systemd: ");
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
