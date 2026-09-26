//! CPU information.

use raw_cpuid::CpuId;

// What the three calibrators below decide, as opposed to what they read, is
// in `common::tsc_cal`, where a host test can drive it: the reference
// frequencies, the arithmetic of a measurement window, the range of
// frequencies a TSC can have, and the bound on waiting for a counter that may
// have stopped.
use crate::common::tsc_cal::{self, Verdict, WaitEnd, PIT_REF_HZ, PM_TIMER_HZ};

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
    // speaker gate) dead, so the wait must be bounded -- but not by TSC
    // cycles: the frequency is the very thing being measured. The old
    // 200 M-cycle cap was "~100 ms at 2 GHz"; at 3.7 GHz it is 54.1 ms,
    // less than the 54.9 ms the counter takes to reach zero, so every
    // part above ~3.64 GHz timed out one tick short and fell through to
    // the 2 GHz guess -- an i9-10900X ran its clock 1.84x fast for it.
    // A dead PIT is one whose counter does not move: latch and read it
    // back, and give up only when a read shows no progress since the last
    // one across a run of reads long enough for any CPU to be sure.
    const DEAD_READS: u32 = 20_000;
    let mut last_count: u16 = PIT_COUNT;
    let mut stale = 0u32;
    loop {
        if gate.read() & 0x20 != 0 {
            break;
        }
        // Latch channel 2 (counter latch command, channel 2, no mode
        // change) and read the frozen count, low byte then high.
        cmd.write(0b1000_0000);
        let lo = data.read() as u16;
        let hi = data.read() as u16;
        let count = lo | (hi << 8);
        if count == last_count {
            stale += 1;
            if stale >= DEAD_READS {
                gate.write(saved);
                return None;
            }
        } else {
            stale = 0;
            last_count = count;
        }
        core::hint::spin_loop();
    }
    let t1 = core::arch::x86_64::_rdtsc();
    gate.write(saved);

    let cycles = t1.saturating_sub(t0);
    tsc_cal::hz_from_window(cycles, PIT_REF_HZ, PIT_COUNT as u64)
}

/// Locate the FADT's PM timer I/O port by walking RSDP -> XSDT/RSDT -> FACP
/// by hand. Deliberately allocation-free: the `acpi` crate builds a table
/// index on the heap, and the caller may run before the heap is guaranteed.
///
/// Returns the port and whether the counter is 32 bits wide (else 24).
///
/// SAFETY: dereferences firmware tables through the physical map; the RSDP
/// address must be the one firmware handed to the bootloader.
unsafe fn pm_timer_port_from_fadt(rsdp_pa: usize) -> Option<(u16, bool)> {
    use crate::common::acpi_tables as acpi;

    // The only thing in here that touches memory. Everything below is
    // `common::acpi_tables` deciding what the bytes mean, which is where the
    // tests are: this used to be a hand-rolled walk of `read_unaligned` at
    // literal offsets that took the `RSD PTR ` signature on faith and never
    // looked at a checksum or at the `RSDT`/`XSDT` signature -- and what
    // follows an accepted RSDP is a list of physical addresses this function
    // then dereferences, at boot, with no fault handler worth the name.
    let at = |pa: u64, len: usize| -> &'static [u8] {
        core::slice::from_raw_parts(crate::mem::phys_to_virt(pa as usize) as *const u8, len)
    };
    // 36 bytes: an ACPI 2.0 RSDP in full. `rsdp_sdt` checks the signature and
    // both checksums before any of it is believed.
    let sdt_ptr = acpi::rsdp_sdt(at(rsdp_pa as u64, 36))?;
    let len = acpi::table_length(at(sdt_ptr.phys, acpi::SDT_HEADER_LEN))?;
    let sdt = at(sdt_ptr.phys, len);
    if !acpi::table_is(sdt, if sdt_ptr.wide { b"XSDT" } else { b"RSDT" }) {
        crate::klog_warn!("[acpi] the RSDP names something that is not an RSDT/XSDT");
        return None;
    }
    for pa in acpi::sdt_entries(sdt, sdt_ptr.wide) {
        let header = at(pa, acpi::SDT_HEADER_LEN);
        if &header[..4] != b"FACP" {
            continue;
        }
        let len = acpi::table_length(header)?;
        let fadt = at(pa, len);
        if !acpi::table_is(fadt, b"FACP") {
            crate::klog_warn!("[acpi] the FADT does not check out; not calibrating against it");
            return None;
        }
        return acpi::pm_timer_from_fadt(fadt);
    }
    None
}

/// Measure the TSC against the ACPI PM timer: a 3.579545 MHz free-running
/// counter that every ACPI machine has, and the reference Linux checks its
/// own TSC calibration against. Unlike PIT channel 2 it needs no gate and is
/// not left dead by firmware, and unlike CPUID.15H/16H it measures rather
/// than declares.
///
/// SAFETY: reads the PM timer port; single core, boot only.
unsafe fn calibrate_tsc_hz_via_pm_timer(port: u16, wide: bool) -> Option<u64> {
    use x86_64::instructions::port::Port;
    let mask: u32 = if wide { u32::MAX } else { 0x00ff_ffff };
    // 50 ms of PM ticks: long enough that jitter on the port reads is noise.
    const WINDOW_TICKS: u32 = (PM_TIMER_HZ / 20) as u32;
    // Reads, not time: the time is the thing being measured. A PM timer read
    // is an I/O port cycle, on the order of a microsecond, so the window needs
    // some tens of thousands of them and this is two orders of magnitude of
    // headroom over that.
    const WINDOW_READS: u32 = 2_000_000;
    // A dead port reads all ones or all zeros and never moves. Ask for one
    // single tick first so that costs a tenth of a second rather than the
    // whole window's budget.
    const ALIVE_READS: u32 = 100_000;
    let mut pm = Port::<u32>::new(port);
    if matches!(
        tsc_cal::wait_for_ticks(1, mask, ALIVE_READS, || pm.read()),
        WaitEnd::Stalled(_)
    ) {
        return None;
    }
    // And now the window. This loop used to have **no bound at all**: it read
    // the port until the counter had advanced far enough, so a PM timer parked
    // by an SMI or by firmware after the liveness probe spun the boot CPU for
    // ever, with nothing printed and nothing to say why. The liveness probe
    // above bounded its own wait and this one did not, which is the whole of
    // the bug: two waits on the same counter, one of them careful.
    let t0 = core::arch::x86_64::_rdtsc();
    let WaitEnd::Reached(elapsed) =
        tsc_cal::wait_for_ticks(WINDOW_TICKS, mask, WINDOW_READS, || pm.read())
    else {
        return None;
    };
    let cycles = core::arch::x86_64::_rdtsc().saturating_sub(t0);
    tsc_cal::hz_from_window(cycles, PM_TIMER_HZ, elapsed as u64)
}

/// 0 until first asked. The first caller gets a provisional answer -- CPUID
/// or the PIT -- because it may be too early for anything better; see
/// [`recalibrate_tsc_hz`].
static TSC_HZ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// TSC frequency in Hz. CPUID first (instant), then a short PIT measure;
/// never rounded to MHz. Do not call this from a path that must not stall:
/// PIT channel 2 is missing on some real machines (see calibrator timeout).
///
/// This is the provisional value; it is checked against the ACPI PM timer
/// once the firmware tables are reachable (`recalibrate_tsc_hz`).
pub fn tsc_hz() -> u64 {
    use core::sync::atomic::Ordering;
    let hz = TSC_HZ.load(Ordering::Relaxed);
    if hz != 0 {
        return hz;
    }
    let hz = tsc_hz_from_cpuid()
        .or_else(|| unsafe { calibrate_tsc_hz_via_pit() })
        .unwrap_or(2_000_000_000);
    // Another CPU cannot be here yet (boot, BSP only), but a racing first
    // caller on the same path would only store the same measurement class.
    let _ = TSC_HZ.compare_exchange(0, hz, Ordering::Relaxed, Ordering::Relaxed);
    TSC_HZ.load(Ordering::Relaxed)
}

/// Outcome of [`recalibrate_tsc_hz`], for the boot log.
pub enum TscRecalibration {
    /// No PM timer reachable (no RSDP, no FADT, dead port); the provisional
    /// value stands.
    Unavailable,
    /// The PM timer agrees with the provisional value to within tolerance.
    Confirmed { hz: u64, measured: u64 },
    /// The provisional value was wrong and has been replaced.
    Corrected { was: u64, now: u64 },
}

/// Check the provisional TSC frequency against the ACPI PM timer and replace
/// it if they disagree by more than 0.5%.
///
/// The provisional chain is CPUID.15H, CPUID.16H, PIT channel 2, then a
/// 2 GHz guess -- and on real machines every link can be wrong: 16H reports a
/// nominal base clock, PIT channel 2 is frequently left dead by firmware, and
/// the guess is a guess. A machine that fell through to it with a 3.7 GHz
/// TSC ran its clock 1.84x fast: every timeout short, every benchmark and
/// audio stream measured against a clock that was not counting seconds.
///
/// Must run on the BSP before the APs are started and before the LAPIC tick
/// is armed, so both derive their tick counts from the corrected value, and
/// while nothing outside the kernel can observe the clock: the correction
/// keeps `timer_now` in the pure `tsc * mult` form the vDSO relies on, which
/// means the reading itself moves (backwards, if the clock was fast) once,
/// here, at boot.
pub fn recalibrate_tsc_hz() -> TscRecalibration {
    use core::sync::atomic::Ordering;
    let rsdp = super::special::pc_firmware_tables().0 as usize;
    if rsdp == 0 {
        return TscRecalibration::Unavailable;
    }
    let Some((port, wide)) = (unsafe { pm_timer_port_from_fadt(rsdp) }) else {
        return TscRecalibration::Unavailable;
    };
    let Some(measured) = (unsafe { calibrate_tsc_hz_via_pm_timer(port, wide) }) else {
        return TscRecalibration::Unavailable;
    };
    let provisional = tsc_hz();
    if tsc_cal::verdict(provisional, measured) == Verdict::Confirmed {
        return TscRecalibration::Confirmed {
            hz: provisional,
            measured,
        };
    }
    TSC_HZ.store(measured, Ordering::Relaxed);
    super::timer::tsc_hz_changed(measured);
    TscRecalibration::Corrected {
        was: provisional,
        now: measured,
    }
}

/// CPUID.15H crystal × ratio, then leaf 16H base MHz. Instant, no I/O.
fn tsc_hz_from_cpuid() -> Option<u64> {
    use core::arch::x86_64::__cpuid;
    let max = __cpuid(0).eax;
    if max >= 0x15 {
        let r = __cpuid(0x15);
        if let Some(hz) = tsc_cal::tsc_hz_from_15h(r.eax, r.ebx, r.ecx) {
            return Some(hz);
        }
    }
    CpuId::new()
        .get_processor_frequency_info()
        .map(|info| info.processor_base_frequency())
        .and_then(|mhz| tsc_cal::tsc_hz_from_base_mhz(mhz as u32))
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
            // Dense logical CPU id (0..NCPU), resolved from the sparse Local APIC
            // ID through the table populated during SMP bring-up (see `smp.rs`).
            // The raw APIC ID must NOT be used to index per-CPU arrays: it is not
            // contiguous and can exceed the CPU count, causing out-of-bounds
            // panics. `lock` owns the apic->logical map so the kernel and the lock
            // crate agree on a single id space.
            lock::current_cpu_id()
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
            super::smp::cpu_count() as u8
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
