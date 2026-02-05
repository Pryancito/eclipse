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
mod pci;
mod nvidia;
mod virtio;
mod filesystem;
mod binaries;
mod ata;
mod fd;  // File descriptor management

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
/// 
/// Parámetros (x86_64 calling convention):
/// - RDI: framebuffer_info_ptr - Pointer to framebuffer information
/// - RSI: kernel_phys_base - Physical base address where kernel is loaded
/// - RDX: pml4_phys - Physical address of Higher Half page tables
#[no_mangle]
#[link_section = ".init"]
pub extern "C" fn _start(framebuffer_info_ptr: u64, kernel_phys_base: u64, pml4_phys: u64) -> ! {
    // Inicializar serial para debugging
    serial::init();
    serial::serial_print("DEBUG: Entered _start\n");
    serial::serial_print("Framebuffer: ");
    serial::serial_print_hex(framebuffer_info_ptr);
    serial::serial_print("\n");
    serial::serial_print("Phys Base: ");
    serial::serial_print_hex(kernel_phys_base);
    serial::serial_print("\n");
    serial::serial_print("PML4 (Higher Half): ");
    serial::serial_print_hex(pml4_phys);
    serial::serial_print("\n");
    
    serial::serial_print("Eclipse Microkernel v0.1.0 starting...\n");
    
    // Cargar GDT
    serial::serial_print("Loading GDT...\n");
    boot::load_gdt();
    
    // Habilitar SSE
    serial::serial_print("Enabling SSE...\n");
    boot::enable_sse();

    // CRITICAL: Load Higher Half page tables BEFORE doing anything else
    // This switches from UEFI identity mapping to our Higher Half mapping
    serial::serial_print("Loading Higher Half page tables (CR3)...\n");
    unsafe {
        core::arch::asm!(
            "mov cr3, {}",
            in(reg) pml4_phys,
            options(nostack, preserves_flags)
        );
    }
    serial::serial_print("✓ Higher Half page tables loaded\n");

    // Configurar paginación (now just verifies the Higher Half setup)
    serial::serial_print("Verifying paging...\n");
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
    serial::serial_print("Early heap allocation successful, ptr: ");
    serial::serial_print_hex(test_vec.as_ptr() as u64);
    serial::serial_print("\n");
    core::mem::drop(test_vec); // Free it
     
    
    // Inicializar sistema IPC
    serial::serial_print("Initializing IPC system...\n");
    ipc::init();
    
    // Inicializar proceso kernel (PID 0)
    serial::serial_print("Initializing kernel process (PID 0)...\n");
    process::init_kernel_process();
    
    // Inicializar scheduler
    serial::serial_print("Initializing scheduler...\n");
    scheduler::init();
    
    // Inicializar syscalls
    serial::serial_print("Initializing syscalls...\n");
    syscalls::init();
    
    // Initialize file descriptor system
    serial::serial_print("Initializing file descriptor system...\n");
    fd::init();
    
    // Inicializar servidores del sistema
    serial::serial_print("Initializing system servers...\n");
    servers::init_servers();
    
    // Inicializar subsistema PCI
    serial::serial_print("Initializing PCI subsystem...\n");
    pci::init();
    
    // Inicializar subsistema NVIDIA GPU
    serial::serial_print("Initializing NVIDIA GPU subsystem...\n");
    nvidia::init();
    
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
pub fn kernel_main(framebuffer_info_ptr: u64) -> ! {
    // Store framebuffer info for graphics server
    boot::set_framebuffer_info(framebuffer_info_ptr);
    
    serial::serial_print("===== FORK FIX VERSION - TESTING =====\n");
    serial::serial_print("Entering kernel main loop...\n");
    
    // Intentar montar el sistema de archivos
    serial::serial_print("[KERNEL] Attempting to mount root filesystem...\n");
    let mut init_loaded = false;
    
    match filesystem::mount_root() {
        Ok(_) => {
            serial::serial_print("[KERNEL] Root filesystem mounted successfully\n");
            
            // TEMPORARY: Skip loading from disk to test embedded init with fork() fix
            // The eclipse-systemd on disk crashes immediately (exit code 10)
            // For now, test with the simpler embedded init
            serial::serial_print("[KERNEL] Skipping disk systemd (crashes), using embedded init...\n");
            // Don't load from disk - use embedded binary
            // init_loaded stays false
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
    
    serial::serial_print("\n[KERNEL] System initialization complete!\n");
    serial::serial_print("[KERNEL] Starting scheduler to run init process...\n\n");
    
    // Perform initial scheduling to start running the init process
    scheduler::schedule();
    
    loop {
        // Main loop del microkernel
        // Procesar mensajes IPC
        ipc::process_messages();
        
        // Yield CPU
        unsafe { core::arch::asm!("hlt") };
    }
}
