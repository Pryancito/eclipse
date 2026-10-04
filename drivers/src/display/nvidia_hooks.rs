//! Real (non-default) `nvidia_rm_sys::hooks::KernelHooks` implementation,
//! backed by Eclipse's existing PCI/MMIO/port-IO/timer primitives. Every
//! vendored NVIDIA C file that reaches out through `os-interface.h` for
//! hardware access (PCI config space, MMIO mappings, legacy I/O ports,
//! monotonic time/delay) ends up here instead of the crate's safe-default
//! stubs.
//!
//! `pci_config_read`/`pci_config_write`'s `pci_handle` is whatever
//! `os_pci_init_handle` (nvidia-rm-sys/src/os_interface.rs) packed: the
//! bus/device/function tuple, tagged with a high "valid" bit so a real
//! bus=device=function=0 location never collides with 0/null.

use crate::builder::IoMapper;
use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
use crate::bus::{drivers_timer_now_as_micros, phys_to_virt};
use alloc::sync::Arc;
use core::hint::spin_loop;
use lock::Mutex;
use nvidia_rm_sys::hooks::KernelHooks;
use pci::{Location, PortOps};

/// Decode a handle packed by `os_pci_init_handle` back into a `Location`.
/// Falls back to bus=device=function=0 for a handle that was never
/// properly packed (e.g. NV_STATUS default 0) -- reads/writes against
/// that location are harmless no-ops from Eclipse's point of view since
/// real hardware only exists at addresses the PCI scan already found.
fn decode_handle(handle: usize) -> Location {
    Location {
        bus: ((handle >> 16) & 0xFF) as u8,
        device: ((handle >> 8) & 0xFF) as u8,
        function: (handle & 0xFF) as u8,
    }
}

pub struct EclipseNvrmHooks {
    mapper: Mutex<Option<Arc<dyn IoMapper>>>,
}

impl EclipseNvrmHooks {
    const fn new() -> Self {
        Self {
            mapper: Mutex::new(None),
        }
    }

    pub fn set_mapper(&self, mapper: Arc<dyn IoMapper>) {
        *self.mapper.lock() = Some(mapper);
    }
}

// Busy-wait helper shared by `delay_us`. Time-based (TSC via
// drivers_timer_now_as_micros), with the escape hatch scoped to what it was
// actually for: a TRULY frozen timer. The previous version capped TOTAL
// iterations at 10M, which real hardware showed truncates any long delay to
// ~140 ms regardless of the (healthy, advancing) timer -- the "500 ms" SEC2
// silent window really waited ~140 ms, and every long RM osDelayUs was
// silently cut short, shrinking count-based firmware waits. Now the delay
// runs to true completion as long as the timer advances, and only bails if
// the reading stays IDENTICAL for 10M consecutive spins (a genuinely dead
// timer, where waiting can never terminate anyway).
fn udelay(us: u64) {
    let t0 = unsafe { drivers_timer_now_as_micros() };
    const MAX_STUCK_SPINS: u64 = 10_000_000;
    let mut last = t0;
    let mut stuck = 0u64;
    let mut spins: u64 = 0;
    loop {
        let now = unsafe { drivers_timer_now_as_micros() };
        if now.wrapping_sub(t0) >= us {
            break;
        }
        if now == last {
            stuck += 1;
            if stuck >= MAX_STUCK_SPINS {
                log::warn!(
                    "[NVRM-HOOKS] delay_us aborted ({}us requested — timer genuinely frozen)",
                    us
                );
                break;
            }
        } else {
            stuck = 0;
            last = now;
        }
        spin_loop();
        // The RM busy-delays for tens of microseconds at a time all through GSP
        // bringup and register settling — long, frequent windows during which
        // this CPU cannot ack a peer's TLB shootdown. Another CPU running a
        // munmap/VM_BIND unmap spin-waits for that ack while holding the VMAR
        // inner+page_table locks (no timeout, by design), so a single delaying
        // CPU wedges the whole address-space machinery behind it — the vmar.rs
        // spinlock convoy seen only on real hardware (the RM barely runs under
        // QEMU). Drain our own shootdown queue at a coarse cadence, exactly as
        // os_acquire_spinlock and the kernel ticket lock do. Cheap: one relaxed
        // load when nothing is queued.
        spins = spins.wrapping_add(1);
        if spins & 511 == 0 {
            lock::pump();
        }
    }
}

impl KernelHooks for EclipseNvrmHooks {
    fn pci_config_read(&self, pci_handle: usize, offset: u32, len: u32) -> u32 {
        let loc = decode_handle(pci_handle);
        let ops = &PortOpsImpl;
        unsafe {
            match len {
                1 => PCI_ACCESS.read8(ops, loc, offset as u16) as u32,
                2 => PCI_ACCESS.read16(ops, loc, offset as u16) as u32,
                _ => PCI_ACCESS.read32(ops, loc, offset as u16),
            }
        }
    }

    fn pci_config_write(&self, pci_handle: usize, offset: u32, len: u32, value: u32) {
        let loc = decode_handle(pci_handle);
        let ops = &PortOpsImpl;
        unsafe {
            match len {
                1 => PCI_ACCESS.write8(ops, loc, offset as u16, value as u8),
                2 => PCI_ACCESS.write16(ops, loc, offset as u16, value as u16),
                _ => PCI_ACCESS.write32(ops, loc, offset as u16, value),
            }
        }
    }

    fn map_kernel_space(&self, phys: u64, size: u64) -> u64 {
        let guard = self.mapper.lock();
        if let Some(mapper) = guard.as_ref() {
            if let Some(vaddr) = mapper.query_or_map(phys as usize, size as usize) {
                return vaddr as u64;
            }
        }
        drop(guard);
        phys_to_virt(phys as usize) as u64
    }

    fn unmap_kernel_space(&self, _virt: u64, _size: u64) {
        // Eclipse's `IoMapper` has no unmap primitive (device mappings live
        // for the lifetime of the kernel) -- confirmed via
        // drivers/src/builder/mod.rs, which only exposes `query_or_map`.
    }

    fn io_read(&self, port: u32, len: u32) -> u32 {
        let ops = &PortOpsImpl;
        unsafe {
            match len {
                1 => ops.read8(port as u16) as u32,
                2 => ops.read16(port as u16) as u32,
                _ => ops.read32(port),
            }
        }
    }

    fn io_write(&self, port: u32, len: u32, value: u32) {
        let ops = &PortOpsImpl;
        unsafe {
            match len {
                1 => ops.write8(port as u16, value as u8),
                2 => ops.write16(port as u16, value as u16),
                _ => ops.write32(port, value),
            }
        }
    }

    fn monotonic_time_ns(&self) -> u64 {
        unsafe { drivers_timer_now_as_micros() }.saturating_mul(1000)
    }

    fn delay_us(&self, us: u32) {
        udelay(us as u64);
    }
}

static ECLIPSE_NVRM_HOOKS: EclipseNvrmHooks = EclipseNvrmHooks::new();

/// Registers Eclipse's real `KernelHooks` with `nvidia-rm-sys` and stashes
/// the `IoMapper` so `map_kernel_space` can use it. Idempotent -- safe to
/// call once per matched GPU (`register_hooks` just overwrites the global
/// pointer; the mapper is shared across all NVIDIA devices).
pub fn install(mapper: &Option<Arc<dyn IoMapper>>) {
    if let Some(m) = mapper {
        ECLIPSE_NVRM_HOOKS.set_mapper(m.clone());
    }
    nvidia_rm_sys::hooks::register_hooks(&ECLIPSE_NVRM_HOOKS);
}

#[cfg(test)]
mod nvidia_hooks_tests {
    use super::*;
    use crate::nvme::nvme_queue::test_clock;

    /// The handle the RM carries around is packed on the other side of the FFI
    /// boundary, by `os_pci_init_handle` in `nvidia-rm-sys`, and unpacked here.
    /// Nothing links the two but this layout: if either side moves a field, the
    /// RM goes on reading and WRITING config space at a different PCI function
    /// than the GPU it thinks it is talking to. So the test is the round trip
    /// through the real packer, not a copy of its arithmetic.
    #[test]
    fn a_packed_pci_handle_decodes_back_to_the_same_location() {
        for (bus, slot, function) in [
            (0u8, 0u8, 0u8),
            (0x01, 0x00, 0x00),
            (0x65, 0x00, 0x01),
            (0xFF, 0xFF, 0x07),
        ] {
            let handle = nvidia_rm_sys::os_interface::os_pci_init_handle(
                0,
                bus,
                slot,
                function,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            ) as usize;
            let loc = decode_handle(handle);
            assert_eq!(
                (loc.bus, loc.device, loc.function),
                (bus, slot, function),
                "handle {:#x} decoded to {:?}",
                handle,
                (loc.bus, loc.device, loc.function)
            );
        }
    }

    /// The top bit is a "valid handle" tag, there so that a real
    /// bus=device=function=0 location -- this GPU's own function 0 -- does not
    /// pack to 0 and read as a null handle. Decoding has to mask it off rather
    /// than let it land in the bus number.
    #[test]
    fn the_valid_tag_never_reaches_the_bus_number() {
        let handle = nvidia_rm_sys::os_interface::os_pci_init_handle(
            0,
            0,
            0,
            0,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        ) as usize;
        assert_ne!(handle, 0, "a valid location must not pack to a null handle");
        let loc = decode_handle(handle);
        assert_eq!((loc.bus, loc.device, loc.function), (0, 0, 0));
    }

    /// A handle that was never packed (an `NV_STATUS` default of 0 arriving
    /// where a handle was expected) decodes to 0/0/0 rather than to something
    /// random: reads there are no-ops, because real hardware only exists where
    /// the PCI scan already found it.
    #[test]
    fn an_unpacked_handle_decodes_to_the_harmless_location() {
        let loc = decode_handle(0);
        assert_eq!((loc.bus, loc.device, loc.function), (0, 0, 0));
    }

    /// The RM builds every timeout out of this clock as an absolute deadline
    /// (`now + timeout`), so a reading that wraps round sends the deadline
    /// backwards and the wait ends instantly -- or never. Microseconds times a
    /// thousand overflows a `u64` above ~1.8e16 us, which a TSC-derived
    /// reading can reach on a machine that has been up a while or whose TSC
    /// starts high; it has to saturate, not wrap.
    #[test]
    fn the_nanosecond_clock_saturates_instead_of_wrapping() {
        let hooks = EclipseNvrmHooks::new();
        test_clock::set_auto_advance(0);

        test_clock::set(0);
        assert_eq!(hooks.monotonic_time_ns(), 0);

        test_clock::set(1_234_567);
        assert_eq!(hooks.monotonic_time_ns(), 1_234_567_000);

        test_clock::set(u64::MAX);
        assert_eq!(
            hooks.monotonic_time_ns(),
            u64::MAX,
            "a clock that wraps sends every RM deadline into the past"
        );
        // One microsecond above where the multiply stops fitting: still
        // forward, never a small number.
        test_clock::set(u64::MAX / 1000 + 1);
        assert_eq!(hooks.monotonic_time_ns(), u64::MAX);

        test_clock::set(0);
    }

    /// The delay must run to true completion while the timer advances. The
    /// previous version capped TOTAL spins, which on real hardware truncated
    /// every long wait to about 140 ms however healthy the timer was: the
    /// "500 ms" SEC2 silent window really waited 140 ms, and count-based
    /// firmware waits shrank with it.
    #[test]
    fn a_delay_waits_the_whole_time_it_was_asked_for() {
        test_clock::set(0);
        // One microsecond per reading: the loop's own progress, nothing else.
        test_clock::set_auto_advance(1);
        for us in [1u64, 10, 1_000, 500_000] {
            test_clock::set(0);
            udelay(us);
            let reached = {
                test_clock::set_auto_advance(0);
                let now = test_clock::now();
                test_clock::set_auto_advance(1);
                now
            };
            assert!(
                reached >= us,
                "a {} us delay returned after only {} us",
                us,
                reached
            );
            // And it stops there. One reading per microsecond is all this
            // clock gives, so a deadline check that is off by one spends a
            // whole extra microsecond on every delay the RM asks for -- and
            // the RM asks for tens of thousands of them per GSP boot.
            assert!(
                reached <= us + 1,
                "a {} us delay overshot to {} us",
                us,
                reached
            );
        }
        test_clock::set_auto_advance(0);
        test_clock::set(0);
    }

    /// A delay of zero is not a delay: the first reading is already past the
    /// deadline, so it must not spin at all.
    #[test]
    fn a_zero_delay_returns_at_once() {
        test_clock::set_auto_advance(0);
        test_clock::set(50_000);
        udelay(0);
        assert_eq!(
            test_clock::now(),
            50_000,
            "a zero delay moved the clock, so it spun"
        );
        test_clock::set(0);
    }

    /// And a delay asked for across the point where the microsecond reading
    /// wraps still ends: the elapsed-time check is a wrapping subtraction, so
    /// a timer that rolls over mid-wait does not turn a 10 us delay into a
    /// half-million-year one.
    #[test]
    fn a_delay_across_the_timer_wrap_still_ends() {
        test_clock::set(u64::MAX - 4);
        test_clock::set_auto_advance(1);
        udelay(10);
        test_clock::set_auto_advance(0);
        // It came back, and it came back past the wrap rather than before it.
        assert!(test_clock::now() < 100, "the wait did not cross the wrap");
        test_clock::set(0);
    }
}
