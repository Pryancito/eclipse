//! CPU information.

use raw_cpuid::CpuId;

/// PIT (8254) channel 0 reference frequency in Hz. Fixed by the spec — every
/// x86 PC (and QEMU) clocks the PIT at 1.193182 MHz.
const PIT_REF_HZ: u64 = 1_193_182;

/// Measure the TSC frequency by counting TSC cycles while the PIT channel 2
/// counts down a known number of ticks. Channel 2 is the speaker channel and
/// is not used by the kernel timer (we use the LAPIC timer), so there is no
/// conflict with the system tick.
///
/// Returns Hz, not MHz: truncating to MHz before building the ns multiplier
/// stretches every timeout (including the synthetic 60 Hz vblank) by the
/// discarded remainder.
///
/// SAFETY: touches the legacy 8254/0x61 ports; must only be called from a
/// single core early in boot, before any other code uses the PIT.
unsafe fn calibrate_tsc_hz_via_pit() -> Option<u64> {
    use x86_64::instructions::port::Port;

    // ~54.9 ms gate window (65535 / 1.193182 MHz). Long enough that IRQ
    // jitter is irrelevant; short enough that a slow VM still finishes.
    const PIT_COUNT: u16 = 0xFFFF;

    let mut gate = Port::<u8>::new(0x61);
    let mut cmd = Port::<u8>::new(0x43);
    let mut data = Port::<u8>::new(0x42);

    let saved = gate.read();
    // Speaker off (bit 1 = 0), gate low (bit 0 = 0).
    gate.write(saved & 0xFC);

    // Channel 2, access lo+hi, mode 0 (interrupt on terminal count), binary.
    cmd.write(0b1011_0000);
    data.write((PIT_COUNT & 0xFF) as u8);
    data.write((PIT_COUNT >> 8) as u8);

    // Raise gate → counter starts decrementing on the next PIT tick.
    let t0 = core::arch::x86_64::_rdtsc();
    gate.write((saved & 0xFC) | 0x01);

    // Mode 0: OUT2 (bit 5 of 0x61) stays low until the counter hits zero,
    // then goes high. Many real laptops/firmware leave PIT channel 2 (the
    // speaker gate) dead: the old 2e9 `spin_loop` cap froze boot at 51%
    // for tens of seconds. Bound the wait with TSC (~100 ms at 2 GHz)
    // so a missing PIT falls through to CPUID immediately.
    const TSC_TIMEOUT: u64 = 200_000_000;
    loop {
        if gate.read() & 0x20 != 0 {
            break;
        }
        if core::arch::x86_64::_rdtsc().wrapping_sub(t0) > TSC_TIMEOUT {
            gate.write(saved);
            return None;
        }
        core::hint::spin_loop();
    }
    let t1 = core::arch::x86_64::_rdtsc();
    gate.write(saved);

    let cycles = t1.saturating_sub(t0);
    // hz = cycles * PIT_REF_HZ / PIT_COUNT
    let hz = cycles.saturating_mul(PIT_REF_HZ) / PIT_COUNT as u64;
    if (100_000_000..=20_000_000_000).contains(&hz) {
        Some(hz)
    } else {
        None
    }
}

/// TSC frequency in Hz. CPUID first (instant), then a short PIT measure;
/// never rounded to MHz. Do not call this from a path that must not stall:
/// PIT channel 2 is missing on some real machines (see calibrator timeout).
pub fn tsc_hz() -> u64 {
    static TSC_HZ: spin::Once<u64> = spin::Once::new();
    *TSC_HZ.call_once(|| {
        if let Some(hz) = tsc_hz_from_cpuid() {
            return hz;
        }
        if let Some(hz) = unsafe { calibrate_tsc_hz_via_pit() } {
            return hz;
        }
        2_000_000_000
    })
}

/// CPUID.15H crystal × ratio, then leaf 16H base MHz. Instant, no I/O.
fn tsc_hz_from_cpuid() -> Option<u64> {
    use core::arch::x86_64::__cpuid;
    let max = __cpuid(0).eax;
    if max >= 0x15 {
        let r = __cpuid(0x15);
        if r.eax != 0 && r.ebx != 0 && r.ecx != 0 {
            let hz = (r.ecx as u64).saturating_mul(r.ebx as u64) / r.eax as u64;
            if (100_000_000..=20_000_000_000).contains(&hz) {
                return Some(hz);
            }
        }
    }
    CpuId::new()
        .get_processor_frequency_info()
        .map(|info| info.processor_base_frequency())
        .filter(|&f| f >= 100)
        .map(|mhz| mhz as u64 * 1_000_000)
}

/// Flush GPUs and block devices before a reboot or power-off. A warm reset
/// does not power-cycle PCIe, so a GPU with a live GSP-RM / locked WPR2 would
/// otherwise stall firmware POST; NVMe wants CC.SHN so DRAM-less SSDs persist
/// their FTL maps.
fn quiesce_devices() {
    for d in crate::drivers::all_drm().as_vec().iter() {
        let _ = d.quiesce_for_reboot();
    }
    for d in crate::drivers::all_block().as_vec().iter() {
        d.quiesce_for_reboot();
    }
}

hal_fn_impl! {
    impl mod crate::hal_fn::cpu {
        fn cpu_id() -> u8 {
            CpuId::new()
                .get_feature_info()
                .unwrap()
                .initial_local_apic_id()
        }

        fn cpu_frequency() -> u16 {
            // Prefer measuring the TSC directly against the PIT: on modern
            // Intel the TSC runs at the nominal (non-turbo) frequency,
            // which is NOT the same as CPUID's "base frequency"; on AMD
            // and on QEMU guests CPUID leaf 0x16 is absent altogether.
            // Without calibration the kernel clock ran ~1.8× too fast,
            // collapsing TCP RTOs and inflating uptime on real hardware.
            // Integer MHz is only for callers that still want MHz; the
            // ns multiplier uses [`tsc_hz`] so the discarded remainder
            // does not stretch sleeps and vblank pacing.
            (tsc_hz() / 1_000_000).clamp(100, 20_000) as u16
        }

        fn cpu_brand() -> alloc::string::String {
            use core::arch::x86_64::__cpuid;
            let mut brand = alloc::vec::Vec::new();
            for leaf in 0x80000002..=0x80000004 {
                let res = __cpuid(leaf);
                for reg in &[res.eax, res.ebx, res.ecx, res.edx] {
                    brand.extend_from_slice(&reg.to_le_bytes());
                }
            }
            let brand_str = core::str::from_utf8(&brand)
                .unwrap_or("")
                .trim_matches('\0')
                .trim();
            alloc::string::String::from(brand_str)
        }

        fn cpu_count() -> u8 {
            super::smp::CPU_COUNT.load(core::sync::atomic::Ordering::Acquire) as u8
        }

        fn cpu_temperature_mc() -> Option<i32> {
            super::power::cpu_temperature_mc()
        }

        fn pstate_governor_summary() -> Option<(u32, u8, u8)> {
            super::power::governor_summary()
        }

        fn reset() -> ! {
            info!("resetting...");
            quiesce_devices();
            use zcore_drivers::io::{Io, Pmio};

            // Keyboard controller pulse on the reset line.
            Pmio::<u8>::new(0x64).write(0xFE);
            // PCI reset: 0x06 = system reset, 0x0E = hard reset.
            Pmio::<u8>::new(0xCF9).write(0x06);
            Pmio::<u8>::new(0xCF9).write(0x0E);
            // Triple fault if the chipset ignored the above.
            unsafe {
                let idtr: [u16; 5] = [0, 0, 0, 0, 0];
                core::arch::asm!("lidt [{}]", in(reg) &idtr);
                core::arch::asm!("int3");
            }
            loop {
                super::interrupt::wait_for_interrupt();
            }
        }

        fn power_off() -> ! {
            info!("powering off...");
            quiesce_devices();
            super::drivers::enter_s5();
            // Still alive: park. Do not fall through to a warm reset — the
            // caller asked to power off, and a reboot here made "Apagar" in
            // lunarbar look like a restart.
            loop {
                super::interrupt::wait_for_interrupt();
            }
        }
    }
}
