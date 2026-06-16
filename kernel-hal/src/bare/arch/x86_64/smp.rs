//! x86_64 Symmetric Multi-Processing startup.
//!
//! Brings up application processors using the LAPIC INIT/SIPI sequence.
//! The AP trampoline is assembled inline (avoiding x86-smpboot's Rust code
//! which causes R_X86_64_64 against local symbol link errors).
//! AP LAPIC IDs are enumerated from the ACPI MADT.

use alloc::vec::Vec;
use core::arch::global_asm;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use acpi::{AcpiHandler, AcpiTables, PhysicalMapping};
use x86::controlregs::cr3;

use crate::{mem::phys_to_virt, CachePolicy, MMUFlags, KCONFIG};

const PAGE_SIZE: usize = 4096;

// Per-AP stacks: 256 KB each, allocated from the kernel heap on demand.
const STACK_SIZE: usize = 256 * 1024;
// At most MAX_CORE_NUM logical CPUs total; the BSP is logical 0, so up to
// MAX_CORE_NUM - 1 application processors.
const MAX_APS: usize = crate::config::MAX_CORE_NUM - 1;

// Trampoline lives at physical 0x6000; SIPI vector = 6
const TRAMPOLINE_PADDR: usize = 0x6000;
const SIPI_VECTOR: u8 = 6;

// Data slots in the trampoline page (physical addresses):
const SLOT_LOGICAL: usize = 0x6FD8; // u8: dense logical CPU id for the starting AP
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
    // Far-jump to 64-bit code. Encoded by hand (66 EA imm32 sel16): the
    // 16-bit push/retf pair mixes operand sizes and is fragile across
    // hypervisors/emulators; the direct ljmpl is the canonical switch.
    "  .byte 0x66, 0xea",
    "  .long ap_start64_paddr",
    "  .word 0x8",

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
    "  .quad 0x0000000000000000", // null
    "  .quad 0x00209A0000000000", // 64-bit code
    "  .quad 0x0000920000000000", // 64-bit data
    ".align 4",
    "  .word 0", // padding
    "gdt_64_ptr_smp:",
    "  .word gdt_64_ptr_smp - gdt_64_smp - 1",
    "  .long gdt_64_paddr",
    "ap_trampoline_end:",
    ".code64", // restore default for remaining file
);

extern "C" {
    fn ap_trampoline_start();
    fn ap_trampoline_end();
}

// ─── AP stacks ───────────────────────────────────────────────────────────────

/// Allocate a zeroed, page-aligned AP stack from the kernel heap and return its
/// top address. Leaked intentionally: AP stacks live for the lifetime of the CPU.
fn alloc_ap_stack() -> Option<usize> {
    use alloc::alloc::{alloc_zeroed, Layout};
    let layout = Layout::from_size_align(STACK_SIZE, PAGE_SIZE).unwrap();
    let base = unsafe { alloc_zeroed(layout) };
    if base.is_null() {
        return None;
    }
    Some(base as usize + STACK_SIZE)
}

/// Number of APs that have signalled they are running.
pub static AP_ONLINE_COUNT: AtomicUsize = AtomicUsize::new(0);

// ─── CPU topology: dense logical id  <->  Local APIC ID ─────────────────────────
//
// Local APIC IDs are sparse (cores/threads/sockets leave gaps), so they cannot be
// used directly to index per-CPU arrays. We assign each online CPU a dense logical
// id (0..NCPU, BSP = 0). The forward map (apic -> logical) lives in `lock` so the
// lock crate and the kernel share one id space; here we keep the reverse map
// (logical -> apic) needed to direct IPIs to the right hardware APIC.

/// logical id -> Local APIC ID. Index 0 is the BSP.
static LOGICAL_TO_APIC: [AtomicU8; crate::config::MAX_CORE_NUM] = {
    const ZERO: AtomicU8 = AtomicU8::new(0);
    [ZERO; crate::config::MAX_CORE_NUM]
};

/// Number of logical ids assigned so far (next id to hand out).
pub(super) static CPU_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Raw Local APIC ID of the calling CPU (MMIO when mapped, else CPUID).
fn raw_apic_id() -> u8 {
    lock::hardware_apic_id()
}

/// Assign the next dense logical id to `apic_id`, wiring up both the forward map
/// (apic -> logical, owned by `lock`) and the reverse map (logical -> apic).
/// Returns the assigned logical id. Must run before the target CPU executes any
/// lock-taking code.
fn register_cpu(apic_id: u8) -> usize {
    let logical = CPU_COUNT.fetch_add(1, Ordering::AcqRel);
    assert!(
        logical < crate::config::MAX_CORE_NUM,
        "[smp] more online CPUs than MAX_CORE_NUM={}",
        crate::config::MAX_CORE_NUM
    );
    LOGICAL_TO_APIC[logical].store(apic_id, Ordering::Release);
    lock::set_logical_cpu_id(apic_id, logical as u8);
    logical
}

/// Translate a dense logical CPU id back to its Local APIC ID (for IPI delivery).
pub(super) fn logical_to_apic(logical: usize) -> u32 {
    LOGICAL_TO_APIC
        .get(logical)
        .map(|a| a.load(Ordering::Acquire) as u32)
        .unwrap_or(0)
}

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
    // TSC ticks per microsecond ≈ CPU base frequency in MHz.
    // Apply a 4 GHz floor so the INIT→SIPI gap and AP-online wait are never
    // too short on a fast CPU where CPUID isn't available.  cpu_frequency()
    // now returns the raw CPUID value (no global floor) so we add the floor
    // here explicitly — it is safe to wait *longer* than needed, but not shorter.
    let ticks_per_us = crate::cpu::cpu_frequency().max(4000) as u64;
    let ticks = us.saturating_mul(ticks_per_us);
    let start = unsafe { core::arch::x86_64::_rdtsc() };
    while unsafe { core::arch::x86_64::_rdtsc() }.wrapping_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

// ─── Public API ──────────────────────────────────────────────────────────────

/// Start all APs found in the ACPI MADT.  Called once from BSP `primary_init()`.
pub fn start_application_processors() {
    // The BSP is always logical CPU 0. Register it before anything else so its
    // apic->logical mapping is in place even on a uniprocessor (early-return) path.
    register_cpu(raw_apic_id());

    let acpi_rsdp = KCONFIG.acpi_rsdp as usize;
    if acpi_rsdp == 0 {
        warn!("[smp] No ACPI RSDP — skipping AP startup");
        return;
    }

    let ap_lapic_ids = match enumerate_aps(acpi_rsdp) {
        Some(ids) => ids,
        None => return,
    };

    if ap_lapic_ids.is_empty() {
        warn!("[smp] no application processors in ACPI MADT");
        return;
    }

    warn!(
        "[smp] starting {} AP(s), LAPIC IDs: {:?}",
        ap_lapic_ids.len(),
        ap_lapic_ids
    );

    // Identity-map 0x6000..0x8000 so the trampoline can run after enabling paging.
    {
        use crate::vm::GenericPageTable;
        let mut pt = crate::vm::PageTable::from_current();
        let flags = MMUFlags::READ
            | MMUFlags::WRITE
            | MMUFlags::EXECUTE
            | MMUFlags::from_bits_truncate(CachePolicy::Cached as usize);
        if let Err(e) = pt.map_cont(TRAMPOLINE_PADDR, PAGE_SIZE * 2, TRAMPOLINE_PADDR, flags) {
            crate::klog_warn!(
                "[smp] identity-map 0x6000 failed: {:?} — AP may triple-fault",
                e
            );
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
            crate::klog_warn!(
                "[smp] Too many APs (max {}), skipping LAPIC {}",
                MAX_APS,
                lapic_id
            );
            break;
        }

        // Assign this AP its dense logical id *before* it starts running, so the
        // very first lock it takes resolves to the right per-CPU slot.
        let logical = register_cpu(lapic_id as u8);

        let stack_top = match alloc_ap_stack() {
            Some(top) => top,
            None => {
                crate::klog_warn!("[smp] failed to allocate stack for LAPIC {}", lapic_id);
                break;
            }
        };
        unsafe { (phys_to_virt(SLOT_STACK) as *mut usize).write_volatile(stack_top) };
        unsafe {
            (phys_to_virt(SLOT_LOGICAL) as *mut u8).write_volatile(logical as u8);
        }

        crate::klog_info!(
            "[smp] Starting AP LAPIC {} (logical CPU {}) stack={:#x}",
            lapic_id,
            logical,
            stack_top
        );

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

    warn!(
        "[smp] done — {}/{} AP(s) online",
        started,
        ap_lapic_ids.len()
    );
}

/// Called by each AP from `secondary_init()` to announce it is running.
pub fn ap_signal_online() {
    // Mark this AP online so cross-CPU TLB shootdowns wait for it (and only
    // CPUs that actually came up — partial bring-up must not hang the waiter).
    crate::common::ipi::mark_cpu_online(super::cpu::cpu_id() as usize);
    AP_ONLINE_COUNT.fetch_add(1, Ordering::Release);
}

/// Dense logical CPU id written by the BSP for the AP currently being started.
pub fn ap_trampoline_logical_id() -> u8 {
    unsafe { (phys_to_virt(SLOT_LOGICAL) as *const u8).read_volatile() }
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
