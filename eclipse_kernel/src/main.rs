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
mod drm;
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
mod sw_cursor; // Software cursor for real-hardware (non-VirtIO) EFI GOP framebuffer
mod sync;    // Synchronization primitives
mod drm_scheme; // DRM scheme for ioctl

#[cfg(not(test))]
#[global_allocator]
static GLOBAL_ALLOC: KernelAllocator = KernelAllocator;

struct KernelAllocator;

unsafe impl core::alloc::GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        memory::ALLOCATOR.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        memory::ALLOCATOR.dealloc(ptr, layout)
    }

    unsafe fn alloc_zeroed(&self, layout: core::alloc::Layout) -> *mut u8 {
        memory::ALLOCATOR.alloc_zeroed(layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: core::alloc::Layout, new_size: usize) -> *mut u8 {
        memory::ALLOCATOR.realloc(ptr, layout, new_size)
    }
}

/// Stack de arranque (16KB)
/// Used to ensure we run on a Higher Half stack immediately after boot
#[repr(align(16))]
struct BootStack {
    stack: [u8; 65536],
}

static mut BOOT_STACK: BootStack = BootStack { stack: [0; 65536] };

/// Panic handler del kernel
#[cfg(not(test))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    serial::serial_printf(format_args!("\n[KERNEL] PANIC on CPU {}: {}\n", crate::process::get_cpu_id(), info));
    loop {
        unsafe { core::arch::asm!("hlt") };
    }
}

#[cfg(not(test))]
#[alloc_error_handler]
fn alloc_error_handler(layout: alloc::alloc::Layout) -> ! {
    panic!("allocation error: {:?}", layout)
}

/// Punto de entrada del kernel, llamado desde el bootloader UEFI
/// 
/// Parámetros (x86_64 calling convention):
/// - RDI: boot_info_ptr - Pointer to BootInfo structure
#[cfg(not(test))]
#[no_mangle]
#[link_section = ".init"]
pub extern "C" fn _start(boot_info_ptr: u64) -> ! {
    // 1. Raw serial diagnostic (High Priority)
    // Write 'K' to COM1 (0x3f8) immediately to signal kernel entry.
    unsafe {
        core::arch::asm!(
            "mov dx, 0x3f8",
            "mov al, 'K'",
            "out dx, al",
            options(nomem, nostack, preserves_flags)
        );
    }

    // 1. Initialize serial for diagnostics immediately
    // Explicit raw asm for early confirmation (COM1)
    unsafe {
        for &b in b"[KERNEL] _start reached via COM1\n" {
            let mut timeout = 1_000;
            let mut status: u8;
            while timeout > 0 {
                core::arch::asm!("in al, dx", in("dx") 0x3F8u16 + 5, out("al") status);
                if (status & 0x20) != 0 { break; }
                timeout -= 1;
            }
            core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") b);
        }
    }

    // 1. Initialize serial for diagnostics immediately
    serial::init();

    // 1.5 Enable SSE early (embedded-graphics requires it)
    boot::enable_sse();

    // 1.6 Zero BSS (linker symbols __bss_start, __bss_end)
    extern "C" {
        static mut __bss_start: u8;
        static mut __bss_end: u8;
    }
    unsafe {
        let mut curr = &raw mut __bss_start;
        let end = &raw mut __bss_end;
        while curr < end {
            curr.write_volatile(0);
            curr = curr.add(1);
        }
    }

    // 2. Initialize boot info
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
    // Redundant but safe: ensure interrupts stay disabled after stack switch.
    unsafe { core::arch::asm!("cli", options(nomem, nostack, preserves_flags)); }
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
    memory::init_pat();
    cpu::detect_features();

    // Stage 4: Subsystem initialization
    serial::serial_print("Verifying paging...\n");
    memory::init_paging(kernel_phys_base);
    progress::bar(60);
    
    serial::serial_print("DEBUG: init memory system...\n");
    memory::init();
    serial::serial_print("DEBUG: init progress system...\n");
    progress::bar(65);
    serial::serial_print("DEBUG: init interrupts...\n");

    interrupts::init();
    
    // Stage 4.5: ACPI and APIC discovery
    serial::serial_print("Initializing ACPI...\n");
    acpi::init(boot_info.rsdp_addr);
    
    serial::serial_print("Initializing Local APIC...\n");
    apic::init();
    // Calibrate the LAPIC timer against the PIT on the BSP so all CPUs
    // can use the same count when they call apic::init_timer() later.
    apic::calibrate_timer();
    // Start LAPIC periodic timer on BSP for SMP. Drives system tick when PIT delivery
    // is unreliable. Keep PIT unmasked so we have a fallback (both can fire).
    apic::init_timer(crate::interrupts::APIC_TIMER_VECTOR);
    // Mask PIT (IRQ 0) on the BSP so only the LAPIC timer drives the system tick.
    // This avoids "double-ticking" when both interrupts are enabled.
    crate::interrupts::mask_pit_irq();
    progress::bar(70);
    
    // Init DevFS before other subsystems
    filesystem::init_devfs();
    progress::bar(71);
    serial::serial_print("Starting secondary CPUs...\n");
    cpu::start_aps();
    serial::serial_print("DEBUG: AP discovery complete. Calling progress::bar(75)...\n");
    progress::bar(75);
    serial::serial_print("DEBUG: progress::bar(75) done. Calling memory::remove_identity_mapping()...\n");

    // Stage 3: Strict User/Kernel Separation - Moved after AP startup
    serial::serial_print("DEBUG: Removing identity mapping...\n");
    memory::remove_identity_mapping();
    serial::serial_print("DEBUG: memory::remove_identity_mapping() done.\n");
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
    
    serial::serial_print("[INIT] Initializing DRM...\n");
    drm::init();
    
    serial::serial_print("[INIT] Initializing USB HID...\n");
    usb_hid::init();
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
    progress::bar(89);

    serial::serial_print("[INIT] Initializing Filesystem...\n");
    filesystem::init();
    // Notify APs that system boot is complete, so they can start their scheduler loops.
    // We do this BEFORE loading the init process to ensure all cores are ready.
    serial::serial_print("[BOOT] Releasing APs for scheduler...\n");
    crate::cpu::SYSTEM_BOOT_COMPLETE.store(true, core::sync::atomic::Ordering::SeqCst);
    
    // Small delay to allow APs to enter their respective loops and print their names
    // before we start hitting the filesystem heavily.
    for _ in 0..1_000_000 { crate::cpu::pause(); }
    serial::serial_print("[BOOT] APs released, proceeding to init\n");
    
    // Final Stage: Jump to main loop
    kernel_main(boot_info);
}

/// Función principal del kernel
extern "C" fn kernel_main(_boot_info: &boot::BootInfo) -> ! {
    // Framebuffer info is now handled centrally by boot::get_framebuffer_info()
    // No need to store it manually
    
    serial::serial_print("Entering kernel main loop...\n");
    // Save kernel CR3 immediately (before any process runs) for exec() of service binaries
    crate::memory::save_kernel_cr3();

   
    // Cargar init desde /sbin/eclipse-init (root debe estar montado).
    // Use read_file_alloc so the entire binary is allocated at the exact size
    // reported by the filesystem, avoiding truncation for binaries > 2 MiB.
    if !filesystem::is_mounted() {
        serial::serial_print("[KERNEL] ERROR: Root not mounted, cannot load /sbin/eclipse-init\n");
        loop { crate::cpu::idle(); }
    }
    let init_data: alloc::vec::Vec<u8> = match filesystem::read_file_alloc("/sbin/eclipse-init") {
        Err(e) => {
            serial::serial_printf(format_args!("[KERNEL] ERROR: Cannot read /sbin/eclipse-init: {}\n", e));
            loop { crate::cpu::idle(); }
        }
        Ok(data) if data.is_empty() => {
            serial::serial_print("[KERNEL] ERROR: /sbin/eclipse-init is empty\n");
            loop { crate::cpu::idle(); }
        }
        Ok(data) => {
            serial::serial_printf(format_args!("[KERNEL] Loaded /sbin/eclipse-init ({} bytes)\n", data.len()));
            data
        }
    };
    let init_slice: &[u8] = &init_data;

    match process::spawn_process(init_slice, "init") {
        Ok(pid) => {
            serial::serial_printf(format_args!("[KERNEL] Init process loaded with PID: {}\n", pid));
            progress::bar(95);
            scheduler::enqueue_process(pid);
            serial::serial_print("[KERNEL] Init process scheduled for execution\n");
        }
        Err(e) => {
            serial::serial_printf(format_args!("[KERNEL] Failed to spawn init process: {}\n", e));
            loop { crate::cpu::idle(); }
        }
    }
    
    let cpu_id = crate::process::get_cpu_id();
    serial::serial_printf(format_args!("\n[C{}] [KERNEL] System initialization complete!\n\n", cpu_id));
    progress::bar(100);
    //progress::stop_logging();

    loop {
        // Heartbeat IPC (solo un núcleo lo imprimirá cada 5s)
        ipc::p2p_heartbeat();

        // Procesar mensajes IPC mientras haya pendientes
        if crate::ipc::has_pending_messages() {
            ipc::process_messages();
        } else {
            // Si no hay mensajes ni otros procesos listos, "dormir" hasta la siguiente interrupción
            crate::cpu::idle();
        }
        
        // Intentar planificar otros procesos (p.ej. tras recibir un mensaje o una interrupción)
        crate::scheduler::schedule();
    }
}
