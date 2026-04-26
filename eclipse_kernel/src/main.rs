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

// Módulos del microkernel
mod ai_core;
mod boot;
mod memory;
mod vm_object;
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
mod e1000e;
mod eth;
mod filesystem;
mod binaries;
mod ata;
mod ahci;
mod nvme;
mod storage;
mod progress;
mod fd;  // File descriptor management
mod random_scheme;
mod scheme; // Redox-style scheme system
mod tty;    // Dedicated TTY Scheme
mod pty;    // PTY Scheme
mod pipe;   // Pipe anónimas (sys_pipe / POSIX pipe)
pub mod bcache; // Buffer Cache
pub mod epoll;
mod eventfd;
mod signalfd;
mod timerfd;
mod usb_hid; // USB HID (stub)
mod acpi;    // ACPI discovery
mod apic;    // Local APIC
mod sw_cursor; // Software cursor for real-hardware (non-VirtIO) EFI GOP framebuffer
mod sync;    // Synchronization primitives
mod net;
mod sys_scheme;
mod proc_scheme;
mod kqueue;
pub mod drm_scheme; // DRM scheme for ioctl
mod page_cache;

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
    
    // Intentar mostrar pantalla azul (BSOD) si el hardware está disponible
    progress::panic_bsod(info);

    loop {
        unsafe { core::arch::asm!("hlt") };
    }
}

#[cfg(not(test))]
#[alloc_error_handler]
fn alloc_error_handler(layout: alloc::alloc::Layout) -> ! {
    let pid = crate::syscalls::LAST_SYSCALL_PID.load(core::sync::atomic::Ordering::Relaxed);
    let num = crate::syscalls::LAST_SYSCALL_NUM.load(core::sync::atomic::Ordering::Relaxed);
    panic!("allocation error: {:?} (Last Syscall: {} from PID {})", layout, num, pid)
}

/// Punto de entrada del kernel, llamado desde el bootloader UEFI
/// 
/// Parámetros (x86_64 calling convention):
/// - RDI: boot_info_ptr - Pointer to BootInfo structure
#[cfg(not(test))]
#[no_mangle]
#[link_section = ".init"]
pub extern "C" fn _start(boot_info_ptr: u64) -> ! {
    // 0. Emit a raw serial byte before ANY initialization to confirm the kernel
    //    is executing.  This bypasses SERIAL_INITIALIZED and does not touch any
    //    static variable, so it is safe even before BSS has been zeroed.
    unsafe {
        core::arch::asm!(
            // Wait until COM1 Transmitter Holding Register is empty (LSR bit 5)
            "2:",
            "mov dx, 0x3FD",       // COM1 Line Status Register
            "in al, dx",
            "test al, 0x20",
            "jz 2b",
            // Send 'K' to indicate kernel entry
            "mov dx, 0x3F8",
            "mov al, 0x4B",        // 'K'
            "out dx, al",
            out("dx") _,
            out("al") _,
            options(nomem, nostack, preserves_flags),
        );
    }

    // 1. Zero BSS first (before serial::init so SERIAL_INITIALIZED is not erased)
    // BSS must be zeroed before any static variable is written, because BSS
    // zeroing resets every zero-initialized static (including SERIAL_INITIALIZED).
    // If serial::init() runs first it sets SERIAL_INITIALIZED=true, but then BSS
    // zeroing immediately clears it back to false, silencing all serial output.
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

    // 2. Initialize serial for diagnostics (after BSS zeroing so the init sticks)
    serial::init();

    boot::load_gdt();
    boot::enable_cpu_features();

    // 3. Initialize boot info
    boot::init(boot_info_ptr);

    // Confirm kernel is running with serial output before any framebuffer access
    serial::serial_print("[KERNEL] Entered _start (Higher Half)\n");

    // Switch to Higher Half Boot Stack immediately to allow removing identity mapping later

    // Diagnostic framebuffer squares using identity-mapped (physical) address only.
    // NOTE: HHDM-based framebuffer access (phys_to_virt) is intentionally NOT done
    // here because no IDT is loaded yet; a page fault would triple-fault and freeze
    // the system with no serial output.
    unsafe {
        if let Some((fb_base, _, _, pitch, _, _)) = boot::get_fb_info() {
            let fb = fb_base as *mut u32;
            // CYAN square at x=40
            for y in 0..10 {
                for x in 40..50 {
                    *fb.add(y * (pitch as usize / 4) + x) = 0x00FFFF;
                }
            }
            // YELLOW square at x=50
            for y in 0..10 {
                for x in 50..60 {
                    *fb.add(y * (pitch as usize / 4) + x) = 0xFFFF00;
                }
            }
            // WHITE square at x=60
            for y in 0..10 {
                for x in 60..70 {
                    *fb.add(y * (pitch as usize / 4) + x) = 0xFFFFFF;
                }
            }
        }
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
            in(reg) kernel_bootstrap as *const () as u64,
            in("rdi") boot_info_ptr, // Pass the original boot_info_ptr to kernel_bootstrap
            options(noreturn)
        );
    }
}

/// Entry point in Higher Half with clean stack
extern "C" fn kernel_bootstrap(_boot_info_ptr: u64) -> ! {
    // Redundant but safe: ensure interrupts stay disabled after stack switch.
    unsafe { core::arch::asm!("cli", options(nomem, nostack, preserves_flags)); }
    serial::serial_print("[KERNEL] kernel_bootstrap entry\n");

    let cpu_id = crate::process::get_cpu_id();
    serial::serial_printf(format_args!("\n\n!!! KERNEL BOOT START v3 !!! CPU ID: {} (Raw APIC info in get_cpu_id)\n\n", cpu_id));
    // Stage 1: Get BootInfo from centralized storage (already initialized in _start)
    // (Accessed after load_gdt() so that gs:[16] reads are safe.)
    let boot_info = boot::get_boot_info();
    
    let _pml4_phys = boot_info.pml4_addr;
    let kernel_phys_base = boot_info.kernel_phys_base;

    // progress::bar(60) will be called after paging init
    serial::serial_print("Switched to Higher Half Stack successfully\n");
    memory::init_pat();
    cpu::detect_features();
    // Stage 4: Subsystem initialization
    serial::serial_print("Verifying paging...\n");
    memory::init_paging(kernel_phys_base);
    memory::frame_allocator::init(boot_info);
    memory::init(
        boot_info.heap_phys_base,
        boot_info.heap_phys_size,
        boot_info.conventional_mem_total_bytes,
    );
    progress::init();
    progress::bar(65);

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
    // Register display:, input:, snd:, net:, sys: schemes so display_service can open display:
    servers::init(); 
    crate::scheme::register_scheme("sys", alloc::sync::Arc::new(sys_scheme::SysScheme::new()));
    crate::scheme::register_scheme("proc", alloc::sync::Arc::new(proc_scheme::ProcScheme::new()));
    crate::scheme::register_scheme("drm", alloc::sync::Arc::new(drm_scheme::DrmScheme));
    crate::scheme::register_scheme("eventfd", crate::eventfd::get_eventfd_scheme().clone());
    crate::scheme::register_scheme("timerfd", crate::timerfd::get_timerfd_scheme().clone());
    progress::bar(86);
    
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
    progress::init();
    serial::serial_print("[INIT] Initializing Intel e1000e Ethernet...\n");
    e1000e::init();
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

    // Pre-create the virtual /run directories needed by labwc and seatd.
    // This must happen after filesystem::init() so the virtual overlay is ready.
    let _ = filesystem::mkdir_path("/run", 0o755);
    let _ = filesystem::mkdir_path("/run/user", 0o755);
    let _ = filesystem::mkdir_path("/run/user/0", 0o700);
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
        loop { crate::cpu::idle(100); }
    }
    let init_data: alloc::vec::Vec<u8> = match filesystem::read_file_alloc("/sbin/eclipse-init") {
        Err(e) => {
            serial::serial_printf(format_args!("[KERNEL] ERROR: Cannot read /sbin/eclipse-init: {}\n", e));
            loop { crate::cpu::idle(100); }
        }
        Ok(data) if data.is_empty() => {
            serial::serial_print("[KERNEL] ERROR: /sbin/eclipse-init is empty\n");
            loop { crate::cpu::idle(100); }
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
            // Write completion messages BEFORE enqueuing so that APs cannot start running init
            // and interleave its serial output with these kernel messages.
            serial::serial_print("[KERNEL] Init process scheduled for execution\n");
            let cpu_id = crate::process::get_cpu_id();
            serial::serial_printf(format_args!("\n[C{}] [KERNEL] System initialization complete!\n\n", cpu_id));
            progress::bar(100);
            scheduler::enqueue_process(pid);
        }
        Err(e) => {
            serial::serial_printf(format_args!("[KERNEL] Failed to spawn init process: {}\n", e));
            loop { crate::cpu::idle(100); }
        }
    }

    loop {
        ipc::process_messages();
        crate::scheduler::tick();
        let sleep = crate::scheduler::schedule();
        if !ipc::has_pending_messages() {
            crate::cpu::idle(sleep);
        }
    }
}
