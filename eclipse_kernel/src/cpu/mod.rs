//! CPU Management and SMP Support

use core::arch::global_asm;

// The trampoline code must be 16-bit at the start.
// It will be copied to 0x8000 (physical).
global_asm!(r#"
.section .trampoline, "ax"
.intel_syntax noprefix
.code16
.global trampoline_start
.global trampoline_end

trampoline_start:
    .set T_GDT_PTR, t_gdt_ptr - trampoline_start
    .set T_GDT,     t_gdt - trampoline_start
    .set T_CR3,     t_cr3 - trampoline_start
    .set T_STACK,   t_stack - trampoline_start
    .set T_ENTRY,   t_entry - trampoline_start

    cli
    xor ax, ax
    mov ds, ax
    mov ss, ax
    mov sp, 0x1000
    
    # 1. Enable PAE
    mov eax, cr4
    or eax, 0x20
    mov cr4, eax

    # 2. Load CR3
    mov eax, [0x1000 + T_CR3]
    mov cr3, eax

    # 3. Enable Long Mode (EFER.LME)
    mov ecx, 0xC0000080
    rdmsr
    or eax, 0x100
    wrmsr

    # 4. Enable Paging and Protected Mode
    mov eax, cr0
    or eax, 0x80000001
    mov cr0, eax

    # 5. Load 64-bit GDT
    lgdt [0x1000 + T_GDT_PTR]

    # 6. Far jump to 64-bit mode (CS=0x08)
    # Push 32-bit 0x08 (Code segment)
    .byte 0x66, 0x6a, 0x08
    
    # Push 32-bit LONG_TARGET (EIP)
    .byte 0x66, 0x68
    .long 0x1000 + (long_mode_64 - trampoline_start)
    
    # 32-bit retf (pops EIP, then pops CS)
    .byte 0x66, 0xcb

.code64
long_mode_64:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    
    # Reload CR3 with the full 64-bit physical address (the 32-bit mov above may have
    # truncated it on systems where the PML4 is above 4 GiB).
    mov rax, [0x1000 + T_CR3]
    mov cr3, rax
    
    mov rsp, [0x1000 + T_STACK]
    mov rax, [0x1000 + T_ENTRY]
    jmp rax

.align 16
t_gdt:
    .quad 0x0000000000000000 # Null
    .quad 0x00209a0000000000 # Code64 (Present, DPL0, Code, Exec/Read, L=1)
    .quad 0x0000920000000000 # Data64 (Present, DPL0, Data, Read/Write)
t_gdt_end:

t_gdt_ptr:
    .short t_gdt_end - t_gdt - 1
    .long 0x1000 + T_GDT

.align 8
trampoline_cr3:
t_cr3:   .quad 0
trampoline_stack:
t_stack: .quad 0
trampoline_entry:
t_entry: .quad 0

trampoline_end:
"#);

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

extern "C" {
    /// Linker-defined start of trampoline section
    pub static trampoline_start: u8;
    /// Linker-defined end of trampoline section
    pub static trampoline_end: u8;
    /// Address of variable in trampoline to store CR3
    pub static mut trampoline_cr3: u64;
    /// Address of variable in trampoline to store stack top
    pub static mut trampoline_stack: u64;
    /// Address of variable in trampoline to store entry point
    pub static mut trampoline_entry: u64;
}

static AP_READY: AtomicUsize = AtomicUsize::new(0);
static TSC_COUNTS_PER_US: AtomicU64 = AtomicU64::new(10000); // Default to 10GHz (safe upper bound)

pub fn update_tsc_frequency(counts_per_us: u64) {
    TSC_COUNTS_PER_US.store(counts_per_us, Ordering::SeqCst);
}

pub fn start_aps() {
    let acpi = crate::acpi::get_info();
    let bsp_id = crate::apic::get_id();
    
    serial_printf(format_args!("[CPU] Starting {} secondary cores...\n", acpi.cpu_count - 1));

    unsafe {
        // 0. Ensure TRUE linear physical mapping for low memory (0..2MiB)
        // This bypasses any bootloader-provided kernel_phys_base offset.
        crate::memory::map_physical_low_memory();

        // 1. Copy trampoline to physical 0x1000
        let trampoline_phys = 0x1000;
        // With map_physical_low_memory, virtual 0x1000 is now TRUE physical 0x1000
        let trampoline_virt = 0x1000;
        
        let src_ptr = &raw const trampoline_start;
        let size = &raw const trampoline_end as u64 - src_ptr as u64;
        
        // Ensure size is reasonable
        if size > 4096 {
            serial_printf(format_args!("[CPU] ERROR: Trampoline too large ({} bytes)\n", size));
            return;
        }

        serial_printf(format_args!("[CPU] Writing trampoline: virt=0x1000, size={}\n", size));
        core::ptr::copy_nonoverlapping(src_ptr, trampoline_virt as *mut u8, size as usize);
        
        let hh_virt = crate::memory::phys_to_virt(0x1000);
        let hh_val = unsafe { core::ptr::read_volatile(hh_virt as *const u64) };
        let virt_val = unsafe { core::ptr::read_volatile(trampoline_virt as *const u64) };
        
        serial_printf(format_args!("[CPU] Sanity Check: Virtual 0x1000 reads {:#x}, Physical 0x1000 (via HHDM) reads {:#x}\n", 
            virt_val, hh_val));

        if virt_val != hh_val {
            serial_printf(format_args!("[CPU] CRITICAL: Virtual 0x1000 is NOT mapped to physical 0x1000! APs will fail.\n"));
        }

        // Marker at 0x7000 (also in the forced map)
        let ptr_a = 0x7000 as *mut u32;
        core::ptr::write_volatile(ptr_a, 0);





        // 2. Set common trampoline data
        let cr3: u64; 
        core::arch::asm!("mov {}, cr3", out(reg) cr3);
        
        // Let's get the offsets relative to trampoline_start
        let offset_cr3 = (&raw const trampoline_cr3 as u64) - (src_ptr as u64);
        let offset_stack = (&raw const trampoline_stack as u64) - (src_ptr as u64);
        let offset_entry = (&raw const trampoline_entry as u64) - (src_ptr as u64);

        let copy_cr3 = (trampoline_virt + offset_cr3) as *mut u64;
        let copy_stack = (trampoline_virt + offset_stack) as *mut u64;
        let copy_entry = (trampoline_virt + offset_entry) as *mut u64;

        *copy_cr3 = cr3;
        *copy_entry = ap_entry as *const () as u64;

        let layout = alloc::alloc::Layout::from_size_align(16384, 16).unwrap();
        let ptr = alloc::alloc::alloc_zeroed(layout);
        let ap_stack = ptr as u64 + 16384;
        *copy_stack = ap_stack;
        *copy_cr3 = cr3;
        *copy_entry = ap_entry as *const () as u64;
        
        // 3. Identity mapping is already active (provided by bootloader)
        // crate::memory::set_identity_map(true);
        
        serial_printf(format_args!("[CPU] Emitting INIT-SIPI to {} secondary cores...\n", acpi.cpu_count - 1));
        
        // Reset ready count
        AP_READY.store(0, Ordering::SeqCst);
        
        for i in 0..acpi.cpu_count as usize {
            let target_apic_id: u32 = acpi.apic_ids[i];
            if target_apic_id == bsp_id { continue; }

            serial_printf(format_args!("[CPU] Starting AP {} (APIC ID {})...\n", i, target_apic_id));
            
            // Allocate unique stack for this AP
            let layout = alloc::alloc::Layout::from_size_align(16384, 16).unwrap();
            let ptr = alloc::alloc::alloc_zeroed(layout);
            if ptr.is_null() {
                serial_printf(format_args!("[CPU] ERROR: Failed to allocate stack for AP {}\n", target_apic_id));
                continue;
            }
            let ap_stack = ptr as u64 + 16384;
            *copy_stack = ap_stack;
            
            serial_printf(format_args!("[CPU] AP {} stack allocated at {:p}\n", target_apic_id, ptr));

            // Memory fence: ensure trampoline data (cr3/stack/entry) is fully visible
            // to the AP before the SIPI fires and the AP starts executing.
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

            // Record current ready count BEFORE starting the AP
            let expected_ready = AP_READY.load(Ordering::SeqCst) + 1;

            // Send INIT-SIPI sequence
            serial_printf(format_args!("[CPU] sending INIT to AP {}...\n", target_apic_id));
            crate::apic::send_ipi_exact(target_apic_id, 0, 5, true, false);
            wait_ms(10);
            serial_printf(format_args!("[CPU] sending SIPI to AP {}...\n", target_apic_id));
            crate::apic::send_ipi_exact(target_apic_id, 0x01, 6, false, false);
            
            // Give it 1ms to see if it starts on the first SIPI
            wait_ms(1);
            if AP_READY.load(Ordering::SeqCst) < expected_ready {
                serial_printf(format_args!("[CPU] sending second SIPI to AP {}...\n", target_apic_id));
                // Send second SIPI if not ready
                crate::apic::send_ipi_exact(target_apic_id, 0x01, 6, false, false);
            }
            
            // Wait for this AP to signal readiness before starting the next one.
            // 1000ms gives slow real hardware enough time to boot.
            let timeout_tick = crate::interrupts::ticks() + 1000;
            while AP_READY.load(Ordering::SeqCst) < expected_ready
                && crate::interrupts::ticks() < timeout_tick
            {
                wait_ms(1);
            }
            
            if AP_READY.load(Ordering::SeqCst) < expected_ready {
                serial_printf(format_args!("[CPU] WARNING: AP {} (APIC ID {}) failed to start (timeout)\n", i, target_apic_id));
            } else {
                serial_printf(format_args!("[CPU] AP {} (APIC ID {}) started successfully\n", i, target_apic_id));
            }
        }
        
        let expected_aps = acpi.cpu_count as usize - 1;
        serial_printf(format_args!("[CPU] AP Discovery Complete: {}/{} cores ready\n", 
            AP_READY.load(Ordering::SeqCst), expected_aps));
    }
}

pub fn rdtsc() -> u64 {
    let low: u32;
    let high: u32;
    unsafe { core::arch::asm!("rdtsc", out("eax") low, out("edx") high, options(nomem, nostack, preserves_flags)); }
    ((high as u64) << 32) | (low as u64)
}

fn delay_us(us: u64) {
    let start = rdtsc();
    let wait_cycles = us * TSC_COUNTS_PER_US.load(Ordering::Relaxed); 
    while rdtsc() - start < wait_cycles {
        crate::cpu::pause();
    }
}

fn delay_ms(ms: u64) {
    delay_us(ms * 1000);
}

/// Wait at least `ms` milliseconds using interrupt ticks (APIC/PIT timer) as the
/// primary clock source.  Falls back to RDTSC if the tick counter is not advancing
/// (e.g., interrupts disabled or timer not yet configured), so this function never
/// hangs even on miscalibrated hardware.
///
/// This is the preferred delay for the AP startup sequence because it is immune to
/// TSC calibration errors that would make `delay_ms` either too short or appear to
/// hang on real hardware.
fn wait_ms(ms: u64) {
    if ms == 0 {
        return;
    }
    let start_tick = crate::interrupts::ticks();
    let target_tick = start_tick.saturating_add(ms);

    // RDTSC-based ceiling: wait at most 3× the expected time so we can exit even if
    // the tick counter is frozen (e.g., when called very early before timer init).
    // With a correctly calibrated TSC the fallback fires at 3×ms real milliseconds;
    // with the default 10 000 counts/µs it fires at 30×ms on a 1 GHz machine, still
    // finite and bounded.
    let tsc_start = rdtsc();
    let tsc_ceiling = ms.saturating_mul(3).saturating_mul(1000)
        .saturating_mul(TSC_COUNTS_PER_US.load(Ordering::Relaxed));

    while crate::interrupts::ticks() < target_tick {
        // RDTSC fallback so we never spin forever.
        if tsc_ceiling > 0 && rdtsc().wrapping_sub(tsc_start) >= tsc_ceiling {
            break;
        }
        crate::cpu::pause();
    }
}


use crate::serial::serial_printf;

/// Detectar si estamos ejecutando bajo un hipervisor (QEMU/KVM, etc.).
/// Usa CPUID leaf 1, ECX[31] = Hypervisor Present.
pub fn is_running_under_hypervisor() -> bool {
    let mut ecx_val: u32;
    unsafe {
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "mov {ecx_out:e}, ecx",
            "pop rbx",
            ecx_out = out(reg) ecx_val,
            inout("eax") 1 => _,
            out("ecx") _,
            out("edx") _,
            options(nomem, nostack, preserves_flags)
        );
    }
    (ecx_val & (1 << 31)) != 0
}

static MONITOR_MWAIT_SUPPORTED: AtomicBool = AtomicBool::new(false);

/// Detectar características avanzadas de la CPU (MONITOR/MWAIT)
pub fn detect_features() {
    // CPUID clobbers RBX (callee-saved); push/pop it around the instruction
    // so the compiler does not have to spill it separately.
    // Capture the ECX result directly with out("ecx") to avoid the need for
    // an intermediate mov that could conflict with the rbx save/restore.
    let ecx_val: u32;
    unsafe {
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "pop rbx",
            inout("eax") 1u32 => _,
            out("ecx") ecx_val,
            out("edx") _,
            options(preserves_flags)
        );
    }
    // Bit 3 de ECX en leaf 1 indica soporte de MONITOR/MWAIT
    if (ecx_val & (1 << 3)) != 0 {
        MONITOR_MWAIT_SUPPORTED.store(true, Ordering::SeqCst);
        serial_printf(format_args!("[CPU] MONITOR/MWAIT support detected\n"));
    }
}

use core::sync::atomic::AtomicBool;

/// Dormir el núcleo de forma eficiente.
/// Intenta usar MONITOR/MWAIT (Nivel 4) si está disponible, 
/// de lo contrario usa HLT (Nivel 3).
pub fn idle() {
    if MONITOR_MWAIT_SUPPORTED.load(Ordering::Relaxed) {
        let addr = crate::scheduler::ready_queue_tail_addr();
        unsafe {
            // 1. Armar el hardware de monitoreo sobre la cola del scheduler.
            //    Si alguien escribe en esta dirección, el próximo MWAIT retornará.
            core::arch::asm!(
                "monitor",
                in("rax") addr,
                in("rcx") 0,
                in("rdx") 0,
                options(nomem, nostack, preserves_flags)
            );
            
            // 2. Entrar en estado de bajo consumo.
            //    Habilitamos interrupciones justo antes para que también puedan despertarnos.
            x86_64::instructions::interrupts::enable();
            core::arch::asm!(
                "mwait",
                in("rax") 0, // hints
                in("rcx") 0, // extensions
                options(nomem, nostack, preserves_flags)
            );
        }
    } else {
        // Fallback: HLT tradicional
        unsafe {
            x86_64::instructions::interrupts::enable_and_hlt();
        }
    }
}

/// Nivel 2: Pause.
/// Indica a la CPU que estamos en un spin-loop para optimizar el consumo
/// y evitar violaciones de orden de memoria especulativa.
#[inline]
pub fn pause() {
    core::hint::spin_loop();
}

/// Nivel 4 (Avanzado): Esperar a que una dirección de memoria cambie.
/// Usa MONITOR/MWAIT si está disponible, de lo contrario cae a pause().
pub fn wait_for_change(addr: *const u32) {
    if MONITOR_MWAIT_SUPPORTED.load(Ordering::Relaxed) {
        unsafe {
            core::arch::asm!(
                "monitor",
                in("rax") addr,
                in("rcx") 0,
                in("rdx") 0,
                options(nomem, nostack, preserves_flags)
            );
            core::arch::asm!(
                "mwait",
                in("rax") 0,
                in("rcx") 0,
                options(nomem, nostack, preserves_flags)
            );
        }
    } else {
        pause();
    }
}

/// AP Entry point (called from trampoline)
#[no_mangle]
pub extern "C" fn ap_entry() -> ! {
    // Trace '[[RUST]]'
    unsafe {
        core::arch::asm!(
            "mov dx, 0x3F8",
            "mov al, 0x5B", "out dx, al", "out dx, al",
            "mov al, 0x52", "out dx, al", // 'R'
            "mov al, 0x55", "out dx, al", // 'U'
            "mov al, 0x53", "out dx, al", // 'S'
            "mov al, 0x54", "out dx, al", // 'T'
            "mov al, 0x5D", "out dx, al", "out dx, al",
            options(nomem, nostack, preserves_flags)
        );
    }
    
    // 1. Load per-CPU GDT and set up IA32_KERNEL_GS_BASE for syscall_entry
    crate::boot::load_gdt();
    
    // Trace '[[GDTOK]]'
    unsafe {
        core::arch::asm!(
            "mov dx, 0x3F8",
            "mov al, 0x5B", "out dx, al", "out dx, al",
            "mov al, 0x47", "out dx, al", // 'G'
            "mov al, 0x44", "out dx, al", // 'D'
            "mov al, 0x54", "out dx, al", // 'T'
            "mov al, 0x4F", "out dx, al", // 'O'
            "mov al, 0x4B", "out dx, al", // 'K'
            "mov al, 0x5D", "out dx, al", "out dx, al",
            options(nomem, nostack, preserves_flags)
        );
    }

    // 2. Load the shared IDT (populated by the BSP in interrupts::init())
    crate::interrupts::load_idt();

    // 3. Enable the Local APIC for this AP
    crate::apic::init();

    // 4. Enable SSE (per-CPU CR0/CR4 bits; must mirror boot::enable_sse() on BSP)
    crate::boot::enable_sse();

    // 5. Program the Page Attribute Table MSR (IA32_PAT) – this is a per-CPU MSR
    crate::memory::init_pat();

    // 6. Enable SYSCALL/SYSRET and set up STAR/LSTAR/SFMASK – per-CPU MSRs
    crate::interrupts::init_ap();

    // 7. Allocate a dedicated kernel interrupt stack for this AP.
    // This stack is used by the CPU when transitioning from ring 3 → ring 0
    // (hardware interrupts, exceptions, SYSCALL) on this AP.
    let kernel_stack = alloc::vec![0u8; crate::process::KERNEL_STACK_SIZE];
    let kernel_stack_top = (kernel_stack.as_ptr() as u64 + crate::process::KERNEL_STACK_SIZE as u64) & !0xF;
    core::mem::forget(kernel_stack); // Leak: AP stack is permanent
    crate::boot::set_tss_stack(kernel_stack_top);

    // 8. Start the per-AP Local APIC periodic timer so this core receives
    // scheduling interrupts independently of the BSP's PIT (IRQ 0).
    crate::apic::init_timer(crate::interrupts::APIC_TIMER_VECTOR);

    // 8.5 Switch to the permanent kernel stack.
    // This removes the dependency on the initial 16KB trampoline stack.
    // Note: We use naked assembly here because switching RSP mid-function is extremely dangerous in Rust.
    // We'll jump to a new "safe" loop after the switch.
    
    serial_printf(format_args!("[CPU] AP (APIC ID {}) switching to permanent stack...\n", crate::apic::get_id()));
    
    unsafe {
        core::arch::asm!(
            "mov rsp, {0}",
            "mov rbp, 0",
            "jmp {1}",
            in(reg) kernel_stack_top,
            in(reg) ap_main_loop as u64,
            options(noreturn)
        );
    }
}

/// The permanent main loop for Application Processors.
/// Runs on the dedicated kernel stack.
extern "C" fn ap_main_loop() -> ! {
    // 9. Signal the BSP that this AP is fully initialized.
    // This MUST happen after all per-CPU hardware setup so that the BSP
    // does not proceed to start the next AP before this one is ready.
    AP_READY.fetch_add(1, Ordering::SeqCst);

    serial_printf(format_args!("[CPU] AP (APIC ID {}) fully initialized, entering scheduler loop\n",
        crate::apic::get_id()));

    // Enable interrupts; the APIC timer will drive schedule() from here on.
    unsafe { core::arch::asm!("sti", options(nomem, nostack, preserves_flags)); }

    loop {
        idle();
    }
}
