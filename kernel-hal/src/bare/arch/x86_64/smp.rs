//! x86_64 Symmetric Multi-Processing startup.
//!
//! Brings up application processors using the LAPIC INIT/SIPI sequence.
//! The AP trampoline is assembled inline (avoiding x86-smpboot's Rust code
//! which causes R_X86_64_64 against local symbol link errors).
//! AP LAPIC IDs are enumerated from the ACPI MADT.

use alloc::vec::Vec;
use core::arch::global_asm;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use acpi::{AcpiHandler, AcpiTables, PhysicalMapping};
use x86::controlregs::cr3;

use crate::{mem::phys_to_virt, CachePolicy, MMUFlags, KCONFIG};

const PAGE_SIZE: usize = 4096;

// Per-AP stacks: 256 KB each, up to 7 APs
const STACK_SIZE: usize = 256 * 1024;
const MAX_APS: usize = 7;

// Trampoline lives at physical 0x6000; SIPI vector = 6
const TRAMPOLINE_PADDR: usize = 0x6000;
const SIPI_VECTOR: u8 = 6;

// Data slots in the trampoline page (physical addresses):
const SLOT_STACK: usize = 0x6FE8; // usize: AP initial RSP
const SLOT_ENTRY: usize = 0x6FF0; // usize: 64-bit entry function
const SLOT_CR3: usize = 0x6FF8; // u32:   BSP CR3 (PML4 physical)

// ─── Trampoline assembly ──────────────────────────────────────────────────────
//
// Copied verbatim from x86-smpboot/src/boot_ap.S and included here so only
// the assembly object is linked (not the crate's Rust helper functions, which
// emit R_X86_64_64 against local symbols that the bare-metal linker rejects).
//
// Memory map within physical page 0x6000:
//   +0x000 : 16-bit/32-bit/64-bit trampoline code  (from ap_trampoline_start)
//   +0x6FE8: SLOT_STACK (stack top)
//   +0x6FF0: SLOT_ENTRY (entry fn ptr)
//   +0x6FF8: SLOT_CR3   (PML4 physical addr, u32)

global_asm!(
    // ── Symbolic constants (identical to boot_ap.S) ──
    ".equ ap_start64_paddr,      ap_trampoline64 - ap_trampoline_start + 0x6000",
    ".equ gdt_64_paddr,          gdt_64_smp     - ap_trampoline_start + 0x6000",
    ".equ gdt_64_pointer_paddr,  gdt_64_ptr_smp - ap_trampoline_start + 0x6000",
    ".equ cr3_ptr,   0x6ff8",
    ".equ entry_ptr, 0x6ff0",
    ".equ stack_ptr, 0x6fe8",
    ".equ temp_stack_top, 0x6fe0",

    ".section .text",
    ".code16",
    ".global ap_trampoline_start",
    ".global ap_trampoline_end",
    "ap_trampoline_start:",
    "  cli",
    "  xor  ax, ax",
    "  mov  ds, ax",
    "  mov  es, ax",
    "  mov  ss, ax",
    // CR4: PAE(5) | PGE(7) | OSFXSR(9) | OSXMMEXCPT(10)
    "  mov  eax, cr4",
    "  or   eax, (1 << 5) | (1 << 7) | (1 << 9) | (1 << 10)",
    "  mov  cr4, eax",
    // CR3
    "  mov  eax, [cr3_ptr]",
    "  mov  cr3, eax",
    // EFER: LME(8) | NXE(11)
    "  mov  ecx, 0xC0000080",
    "  rdmsr",
    "  or   eax, (1 << 8) | (1 << 11)",
    "  wrmsr",
    // CR0: PE(0) | MP(1) | PG(31)
    "  mov  eax, cr0",
    "  or   eax, (1 << 0) | (1 << 1) | (1 << 31)",
    "  mov  cr0, eax",
    // Temporary stack
    "  mov  esp, temp_stack_top",
    // Load 64-bit GDT
    "  lgdt [gdt_64_pointer_paddr]",
    // Far-return to 64-bit code
    "  push 0x8",
    "  lea  eax, [ap_start64_paddr]",
    "  push eax",
    "  retf",

    ".code64",
    "ap_trampoline64:",
    "  xor  ax, ax",
    "  mov  ss, ax",
    "  mov  ds, ax",
    "  mov  es, ax",
    "  mov  fs, ax",
    "  mov  gs, ax",
    "  mov  rsp, [stack_ptr]",
    "  mov  rax, [entry_ptr]",
    "  call rax",
    "1:",
    "  hlt",
    "  jmp 1b",

    // GDT
    ".align 4",
    "gdt_64_smp:",
    "  .quad 0x0000000000000000",   // null
    "  .quad 0x00209A0000000000",   // 64-bit code
    "  .quad 0x0000920000000000",   // 64-bit data
    ".align 4",
    "  .word 0",                    // padding
    "gdt_64_ptr_smp:",
    "  .word gdt_64_ptr_smp - gdt_64_smp - 1",
    "  .long gdt_64_paddr",

    "ap_trampoline_end:",
    ".code64",                      // restore default for remaining file
);

extern "C" {
    fn ap_trampoline_start();
    fn ap_trampoline_end();
}

// ─── AP stacks ───────────────────────────────────────────────────────────────

#[repr(align(4096))]
struct ApStack([u8; STACK_SIZE]);

static mut AP_STACKS: [ApStack; MAX_APS] = [const { ApStack([0u8; STACK_SIZE]) }; MAX_APS];

/// Number of APs that have signalled they are running.
pub static AP_ONLINE_COUNT: AtomicUsize = AtomicUsize::new(0);

// ─── ACPI handler ────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AcpiMap;

impl AcpiHandler for AcpiMap {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> PhysicalMapping<Self, T> {
        let aligned_start = physical_address & !(PAGE_SIZE - 1);
        let aligned_end = (physical_address + size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        PhysicalMapping::new(
            physical_address,
            NonNull::new_unchecked(phys_to_virt(physical_address) as *mut T),
            size,
            aligned_end - aligned_start,
            self.clone(),
        )
    }

    fn unmap_physical_region<T>(_region: &PhysicalMapping<Self, T>) {}
}

// ─── Timing ──────────────────────────────────────────────────────────────────

fn delay_us(us: u64) {
    let start = unsafe { core::arch::x86_64::_rdtsc() };
    // Conservative 1 GHz assumption; actual delay will be shorter on faster CPUs.
    let ticks = us.saturating_mul(1_000); // 1 GHz = 1000 ticks/µs
    let end = start.wrapping_add(ticks);
    while unsafe { core::arch::x86_64::_rdtsc() }.wrapping_sub(start) < ticks {
        let _ = end; // use binding
        core::hint::spin_loop();
    }
}

// ─── Public API ──────────────────────────────────────────────────────────────

/// Start all APs found in the ACPI MADT.  Called once from BSP `primary_init()`.
pub fn start_application_processors() {
    let acpi_rsdp = KCONFIG.acpi_rsdp as usize;
    if acpi_rsdp == 0 {
        crate::klog_warn!("[smp] No ACPI RSDP — skipping AP startup");
        return;
    }

    let ap_lapic_ids = match enumerate_aps(acpi_rsdp) {
        Some(ids) => ids,
        None => return,
    };

    if ap_lapic_ids.is_empty() {
        crate::klog_info!("[smp] ACPI MADT: no application processors found");
        return;
    }

    crate::klog_info!("[smp] Found {} AP(s), LAPIC IDs: {:?}", ap_lapic_ids.len(), ap_lapic_ids);

    // Identity-map 0x6000..0x8000 so the trampoline can run after enabling paging.
    {
        use crate::vm::GenericPageTable;
        let mut pt = crate::vm::PageTable::from_current();
        let flags = MMUFlags::READ
            | MMUFlags::WRITE
            | MMUFlags::EXECUTE
            | MMUFlags::from_bits_truncate(CachePolicy::Cached as usize);
        if let Err(e) = pt.map_cont(TRAMPOLINE_PADDR, PAGE_SIZE * 2, TRAMPOLINE_PADDR, flags) {
            crate::klog_warn!("[smp] identity-map 0x6000 failed: {:?} — AP may triple-fault", e);
        }
        core::mem::forget(pt);
    }

    // Copy trampoline to physical 0x6000.
    unsafe { install_trampoline() };

    // Write BSP's CR3 and entry function.
    unsafe {
        (phys_to_virt(SLOT_CR3) as *mut u32).write_volatile(cr3() as u32);
        (phys_to_virt(SLOT_ENTRY) as *mut usize).write_volatile(KCONFIG.ap_fn as usize);
    }

    let mut started = 0usize;
    for (idx, &lapic_id) in ap_lapic_ids.iter().enumerate() {
        if idx >= MAX_APS {
            crate::klog_warn!("[smp] Too many APs (max {}), skipping LAPIC {}", MAX_APS, lapic_id);
            break;
        }

        let stack_top = unsafe { AP_STACKS[idx].0.as_ptr().add(STACK_SIZE) as usize };
        unsafe { (phys_to_virt(SLOT_STACK) as *mut usize).write_volatile(stack_top) };

        crate::klog_info!("[smp] Starting AP LAPIC {} stack={:#x}", lapic_id, stack_top);

        // INIT IPI → wait 10 ms → SIPI × 2
        zcore_drivers::irq::x86::Apic::send_init_ipi(lapic_id);
        delay_us(10_000);
        zcore_drivers::irq::x86::Apic::send_sipi(SIPI_VECTOR, lapic_id);
        delay_us(200);
        zcore_drivers::irq::x86::Apic::send_sipi(SIPI_VECTOR, lapic_id);
        delay_us(200);

        // Wait up to 100 ms for AP to come online.
        let before = AP_ONLINE_COUNT.load(Ordering::Acquire);
        for _ in 0..100 {
            delay_us(1_000);
            if AP_ONLINE_COUNT.load(Ordering::Acquire) > before {
                started += 1;
                break;
            }
        }
    }

    crate::klog_info!("[smp] SMP init done — {}/{} AP(s) came online", started, ap_lapic_ids.len());
}

/// Called by each AP from `secondary_init()` to announce it is running.
pub fn ap_signal_online() {
    AP_ONLINE_COUNT.fetch_add(1, Ordering::Release);
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

unsafe fn install_trampoline() {
    let src = ap_trampoline_start as *const u8;
    let end = ap_trampoline_end as *const u8;
    let len = end.offset_from(src) as usize;
    let dst = phys_to_virt(TRAMPOLINE_PADDR) as *mut u8;
    core::ptr::copy_nonoverlapping(src, dst, len);
}

fn enumerate_aps(acpi_rsdp: usize) -> Option<Vec<u32>> {
    let tables = match unsafe { AcpiTables::from_rsdp(AcpiMap, acpi_rsdp) } {
        Ok(t) => t,
        Err(e) => {
            crate::klog_warn!("[smp] ACPI parse failed: {:?}", e);
            return None;
        }
    };

    let info = match tables.platform_info() {
        Ok(i) => i,
        Err(e) => {
            crate::klog_warn!("[smp] ACPI platform_info failed: {:?}", e);
            return None;
        }
    };

    let proc_info = match info.processor_info {
        Some(p) => p,
        None => {
            crate::klog_warn!("[smp] ACPI: no processor info in MADT");
            return None;
        }
    };

    use acpi::platform::ProcessorState;
    let ids: Vec<u32> = proc_info
        .application_processors
        .iter()
        .filter(|p| p.state != ProcessorState::Disabled)
        .map(|p| p.local_apic_id)
        .collect();

    Some(ids)
}
