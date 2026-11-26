//! The kernel's own window onto the memory of a device a process drives.
//!
//! A process that drives a device maps the device's memory for itself. The
//! kernel still reads some of it. A framebuffer a GPU's driver placed in
//! VRAM is copied to the panel, read for the pointer's image and handed out
//! by PRIME through the kernel's alias of its address (`phys_to_virt`), and
//! that alias is mapped only where the boot's map of memory reaches: on a
//! PC, a BAR above the top of RAM is in it as far as the boot framebuffer
//! goes, and no further. The bus scan maps a device's registers for whoever
//! claims it. The rest is mapped here, when the driver tells the kernel of
//! a range of it (`gem_mmap::register_device_memory`), which is when the
//! kernel's own driver maps it for itself.
//!
//! That is once processes run, and the kernel's half of an address space is
//! a copy of the top-level entries of its tables, taken when the address
//! space is made: a mapping made later is in all of them only below an
//! entry that was already there. So the bus scan makes those entries, for
//! every BAR of a device it leaves to a process ([`reach`]).

use alloc::sync::Arc;

pub use crate::builder::IoMapper;

/// The mapper the bus scan was given: the kernel's page tables.
static MAPPER: lock::Mutex<Option<Arc<dyn IoMapper>>> = lock::Mutex::new(None);

/// Keep the bus scan's `mapper` for [`map`].
pub fn keep_mapper(mapper: &Arc<dyn IoMapper>) {
    *MAPPER.lock() = Some(mapper.clone());
}

/// What a top-level entry of the kernel's tables spans, where it spans
/// least (riscv64's Sv39; 512 times as much on the others).
const TOP_SPAN: usize = 1 << 30;

/// A page: what is asked of the mapper at a time.
const PAGE: usize = 4096;

/// At the bus scan, for a device it leaves to a process: make the top of
/// the kernel's tables reach each of its `bars` (`(base, length)`, `(0, 0)`
/// for none), by mapping one page under every top-level entry a BAR is
/// below. Nothing else may be there yet. RAM and the boot framebuffer have
/// theirs from the boot's own map; the BARs of a second GPU, on a machine
/// that decodes above 4 GiB, are hundreds of GiB from either. Without it
/// the window [`map`] makes when that GPU's driver registers a buffer
/// would be in the driver's address space and in no other, and the first
/// client to show the buffer would fault in the kernel.
pub fn reach(bars: &[(usize, usize)]) {
    let Some(mapper) = MAPPER.lock().clone() else {
        return;
    };
    for &(base, len) in bars {
        for pa in spans(base, len) {
            // Its answer is not needed: a page that cannot be mapped here
            // is asked for again by `map`, which is what says no.
            mapper.query_or_map(pa, PAGE);
        }
    }
}

/// The first page of `[base, base + len)` and of every [`TOP_SPAN`] that
/// begins inside it.
fn spans(base: usize, len: usize) -> impl Iterator<Item = usize> {
    let end = match base.checked_add(len) {
        Some(end) if base != 0 && len != 0 => end,
        _ => base,
    };
    let first = (base < end).then_some(base & !(PAGE - 1));
    let later = (base / TOP_SPAN + 1..)
        .map_while(|n| n.checked_mul(TOP_SPAN))
        .take_while(move |pa| *pa < end);
    first.into_iter().chain(later)
}

/// Make sure the kernel's window onto the device memory at
/// `[phys, phys + len)` is mapped, uncached. `alias` is where the kernel
/// reads physical memory (its `phys_to_virt`). Whether the range is
/// mapped: not on a machine whose bus scan had no mapper, and not where
/// the mapper puts device memory anywhere but at its alias, which is
/// where the kernel is going to read it.
pub fn map(phys: u64, len: u64, alias: impl Fn(usize) -> usize) -> bool {
    // With no lock held: a mapping is page tables written, of a range that
    // may be a whole BAR.
    let mapper = MAPPER.lock().clone();
    mapper.is_some_and(|mapper| map_with(&*mapper, phys, len, alias))
}

/// [`map`] with `mapper`. Every page is asked for: a mapper answers for a
/// range by its first page, and a range may begin in what is mapped already
/// (the boot framebuffer, at the base of the console GPU's BAR1) and end
/// past it. A page that is mapped is left as it is.
fn map_with(mapper: &dyn IoMapper, phys: u64, len: u64, alias: impl Fn(usize) -> usize) -> bool {
    const PAGE: u64 = self::PAGE as u64;
    let Some(end) = phys.checked_add(len) else {
        return false;
    };
    let mut pa = phys & !(PAGE - 1);
    while pa < end {
        match mapper.query_or_map(pa as usize, PAGE as usize) {
            Some(va) if va == alias(pa as usize) => {}
            _ => return false,
        }
        pa += PAGE;
    }
    true
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use alloc::vec::Vec;

    /// Maps what is asked at its alias ([`SAME`]), but nothing from `hole`
    /// on, and `elsewhere` somewhere else.
    struct Fake {
        asked: lock::Mutex<Vec<(usize, usize)>>,
        hole: usize,
        elsewhere: usize,
    }

    /// The alias of these tests: physical memory is read where it is.
    const SAME: fn(usize) -> usize = |pa| pa;

    impl Fake {
        fn new() -> Fake {
            Fake {
                asked: lock::Mutex::new(Vec::new()),
                hole: usize::MAX,
                elsewhere: usize::MAX,
            }
        }
    }

    impl IoMapper for Fake {
        fn query_or_map(&self, paddr: usize, size: usize) -> Option<usize> {
            self.asked.lock().push((paddr, size));
            if paddr >= self.hole {
                return None;
            }
            Some(if paddr == self.elsewhere {
                paddr + 0x1000_0000
            } else {
                paddr
            })
        }
    }

    /// Every page of the range is asked for, one at a time, from the page
    /// its first byte is in to the page its last byte is in: the mapper
    /// answers for a range by its first page alone, and that page is
    /// already mapped when the range begins in the boot framebuffer.
    #[test]
    fn every_page_of_the_range_is_asked_for() {
        let fake = Fake::new();
        assert!(map_with(&fake, 0xE000_0800, 0x2000, SAME));
        assert_eq!(
            *fake.asked.lock(),
            [
                (0xE000_0000, 0x1000),
                (0xE000_1000, 0x1000),
                (0xE000_2000, 0x1000)
            ]
        );
        // A range that ends on a page boundary asks for no page past it.
        let fake = Fake::new();
        assert!(map_with(&fake, 0xE000_0000, 0x1000, SAME));
        assert_eq!(*fake.asked.lock(), [(0xE000_0000, 0x1000)]);
        // An empty range is no page at all.
        let fake = Fake::new();
        assert!(map_with(&fake, 0xE000_0000, 0, SAME));
        assert!(fake.asked.lock().is_empty());
    }

    /// A range the mapper cannot map whole is not mapped as far as the
    /// kernel is concerned: it would read the part that is missing.
    #[test]
    fn a_range_with_a_page_that_cannot_be_mapped_is_refused() {
        let fake = Fake {
            hole: 0xE000_2000,
            ..Fake::new()
        };
        assert!(map_with(&fake, 0xE000_0000, 0x2000, SAME));
        assert!(!map_with(&fake, 0xE000_0000, 0x2001, SAME));
    }

    /// The kernel reads the memory at its alias. A mapper that maps device
    /// memory somewhere else (riscv, for a BAR above 512 GiB) has not
    /// mapped what the kernel is going to read.
    #[test]
    fn a_page_mapped_anywhere_but_at_its_alias_is_refused() {
        let fake = Fake {
            elsewhere: 0xE000_1000,
            ..Fake::new()
        };
        assert!(map_with(&fake, 0xE000_0000, 0x1000, SAME));
        assert!(!map_with(&fake, 0xE000_0000, 0x2000, SAME));
    }

    /// A range that wraps the address space is no range.
    #[test]
    fn a_range_past_the_end_of_the_address_space_is_refused() {
        let fake = Fake::new();
        assert!(!map_with(&fake, u64::MAX - 0xFFF, 0x2000, SAME));
        assert!(fake.asked.lock().is_empty());
    }

    /// One page under each top-level entry a BAR is below: its first, and
    /// the first of every GiB that begins inside it. A BAR of a GPU is
    /// seldom more than one; a 4 GiB one is four on riscv64.
    #[test]
    fn a_bar_is_reached_by_a_page_under_each_top_level_entry() {
        let pages = |base, len| spans(base, len).collect::<Vec<_>>();
        // 256 MiB of VRAM above 4 GiB, as the firmware of a PC places it.
        assert_eq!(pages(0x383f_e000_0000, 0x1000_0000), [0x383f_e000_0000]);
        // 4 GiB.
        assert_eq!(
            pages(0x8_0000_0000, 0x1_0000_0000),
            [0x8_0000_0000, 0x8_4000_0000, 0x8_8000_0000, 0x8_c000_0000]
        );
        // One that begins inside a GiB and ends in the next.
        assert_eq!(
            pages(0xe000_0800, 0x4000_0000),
            [0xe000_0000, 0x1_0000_0000]
        );
        // No BAR, the upper half of a 64-bit one, an I/O BAR.
        assert!(pages(0, 0).is_empty());
        assert!(pages(0xe000_0000, 0).is_empty());
        assert!(pages(0, 0x1000).is_empty());
        // A length the address space has no room for is no BAR.
        assert!(pages(usize::MAX - 0xfff, 0x2000).is_empty());
        // And one that ends at its very top stops there.
        assert_eq!(
            pages(usize::MAX - 0x3fff_ffff, 0x3fff_ffff),
            [usize::MAX - 0x3fff_ffff]
        );
    }

    /// The scan's mapper is asked for those pages, whatever it answers,
    /// and nothing is asked when there was no scan.
    #[test]
    fn the_bars_of_a_device_left_to_a_process_are_reached_at_the_scan() {
        let _serialised = crate::test_globals::LOCK.lock();
        *MAPPER.lock() = None;
        reach(&[(0xe000_0000, 0x100_0000)]);
        let fake = Arc::new(Fake {
            hole: 0xf000_0000,
            ..Fake::new()
        });
        let mapper: Arc<dyn IoMapper> = fake.clone();
        keep_mapper(&mapper);
        reach(&[
            (0xe000_0000, 0x100_0000),
            (0, 0),
            (0xf000_0000, 0x200_0000),
            (0x8_0000_0000, 0x8000_0000),
        ]);
        assert_eq!(
            *fake.asked.lock(),
            [
                (0xe000_0000, 0x1000),
                (0xf000_0000, 0x1000),
                (0x8_0000_0000, 0x1000),
                (0x8_4000_0000, 0x1000)
            ]
        );
        *MAPPER.lock() = None;
    }

    /// With no bus scan there is no mapper, and nothing is said mapped.
    /// With one, the range is mapped through it.
    #[test]
    fn the_scans_mapper_is_the_one_that_maps() {
        let _serialised = crate::test_globals::LOCK.lock();
        *MAPPER.lock() = None;
        assert!(!map(0xE000_0000, 0x1000, SAME));
        let fake = Arc::new(Fake::new());
        let mapper: Arc<dyn IoMapper> = fake.clone();
        keep_mapper(&mapper);
        assert!(map(0xE000_0000, 0x1000, SAME));
        assert_eq!(*fake.asked.lock(), [(0xE000_0000, 0x1000)]);
        *MAPPER.lock() = None;
    }
}
