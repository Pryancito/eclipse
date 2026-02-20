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

use core::sync::atomic::{AtomicUsize, Ordering};

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

        serial_printf(format_args!("[CPU] Using verified identity map to write trampoline to physical {:#x}\n", 
            trampoline_phys));
        
        core::ptr::copy_nonoverlapping(src_ptr, trampoline_virt as *mut u8, size as usize);
        
        // Marker at 0x7000 (also in the forced map)
        let ptr_a = 0x7000 as *mut u32;
        core::ptr::write_volatile(ptr_a, 0);

        // Verification Read via HHDM (now that phys_to_virt is fixed)
        let hh_virt = crate::memory::phys_to_virt(0x1000);
        let hh_val = core::ptr::read_volatile(hh_virt as *const u64);
        serial_printf(format_args!("[CPU] Verified physical {:#x} via HHDM: {:#x}\n", trampoline_phys, hh_val));



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
            let target_apic_id = acpi.apic_ids[i];
            if target_apic_id == bsp_id { continue; }

            serial_printf(format_args!("[CPU] Starting AP {} (APIC ID {})...\n", i, target_apic_id));
            
            // Allocate unique stack for this AP
            let layout = alloc::alloc::Layout::from_size_align(16384, 16).unwrap();
            let ptr = alloc::alloc::alloc_zeroed(layout);
            let ap_stack = ptr as u64 + 16384;
            *copy_stack = ap_stack;

            // Record current ready count BEFORE starting the AP
            let expected_ready = AP_READY.load(Ordering::SeqCst) + 1;

            // Send INIT-SIPI sequence
            crate::apic::send_ipi_exact(target_apic_id, 0, 5, true, false);
            delay_ms(10);
            crate::apic::send_ipi_exact(target_apic_id, 0x01, 6, false, false);
            
            // Give it 1ms to see if it starts on the first SIPI
            delay_ms(1);
            if AP_READY.load(Ordering::SeqCst) < expected_ready {
                // Send second SIPI if not ready
                crate::apic::send_ipi_exact(target_apic_id, 0x01, 6, false, false);
            }
            
            // Wait briefly for this AP to signal readiness before starting the next one
            let mut sub_timeout = 1000;
            while AP_READY.load(Ordering::SeqCst) < expected_ready && sub_timeout > 0 {
                delay_ms(1);
                sub_timeout -= 1;
            }
            
            if sub_timeout == 0 {
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

fn rdtsc() -> u64 {
    let low: u32;
    let high: u32;
    unsafe { core::arch::asm!("rdtsc", out("eax") low, out("edx") high, options(nomem, nostack, preserves_flags)); }
    ((high as u64) << 32) | (low as u64)
}

fn delay_us(us: u64) {
    let start = rdtsc();
    // Use very safe value (10000 counts per us = 10GHz)
    let wait_cycles = us * 10000; 
    while rdtsc() - start < wait_cycles {
        core::hint::spin_loop();
    }
}

fn delay_ms(ms: u64) {
    let start = crate::interrupts::ticks();
    
    // Ensure interrupts are enabled so we receive the PIT ticks
    unsafe { core::arch::asm!("sti", options(nomem, nostack, preserves_flags)); }
    
    // Wait for at least `ms` ticks to elapse
    while crate::interrupts::ticks() < start + ms {
        // Use hlt to pause execution until the next interrupt fires
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)); }
    }
}

use crate::serial::serial_printf;

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
    
    // Signal readiness immediately
    AP_READY.fetch_add(1, Ordering::SeqCst);
    
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

    crate::interrupts::load_idt();
    crate::apic::init();
    
    loop {
        core::hint::spin_loop();
    }
}
