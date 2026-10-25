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
#[cfg(not(test))]
use crate::bus::pci::PortOpsImpl;
use crate::bus::pci::PCI_ACCESS;
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

/// The port I/O the hooks go through: the real `in`/`out` of
/// [`PortOpsImpl`] in the kernel, and under test a double that records every
/// access and answers what the test loaded, because an `in` instruction in a
/// host process is a fault, not a read.
#[cfg(not(test))]
fn port_ops() -> &'static PortOpsImpl {
    &PortOpsImpl
}
#[cfg(test)]
fn port_ops() -> &'static fake_ports::FakePorts {
    &fake_ports::PORTS
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
        let ops = port_ops();
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
        let ops = port_ops();
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
        let ops = port_ops();
        unsafe {
            match len {
                1 => ops.read8(port as u16) as u32,
                2 => ops.read16(port as u16) as u32,
                _ => ops.read32(port),
            }
        }
    }

    fn io_write(&self, port: u32, len: u32, value: u32) {
        let ops = port_ops();
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

/// The port I/O double behind [`port_ops`] under test: every access lands in
/// `log` as `(read, port, len, value)`, and every read answers `answer`.
/// Process-global like the real ports, so a test that uses it holds
/// `crate::test_globals::LOCK` and empties the log first.
#[cfg(test)]
pub(super) mod fake_ports {
    use alloc::vec::Vec;
    use lock::Mutex;
    use pci::PortOps;

    pub(super) struct FakePorts {
        pub log: Mutex<Vec<(bool, u32, u32, u32)>>,
        pub answer: Mutex<u32>,
    }

    pub(super) static PORTS: FakePorts = FakePorts {
        log: Mutex::new(Vec::new()),
        answer: Mutex::new(0),
    };

    impl FakePorts {
        fn read(&self, port: u32, len: u32) -> u32 {
            let v = *self.answer.lock();
            self.log.lock().push((true, port, len, v));
            v
        }
        fn write(&self, port: u32, len: u32, v: u32) {
            self.log.lock().push((false, port, len, v));
        }
    }

    impl PortOps for FakePorts {
        unsafe fn read8(&self, port: u16) -> u8 {
            self.read(port as u32, 1) as u8
        }
        unsafe fn read16(&self, port: u16) -> u16 {
            self.read(port as u32, 2) as u16
        }
        unsafe fn read32(&self, port: u32) -> u32 {
            self.read(port, 4)
        }
        unsafe fn write8(&self, port: u16, val: u8) {
            self.write(port as u32, 1, val as u32)
        }
        unsafe fn write16(&self, port: u16, val: u16) {
            self.write(port as u32, 2, val as u32)
        }
        unsafe fn write32(&self, port: u32, val: u32) {
            self.write(port, 4, val)
        }
    }
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
        // And the RM's `delay_us` hook is that delay, through the same clock.
        EclipseNvrmHooks::new().delay_us(0);
        assert_eq!(test_clock::now(), 50_000, "the hook spun on a zero delay");
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

    /// A delay on a timer that never moves cannot end by the clock, and the
    /// RM asks for them with interrupts off: it gives up after the reading has
    /// stayed identical for ten million spins, and says so, instead of
    /// hanging the CPU for good.
    #[test]
    fn a_delay_on_a_frozen_timer_gives_up_instead_of_hanging() {
        test_clock::set_auto_advance(0);
        test_clock::set(7);
        udelay(10);
        assert_eq!(
            test_clock::now(),
            7,
            "the clock never moved, and it came back"
        );
        test_clock::set(0);
    }

    use crate::builder::IoMapper;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// An `IoMapper` that answers `paddr + OFFSET` for what it was asked to
    /// map, or nothing when `refuse` is set, and counts the calls.
    struct FakeMapper {
        calls: AtomicUsize,
        refuse: bool,
    }
    const OFFSET: usize = 0xFFFF_8000_0000_0000;
    impl IoMapper for FakeMapper {
        fn query_or_map(&self, paddr: usize, size: usize) -> Option<usize> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            assert_eq!(size, 0x1000, "the size asked for is the size mapped");
            (!self.refuse).then(|| paddr + OFFSET)
        }
    }

    /// The RM maps its registers through the kernel's `IoMapper` once one has
    /// been stashed; before that, or when the mapper has nothing for the
    /// range, through the identity window `phys_to_virt`. Unmapping is a
    /// no-op: device mappings live as long as the kernel.
    #[test]
    fn registers_are_mapped_through_the_stashed_mapper_or_the_identity_window() {
        let hooks = EclipseNvrmHooks::new();
        assert_eq!(
            hooks.map_kernel_space(0xF000_0000, 0x1000),
            0xF000_0000,
            "no mapper yet: phys_to_virt, the identity on the host"
        );
        let mapper = Arc::new(FakeMapper {
            calls: AtomicUsize::new(0),
            refuse: false,
        });
        hooks.set_mapper(mapper.clone());
        assert_eq!(
            hooks.map_kernel_space(0xF000_0000, 0x1000),
            (0xF000_0000 + OFFSET) as u64
        );
        assert_eq!(mapper.calls.load(Ordering::Relaxed), 1);
        hooks.set_mapper(Arc::new(FakeMapper {
            calls: AtomicUsize::new(0),
            refuse: true,
        }));
        assert_eq!(
            hooks.map_kernel_space(0xF000_0000, 0x1000),
            0xF000_0000,
            "a mapper with nothing for the range: the identity window"
        );
        hooks.unmap_kernel_space(0xF000_0000, 0x1000);
    }

    fn ports_log() -> Vec<(bool, u32, u32, u32)> {
        fake_ports::PORTS.log.lock().drain(..).collect()
    }

    /// Legacy port I/O goes straight to the port with the width asked for:
    /// 1, 2 or 4 bytes, and anything else is a dword, as `os_io_*` in the
    /// RM's OS layer has it.
    #[test]
    fn port_io_goes_to_the_port_with_the_width_asked_for() {
        let _serialised = crate::test_globals::LOCK.lock();
        let hooks = EclipseNvrmHooks::new();
        *fake_ports::PORTS.answer.lock() = 0x1122_3344;
        ports_log();
        assert_eq!(hooks.io_read(0x3C4, 1), 0x44, "a byte, as a byte");
        assert_eq!(hooks.io_read(0x3C4, 2), 0x3344);
        assert_eq!(hooks.io_read(0x3C4, 4), 0x1122_3344);
        assert_eq!(
            hooks.io_read(0x3C4, 3),
            0x1122_3344,
            "any other width is a dword"
        );
        hooks.io_write(0x3C5, 1, 0xAB);
        hooks.io_write(0x3C5, 2, 0xCDEF);
        hooks.io_write(0x3C5, 4, 0xCAFE_BABE);
        hooks.io_write(0x3C5, 8, 0xDEAD_BEEF);
        assert_eq!(
            ports_log(),
            [
                (true, 0x3C4, 1, 0x1122_3344),
                (true, 0x3C4, 2, 0x1122_3344),
                (true, 0x3C4, 4, 0x1122_3344),
                (true, 0x3C4, 4, 0x1122_3344),
                (false, 0x3C5, 1, 0xAB),
                (false, 0x3C5, 2, 0xCDEF),
                (false, 0x3C5, 4, 0xCAFE_BABE),
                (false, 0x3C5, 4, 0xDEAD_BEEF),
            ]
        );
    }

    /// `CONFIG_ADDRESS` (0xCF8) takes the enable bit, the location from the
    /// handle and the dword-aligned offset; `CONFIG_DATA` (0xCFC) carries the
    /// dword, from which a byte or a word is picked by the offset's low bits.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn pci_config_reads_go_through_the_legacy_ports_at_the_handles_location() {
        let _serialised = crate::test_globals::LOCK.lock();
        let hooks = EclipseNvrmHooks::new();
        // bus 0x65, device 0, function 1: the second function of the GPU.
        let handle = nvidia_rm_sys::os_interface::os_pci_init_handle(
            0,
            0x65,
            0,
            1,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        ) as usize;
        let address = 0x8000_0000 | (0x65 << 16) | (1 << 8);
        *fake_ports::PORTS.answer.lock() = 0x1122_3344;
        ports_log();
        assert_eq!(hooks.pci_config_read(handle, 0, 4), 0x1122_3344);
        assert_eq!(hooks.pci_config_read(handle, 2, 2), 0x1122, "the high word");
        assert_eq!(hooks.pci_config_read(handle, 1, 1), 0x33, "the second byte");
        assert_eq!(hooks.pci_config_read(handle, 0x10, 4), 0x1122_3344, "BAR0");
        assert_eq!(
            ports_log(),
            [
                (false, 0xCF8, 4, address),
                (true, 0xCFC, 4, 0x1122_3344),
                (false, 0xCF8, 4, address),
                (true, 0xCFC, 4, 0x1122_3344),
                (false, 0xCF8, 4, address),
                (true, 0xCFC, 4, 0x1122_3344),
                (false, 0xCF8, 4, address | 0x10),
                (true, 0xCFC, 4, 0x1122_3344),
            ]
        );
    }

    /// A dword is written as it is; a byte or a word is merged into the dword
    /// that is there (read first), so the RM's byte-wide writes to a command
    /// or latency register leave their neighbours alone.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn pci_config_writes_merge_a_byte_or_a_word_into_the_dword_there() {
        let _serialised = crate::test_globals::LOCK.lock();
        let hooks = EclipseNvrmHooks::new();
        let handle = nvidia_rm_sys::os_interface::os_pci_init_handle(
            0,
            0x65,
            0,
            0,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        ) as usize;
        let address = 0x8000_0000 | (0x65 << 16);
        *fake_ports::PORTS.answer.lock() = 0x1122_3344;
        ports_log();
        hooks.pci_config_write(handle, 4, 4, 0xCAFE_BABE);
        hooks.pci_config_write(handle, 0, 1, 0xAB);
        hooks.pci_config_write(handle, 0, 2, 0xCDEF);
        assert_eq!(
            ports_log(),
            [
                (false, 0xCF8, 4, address | 4),
                (false, 0xCFC, 4, 0xCAFE_BABE),
                (false, 0xCF8, 4, address),
                (true, 0xCFC, 4, 0x1122_3344),
                (false, 0xCF8, 4, address),
                (false, 0xCFC, 4, 0x1122_33AB),
                (false, 0xCF8, 4, address),
                (true, 0xCFC, 4, 0x1122_3344),
                (false, 0xCF8, 4, address),
                (false, 0xCFC, 4, 0x1122_CDEF),
            ]
        );
    }

    /// `install` registers these hooks with `nvidia-rm-sys`, so the RM's OS
    /// layer reaches the kernel through them from then on, and stashes the
    /// mapper when it is given one; without a mapper the registration still
    /// happens and the identity window serves.
    #[test]
    fn install_registers_the_hooks_and_stashes_the_mapper_it_is_given() {
        let _serialised = crate::test_globals::LOCK.lock();
        use nvidia_rm_sys::os_interface::{os_io_read_word, os_map_kernel_space};
        *fake_ports::PORTS.answer.lock() = 0x5A5A;
        ports_log();
        install(&None);
        assert_eq!(
            os_io_read_word(0x3DA),
            0x5A5A,
            "the RM reaches the ports through us"
        );
        assert_eq!(ports_log(), [(true, 0x3DA, 2, 0x5A5A)]);
        assert_eq!(
            os_map_kernel_space(0xF000_0000, 0x1000, 0) as usize,
            0xF000_0000,
            "no mapper: the identity window"
        );
        let mapper: Arc<dyn IoMapper> = Arc::new(FakeMapper {
            calls: AtomicUsize::new(0),
            refuse: false,
        });
        install(&Some(mapper));
        assert_eq!(
            os_map_kernel_space(0xF000_0000, 0x1000, 0) as usize,
            0xF000_0000 + OFFSET,
            "the stashed mapper"
        );
        // Leave the global as the other tests found it: no mapper stashed.
        ECLIPSE_NVRM_HOOKS.set_mapper(Arc::new(FakeMapper {
            calls: AtomicUsize::new(0),
            refuse: true,
        }));
    }
}
