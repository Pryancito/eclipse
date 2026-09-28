//! Kernel counter.

use core::fmt::{Debug, Error, Formatter};
use core::slice::from_raw_parts;
use core::sync::atomic::{AtomicUsize, Ordering};

const KCOUNTER_MAGIC: u64 = 1_547_273_975;

/// Kernel counter type.
#[repr(u64)]
#[derive(Debug)]
#[allow(dead_code)]
enum DescriptorType {
    /// Padding
    Padding = 0,
    /// Sum
    Sum = 1,
    /// Min
    Min = 2,
    /// Max
    Max = 3,
}

/// Kernel counter descriptor.
#[repr(C)]
#[derive(Debug)]
pub struct Descriptor {
    name: [u8; 56],
    desc_type: DescriptorType,
}

impl Descriptor {
    /// Bytes the name field holds. Fixed by the on-disk format the descriptor
    /// VMO uses, which Zircon's own `kcounter` tool parses.
    pub const MAX_NAME_LEN: usize = 56;

    /// Create a kcounter descriptor by `name`.
    ///
    /// A name longer than [`Self::MAX_NAME_LEN`] is a COMPILE error and used to
    /// be a silent truncation: the fill macro writes the first 56 bytes and
    /// stops, so two counters whose names first differ at byte 57 became the
    /// same name in the dump, with no hint that anything had been cut. Every
    /// call site is a `static` initialiser through the `kcounter!` macro, so
    /// this is evaluated at compile time and the build stops instead.
    pub const fn new(name: &'static str) -> Self {
        macro_rules! try_fill_char {
            (@1, $dst: ident, $src: ident, $idx: expr) => {
                if $src.len() > $idx {
                    $dst[$idx] = $src[$idx];
                }
            };
            (@4, $dst: ident, $src: ident, $idx: expr) => {
                try_fill_char!(@1, $dst, $src, $idx);
                try_fill_char!(@1, $dst, $src, $idx + 1);
                try_fill_char!(@1, $dst, $src, $idx + 2);
                try_fill_char!(@1, $dst, $src, $idx + 3);
            };
            (@16, $dst: ident, $src: ident, $idx: expr) => {
                try_fill_char!(@4, $dst, $src, $idx);
                try_fill_char!(@4, $dst, $src, $idx + 4);
                try_fill_char!(@4, $dst, $src, $idx + 8);
                try_fill_char!(@4, $dst, $src, $idx + 12);
            };
        }
        macro_rules! str_to_array56 {
            ($str: expr) => {{
                let bytes = $str.as_bytes();
                let mut arr = [0; 56];
                try_fill_char!(@16, arr, bytes, 0);
                try_fill_char!(@16, arr, bytes, 16);
                try_fill_char!(@16, arr, bytes, 32);
                try_fill_char!(@4, arr, bytes, 48);
                try_fill_char!(@4, arr, bytes, 52);
                arr
            }};
        }
        assert!(
            name.len() <= Self::MAX_NAME_LEN,
            "kcounter name longer than 56 bytes would be silently truncated"
        );
        Self {
            name: str_to_array56!(name),
            desc_type: DescriptorType::Sum,
        }
    }

    /// The name, without the NUL padding.
    ///
    /// Total on purpose. The bytes are read back out of a `&'static
    /// [Descriptor]` the LINKER assembled from every crate in the image, so
    /// this walks whatever is in the section rather than what this `new` put
    /// there. The `Debug` impl used to scan for the NUL itself and then
    /// `unwrap()` the `from_utf8`, which turns a single bad byte in that
    /// section into a panic **while printing the counters** -- the one moment
    /// somebody is looking at them because something else already went wrong.
    /// A name that fills all 56 bytes has no NUL at all, which is legal in the
    /// format and which that scan handled only by accident.
    pub fn name(&self) -> &str {
        let len = self
            .name
            .iter()
            .position(|&c| c == b'\0')
            .unwrap_or(Self::MAX_NAME_LEN);
        match core::str::from_utf8(&self.name[..len]) {
            Ok(s) => s,
            Err(e) => core::str::from_utf8(&self.name[..e.valid_up_to()]).unwrap_or(""),
        }
    }
}

/// Kernel counter.
#[derive(Debug)]
#[repr(transparent)]
pub struct Counter(AtomicUsize);

impl Default for Counter {
    fn default() -> Self {
        Self::new()
    }
}

impl Counter {
    /// Create a new KCounter.
    pub const fn new() -> Self {
        Counter(AtomicUsize::new(0))
    }

    /// Add a value to the counter.
    pub fn add(&self, x: usize) {
        self.0.fetch_add(x, Ordering::Relaxed);
    }

    /// Get the value of counter.
    pub fn get(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

/// Head of the descriptor table.
#[repr(C)]
#[derive(Debug)]
pub struct DescriptorVmoHeader {
    magic: u64,
    max_cpus: u64,
    descriptor_table_size: usize,
}

impl Default for DescriptorVmoHeader {
    fn default() -> Self {
        Self {
            magic: KCOUNTER_MAGIC,
            max_cpus: 1,
            descriptor_table_size: 0,
        }
    }
}

/// Kernel counters array.
pub struct AllCounters {
    desc: &'static [Descriptor],
    counters: &'static [Counter],
}

#[allow(unsafe_code)]
impl AllCounters {
    /// Get kcounter descriptor table from symbols.
    pub fn get() -> Self {
        let desc_start = kcounters_desc_start as *const () as usize as *const Descriptor;
        let desc_end = kcounters_desc_end as *const () as usize as *const Descriptor;
        let desc = unsafe { from_raw_parts(desc_start, desc_end.offset_from(desc_start) as _) };

        let arena_start = kcounters_arena_start as *const () as usize as *const Counter;
        let arena_end = kcounters_arena_end as *const () as usize as *const Counter;
        let counters =
            unsafe { from_raw_parts(arena_start, arena_end.offset_from(arena_start) as _) };

        Self { desc, counters }
    }

    /// Data of the kcounter descriptor VMO, consists of the [`DescriptorVmoHeader`]
    /// and an table of [`Descriptor`].
    pub fn raw_desc_vmo_data() -> &'static [u8] {
        let desc_vmo_start = kcounters_desc_vmo_start as *const () as usize;
        let desc_vmo_end = kcounters_desc_end as *const () as usize;
        unsafe { from_raw_parts(desc_vmo_start as *const _, desc_vmo_end - desc_vmo_start) }
    }

    /// Data of the kcounter arena VMO: the table of [`Counter`] values, one per
    /// descriptor, and NOTHING else.
    ///
    /// The doc comment here was a copy of the one above and described the
    /// descriptor VMO -- a header plus a table of [`Descriptor`]. This VMO has
    /// no header: it starts at `kcounters_arena_start`, and a reader that
    /// skipped a header that is not there would be one counter out on every
    /// value it reported.
    pub fn raw_arena_vmo_data() -> &'static [u8] {
        let arena_vmo_start = kcounters_arena_start as *const () as usize;
        let arena_vmo_end = kcounters_arena_end as *const () as usize;
        unsafe { from_raw_parts(arena_vmo_start as *const _, arena_vmo_end - arena_vmo_start) }
    }
}

impl Debug for AllCounters {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), Error> {
        f.write_str("AllCounters ")?;
        f.debug_map()
            .entries(
                self.desc
                    .iter()
                    .zip(self.counters)
                    .map(|(desc, counter)| (desc.name(), counter.get())),
            )
            .finish()
    }
}

unsafe extern "C" {
    fn kcounters_desc_vmo_start();
    fn kcounters_desc_start();
    fn kcounters_desc_end();
    fn kcounters_arena_start();
    fn kcounters_arena_end();
}

#[used]
#[cfg_attr(target_os = "none", unsafe(link_section = ".kcounter.desc.header"))]
static DESCRIPTOR_VMO_HEADER: [u64; 2] = [
    KCOUNTER_MAGIC, // magic
    1,              // max_cpus
                    // descriptor_table_size is filled in linker.ld
];

/// Define a new kernel counter.
#[macro_export]
macro_rules! kcounter {
    ($var:ident, $name:expr) => {
        #[used]
        #[cfg_attr(target_os = "none", unsafe(link_section = concat!(".bss.kcounter.", $name)))]
        static $var: $crate::util::kcounter::Counter = {
            use $crate::util::kcounter::{Counter, Descriptor};
            #[used]
            #[cfg_attr(target_os = "none", unsafe(link_section = concat!(".kcounter.desc.", $name)))]
            static DESCRIPTOR: Descriptor = Descriptor::new($name);
            Counter::new()
        };
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, size_of};

    /// `AllCounters::get` turns two linker symbols into a slice with
    /// `desc_end.offset_from(desc_start)`, which counts ELEMENTS: the byte
    /// distance between the two symbols has to be an exact multiple of this
    /// size or the pointer arithmetic is undefined and the table is read
    /// misaligned. Nothing in the crate said so, and adding one field to
    /// `Descriptor` is all it takes.
    #[test]
    fn the_descriptor_is_the_64_bytes_the_linker_lays_out() {
        assert_eq!(size_of::<Descriptor>(), 64, "56 de nombre y 8 de tipo");
        assert_eq!(align_of::<Descriptor>(), 8, "el u64 del tipo manda");
        assert_eq!(Descriptor::MAX_NAME_LEN, 56);
        // The name field is the whole first 56 bytes, so a descriptor's name
        // starts where the previous one ended.
        assert_eq!(size_of::<[u8; Descriptor::MAX_NAME_LEN]>(), 56);
        // And a counter is one machine word, which is what makes the arena a
        // plain array indexed in step with the descriptor table.
        assert_eq!(size_of::<Counter>(), size_of::<usize>());
        assert_eq!(align_of::<Counter>(), align_of::<usize>());
        // `repr(transparent)` over AtomicUsize: the linker writes zeros into
        // `.bss` and that has to BE the counter, with no header of its own.
        assert_eq!(size_of::<Counter>(), size_of::<AtomicUsize>());
        assert_eq!(Counter::new().get(), 0, "un contador recien puesto a cero");
    }

    #[test]
    fn the_vmo_header_matches_what_the_linker_fills_in() {
        // The static in this file is `[u64; 2]` and the struct has three
        // fields: the third, `descriptor_table_size`, is written by linker.ld
        // straight after them. So the struct has to be exactly those two words
        // plus one more, in that order, or the size lands in the wrong place
        // and every reader of the descriptor VMO gets a bogus table length.
        assert_eq!(size_of::<DescriptorVmoHeader>(), 24);
        assert_eq!(align_of::<DescriptorVmoHeader>(), 8);
        assert_eq!(size_of::<u64>() * 2, 16, "lo que cubre el static");
        assert_eq!(
            size_of::<usize>(),
            8,
            "el tercer campo es usize y el enlazador escribe 8 bytes"
        );
        let h = DescriptorVmoHeader::default();
        assert_eq!(h.magic, KCOUNTER_MAGIC);
        assert_eq!(h.max_cpus, 1);
        assert_eq!(h.descriptor_table_size, 0, "lo rellena el enlazador");
        // The static and the struct have to agree on the first two words, and
        // they are written out separately in this file.
        assert_eq!(DESCRIPTOR_VMO_HEADER[0], KCOUNTER_MAGIC);
        assert_eq!(DESCRIPTOR_VMO_HEADER[1], h.max_cpus);
        // The magic is what Zircon's `kcounter` tool looks for; a different
        // number means the tool reports the VMO as not a counter VMO at all.
        assert_eq!(KCOUNTER_MAGIC, 1_547_273_975);
    }

    #[test]
    fn a_name_comes_back_out_the_way_it_went_in() {
        // The fill macro writes the 56 bytes in four blocks of 16 and two of 4,
        // each one guarded by its own length check, so an off-by-one in any of
        // them drops or duplicates a character somewhere in the middle of a
        // name nobody reads closely.
        for name in [
            "",
            "a",
            "vmo.page_alloc",
            "exceptions.pgfault",
            "VmObjectPaged.create",
            "0123456789012345",                 // el limite de un bloque de 16
            "01234567890123456789012345678901", // dos bloques
            "012345678901234567890123456789012345678901234567", // tres
            "0123456789012345678901234567890123456789012345678901", // + un bloque de 4
            "01234567890123456789012345678901234567890123456789012345", // los 56 justos
        ] {
            let d = Descriptor::new(name);
            assert_eq!(d.name(), name, "{name:?}");
        }
        // The 56-byte name is the one with NO trailing NUL, and it still reads
        // back whole: the scan has to fall back to the full width and not stop
        // at a NUL that is not there.
        let full = "01234567890123456789012345678901234567890123456789012345";
        assert_eq!(full.len(), Descriptor::MAX_NAME_LEN);
        let d = Descriptor::new(full);
        assert!(!d.name.contains(&0), "los 56 bytes van sin NUL");
        assert_eq!(d.name(), full);
        // Anything shorter is NUL-padded, and the padding is zeros and not
        // whatever was in the section before.
        let d = Descriptor::new("ab");
        assert_eq!(&d.name[..3], b"ab\0");
        assert!(d.name[2..].iter().all(|&b| b == 0), "el relleno es cero");
    }

    #[test]
    fn reading_a_name_never_panics_whatever_is_in_the_section() {
        // `name()` walks bytes out of a `&'static [Descriptor]` the linker
        // assembled, so it reads whatever is in that section -- not only what
        // `new` put there. The Debug impl used to `unwrap()` the `from_utf8`,
        // which turns one bad byte into a panic while PRINTING the counters,
        // and printing them is what you do when something has already gone
        // wrong.
        let mut d = Descriptor::new("vmo.page_alloc");
        // A truncated multi-byte character: valid UTF-8 up to a point.
        d.name = [0u8; 56];
        d.name[..2].copy_from_slice(&[b'o', b'k']);
        d.name[2] = 0xC3; // primer byte de una e con tilde, sin el segundo
        assert_eq!(d.name(), "ok", "se queda con lo que era valido");
        // Nothing valid at all.
        d.name = [0xFF; 56];
        assert_eq!(d.name(), "");
        // All NULs is the empty name, not a panic.
        d.name = [0u8; 56];
        assert_eq!(d.name(), "");
        // A NUL in the middle ends the name there, which is what the format
        // says and what the C tool does.
        d.name = [b'x'; 56];
        d.name[3] = 0;
        assert_eq!(d.name(), "xxx");
    }

    #[test]
    fn a_counter_adds_up_and_starts_at_zero() {
        // `add` is `fetch_add(Relaxed)`: the counters are only ever read for a
        // dump, so ordering does not matter, but the SUM does -- a counter that
        // stored instead of added would report the last event rather than how
        // many there were, and that reads as a plausible number.
        let c = Counter::new();
        assert_eq!(c.get(), 0);
        c.add(1);
        c.add(1);
        assert_eq!(c.get(), 2, "suma, no sobreescribe");
        c.add(0);
        assert_eq!(c.get(), 2);
        c.add(40);
        assert_eq!(c.get(), 42);
        // Default is the same as new, which the `kcounter!` macro relies on:
        // the static lands in `.bss` and `.bss` is zeros.
        assert_eq!(Counter::default().get(), Counter::new().get());
    }

    #[test]
    fn the_descriptor_type_is_the_number_zircon_reads() {
        // `repr(u64)` and the values are the on-disk format: a `Sum` written as
        // anything but 1 makes the tool add up a counter as a minimum or a
        // maximum, which gives a number that looks like a number.
        assert_eq!(DescriptorType::Padding as u64, 0);
        assert_eq!(DescriptorType::Sum as u64, 1);
        assert_eq!(DescriptorType::Min as u64, 2);
        assert_eq!(DescriptorType::Max as u64, 3);
        assert_eq!(size_of::<DescriptorType>(), 8);
        // And every descriptor this crate makes is a Sum, which is what every
        // `kcounter!` in the tree means: how many times something happened.
        let d = Descriptor::new("x");
        assert_eq!(d.desc_type as u64, DescriptorType::Sum as u64);
    }
}
