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
mod ahci;
mod nvme;
mod storage;
mod progress;
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
    // DIAGNÓSTICO: RED SQUARE (30,0) al entrar en el kernel
    // El framebuffer info está al inicio de BootInfo
    unsafe {
        let base_ptr = boot_info_ptr as *const u64;
        let fb_base = *base_ptr;
        let pitch_ptr = (boot_info_ptr + 16) as *const u32; // base(8) + width(4) + height(4)
        let pitch = *pitch_ptr;
        
        if fb_base != 0 && fb_base != 0xDEADBEEF {
            let fb = fb_base as *mut u32;
            for y in 0..10 {
                for x in 30..40 {
                    *fb.add(y * pitch as usize + x) = 0xFF0000;
                }
            }
        }
    }

    // 1. Initialize serial for diagnostics immediately
    
    // Explicit raw asm for early confirmation (COM1)
    unsafe {
        for &b in b"[KERNEL] _start reached via COM1\n" {
            let mut timeout = 1_000_000;
            let mut status: u8;
            while timeout > 0 {
                core::arch::asm!("in al, dx", in("dx") 0x3F8u16 + 5, out("al") status);
                if (status & 0x20) != 0 { break; }
                timeout -= 1;
            }
            core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") b);
        }
    }

    serial::init();
    
    // VERIFICACIÓN MANUAL DE DIRECCIONES
    serial::serial_print("[KERNEL] FB Phys Base: ");
    serial::serial_print_hex(boot_info_ptr); // This is just the ptr, let's get the base from it
    serial::serial_print("\n");
    
    let fb_base_val: u64 = unsafe { *(boot_info_ptr as *const u64) };
    serial::serial_print("[KERNEL] FB Base Value: ");
    serial::serial_print_hex(fb_base_val);
    serial::serial_print("\n");

    let cr3_val: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3_val); }
    serial::serial_print("[KERNEL] Current CR3: ");
    serial::serial_print_hex(cr3_val);
    serial::serial_print("\n");

    // 2. Initialize boot info (Necessary for bar() to find the framebuffer)
    boot::init(boot_info_ptr);
    
    // DIAGNÓSTICO: CYAN SQUARE (40,0) después de boot::init
    unsafe {
        if let Some((fb_base, _, _, pitch, _)) = boot::get_fb_info() {
            let fb = fb_base as *mut u32;
            for y in 0..10 {
                for x in 40..50 {
                    *fb.add(y * (pitch as usize / 4) + x) = 0x00FFFF; // Cyan
                }
            }
        }
    }

    // DIAGNÓSTICO: YELLOW SQUARE (50,0) antes de progress::bar(42)
    unsafe {
        if let Some((fb_base, _, _, pitch, _)) = boot::get_fb_info() {
            let fb = fb_base as *mut u32; // Identity (physical)
            for y in 0..10 {
                for x in 50..60 {
                    *fb.add(y * (pitch as usize / 4) + x) = 0xFFFF00; // Yellow
                }
            }
        }
    }

    // DIAGNÓSTICO: ORANGE SQUARE (60,0) usando HHDM (Virtual)
    // Esto verifica si el mapeo 0xFFFF9000... es válido
    unsafe {
        if let Some((fb_base, _, _, pitch, _)) = boot::get_fb_info() {
            let virt = crate::memory::phys_to_virt(fb_base) as *mut u32;
            for y in 0..10 {
                for x in 60..70 {
                    *virt.add(y * (pitch as usize / 4) + x) = 0xFFA500; // Orange
                }
            }
        }
    }

    // 4. Initial progress (distinguish from bootloader's 40%)
    progress::bar(42);
    
    // DIAGNÓSTICO: WHITE SQUARE (60,0) después de progress::bar(42)
    unsafe {
        if let Some((fb_base, _, _, pitch, _)) = boot::get_fb_info() {
            let fb = fb_base as *mut u32;
            for y in 0..10 {
                for x in 60..70 {
                    *fb.add(y * (pitch as usize / 4) + x) = 0xFFFFFF; // White
                }
            }
        }
    }

    serial::serial_print("DEBUG: Entered _start (Higher Half)\n");

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
    serial::serial_print("[KERNEL] kernel_bootstrap entry\n");
    let cpu_id = crate::process::get_cpu_id();
    serial::serial_printf(format_args!("\n\n!!! KERNEL BOOT START v3 !!! CPU ID: {} (Raw APIC info in get_cpu_id)\n\n", cpu_id));
    // Stage 1: Get BootInfo from centralized storage (already initialized in _start)
    let boot_info = boot::get_boot_info();
    
    let pml4_phys = boot_info.pml4_addr;
    let kernel_phys_base = boot_info.kernel_phys_base;

    // progress::bar(60) will be called after paging init
    serial::serial_print("Switched to Higher Half Stack successfully\n");

    // Stage 2: Basic hardware initialization
    boot::load_gdt();
    boot::enable_sse();

    // Stage 4: Subsystem initialization
    serial::serial_print("Verifying paging...\n");
    memory::init_paging(kernel_phys_base);
    progress::bar(60);
    
    interrupts::init();
    
    // Stage 4.5: ACPI and APIC discovery
    serial::serial_print("Initializing ACPI...\n");
    acpi::init(boot_info.rsdp_addr);
    
    serial::serial_print("Initializing Local APIC...\n");
    apic::init();
    progress::bar(65);
    
    serial::serial_print("Initializing memory system...\n");
    memory::init();
    progress::bar(70);
    
    // Init DevFS before other subsystems
    filesystem::init_devfs();
    
    serial::serial_print("Starting secondary CPUs...\n");
    cpu::start_aps();
    progress::bar(75);

    // Stage 3: Strict User/Kernel Separation - Moved after AP startup
    memory::remove_identity_mapping();
    progress::bar(80);
    serial::serial_print("✓ Identity mapping removed (Strict User/Kernel Separation active)\n");
     
    serial::serial_print("[INIT] Initializing IPC...\n");
    ipc::init();
    serial::serial_print("[INIT] Initializing kernel process...\n");
    process::init_kernel_process();
    serial::serial_print("[INIT] Initializing scheduler...\n");
    scheduler::init();
    progress::bar(85);
    serial::serial_print("[INIT] Initializing syscalls...\n");
    syscalls::init();
    serial::serial_print("[INIT] Initializing scheme system...\n");
    crate::scheme::init(); // Initialize Redox-style scheme system
    serial::serial_print("[INIT] Initializing file descriptors...\n");
    fd::init();
    serial::serial_print("[INIT] Initializing services...\n");
    servers::init(); // Register display:, input:, snd:, net: schemes so display_service can open display:
    progress::bar(86);
    
    // crate::video::init();
    serial::serial_print("[INIT] Initializing PCI...\n");
    pci::init();
    progress::bar(87);
    
    serial::serial_print("[INIT] Initializing USB HID...\n");
    usb_hid::init(); // enable USB HID testing
    progress::bar(88);
    
    serial::serial_print("[INIT] Initializing NVIDIA...\n");
    nvidia::init();
    serial::serial_print("[INIT] Initializing VirtIO...\n");
    virtio::init();
    serial::serial_print("[INIT] Initializing NVMe...\n");
    nvme::init();
    serial::serial_print("[INIT] Initializing AHCI...\n");
    ahci::init();
    serial::serial_print("[INIT] Initializing ATA...\n");
    ata::init();
    // Register disk: scheme AFTER all storage drivers have registered their devices.
    // This is essential on real hardware (AHCI) where virtio::init() is a no-op.
    storage::register_disk_scheme();
    // List all detected storage devices before mounting
    storage::list_devices();
    progress::bar(89);

    serial::serial_print("[INIT] Initializing Filesystem...\n");
    filesystem::init();
    progress::bar(90);
    
    serial::serial_print("[INIT] Initialization Complete. Entering kernel_main.\n");
    
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
                progress::bar(95);
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
    
    progress::bar(100);
    serial::serial_print("\n[KERNEL] System initialization complete!\n\n");

    loop {
        // Procesar mensajes IPC mientras haya pendientes
        if crate::ipc::has_pending_messages() {
            ipc::process_messages();
        } else {
            // Si no hay mensajes ni otros procesos listos, "dormir" hasta la siguiente interrupción
            unsafe {
                x86_64::instructions::interrupts::enable_and_hlt();
            }
        }
        
        // Intentar planificar otros procesos (p.ej. tras recibir un mensaje o una interrupción)
        crate::scheduler::schedule();
    }
}
