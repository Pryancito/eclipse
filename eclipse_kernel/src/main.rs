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
mod cpu;
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
mod scheme; // Redox-style scheme system
mod bcache; // Buffer Cache
mod usb_hid; // USB HID (stub)
mod acpi;    // ACPI discovery
mod apic;    // Local APIC

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
    let cpu_id = crate::process::get_cpu_id();
    serial::serial_printf(format_args!("\n\n!!! KERNEL BOOT START v3 !!! CPU ID: {} (Raw APIC info in get_cpu_id)\n\n", cpu_id));
    // Stage 1: Initialize BootInfo in centralized storage
    boot::init(boot_info_ptr);
    let boot_info = boot::get_boot_info();
    
    let pml4_phys = boot_info.pml4_addr;
    let kernel_phys_base = boot_info.kernel_phys_base;

    serial::serial_print("Switched to Higher Half Stack successfully\n");

    // Stage 2: Basic hardware initialization
    boot::load_gdt();
    boot::enable_sse();

    // Stage 4: Subsystem initialization
    serial::serial_print("Verifying paging...\n");
    memory::init_paging(kernel_phys_base);
    
    interrupts::init();
    
    // Stage 4.5: ACPI and APIC discovery
    serial::serial_print("Initializing ACPI...\n");
    acpi::init(boot_info.rsdp_addr);
    
    serial::serial_print("Initializing Local APIC...\n");
    apic::init();
    
    serial::serial_print("Initializing memory system...\n");
    memory::init();
    
    // Init DevFS before other subsystems
    filesystem::init_devfs();
    
    serial::serial_print("Starting secondary CPUs...\n");
    cpu::start_aps();

    // Stage 3: Strict User/Kernel Separation - Moved after AP startup
    memory::remove_identity_mapping();
    serial::serial_print("✓ Identity mapping removed (Strict User/Kernel Separation active)\n");
     
    ipc::init();
    process::init_kernel_process();
    scheduler::init();
    syscalls::init();
    crate::scheme::init(); // Initialize Redox-style scheme system
    fd::init();
    servers::init(); // Register display:, input:, snd:, net: schemes so display_service can open display:
    // crate::video::init();
    pci::init();
    usb_hid::init(); // enable USB HID testing
    nvidia::init();
    virtio::init();
    ata::init();
    filesystem::init();
    
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
    // Save kernel CR3 immediately (before any process runs) for exec() of service binaries
    crate::memory::save_kernel_cr3();

    // Mount is now handled by userspace filesystem_service via SYS_MOUNT
    serial::serial_print("[KERNEL] Waiting for userspace to mount root filesystem...\n");
    let mut init_loaded = false;
    
    // Load embedded init
    if !init_loaded {
        serial::serial_print("\n[KERNEL] Loading init process from embedded binary...\n");
        match process::spawn_process(INIT_BINARY) {
            Ok(pid) => {
                serial::serial_print("[KERNEL] Init process loaded with PID: ");
                serial::serial_print_dec(pid as u64);
                serial::serial_print("\n");
                scheduler::enqueue_process(pid);
                serial::serial_print("[KERNEL] Init process scheduled for execution\n");
            },
            Err(e) => {
                serial::serial_print("[KERNEL] Failed to spawn init process: ");
                serial::serial_print(e);
                serial::serial_print("\n");
            }
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
