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

/// Stack de arranque (16KB)
/// Used to ensure we run on a Higher Half stack immediately after boot
#[repr(align(16))]
struct BootStack {
    stack: [u8; 65536],
}

static mut BOOT_STACK: BootStack = BootStack { stack: [0; 65536] };

/// Panic handler del kernel
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    serial::serial_print("at ");
    if let Some(location) = info.location() {
        serial::serial_print(location.file());
        serial::serial_print(":");
        serial::serial_print_dec(location.line() as u64);
    }
    serial::serial_print("\n  Message: ");
    let mut writer = crate::serial::SerialWriter;
    let _ = core::fmt::write(&mut writer, format_args!("{}", info.message()));
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
/// - RDI: boot_info_ptr - Pointer to BootInfo structure
#[no_mangle]
#[link_section = ".init"]
pub extern "C" fn _start(boot_info_ptr: u64) -> ! {
    // Initialize serial for debugging using early (bootloader provided) stack
    serial::init();
    serial::serial_print("DEBUG: Entered _start (Higher Half)\n");
    
    if boot_info_ptr == 0 {
        panic!("BootInfo pointer is null!");
    }

    unsafe {
        serial::serial_print("[KERNEL] BOOT_STACK addr: ");
        serial::serial_print_hex(&raw const BOOT_STACK as u64);
        serial::serial_print("\n");
    }

    // Switch to Higher Half Boot Stack immediately to allow removing identity mapping later
    // Ensure stack top is 16-byte aligned
    let stack_top = (unsafe { &raw mut BOOT_STACK.stack } as u64) + 65536;
    let stack_top_aligned = stack_top & !0xF;

    unsafe {
        core::arch::asm!(
            "mov rsp, {0}",
            "mov rbp, 0",
            "jmp {1}",
            in(reg) stack_top_aligned,
            in(reg) kernel_bootstrap as u64,
            in("rdi") boot_info_ptr, // Pass the original boot_info_ptr to kernel_bootstrap
            options(noreturn)
        );
    }
}

/// Entry point in Higher Half with clean stack
extern "C" fn kernel_bootstrap(boot_info_ptr: u64) -> ! {
    // Stage 1: Initialize BootInfo in centralized storage
    boot::init(boot_info_ptr);
    let boot_info = boot::get_boot_info();
    
    let pml4_phys = boot_info.pml4_addr;
    let kernel_phys_base = boot_info.kernel_phys_base;

    serial::serial_print("Switched to Higher Half Stack successfully\n");

    // Stage 2: Basic hardware initialization
    boot::load_gdt();
    boot::enable_sse();

    // Stage 3: Strict User/Kernel Separation
    // Remove the 16GB identity mapping provided by the bootloader.
    // After this, only Higher Half (Kernel) and explicitly mapped User locations are valid.
    memory::remove_identity_mapping();
    serial::serial_print("✓ Identity mapping removed (Strict User/Kernel Separation active)\n");

    // Stage 4: Subsystem initialization
    serial::serial_print("Verifying paging...\n");
    memory::init_paging(kernel_phys_base);
    
    interrupts::init();
    
    serial::serial_print("Testing IDT with breakpoint...\n");
    x86_64::instructions::interrupts::int3();
    serial::serial_print("IDT test passed\n");
    
    serial::serial_print("Initializing memory system...\n");
    memory::init();
    
    serial::serial_print("Testing early heap allocation...\n");
    let test_vec = vec![0u8; 128];
    serial::serial_print("Early heap allocation successful, ptr: ");
    serial::serial_print_hex(test_vec.as_ptr() as u64);
    serial::serial_print("\n");
    core::mem::drop(test_vec);
     
    ipc::init();
    process::init_kernel_process();
    scheduler::init();
    syscalls::init();
    fd::init();
    // servers::init_servers();
    // pci::init();
    // nvidia::init();
    // virtio::init();
    // ata::init();
    // filesystem::init();
    
    serial::serial_print("Microkernel initialized successfully!\n");
    
    // Final Stage: Jump to main loop
    kernel_main(boot_info);
}

/// Init process binary embedded in kernel
pub static INIT_BINARY: &[u8] = include_bytes!("../userspace/init/target/x86_64-unknown-none/release/eclipse-init");

/// Función principal del kernel
pub fn kernel_main(_boot_info: &boot::BootInfo) -> ! {
    // Framebuffer info is now handled centrally by boot::get_framebuffer_info()
    // No need to store it manually
    
    serial::serial_print("Entering kernel main loop...\n");
    
    // Intentar montar el sistema de archivos
    serial::serial_print("[KERNEL] Attempting to mount root filesystem...\n");
    let mut init_loaded = false;
    
    match filesystem::mount_root() {
        Ok(_) => {
            serial::serial_print("[KERNEL] Root filesystem mounted successfully\n");
            serial::serial_print("[KERNEL] Skipping disk systemd (crashes), using embedded init...\n");
        }
        Err(e) => {
            serial::serial_print("[KERNEL] Failed to mount filesystem: ");
            serial::serial_print(e);
            serial::serial_print("\n");
        }
    }
    
    // Load embedded init
    if !init_loaded {
        serial::serial_print("\n[KERNEL] Loading init process from embedded binary...\n");
        if let Some(pid) = elf_loader::load_elf(INIT_BINARY) {
            serial::serial_print("[KERNEL] Init process loaded with PID: ");
            serial::serial_print_dec(pid as u64);
            serial::serial_print("\n");
            scheduler::enqueue_process(pid);
            serial::serial_print("[KERNEL] Init process scheduled for execution\n");
        }
    }
    
    serial::serial_print("\n[KERNEL] System initialization complete!\n\n");
    
    loop {
        ipc::process_messages();
        crate::scheduler::tick();
        crate::scheduler::schedule();
        for _ in 0..10000 {
            core::hint::spin_loop();
        }
    }
}
