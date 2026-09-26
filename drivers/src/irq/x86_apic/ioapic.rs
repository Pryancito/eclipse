use alloc::vec::Vec;
use core::{fmt, ptr::NonNull};

use crate::sync::Mutex;
use acpi::platform::interrupt::InterruptModel;
use acpi::{AcpiHandler, AcpiTables, PhysicalMapping};
use x2apic::ioapic::{IoApic as IoApicInner, IrqFlags, IrqMode, RedirectionTableEntry};

use super::{IrqPolarity, IrqTriggerMode, Phys2VirtFn, X86_INT_BASE};

const PAGE_SIZE: usize = 4096;

/// The largest `max_table_entry` this driver will bring an I/O APIC up on.
///
/// [`IoApicInner::init`] maps entry `i` to vector `i + offset` and computes both
/// its loop bound (`max_table_entry() + 1`) and every one of those vectors in a
/// `u8`. With `offset` = [`X86_INT_BASE`] that arithmetic wraps for anything
/// above this, and it happens inside the upstream crate, before this driver
/// gets a chance to touch a single entry: a panic during driver init where
/// overflow checks are on, and, in the release kernel where they are off, a
/// loop bound of zero -- nothing programmed at all, every entry left exactly as
/// the firmware handed it over, unmasked, pointing at whatever vector the last
/// kernel to run gave it.
///
/// A register that reports more entries than this is not reporting a real I/O
/// APIC anyway. An unmapped or powered-down MMIO window reads as all ones,
/// which is 256 entries, and that is the read this guard is really for; QEMU
/// always answers 24, so no emulated boot has ever produced it.
const MAX_SUPPORTED_ENTRY: u8 = (u8::MAX as usize - X86_INT_BASE) as u8;

/// The value to write to the IOAPICID register to give it the ID `id`.
///
/// The ID is **four** bits, 24..=27; 28..=31 are reserved. [`IoApicInner::id`]
/// reads it back masked to those four, so writing all eight bits of `id` both
/// put something in the reserved bits and could never match on read-back: a
/// board whose ACPI-declared id was 16 or more always logged "the ID register
/// is read-only on this hardware" whatever the register had actually done.
fn ioapic_id_reg(current: u32, id: u8) -> u32 {
    const ID_FIELD: u32 = 0x0F00_0000;
    (current & !ID_FIELD) | ((id as u32 & 0xF) << 24)
}

/// The redirection-table index `gsi` lands on, or `None` when an I/O APIC based
/// at `gsi_start` with `max_entry` as its last entry does not cover it.
///
/// Every caller reaches an [`IoApic`] through [`IoApicList::find`], which
/// bounds `gsi` to this range first, so `None` should be unreachable from
/// inside this module. It is still a subtraction on a number that came out of
/// the ACPI tables: the old form, `(gsi - gsi_start) as u8` written out in four
/// separate methods, underflows below the base and truncates above it, and
/// either one programs a different interrupt line than the caller named.
fn entry_index(gsi: u32, gsi_start: u32, max_entry: u8) -> Option<u8> {
    let idx = gsi.checked_sub(gsi_start)?;
    (idx <= max_entry as u32).then_some(idx as u8)
}

/// Set the trigger mode, polarity, delivery mode and destination of `entry`,
/// leaving its vector and its mask bit as they were.
///
/// The flags word goes out whole -- `set_flags` clears every writable flag it
/// is not given -- so it has to be composed from the entry that is already
/// there. The old code composed it from `tm` and `pol` on top of
/// `IrqFlags::MASKED`, under a comment that read "destination mode: physical".
/// Physical destination mode is bit 11 *clear*; `IrqFlags::MASKED` is bit 16.
/// So `configure` masked the very line it was configuring, and took a vector
/// argument that every caller passed as 0, wiping whatever `map_vector` had
/// programmed. Both are invisible today only because both callers configure,
/// register a handler and then unmask, in that order -- an ordering nothing
/// wrote down and the name does not suggest. Reconfiguring a live line, which
/// is what a level-triggered PCI interrupt being re-routed looks like, took it
/// down for good.
fn program_entry(
    entry: &mut RedirectionTableEntry,
    tm: IrqTriggerMode,
    pol: IrqPolarity,
    dest: u8,
) {
    // Whatever else the entry says, its mask bit is the caller's to change
    // through `toggle`, not this function's.
    let mut flags = entry.flags() & IrqFlags::MASKED;
    if matches!(tm, IrqTriggerMode::Level) {
        flags |= IrqFlags::LEVEL_TRIGGERED;
    }
    if matches!(pol, IrqPolarity::ActiveLow) {
        flags |= IrqFlags::LOW_ACTIVE;
    }
    entry.set_mode(IrqMode::Fixed);
    entry.set_dest(dest);
    entry.set_flags(flags);
}

/// How many bytes of pages the region `physical_address .. +size` spans.
///
/// `physical_address + size + PAGE_SIZE - 1` overflows for an address near the
/// top of the space, which is a panic where overflow checks are on. The address
/// comes out of an ACPI table, and [`IoApicList::new`] already treats a table it
/// cannot parse as "no I/O APICs, carry on booting" -- a panic in here goes
/// straight around that.
fn mapped_length(physical_address: usize, size: usize) -> usize {
    let start = physical_address & !(PAGE_SIZE - 1);
    let span = physical_address.saturating_add(size).saturating_sub(start);
    span.div_ceil(PAGE_SIZE).saturating_mul(PAGE_SIZE)
}

#[derive(Clone)]
struct AcpiMapHandler {
    phys_to_virt: Phys2VirtFn,
}

impl AcpiHandler for AcpiMapHandler {
    /// we just impl this function, `rdsp` crate will use it, and we not.
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> PhysicalMapping<Self, T> {
        unsafe {
            // The address may not be page aligned, so align the span manually.
            let vaddr = (self.phys_to_virt)(physical_address);
            // `NonNull::new_unchecked(0)` is undefined behaviour, and the
            // `acpi` crate reads through the pointer either way, so a mapping
            // that came back as address zero is not something to hand on: say
            // which table it was instead of faulting with no message.
            let virtual_start = NonNull::new(vaddr as *mut T).unwrap_or_else(|| {
                panic!(
                    "[ioapic] ACPI table at {:#x} is not mapped",
                    physical_address
                )
            });
            PhysicalMapping::new(
                physical_address,
                virtual_start,
                size,
                mapped_length(physical_address, size),
                self.clone(),
            )
        }
    }

    /// we do nothing here
    fn unmap_physical_region<T>(_region: &PhysicalMapping<Self, T>) {}
}

/// An I/O APIC structure.
///
/// For local APIC and I/O APIC, we can learn something from here: <https://wiki.osdev.org/APIC>.
pub struct IoApic {
    /// I/O APIC id.
    id: u8,
    /// GSI means Global System Interrupt, `gsi_start` is the base number of GSI
    /// in this I/O APIC.
    gsi_start: u32,
    /// Max entry num of the interrupt redirection table.
    max_entry: u8,
    /// Use `x2apic` crate to help us manipulate IOAPIC.
    inner: Mutex<IoApicInner>,
}

/// A list of I/O-APICs for systems have multiple I/O subsystems.
#[derive(Debug)]
pub struct IoApicList {
    io_apics: Vec<IoApic>,
}

impl IoApic {
    /// Create a new [`IoApic`] from fields parsed from the ACPI table, and
    /// initialize it by disabling all interrupts.
    ///
    /// `None` when the version register does not describe an I/O APIC this
    /// driver can program -- see [`MAX_SUPPORTED_ENTRY`].
    pub fn new(id: u8, base_vaddr: usize, gsi_start: u32) -> Option<Self> {
        let mut inner = unsafe { IoApicInner::new(base_vaddr as u64) };
        let max_entry = unsafe { inner.max_table_entry() };
        if max_entry > MAX_SUPPORTED_ENTRY {
            crate::klog_err!(
                "[ioapic] window at {:#x} reports {} redirection entries \
                 (this driver can program {}) -- not a usable I/O APIC, skipped",
                base_vaddr,
                max_entry as u32 + 1,
                MAX_SUPPORTED_ENTRY as u32 + 1
            );
            return None;
        }
        // The ID field is four bits wide, so an ACPI-declared id is only ever
        // comparable to the register modulo 16.
        //
        // Mutation is right that dropping this `& 0xF` changes nothing
        // observable, and it is worth writing down why rather than trying
        // again: `ioapic_id_reg` truncates the value it writes either way, so
        // the register ends up the same, and both branches below then return
        // the four bits the hardware reports. The only difference is which
        // warning gets logged -- and not logging "the ID register is read-only
        // on this hardware" on a board that never had a problem is the whole
        // point of the truncation.
        let wanted_id = id & 0xF;
        let hardware_id = unsafe { inner.id() };
        // On modern PCH chipsets the IOAPIC ID register may be read-only (always 0).
        // We attempt a software correction but treat a remaining mismatch as a
        // non-fatal warning so the system can continue booting on real hardware.
        let effective_id = if hardware_id != wanted_id {
            log::warn!(
                "IOAPIC ID mismatch: ACPI says {wanted_id}, hardware says {hardware_id}. \
                 Attempting to fix..."
            );
            unsafe {
                let base = base_vaddr as *mut u32;
                base.write_volatile(0x00); // select IOAPICID register (index 0)
                let val = (base.add(4)).read_volatile();
                (base.add(4)).write_volatile(ioapic_id_reg(val, wanted_id));
            }
            // Flush and re-read to see if the write took effect.
            let new_id = unsafe { inner.id() };
            if new_id != wanted_id {
                log::warn!(
                    "IOAPIC ID register is read-only on this hardware \
                     (wanted {wanted_id}, got {new_id}). Continuing with hardware ID."
                );
                // Use the hardware-reported ID so routing still works.
                new_id
            } else {
                wanted_id
            }
        } else {
            wanted_id
        };

        unsafe {
            inner.init(X86_INT_BASE as u8);
        }
        // `0..=max_entry`, not `0..max_entry + 1`: `max_entry` is a `u8`, and
        // the count is what overflows, not the index.
        for i in 0..=max_entry {
            unsafe {
                // disable all interrupts
                inner.disable_irq(i);

                // Clean the redirection table
                let mut entry = inner.table_entry(i);
                entry.set_vector(0);
                entry.set_dest(0);
                entry.set_mode(IrqMode::Fixed);
                entry.set_flags(IrqFlags::MASKED);
                inner.set_table_entry(i, entry);
            }
        }
        Some(Self {
            id: effective_id,
            gsi_start,
            max_entry,
            inner: Mutex::new(inner),
        })
    }

    /// The redirection-table index `gsi` lands on. See [`entry_index`].
    fn index_of(&self, gsi: u32) -> Option<u8> {
        entry_index(gsi, self.gsi_start, self.max_entry)
    }

    /// Set apic entry IRQ state by `gsi`.
    pub fn toggle(&self, gsi: u32, enabled: bool) {
        let Some(idx) = self.index_of(gsi) else {
            return;
        };
        unsafe {
            if enabled {
                self.inner.lock().enable_irq(idx);
            } else {
                self.inner.lock().disable_irq(idx);
            }
        }
    }

    /// Get the IDT vector of the `gsi` from redirection table, or 0 -- the
    /// value this driver leaves in an unprogrammed entry -- for a `gsi` this
    /// I/O APIC does not cover.
    pub fn get_vector(&self, gsi: u32) -> u8 {
        let Some(idx) = self.index_of(gsi) else {
            return 0;
        };
        unsafe { self.inner.lock().table_entry(idx).vector() }
    }

    /// Set the IDT vector of the `gsi` in redirection table.
    pub fn map_vector(&self, gsi: u32, vector: u8) {
        let Some(idx) = self.index_of(gsi) else {
            return;
        };
        let mut inner = self.inner.lock();
        unsafe {
            let mut entry = inner.table_entry(idx);
            entry.set_vector(vector);
            inner.set_table_entry(idx, entry);
        }
    }

    /// Set the interrupt trigger mode, polarity and destination of the `gsi` in
    /// the redirection table. The vector and the mask bit are left alone --
    /// they belong to [`map_vector`](Self::map_vector) and
    /// [`toggle`](Self::toggle). See [`program_entry`].
    pub fn configure(&self, gsi: u32, tm: IrqTriggerMode, pol: IrqPolarity, dest: u8) {
        let Some(idx) = self.index_of(gsi) else {
            return;
        };
        let mut inner = self.inner.lock();
        let mut entry = unsafe { inner.table_entry(idx) };
        program_entry(&mut entry, tm, pol, dest);
        unsafe { inner.set_table_entry(idx, entry) };
    }
}

impl IoApicList {
    /// The list a set of already-built I/O APICs makes, for tests that need
    /// more than one without an ACPI table to name them.
    #[cfg(test)]
    fn from_parts(io_apics: Vec<IoApic>) -> Self {
        Self { io_apics }
    }

    /// Probe all I/O APICs from the ACPI table represented by `acpi_rsdp`.
    pub fn new(acpi_rsdp: usize, phys_to_virt: Phys2VirtFn) -> Self {
        let handler = AcpiMapHandler { phys_to_virt };
        // Parse ACPI table by the physical address of the RSDP.
        // On real hardware, from_rsdp() can fail if the RSDP address is wrong
        // or the checksum is corrupt. Treat this as non-fatal and proceed with
        // no IOAPICs (the system will lose PCI interrupt routing but won't panic).
        let tables = match unsafe { AcpiTables::from_rsdp(handler, acpi_rsdp) } {
            Ok(t) => t,
            Err(e) => {
                crate::klog_err!(
                    "[ioapic] ACPI parse failed (RSDP={:#x}): {:?} — PCI IRQ routing disabled",
                    acpi_rsdp,
                    e
                );
                return Self {
                    io_apics: Vec::new(),
                };
            }
        };
        let io_apics = match tables.platform_info() {
            Ok(info) => match info.interrupt_model {
                InterruptModel::Apic(apic) => apic
                    .io_apics
                    .iter()
                    .filter_map(|i| {
                        IoApic::new(
                            i.id,
                            phys_to_virt(i.address as usize),
                            i.global_system_interrupt_base,
                        )
                    })
                    .collect(),
                _ => {
                    crate::klog_warn!("[ioapic] ACPI non-APIC interrupt model — no I/O APICs");
                    Vec::new()
                }
            },
            Err(e) => {
                crate::klog_err!("[ioapic] ACPI platform info failed: {:?} — no I/O APICs", e);
                Vec::new()
            }
        };
        Self { io_apics }
    }

    /// Get the corresponding I/O APIC of the `gsi`, each I/O-APIC have a range
    /// of GSI number.
    pub fn find(&self, gsi: u32) -> Option<&IoApic> {
        // Through `index_of`, so the range is decided in one place: the old
        // `gsi <= i.gsi_start + i.max_entry as u32` also overflowed for a
        // `global_system_interrupt_base` near the top of the `u32`.
        self.io_apics.iter().find(|i| i.index_of(gsi).is_some())
    }
}

impl fmt::Debug for IoApic {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        struct RedirTable<'a>(&'a IoApic);

        impl<'a> fmt::Debug for RedirTable<'a> {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                let mut inner = self.0.inner.lock();
                f.debug_list()
                    .entries((0..=self.0.max_entry).map(|i| unsafe { inner.table_entry(i) }))
                    .finish()
            }
        }

        let version = unsafe { self.inner.lock().version() };
        f.debug_struct("IoApic")
            .field("id", &self.id)
            .field("version", &version)
            .field("gsi_start", &self.gsi_start)
            .field("max_entry", &self.max_entry)
            .field("redir_table", &RedirTable(self))
            .finish()
    }
}

/// Host tests for the I/O APIC.
///
/// The sibling tests in this module's parent open with a warning that an I/O
/// APIC cannot be stood in for by ordinary memory, because its whole register
/// file is read and written through one index register and one data window:
/// read back a redirection-table entry from a buffer and you get whatever was
/// written last, whichever entry it belonged to. That is true, and it is why
/// the entries this driver programs cannot be read back here.
///
/// It is not the whole picture, though. Two things *are* faithful. The index
/// register is write-only as far as this driver is concerned, so after
/// initialisation it holds the last selector written -- an exact record of how
/// far the loop over the redirection table got, which is the thing the table's
/// size is used for. And reads answer a value the test chooses, which is how
/// the version register gets to say something a real chipset would say and
/// QEMU never does. The rest of what this file decides -- which entry a `gsi`
/// lands on, what goes in an entry's flags word, how wide the ID field is,
/// what span of pages an ACPI table covers -- is arithmetic, and it is asked
/// here directly.
#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use alloc::vec;

    /// A stand-in for an I/O APIC's MMIO window: `[0]` is the index register at
    /// offset 0x00 and `[4]` the data window at offset 0x10, which is where
    /// `x2apic` puts them. Leaked, because a real window outlives every driver
    /// that maps it.
    struct Window {
        base: usize,
    }

    impl Window {
        /// `data` is what every register read answers, until the driver writes.
        fn new(data: u32) -> Self {
            let w: Box<[u32; 1024]> = Box::new([0; 1024]);
            let w = Box::leak(w);
            w[4] = data;
            Self {
                base: w.as_ptr() as usize,
            }
        }

        /// A version register reporting `max_entry` as its last entry, which is
        /// the one read this driver's loop bound comes from. 0x11 is the version
        /// every chipset since the 82093AA reports.
        fn with_entries(max_entry: u8) -> Self {
            Self::new(((max_entry as u32) << 16) | 0x11)
        }

        /// The last selector the driver wrote to the index register.
        fn selector(&self) -> u32 {
            unsafe { *(self.base as *const u32) }
        }
    }

    /// The selector of the high word of entry `irq`, which is the last of the
    /// two words this driver writes per entry.
    fn hi(irq: u8) -> u32 {
        0x10 + 2 * irq as u32 + 1
    }

    /// An entry as it comes back from the table: a vector, and masked or not.
    fn entry(vector: u8, masked: bool) -> RedirectionTableEntry {
        let mut e = RedirectionTableEntry::default();
        e.set_vector(vector);
        if masked {
            e.set_flags(IrqFlags::MASKED);
        }
        e
    }

    // ---- bringing one up --------------------------------------------------

    #[test]
    fn a_plain_24_entry_io_apic_programs_every_one_of_its_entries() {
        // What a PCH, and QEMU, report: 24 entries, so the last is 23.
        let w = Window::with_entries(23);
        let a = IoApic::new(0, w.base, 0).expect("24 entries is an I/O APIC");
        assert_eq!(a.max_entry, 23);
        // The masking loop's last write names entry 23. One short of that means
        // the last line of the chip was left as the firmware had it.
        assert_eq!(w.selector(), hi(23));
    }

    #[test]
    fn a_single_entry_io_apic_programs_the_one_it_has() {
        let w = Window::with_entries(0);
        let a = IoApic::new(0, w.base, 0).expect("one entry is still an I/O APIC");
        assert_eq!(a.max_entry, 0);
        assert_eq!(w.selector(), hi(0));
    }

    #[test]
    fn a_window_that_reads_as_all_ones_is_not_an_io_apic() {
        // An unmapped or powered-down window. Its version register says 256
        // entries, and `IoApicInner::init` then computes `255 + 1` in a `u8`:
        // "attempt to add with overflow" inside driver init where overflow
        // checks are on, and a loop bound of zero -- nothing programmed, the
        // firmware's own table left live -- in the release kernel, where they
        // are off.
        let w = Window::new(0xFFFF_FFFF);
        assert!(IoApic::new(0, w.base, 0).is_none());
        // And it gave up before writing anything: the only selector it ever
        // wrote is the version register it read to decide.
        assert_eq!(w.selector(), 0x01);
    }

    #[test]
    fn the_largest_table_this_driver_can_program_is_programmed_to_its_last_entry() {
        let w = Window::with_entries(MAX_SUPPORTED_ENTRY);
        let a = IoApic::new(0, w.base, 0).expect("the limit is inclusive");
        assert_eq!(a.max_entry, MAX_SUPPORTED_ENTRY);
        assert_eq!(w.selector(), hi(MAX_SUPPORTED_ENTRY));
    }

    #[test]
    fn one_entry_past_what_the_vectors_allow_is_refused() {
        let w = Window::with_entries(MAX_SUPPORTED_ENTRY + 1);
        assert!(IoApic::new(0, w.base, 0).is_none());
        assert_eq!(w.selector(), 0x01);
    }

    #[test]
    fn the_limit_is_where_an_entrys_own_vector_stops_fitting_in_a_byte() {
        // The law `MAX_SUPPORTED_ENTRY` exists for: `IoApicInner::init` gives
        // entry `i` the vector `i + X86_INT_BASE`, in a `u8`.
        assert_eq!(
            MAX_SUPPORTED_ENTRY as usize + X86_INT_BASE,
            u8::MAX as usize
        );
    }

    #[test]
    fn a_gsi_this_io_apic_does_not_cover_touches_none_of_its_entries() {
        // The second I/O APIC of a board, asked about a legacy ISA line that
        // belongs to the first. `(gsi - gsi_start) as u8` underflows here --
        // a panic in a debug kernel, and in the release kernel an index picked
        // out of four billion, which reprograms one of this chip's own lines.
        let w = Window::with_entries(23);
        let a = IoApic::new(0, w.base, 24).unwrap();
        let untouched = w.selector();
        a.toggle(5, true);
        a.map_vector(5, 0x31);
        a.configure(5, IrqTriggerMode::Level, IrqPolarity::ActiveLow, 0);
        // The vector it reports for a line it does not own is the one an
        // unprogrammed entry has, not a neighbour's.
        let vector = a.get_vector(5);
        // And not one register access went out -- reads included, since reading
        // an entry means naming it in the index register first. It still names
        // the last entry `new` programmed.
        assert_eq!(w.selector(), untouched);
        assert_eq!(vector, 0);
    }

    // ---- which entry a gsi lands on --------------------------------------

    #[test]
    fn the_first_gsi_of_an_io_apic_is_its_first_entry() {
        assert_eq!(entry_index(24, 24, 23), Some(0));
    }

    #[test]
    fn the_last_gsi_of_an_io_apic_is_its_last_entry() {
        assert_eq!(entry_index(47, 24, 23), Some(23));
    }

    #[test]
    fn a_gsi_one_past_the_end_belongs_to_no_entry() {
        assert_eq!(entry_index(48, 24, 23), None);
    }

    #[test]
    fn a_gsi_below_the_base_is_not_an_underflow() {
        // A legacy ISA line arriving at the second I/O APIC. `gsi - gsi_start`
        // on a `u32` wraps to about four billion, and `as u8` then picks an
        // entry out of it.
        assert_eq!(entry_index(5, 24, 23), None);
        assert_eq!(entry_index(0, 1, 0), None);
    }

    #[test]
    fn a_gsi_far_above_the_base_does_not_truncate_into_range() {
        // 256 - 0 is 256, and `as u8` is 0: the first line of the chip, which
        // on a PC is the timer.
        assert_eq!(entry_index(256, 0, 23), None);
        assert_eq!(entry_index(280, 24, 23), None);
    }

    #[test]
    fn a_base_near_the_top_of_the_address_space_does_not_wrap() {
        // `gsi_start + max_entry` overflows the `u32`, which is how the old
        // range test was written, and every low `gsi` then looked covered.
        let base = u32::MAX - 4;
        assert_eq!(entry_index(5, base, 23), None);
        assert_eq!(entry_index(base, base, 23), Some(0));
        assert_eq!(entry_index(u32::MAX, base, 23), Some(4));
    }

    // ---- what goes in an entry ------------------------------------------

    #[test]
    fn configuring_a_line_leaves_its_vector_alone() {
        // `configure` used to take a vector, and its one caller passed 0, so it
        // wiped whatever `map_vector` had put there. Vector 0 is #DE.
        let mut e = entry(0x31, true);
        program_entry(&mut e, IrqTriggerMode::Level, IrqPolarity::ActiveLow, 0);
        assert_eq!(e.vector(), 0x31);
    }

    #[test]
    fn configuring_a_masked_line_leaves_it_masked() {
        let mut e = entry(0x31, true);
        program_entry(&mut e, IrqTriggerMode::Edge, IrqPolarity::ActiveHigh, 0);
        assert!(e.flags().contains(IrqFlags::MASKED));
    }

    #[test]
    fn configuring_a_live_line_does_not_take_it_down() {
        // The old flags word started at `IrqFlags::MASKED` under a comment that
        // read "destination mode: physical" -- which is bit 11 *clear*, not bit
        // 16 set. So configuring a line that was already delivering masked it,
        // and nothing unmasked it again.
        let mut e = entry(0x31, false);
        program_entry(&mut e, IrqTriggerMode::Level, IrqPolarity::ActiveLow, 0);
        assert!(!e.flags().contains(IrqFlags::MASKED));
    }

    #[test]
    fn a_level_triggered_active_low_line_gets_both_bits() {
        // Every shared PCI interrupt line.
        let mut e = entry(0x31, true);
        program_entry(&mut e, IrqTriggerMode::Level, IrqPolarity::ActiveLow, 0);
        assert!(e.flags().contains(IrqFlags::LEVEL_TRIGGERED));
        assert!(e.flags().contains(IrqFlags::LOW_ACTIVE));
    }

    #[test]
    fn an_edge_triggered_active_high_line_gets_neither() {
        // And starting from an entry that had both, they have to come off: the
        // flags word is written whole.
        let mut e = entry(0x31, true);
        program_entry(&mut e, IrqTriggerMode::Level, IrqPolarity::ActiveLow, 0);
        program_entry(&mut e, IrqTriggerMode::Edge, IrqPolarity::ActiveHigh, 0);
        assert!(!e.flags().contains(IrqFlags::LEVEL_TRIGGERED));
        assert!(!e.flags().contains(IrqFlags::LOW_ACTIVE));
    }

    #[test]
    fn a_configured_line_is_delivered_by_physical_destination() {
        // Physical mode is the absence of `LOGICAL_DEST`, and the destination
        // is then a local APIC id rather than a bitmap of them.
        let mut e = entry(0x31, true);
        program_entry(&mut e, IrqTriggerMode::Level, IrqPolarity::ActiveLow, 2);
        assert!(!e.flags().contains(IrqFlags::LOGICAL_DEST));
        assert_eq!(e.dest(), 2);
    }

    #[test]
    fn a_configured_line_is_delivered_as_a_fixed_interrupt() {
        let mut e = entry(0x31, true);
        e.set_mode(IrqMode::NonMaskable);
        program_entry(&mut e, IrqTriggerMode::Edge, IrqPolarity::ActiveHigh, 0);
        assert!(matches!(e.mode(), IrqMode::Fixed));
    }

    // ---- the ID register ------------------------------------------------

    #[test]
    fn the_id_field_is_four_bits_and_the_reserved_ones_are_left_alone() {
        // Bits 28..=31 are reserved; the old write put the top half of an
        // eight-bit id in them.
        assert_eq!(ioapic_id_reg(0xF0AB_CDEF, 3), 0xF3AB_CDEF);
        assert_eq!(ioapic_id_reg(0x0000_0000, 0xF), 0x0F00_0000);
    }

    #[test]
    fn an_acpi_id_too_big_for_the_field_is_taken_modulo_sixteen() {
        // And the read-back is compared against the same truncation, so a board
        // declaring id 19 no longer logs "the ID register is read-only" on every
        // boot whatever the register did.
        assert_eq!(ioapic_id_reg(0, 0x13), ioapic_id_reg(0, 3));
    }

    // ---- the ACPI mapping ------------------------------------------------

    #[test]
    fn a_table_inside_one_page_spans_that_page() {
        assert_eq!(mapped_length(0, 1), PAGE_SIZE);
        assert_eq!(mapped_length(0x1234, 4), PAGE_SIZE);
    }

    #[test]
    fn a_table_that_straddles_a_page_boundary_spans_both() {
        assert_eq!(mapped_length(PAGE_SIZE - 1, 2), 2 * PAGE_SIZE);
        assert_eq!(mapped_length(0, PAGE_SIZE + 1), 2 * PAGE_SIZE);
    }

    #[test]
    fn a_page_aligned_page_sized_table_spans_one_page() {
        assert_eq!(mapped_length(PAGE_SIZE, PAGE_SIZE), PAGE_SIZE);
    }

    #[test]
    fn a_table_at_the_top_of_the_address_space_is_not_an_overflow() {
        // A corrupt table names an address like this, and the caller's whole
        // point is that a table it cannot parse leaves the machine booting.
        // Saturating, the span is the rest of the last page, which is a page;
        // wrapping, the end lands below the start and the answer is no pages at
        // all -- for an address the `acpi` crate is about to read through.
        assert_eq!(mapped_length(usize::MAX - 1, 16), PAGE_SIZE);
        assert_eq!(mapped_length(usize::MAX, usize::MAX), PAGE_SIZE);
    }

    #[test]
    fn a_table_of_no_bytes_spans_nothing() {
        assert_eq!(mapped_length(PAGE_SIZE, 0), 0);
    }

    // ---- more than one I/O APIC ------------------------------------------

    #[test]
    fn the_second_io_apic_owns_the_gsis_above_the_first() {
        // Two I/O APICs is a server board, and no emulated boot has one: QEMU
        // gives exactly one, based at GSI 0.
        let (w0, w1) = (Window::with_entries(23), Window::with_entries(7));
        let list = IoApicList::from_parts(vec![
            IoApic::new(0, w0.base, 0).unwrap(),
            IoApic::new(1, w1.base, 24).unwrap(),
        ]);
        assert_eq!(list.find(0).map(|a| a.gsi_start), Some(0));
        assert_eq!(list.find(23).map(|a| a.gsi_start), Some(0));
        assert_eq!(list.find(24).map(|a| a.gsi_start), Some(24));
        assert_eq!(list.find(31).map(|a| a.gsi_start), Some(24));
    }

    #[test]
    fn a_gsi_no_io_apic_covers_is_found_nowhere() {
        let w = Window::with_entries(23);
        let list = IoApicList::from_parts(vec![IoApic::new(0, w.base, 24).unwrap()]);
        assert!(list.find(23).is_none());
        assert!(list.find(48).is_none());
        assert!(IoApicList::from_parts(vec![]).find(0).is_none());
    }
}
