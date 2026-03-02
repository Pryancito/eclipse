//! Local APIC Driver
//!
//! Handles per-CPU interrupt controller configuration and signaling.

use crate::memory::phys_to_virt;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU32, Ordering};

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

static mut LAPIC_BASE: u64 = 0;

/// Calibrated LAPIC timer counts per 1ms (set by BSP before APs start)
static LAPIC_TIMER_COUNT_1MS: AtomicU32 = AtomicU32::new(6250);

/// Calibrate the Local APIC timer using the PIT tick counter as reference.
/// Must be called on the BSP after interrupts::init() and apic::init().
pub fn calibrate_timer() {
    unsafe {
        if LAPIC_BASE == 0 { return; }

        // Use divider /16
        write_reg(LAPIC_REG_TMRDIV, 0x03);
        // Mask the timer and run in one-shot mode to measure frequency
        write_reg(LAPIC_REG_LVT_TIMER, 0x0001_0000); // masked
        write_reg(LAPIC_REG_TMRINIT, 0xFFFF_FFFF);

        // Wait 10 PIT ticks (≈10 ms at 1000 Hz); interrupts must already be on
        core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
        let start = crate::interrupts::ticks();
        while crate::interrupts::ticks() < start + 10 {
            crate::cpu::pause();
        }

        let remaining = read_reg(LAPIC_REG_TMRCURR);
        let elapsed = 0xFFFF_FFFFu32.wrapping_sub(remaining);
        let count_per_ms = if elapsed > 10 { elapsed / 10 } else { 6250 };

        // Stop the one-shot timer
        write_reg(LAPIC_REG_TMRINIT, 0);

        LAPIC_TIMER_COUNT_1MS.store(count_per_ms, Ordering::SeqCst);
        crate::serial::serial_printf(format_args!(
            "[APIC] Timer calibrated: {} counts/ms\n", count_per_ms
        ));
    }
}

/// Start the Local APIC periodic timer on the current CPU.
/// `vector` is the IDT vector that will fire every ~1 ms.
/// calibrate_timer() must have been called on the BSP beforehand.
pub fn init_timer(vector: u8) {
    let count = LAPIC_TIMER_COUNT_1MS.load(Ordering::Relaxed);
    unsafe {
        write_reg(LAPIC_REG_TMRDIV, 0x03);          // divide by 16
        // Periodic mode (bit 17), unmasked, vector
        write_reg(LAPIC_REG_LVT_TIMER, (1 << 17) | (vector as u32));
        write_reg(LAPIC_REG_TMRINIT, count);         // ~1 ms period
    }
}

/// Initialize Local APIC for the current CPU
pub fn init() {
    let lapic_phys = crate::acpi::get_info().lapic_addr;
    unsafe {
        LAPIC_BASE = phys_to_virt(lapic_phys);
        
        let low: u32;
        let high: u32;
        core::arch::asm!("rdmsr", in("ecx") 0x1B, out("eax") low, out("edx") high);
        let apic_base_msr = (high as u64) << 32 | (low as u64);
        crate::serial::serial_printf(format_args!("[APIC] IA32_APIC_BASE MSR: {:#x}\n", apic_base_msr));

        // 1. Enable LAPIC by setting bit 8 in Spurious Interrupt Vector Register
        // Also set the spurious interrupt vector to 0xFF (reserved)
        write_reg(LAPIC_REG_SVR, read_reg(LAPIC_REG_SVR) | 0x100 | 0xFF);
        
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

/// Get the Local APIC ID of the current CPU
pub fn get_id() -> u8 {
    unsafe {
        (read_reg(LAPIC_REG_ID) >> 24) as u8
    }
}

/// Write to an APIC register
unsafe fn write_reg(offset: u32, value: u32) {
    if LAPIC_BASE == 0 { return; }
    let ptr = (LAPIC_BASE + offset as u64) as *mut u32;
    write_volatile(ptr, value);
}

/// Read from an APIC register
unsafe fn read_reg(offset: u32) -> u32 {
    if LAPIC_BASE == 0 { return 0; }
    let ptr = (LAPIC_BASE + offset as u64) as *const u32;
    read_volatile(ptr)
}

/// Send specific IPI to a target APIC ID
pub fn send_ipi_exact(apic_id: u8, vector: u8, delivery_mode: u8, assert: bool, level_trigger: bool) {
    unsafe {
        // Log APIC mode
        let msr: u64;
        let msr_high: u32;
        let msr_low: u32;
        core::arch::asm!("rdmsr", in("ecx") 0x1B, out("eax") msr_low, out("edx") msr_high);
        msr = (msr_low as u64) | ((msr_high as u64) << 32);
        let x2apic = (msr & (1 << 10)) != 0;
        let enabled = (msr & (1 << 11)) != 0;
        crate::serial::serial_printf(format_args!("[APIC] Base MSR: {:#x} (Enabled={}, x2APIC={})\n", msr, enabled, x2apic));

        // Clear Error Status Register before sending
        clear_esr();
        
        while read_reg(LAPIC_REG_ICRL) & (1 << 12) != 0 { crate::cpu::pause(); }
        
        let mut icrl = (vector as u32) | ((delivery_mode as u32) << 8);
        if assert { icrl |= 1 << 14; }
        if level_trigger { icrl |= 1 << 15; }
        
        // Ensure SIPI always has Assert=0 (bit 14=0) regardless of the flag if delivery_mode is 6
        if delivery_mode == 6 {
            icrl &= !(1 << 14);
        }
        
        crate::serial::serial_printf(format_args!("[APIC] IPI to {}: Vector={:#x}, Mode={}, Assert={}, Level={} -> ICR={:#010x}:{:08x}\n", 
            apic_id, vector, delivery_mode, assert, level_trigger, (apic_id as u32) << 24, icrl));

        // Destination shorthand 0 (No shorthand)
        write_reg(LAPIC_REG_ICRH, (apic_id as u32) << 24);
        write_reg(LAPIC_REG_ICRL, icrl);
        
        while read_reg(LAPIC_REG_ICRL) & (1 << 12) != 0 { crate::cpu::pause(); }
        
        // Check for delivery errors (Read ESR twice as per some BIOS implementations)
        let _ = read_esr();
        let esr = read_esr();
        if esr != 0 {
            crate::serial::serial_printf(format_args!("[APIC] ERROR: ESR after IPI to {}: {:#x}\n", apic_id, esr));
        }
    }
}

pub fn broadcast_init() {
    unsafe {
        clear_esr();
        while read_reg(LAPIC_REG_ICRL) & (1 << 12) != 0 { crate::cpu::pause(); }
        
        // Shorthand 3 (All excluding self), Delivery 5 (INIT), Assert 1, Trigger 0 (Edge)
        let icrl = (3 << 18) | (1 << 14) | (5 << 8);
        write_reg(LAPIC_REG_ICRH, 0);
        write_reg(LAPIC_REG_ICRL, icrl);
        
        while read_reg(LAPIC_REG_ICRL) & (1 << 12) != 0 { crate::cpu::pause(); }
        let esr = read_esr();
        if esr != 0 {
            crate::serial::serial_printf(format_args!("[APIC] ERROR: ESR after broadcast INIT: {:#x}\n", esr));
        }
    }
}

pub fn broadcast_sipi(vector: u8) {
    unsafe {
        clear_esr();
        while read_reg(LAPIC_REG_ICRL) & (1 << 12) != 0 { crate::cpu::pause(); }
        
        // Shorthand 3 (All excluding self), Delivery 6 (SIPI), Assert 0, Trigger 0 (Edge)
        let icrl = (3 << 18) | (0 << 14) | (6 << 8) | (vector as u32);
        write_reg(LAPIC_REG_ICRH, 0);
        write_reg(LAPIC_REG_ICRL, icrl);
        
        while read_reg(LAPIC_REG_ICRL) & (1 << 12) != 0 { crate::cpu::pause(); }
        let esr = read_esr();
        if esr != 0 {
            crate::serial::serial_printf(format_args!("[APIC] ERROR: ESR after broadcast SIPI: {:#x}\n", esr));
        }
    }
}

unsafe fn clear_esr() {
    write_reg(LAPIC_REG_ESR, 0);
}

unsafe fn read_esr() -> u32 {
    write_reg(LAPIC_REG_ESR, 0); // Must write before read to update
    read_reg(LAPIC_REG_ESR)
}
