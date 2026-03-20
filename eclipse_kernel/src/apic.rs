//! Local APIC Driver
//!
//! Handles per-CPU interrupt controller configuration and signaling.
//! Supports both xAPIC (MMIO) and x2APIC (MSR) modes.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

// LAPIC Register Offsets
const LAPIC_REG_ID: u32 = 0x20;
const LAPIC_REG_VER: u32 = 0x30;
const LAPIC_REG_TPR: u32 = 0x80;
const LAPIC_REG_EOI: u32 = 0x0B0;
const LAPIC_REG_LDR: u32 = 0x0D0;
const LAPIC_REG_DFR: u32 = 0x0E0;
const LAPIC_REG_SVR: u32 = 0x0F0;
const LAPIC_REG_ESR: u32 = 0x280;
const LAPIC_REG_ICRL: u32 = 0x300;
const LAPIC_REG_ICRH: u32 = 0x310;
const LAPIC_REG_LVT_TIMER: u32 = 0x320;
const LAPIC_REG_LVT_PERF: u32 = 0x340;
const LAPIC_REG_LVT_LINT0: u32 = 0x350;
const LAPIC_REG_LVT_LINT1: u32 = 0x360;
const LAPIC_REG_LVT_ERR: u32 = 0x370;
const LAPIC_REG_TMRINIT: u32 = 0x380;
const LAPIC_REG_TMRCURR: u32 = 0x390;
const LAPIC_REG_TMRDIV: u32 = 0x3E0;

/// x2APIC ICR MSR (replaces the two 32-bit xAPIC ICR registers at 0x300/0x310)
const X2APIC_MSR_ICR: u32 = 0x830;
/// IA32_APIC_BASE MSR number
const MSR_APIC_BASE: u32 = 0x1B;
/// Bit 10 of IA32_APIC_BASE: x2APIC mode enable
const APIC_BASE_X2APIC: u64 = 1 << 10;

/// TSC-Deadline Mode MSR
const MSR_IA32_TSC_DEADLINE: u32 = 0x6E0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ApicTimerMode {
    Periodic,
    OneShot,
    TSCDeadline,
}

static TIMER_MODE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0); // 0=Periodic, 1=OneShot, 2=TSCDeadline

static mut LAPIC_BASE: u64 = 0;
/// True when the CPU is running in x2APIC mode (MSR-based register access)
static IS_X2APIC: AtomicBool = AtomicBool::new(false);
/// True on the Bootstrap Processor (IA32_APIC_BASE MSR bit 8).
/// Set once during apic::init() on the BSP and never changed.
static IS_BSP: AtomicBool = AtomicBool::new(false);

/// Fallback LAPIC timer count per 1ms when calibration cannot measure a
/// realistic value. Derived from a 100 MHz bus / 16 (divider) = 6.25 MHz,
/// giving 6250 counts per millisecond.
const DEFAULT_LAPIC_COUNT_PER_MS: u32 = 6250;

/// Calibrated LAPIC timer counts per 1ms (set by BSP before APs start)
static LAPIC_TIMER_COUNT_1MS: AtomicU32 = AtomicU32::new(DEFAULT_LAPIC_COUNT_PER_MS);

/// Calibrate the Local APIC timer using the PIT tick counter as reference.
/// Must be called on the BSP after interrupts::init() and apic::init().
pub fn calibrate_timer() {
    unsafe {
        if LAPIC_BASE == 0 && !IS_X2APIC.load(Ordering::Relaxed) { return; }

        // Use divider /16
        write_reg(LAPIC_REG_TMRDIV, 0x03);
        // Mask the timer and run in one-shot mode to measure frequency
        write_reg(LAPIC_REG_LVT_TIMER, 0x0001_0000); // masked
        write_reg(LAPIC_REG_TMRINIT, 0xFFFF_FFFF);

        // Wait 10 PIT ticks (≈10 ms at 1000 Hz); interrupts must already be on
        core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
        let start_tsc = crate::cpu::rdtsc();
        let start = crate::interrupts::ticks();
        while crate::interrupts::ticks() < start + 10 {
            crate::cpu::pause();
        }
        let end_tsc = crate::cpu::rdtsc();

        let remaining = read_reg(LAPIC_REG_TMRCURR);
        let elapsed = 0xFFFF_FFFFu32.wrapping_sub(remaining);
        let count_per_ms = if elapsed > 10 { elapsed / 10 } else { DEFAULT_LAPIC_COUNT_PER_MS };

        let tsc_elapsed = end_tsc - start_tsc;
        let tsc_per_ms = tsc_elapsed / 10;
        let tsc_counts_per_us = tsc_per_ms / 1000;

        if tsc_counts_per_us > 0 {
            crate::cpu::update_tsc_frequency(tsc_counts_per_us);
            crate::serial::serial_printf(format_args!(
                "[APIC] TSC calibrated: {} counts/us\n", tsc_counts_per_us
            ));
        }

        // Stop the one-shot timer
        write_reg(LAPIC_REG_TMRINIT, 0);

        LAPIC_TIMER_COUNT_1MS.store(count_per_ms, Ordering::SeqCst);
        crate::serial::serial_printf(format_args!(
            "[APIC] Timer calibrated: {} counts/ms\n", count_per_ms
        ));
    }
}

/// Start the Local APIC timer on the current CPU choosing the best mode.
/// `vector` is the IDT vector that will fire.
pub fn init_timer(vector: u8) {
    let count = LAPIC_TIMER_COUNT_1MS.load(Ordering::Relaxed);
    let has_tsc_deadline = crate::cpu::has_tsc_deadline();
    
    unsafe {
        write_reg(LAPIC_REG_TMRDIV, 0x03); // divide by 16

        if has_tsc_deadline {
            TIMER_MODE.store(2, Ordering::SeqCst);
            // TSC-Deadline mode is bit 18 set, bit 17 clear
            write_reg(LAPIC_REG_LVT_TIMER, (2 << 17) | (vector as u32));
            
            // Trigger first deadline (1ms from now)
            let tsc_per_ms = crate::cpu::get_tsc_frequency() * 1000;
            set_timer_tsc(crate::cpu::rdtsc() + tsc_per_ms);
        } else {
            // Use One-shot (bit 17=0, 18=0) or Periodic (bit 17=1)
            // For now let's stick to One-shot if we want robustness, or Periodic if we want simplicity.
            // Redox recommends One-shot.
            TIMER_MODE.store(1, Ordering::SeqCst);
            write_reg(LAPIC_REG_LVT_TIMER, (vector as u32)); // One-shot
            write_reg(LAPIC_REG_TMRINIT, count);
        }
    }
}

pub fn get_timer_mode() -> ApicTimerMode {
    match TIMER_MODE.load(Ordering::Relaxed) {
        1 => ApicTimerMode::OneShot,
        2 => ApicTimerMode::TSCDeadline,
        _ => ApicTimerMode::Periodic,
    }
}

pub fn set_timer_oneshot(count: u32) {
    unsafe {
        write_reg(LAPIC_REG_TMRINIT, count);
    }
}

pub fn set_timer_tsc(deadline: u64) {
    unsafe {
        let low = (deadline & 0xFFFFFFFF) as u32;
        let high = (deadline >> 32) as u32;
        core::arch::asm!("wrmsr", in("ecx") MSR_IA32_TSC_DEADLINE, in("eax") low, in("edx") high,
            options(nomem, nostack, preserves_flags));
    }
}

pub fn set_timer_ms(ms: u64) {
    let mode = get_timer_mode();
    match mode {
        ApicTimerMode::TSCDeadline => {
            let tsc_per_ms = crate::cpu::get_tsc_frequency() * 1000;
            set_timer_tsc(crate::cpu::rdtsc() + ms * tsc_per_ms);
        }
        ApicTimerMode::OneShot => {
            let count_per_ms = LAPIC_TIMER_COUNT_1MS.load(Ordering::Relaxed);
            set_timer_oneshot((ms as u32).saturating_mul(count_per_ms));
        }
        ApicTimerMode::Periodic => {
            // Periodic mode cannot be easily adjusted for a single shot
        }
    }
}


pub fn get_timer_count_1ms() -> u32 {
    LAPIC_TIMER_COUNT_1MS.load(Ordering::Relaxed)
}

/// Returns the estimated time remaining until the next timer interrupt in MICROSECONDS.
pub fn get_timer_remaining_us() -> u64 {
    let mode = get_timer_mode();
    match mode {
        ApicTimerMode::TSCDeadline => {
            unsafe {
                let low: u32;
                let high: u32;
                // Read the current deadline from the MSR
                core::arch::asm!("rdmsr", in("ecx") MSR_IA32_TSC_DEADLINE, out("eax") low, out("edx") high,
                    options(nomem, nostack, preserves_flags));
                let deadline = (high as u64) << 32 | (low as u64);
                let now = crate::cpu::rdtsc();
                if deadline <= now {
                    return 0;
                }
                let tsc_freq = crate::cpu::get_tsc_frequency(); // counts per us
                (deadline - now) / tsc_freq.max(1)
            }
        }
        ApicTimerMode::OneShot => {
            unsafe {
                let current_count = read_reg(LAPIC_REG_TMRCURR) as u64;
                let count_per_ms = LAPIC_TIMER_COUNT_1MS.load(Ordering::Relaxed) as u64;
                if count_per_ms == 0 { return 1000; }
                (current_count * 1000) / count_per_ms
            }
        }
        ApicTimerMode::Periodic => {
            // Hard to tell exactly without more state, return 1ms as safe default
            1000
        }
    }
}

pub fn get_timer_count_1ms_original() -> u32 {
    LAPIC_TIMER_COUNT_1MS.load(Ordering::Relaxed)
}

/// Initialize Local APIC for the current CPU.
/// Detects x2APIC mode and uses MSR-based access when active.
pub fn init() {
    let lapic_phys = crate::acpi::get_info().lapic_addr;
    unsafe {
        let low: u32;
        let high: u32;
        core::arch::asm!("rdmsr", in("ecx") MSR_APIC_BASE, out("eax") low, out("edx") high,
            options(nomem, nostack, preserves_flags));
        let apic_base_msr = (high as u64) << 32 | (low as u64);
        let x2apic = (apic_base_msr & APIC_BASE_X2APIC) != 0;
        crate::serial::serial_printf(format_args!("[APIC] IA32_APIC_BASE MSR: {:#x} (x2APIC={})\n", apic_base_msr, x2apic));

        if x2apic {
            IS_X2APIC.store(true, Ordering::SeqCst);
            crate::serial::serial_print("[APIC] Using x2APIC mode (MSR-based access)\n");
        } else {
            IS_X2APIC.store(false, Ordering::SeqCst);
            // Map LAPIC MMIO only in xAPIC mode
            if LAPIC_BASE == 0 {
                LAPIC_BASE = crate::memory::map_mmio_range(lapic_phys, 4096);
            }
        }

        // 1. Enable LAPIC by setting bit 8 in Spurious Interrupt Vector Register
        // Also set the spurious interrupt vector to 0xFF (reserved)
        write_reg(LAPIC_REG_SVR, read_reg(LAPIC_REG_SVR) | 0x100 | 0xFF);

        let is_bsp = (low & (1 << 8)) != 0;

        // Record whether this is the BSP so that interrupt handlers can use
        // is_bsp() instead of relying on `cpu_id == 0`, which fails on systems
        // where the Bootstrap Processor has a non-zero LAPIC ID.
        if is_bsp {
            IS_BSP.store(true, Ordering::SeqCst);
        }

        // 1.5 Ensure LINT0 is configured as ExtINT (Delivery mode 7) and Unmasked.
        // This is crucial on real hardware so that legacy 8259 PIC interrupts
        // (like IRQ0 PIT) can still reach the BSP.
        // In x2APIC mode this write goes through MSR 0x835.
        // CRITICAL: We MUST only do this on the BSP! APs should NOT receive PIC interrupts!
        if is_bsp {
            write_reg(LAPIC_REG_LVT_LINT0, 0x00000700);
        } else {
            // Mask LINT0 on APs
            write_reg(LAPIC_REG_LVT_LINT0, 0x00010000);
        }

        // 2. Clear Task Priority Register to allow all interrupts
        write_reg(LAPIC_REG_TPR, 0);

        // 3. Signal End of Interrupt just in case
        eoi();

        crate::serial::serial_printf(format_args!("[APIC] LAPIC initialized on CPU (ID {})\n", get_id()));
        crate::serial::serial_print("[APIC] init() returning...\n");
    }
}

/// Send End of Interrupt signal
pub fn eoi() {
    unsafe {
        write_reg(LAPIC_REG_EOI, 0);
    }
}

/// Returns true if the calling CPU is the Bootstrap Processor.
/// Determined once during apic::init() from IA32_APIC_BASE MSR bit 8.
/// More reliable than checking `cpu_id == 0` because the BSP is not
/// guaranteed to have APIC ID 0 on all real-hardware platforms.
#[inline]
pub fn is_bsp() -> bool {
    IS_BSP.load(Ordering::Relaxed)
}

/// Get the Local APIC ID of the current CPU (32-bit to support x2APIC IDs)
pub fn get_id() -> u32 {
    unsafe {
        if IS_X2APIC.load(Ordering::Relaxed) {
            // x2APIC: MSR 0x802 returns the full 32-bit x2APIC ID in EAX
            let id: u32;
            let _high: u32;
            core::arch::asm!("rdmsr", in("ecx") 0x802u32, out("eax") id, out("edx") _high,
                options(nomem, nostack, preserves_flags));
            id
        } else {
            // xAPIC: bits 31:24 of LAPIC ID register
            (read_reg(LAPIC_REG_ID) >> 24) & 0xFF
        }
    }
}

/// Write to an APIC register.
/// In xAPIC mode: MMIO write to LAPIC_BASE + offset.
/// In x2APIC mode: MSR write to (0x800 + offset/0x10).
/// NOTE: The ICR in x2APIC is a special 64-bit MSR (0x830); callers that need
/// to set both ICR high and low together must use write_icr64() instead.
unsafe fn write_reg(offset: u32, value: u32) {
    if IS_X2APIC.load(Ordering::Relaxed) {
        // x2APIC: MSR address = 0x800 + (xAPIC_offset >> 4)
        let msr = 0x800u32 + (offset >> 4);
        core::arch::asm!("wrmsr", in("ecx") msr, in("eax") value, in("edx") 0u32,
            options(nomem, nostack, preserves_flags));
    } else {
        if LAPIC_BASE == 0 { return; }
        let ptr = (LAPIC_BASE + offset as u64) as *mut u32;
        write_volatile(ptr, value);
    }
}

/// Read from an APIC register.
/// In xAPIC mode: MMIO read from LAPIC_BASE + offset.
/// In x2APIC mode: MSR read from (0x800 + offset/0x10).
unsafe fn read_reg(offset: u32) -> u32 {
    if IS_X2APIC.load(Ordering::Relaxed) {
        let msr = 0x800u32 + (offset >> 4);
        let low: u32;
        let _high: u32;
        core::arch::asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") _high,
            options(nomem, nostack, preserves_flags));
        low
    } else {
        if LAPIC_BASE == 0 { return 0; }
        let ptr = (LAPIC_BASE + offset as u64) as *const u32;
        read_volatile(ptr)
    }
}

/// Write a 64-bit value to the x2APIC ICR MSR (0x830).
/// Only valid in x2APIC mode. High 32 bits = destination ID, low 32 bits = ICR_LO fields.
unsafe fn write_icr64(icr_high: u32, icr_low: u32) {
    core::arch::asm!("wrmsr",
        in("ecx") X2APIC_MSR_ICR,
        in("eax") icr_low,
        in("edx") icr_high,
        options(nomem, nostack, preserves_flags));
}

/// Send specific IPI to a target APIC ID.
/// apic_id is u32 to support x2APIC IDs > 255.
pub fn send_ipi_exact(apic_id: u32, vector: u8, delivery_mode: u8, assert: bool, level_trigger: bool) {
    unsafe {
        let x2apic = IS_X2APIC.load(Ordering::Relaxed);

        // Clear Error Status Register before sending.
        // In x2APIC mode, ESR is read-only; clear_esr() handles this correctly.
        clear_esr();

        let mut icrl = (vector as u32) | ((delivery_mode as u32) << 8);
        if assert { icrl |= 1 << 14; }
        if level_trigger { icrl |= 1 << 15; }

        // Ensure SIPI always has Assert=0 (bit 14=0) regardless of the flag if delivery_mode is 6
        if delivery_mode == 6 {
            icrl &= !(1 << 14);
        }

        if x2apic {
            // x2APIC: single 64-bit MSR write. No delivery pending polling needed (Reserved).
            write_icr64(apic_id, icrl);
        } else {
            // xAPIC: write high register first, then low to trigger delivery
            wait_icr_idle();
            write_reg(LAPIC_REG_ICRH, apic_id << 24);
            write_reg(LAPIC_REG_ICRL, icrl);
            wait_icr_idle();

            // Check for delivery errors
            let esr = read_esr();
            if esr != 0 {
                crate::serial::serial_printf(format_args!("[APIC] ERROR: ESR after IPI to {}: {:#x}\n", apic_id, esr));
            }
        }
    }
}

pub fn broadcast_init() {
    unsafe {
        clear_esr();
        // Shorthand 3 (All excluding self), Delivery 5 (INIT), Assert 1, Trigger 0 (Edge)
        let icrl = (3 << 18) | (1 << 14) | (5 << 8);

        if IS_X2APIC.load(Ordering::Relaxed) {
            write_icr64(0, icrl);
        } else {
            wait_icr_idle();
            write_reg(LAPIC_REG_ICRH, 0);
            write_reg(LAPIC_REG_ICRL, icrl);
            wait_icr_idle();
            let esr = read_esr();
            if esr != 0 {
                crate::serial::serial_printf(format_args!("[APIC] ERROR: ESR after broadcast INIT: {:#x}\n", esr));
            }
        }
    }
}

pub fn broadcast_sipi(vector: u8) {
    unsafe {
        clear_esr();
        // Shorthand 3 (All excluding self), Delivery 6 (SIPI), Assert 0, Trigger 0 (Edge)
        let icrl = (3 << 18) | (0 << 14) | (6 << 8) | (vector as u32);

        if IS_X2APIC.load(Ordering::Relaxed) {
            // x2APIC: SIPI delivery via ICR64. Note: Intel SDM Vol 3A §10.12.9 marks
            // SIPI delivery mode as "reserved" in x2APIC mode. However, many real
            // platforms (including QEMU and several x86 server boards) handle this
            // correctly via the ICR64 MSR. If SIPI is not delivered, the AP times out
            // and the caller logs a warning. Platforms that require a different wakeup
            // mechanism (e.g. ACPI Mailbox / Spin Table) would need additional support.
            write_icr64(0, icrl);
        } else {
            wait_icr_idle();
            write_reg(LAPIC_REG_ICRH, 0);
            write_reg(LAPIC_REG_ICRL, icrl);
            wait_icr_idle();
            let esr = read_esr();
            if esr != 0 {
                crate::serial::serial_printf(format_args!("[APIC] ERROR: ESR after broadcast SIPI: {:#x}\n", esr));
            }
        }
    }
}

/// Send a TLB shootdown IPI to all CPUs except the calling CPU.
pub fn send_tlb_shootdown_ipi() {
    unsafe {
        if LAPIC_BASE == 0 && !IS_X2APIC.load(Ordering::Relaxed) { return; }
        clear_esr();

        let vector = crate::interrupts::TLB_SHOOTDOWN_VECTOR as u32;
        // ICR: destination shorthand = All excluding self (3 << 18),
        //      delivery mode = Fixed (0 << 8), assert (1 << 14), edge trigger.
        let icrl = (3 << 18) | (1 << 14) | vector;

        if IS_X2APIC.load(Ordering::Relaxed) {
            write_icr64(0, icrl);
        } else {
            write_reg(LAPIC_REG_ICRH, 0);
            write_reg(LAPIC_REG_ICRL, icrl);
            wait_icr_idle();
        }
    }
}

/// Send a reschedule IPI to all other CPUs.
/// Called by the scheduler when a new process is ready, to notify idle cores.
pub fn broadcast_reschedule_ipi() {
    unsafe {
        if LAPIC_BASE == 0 && !IS_X2APIC.load(Ordering::Relaxed) { return; }
        clear_esr();

        let vector = crate::interrupts::RESCHEDULE_IPI_VECTOR as u32;
        // ICR: destination shorthand = All excluding self (3 << 18),
        //      delivery mode = Fixed (0 << 8), assert (1 << 14), edge trigger.
        let icrl = (3 << 18) | (1 << 14) | vector;

        if IS_X2APIC.load(Ordering::Relaxed) {
            write_icr64(0, icrl);
        } else {
            write_reg(LAPIC_REG_ICRH, 0);
            write_reg(LAPIC_REG_ICRL, icrl);
            wait_icr_idle();
        }
    }
}

/// Send a reschedule IPI to a specific CPU.
pub fn send_reschedule_ipi(target_apic_id: u32) {
    let vector = crate::interrupts::RESCHEDULE_IPI_VECTOR;
    send_ipi_exact(target_apic_id, vector, 0, true, false);
}

unsafe fn clear_esr() {
    if !IS_X2APIC.load(Ordering::Relaxed) {
        // xAPIC: Must write to ESR to clear/update it before read
        write_reg(LAPIC_REG_ESR, 0);
    }
}

unsafe fn read_esr() -> u32 {
    if IS_X2APIC.load(Ordering::Relaxed) {
        // x2APIC: ESR is read-only (MSR 0x828)
        read_reg(LAPIC_REG_ESR)
    } else {
        // xAPIC: Must write before read to update
        write_reg(LAPIC_REG_ESR, 0);
        read_reg(LAPIC_REG_ESR)
    }
}

/// Wait until the Delivery Status bit in the ICR is cleared or until a timeout.
unsafe fn wait_icr_idle() {
    if IS_X2APIC.load(Ordering::Relaxed) {
        // x2APIC: Bit 12 (Delivery Status) is reserved and undefined.
        // Software does not need to poll this bit.
        return;
    }

    let mut timeout = 100_000;
    while read_reg(LAPIC_REG_ICRL) & (1 << 12) != 0 {
        crate::cpu::pause();
        timeout -= 1;
        if timeout == 0 {
            crate::serial::serial_print("[APIC] WARNING: ICR busy timeout!\n");
            break;
        }
    }
}
