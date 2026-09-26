//! The all-or-nothing contract shared by the two ways `stack_guard` takes a
//! permission away from a run of pages.
//!
//! `bare/stack_guard.rs` does it twice: a **guard band** around a coroutine
//! stack loses every permission, so an overflow faults; a **quarantined**
//! freed stack loses only WRITE, so a stale read stays silent and the
//! use-after-free writer faults at its own instruction. The two differ in
//! three lines and used to be written out twice, which is how they came to
//! disagree about what a huge mapping should be called and how many flushes
//! the end of the job needs.
//!
//! The decision is here, where every build compiles it and the host suite can
//! run it against a page table; `bare/` keeps the registry, the logging and
//! the shootdowns. The rule the whole module exists to keep is that a band is
//! either taken away completely or left **exactly** as it was found: any
//! surprise -- a huge entry covering it, a page that is not mapped, flags that
//! differ from one page to the next, a read-back that does not show the
//! permission gone -- refuses the whole band, and the scheduler falls back to
//! its soft canary.

use crate::common::vm::{GenericPageTable, PageSize};
use crate::{MMUFlags, PAGE_SIZE};

/// What is being taken away from the band.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Take {
    /// Every permission, so the page faults on any access at all: the guard
    /// bands around a live coroutine stack.
    Everything,
    /// The write permission alone: the quarantine on a freed stack, where a
    /// benign stale read is not the corruptor and should not fault, but every
    /// stale write is.
    WriteOnly,
}

impl Take {
    /// The flags a page that currently carries `had` is to be given.
    pub fn applied_to(self, had: MMUFlags) -> MMUFlags {
        match self {
            Take::Everything => MMUFlags::empty(),
            // Not `had - WRITE`: `bitflags` would be the same thing, but this
            // is the form the architectures' `From<MMUFlags>` is read against.
            Take::WriteOnly => MMUFlags::from_bits_truncate(had.bits() & !MMUFlags::WRITE.bits()),
        }
    }

    /// Whether a page whose flags are `flags` still has anything to take away.
    ///
    /// For [`Take::WriteOnly`] an empty flag set cannot contain WRITE, so this
    /// one test is the whole check -- it used to be written twice, and neither
    /// copy could then be shown to do anything.
    pub fn present_in(self, flags: MMUFlags) -> bool {
        match self {
            Take::Everything => !flags.is_empty(),
            Take::WriteOnly => flags.contains(MMUFlags::WRITE),
        }
    }
}

/// Why a band cannot be taken away, in the words the boot log uses.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum BandRefusal {
    /// Zero-sized, or a base or length that is not a whole number of pages.
    NotPageAligned,
    /// Some page of the band has no mapping at all.
    NotMapped,
    /// A huge entry covers the band and there was no frame left to split it.
    NoSplitFrames,
    /// A 2 MiB entry covers the band. On riscv64 and aarch64 the kernel heap
    /// is a window of the physmap, which is mapped with 2 MiB pages, so every
    /// band arrived like this until the callers learned to split first.
    Huge2M,
    /// A 1 GiB entry covers the band.
    Huge1G,
    /// The permission being taken away is not there to take: a second install
    /// over the same band, or a region that is already read-only.
    NothingToTakeAway,
    /// The pages of the band do not agree, so one recorded value could not put
    /// them back.
    NotUniform,
    /// The registry that remembers the band's original flags is full, so
    /// nothing could record how to put it back.
    RegistryFull,
    /// Writing the new flags failed part-way through. The band is rolled back.
    EditFailed,
    /// The new flags were written and the read-back still shows the permission
    /// there. The band is rolled back, because a guard that does not guard
    /// reported as a success is the failure this whole path exists to catch.
    NoEffect,
}

impl BandRefusal {
    /// The reason as the log prints it, after the caller's own noun ("band" or
    /// "region").
    pub fn reason(self, take: Take) -> &'static str {
        match self {
            BandRefusal::NotPageAligned => "is not page aligned",
            BandRefusal::NotMapped => "is not mapped",
            BandRefusal::NoSplitFrames => "has no reserved frame left to split its huge mapping",
            BandRefusal::Huge2M => "is covered by a 2 MiB PTE",
            BandRefusal::Huge1G => "is covered by a 1 GiB PTE",
            BandRefusal::NothingToTakeAway => match take {
                Take::Everything => "already has no permissions (double install?)",
                Take::WriteOnly => "is already not writable",
            },
            BandRefusal::NotUniform => "is not a uniform run of mapped 4 KiB pages",
            BandRefusal::RegistryFull => "has no registry slot left",
            BandRefusal::EditFailed => match take {
                Take::Everything => "could not have its permissions cleared",
                Take::WriteOnly => "could not have its write permission cleared",
            },
            BandRefusal::NoEffect => match take {
                Take::Everything => "is still readable after clearing its permissions",
                Take::WriteOnly => "is still writable after clearing its write permission",
            },
        }
    }
}

/// Name a page size that is not 4 KiB, so the log says which one it was.
pub fn refuse_huge(size: PageSize) -> BandRefusal {
    match size {
        PageSize::Size2M => BandRefusal::Huge2M,
        _ => BandRefusal::Huge1G,
    }
}

/// Whether the band is a whole number of pages starting on one.
///
/// Asked twice: once by the caller **before** anything touches the page table,
/// because splitting the huge entries that cover a band is not free and a band
/// that was never a band should not spend the reserved frames; and again
/// inside [`survey`], which is the one that cannot be skipped.
pub fn check_alignment(base: usize, size: usize) -> Result<(), BandRefusal> {
    if size == 0 || !base.is_multiple_of(PAGE_SIZE) || !size.is_multiple_of(PAGE_SIZE) {
        return Err(BandRefusal::NotPageAligned);
    }
    Ok(())
}

/// Survey the band without touching it: what every one of its pages must look
/// like before any of them is edited.
///
/// Answers the flags all of them share, which is the single value the caller
/// records so that putting the band back is one write per page and cannot fail
/// for any reason ruled out here.
pub fn survey<PT: GenericPageTable + ?Sized>(
    pt: &PT,
    base: usize,
    size: usize,
    take: Take,
) -> Result<MMUFlags, BandRefusal> {
    check_alignment(base, size)?;
    let expect = match pt.query(base) {
        Ok((_, flags, PageSize::Size4K)) => flags,
        Ok((_, _, huge)) => return Err(refuse_huge(huge)),
        Err(_) => return Err(BandRefusal::NotMapped),
    };
    if !take.present_in(expect) {
        return Err(BandRefusal::NothingToTakeAway);
    }
    for off in (0..size).step_by(PAGE_SIZE) {
        match pt.query(base + off) {
            // A page whose frame is physical 0 is refused, and not because it
            // could not be guarded: clearing the flags of such an entry leaves
            // it all-zero, which `is_unused()` reads as "no mapping here" --
            // and `update` refuses to touch an unused entry, so the band could
            // never be put back. No `.bss` page is ever backed by frame 0, so
            // this only ever fires on something already wrong.
            Ok((paddr, flags, PageSize::Size4K))
                if flags == expect && paddr & !(PAGE_SIZE - 1) != 0 => {}
            _ => return Err(BandRefusal::NotUniform),
        }
    }
    Ok(expect)
}

/// Write `flags` into every page of the band, reporting the first failure.
///
/// `update_no_shootdown` because a synchronous shootdown per page would be
/// O(pages x ack-wait) on a path that runs on every executor creation; the
/// caller issues one remote flush for the whole band. The *local* TLB is still
/// invalidated per page.
pub fn set_band_flags<PT: GenericPageTable + ?Sized>(
    pt: &mut PT,
    base: usize,
    size: usize,
    flags: MMUFlags,
) -> Result<(), ()> {
    for off in (0..size).step_by(PAGE_SIZE) {
        if pt
            .update_no_shootdown(base + off, None, Some(flags))
            .is_err()
        {
            return Err(());
        }
    }
    Ok(())
}

/// Read the band back and say whether the permission really went away.
///
/// Verify rather than trust: "an empty `MMUFlags` converts to an entry with no
/// present bit", and "clearing WRITE leaves the page readable and not
/// writable", are per-architecture details of `From<MMUFlags>`. Reading them
/// back is what turns them into something this module knows rather than
/// assumes, and an architecture where they do not hold degrades to the soft
/// canary instead of reporting a guard that does not guard.
///
/// A page that cannot be queried at all counts as taken away for
/// [`Take::Everything`] -- there is nothing left to fault through -- and as
/// *not* taken away for [`Take::WriteOnly`], which needs the page to still be
/// there and merely read-only.
pub fn took_effect<PT: GenericPageTable + ?Sized>(
    pt: &PT,
    base: usize,
    size: usize,
    take: Take,
) -> bool {
    for off in (0..size).step_by(PAGE_SIZE) {
        let gone = match pt.query(base + off) {
            Ok((_, flags, _)) => !take.present_in(flags),
            Err(_) => take == Take::Everything,
        };
        if !gone {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::vm::{Page, PagingError, PagingResult};
    use crate::{PhysAddr, VirtAddr};
    use alloc::collections::{BTreeMap, BTreeSet};

    const K: usize = PAGE_SIZE;
    const BASE: VirtAddr = 0xffff_8000_0010_0000;

    /// A page table that is only a map, so a band can be built with exactly
    /// the shape a test is about.
    #[derive(Default)]
    struct Table {
        pages: BTreeMap<VirtAddr, (PhysAddr, MMUFlags, PageSize)>,
        /// Addresses whose `update` reports a failure, so the caller's
        /// roll-back path can be walked.
        refuse_update: BTreeSet<VirtAddr>,
        /// Every address `update` was asked about, in order.
        written: alloc::vec::Vec<VirtAddr>,
    }

    impl Table {
        /// A run of `n` ordinary 4 KiB pages with the same flags, backed by
        /// frames nobody else uses.
        fn band(n: usize, flags: MMUFlags) -> Self {
            let mut t = Table::default();
            for i in 0..n {
                t.pages
                    .insert(BASE + i * K, (0x20_0000 + i * K, flags, PageSize::Size4K));
            }
            t
        }
        fn flags_at(&self, off: usize) -> MMUFlags {
            self.pages.get(&(BASE + off)).unwrap().1
        }
    }

    impl GenericPageTable for Table {
        fn table_phys(&self) -> PhysAddr {
            0x1000
        }
        fn map(&mut self, page: Page, paddr: PhysAddr, flags: MMUFlags) -> PagingResult {
            self.pages.insert(page.vaddr, (paddr, flags, page.size));
            Ok(())
        }
        fn unmap(&mut self, vaddr: VirtAddr) -> PagingResult<(PhysAddr, PageSize)> {
            let (paddr, _, size) = self.pages.remove(&vaddr).ok_or(PagingError::NotMapped)?;
            Ok((paddr, size))
        }
        fn update(
            &mut self,
            vaddr: VirtAddr,
            paddr: Option<PhysAddr>,
            flags: Option<MMUFlags>,
        ) -> PagingResult<PageSize> {
            self.written.push(vaddr);
            if self.refuse_update.contains(&vaddr) {
                return Err(PagingError::NotMapped);
            }
            let entry = self.pages.get_mut(&vaddr).ok_or(PagingError::NotMapped)?;
            if let Some(p) = paddr {
                entry.0 = p;
            }
            if let Some(f) = flags {
                entry.1 = f;
            }
            Ok(entry.2)
        }
        fn query(&self, vaddr: VirtAddr) -> PagingResult<(PhysAddr, MMUFlags, PageSize)> {
            self.pages
                .get(&vaddr)
                .copied()
                .ok_or(PagingError::NotMapped)
        }
    }

    const RW: MMUFlags = MMUFlags::from_bits_truncate(
        MMUFlags::READ.bits() | MMUFlags::WRITE.bits() | MMUFlags::USER.bits(),
    );
    const RO: MMUFlags =
        MMUFlags::from_bits_truncate(MMUFlags::READ.bits() | MMUFlags::USER.bits());

    // ── what the band has to look like before anything is touched ────────

    #[test]
    fn a_uniform_band_reports_the_one_value_that_puts_it_back() {
        let t = Table::band(4, RW);
        assert_eq!(survey(&t, BASE, 4 * K, Take::Everything), Ok(RW));
        assert_eq!(survey(&t, BASE, 4 * K, Take::WriteOnly), Ok(RW));
    }

    #[test]
    fn a_band_that_is_not_a_whole_number_of_pages_is_refused() {
        let t = Table::band(4, RW);
        // Zero-sized, a base off the page grid, and a length that is not a
        // multiple of the page: none of the three can be surveyed page by
        // page, and the loop below would silently do nothing for the first.
        for (base, size) in [
            (BASE, 0usize),
            (BASE + 1, 2 * K),
            (BASE, 2 * K + 1),
            (BASE, K - 1),
        ] {
            assert_eq!(
                survey(&t, base, size, Take::Everything),
                Err(BandRefusal::NotPageAligned),
                "{base:#x}+{size:#x}"
            );
        }
    }

    #[test]
    fn the_alignment_is_asked_before_the_page_table_is_touched() {
        // The caller asks this one first, because splitting the huge entries
        // that cover a band spends frames out of a small reserved pool and a
        // band that was never a band should not spend them.
        assert_eq!(check_alignment(BASE, 2 * K), Ok(()));
        assert_eq!(check_alignment(BASE, 0), Err(BandRefusal::NotPageAligned));
        assert_eq!(
            check_alignment(BASE + 8, K),
            Err(BandRefusal::NotPageAligned)
        );
        assert_eq!(
            check_alignment(BASE, K + 8),
            Err(BandRefusal::NotPageAligned)
        );
    }

    #[test]
    fn a_band_covered_by_a_huge_entry_says_which_one() {
        // riscv64 and aarch64 map the kernel heap as a window of the physmap,
        // which is 2 MiB pages, so every band arrived here until the callers
        // learned to split first -- and every coroutine stack on those
        // machines ran with a soft canary and no hard guard.
        let mut t = Table::default();
        t.pages.insert(BASE, (0x20_0000, RW, PageSize::Size2M));
        assert_eq!(
            survey(&t, BASE, 2 * K, Take::Everything),
            Err(BandRefusal::Huge2M)
        );
        t.pages.insert(BASE, (0x20_0000, RW, PageSize::Size1G));
        assert_eq!(
            survey(&t, BASE, 2 * K, Take::Everything),
            Err(BandRefusal::Huge1G)
        );
    }

    #[test]
    fn a_band_with_a_page_missing_is_refused_wherever_the_hole_is() {
        let mut t = Table::band(4, RW);
        t.pages.remove(&BASE);
        assert_eq!(
            survey(&t, BASE, 4 * K, Take::Everything),
            Err(BandRefusal::NotMapped),
            "the head page is the one the flags are read from"
        );
        let mut t = Table::band(4, RW);
        t.pages.remove(&(BASE + 3 * K));
        assert_eq!(
            survey(&t, BASE, 4 * K, Take::Everything),
            Err(BandRefusal::NotUniform),
            "the last page is looked at too"
        );
    }

    #[test]
    fn a_band_whose_pages_do_not_agree_is_refused() {
        // One recorded value has to put every page back, so a band that
        // disagrees cannot be restored from it.
        let mut t = Table::band(4, RW);
        t.pages.get_mut(&(BASE + 2 * K)).unwrap().1 = RO;
        assert_eq!(
            survey(&t, BASE, 4 * K, Take::Everything),
            Err(BandRefusal::NotUniform)
        );
    }

    #[test]
    fn a_page_backed_by_frame_zero_is_refused() {
        // Clearing the flags of such an entry leaves it all-zero, which
        // `is_unused()` reads as "no mapping here" -- and `update` refuses to
        // touch an unused entry, so the band could never be put back.
        let mut t = Table::band(3, RW);
        t.pages.get_mut(&(BASE + K)).unwrap().0 = 0;
        assert_eq!(
            survey(&t, BASE, 3 * K, Take::Everything),
            Err(BandRefusal::NotUniform)
        );
        // Only the frame number matters, not the offset bits.
        let mut t = Table::band(3, RW);
        t.pages.get_mut(&(BASE + K)).unwrap().0 = 0xfff;
        assert_eq!(
            survey(&t, BASE, 3 * K, Take::Everything),
            Err(BandRefusal::NotUniform)
        );
    }

    #[test]
    fn a_band_that_has_nothing_left_to_take_is_refused() {
        // A second install over the same band, and a region that is already
        // read-only: in both cases the recorded "original" flags would be the
        // taken-away ones, and putting the band back would leave it taken away.
        let t = Table::band(2, MMUFlags::empty());
        assert_eq!(
            survey(&t, BASE, 2 * K, Take::Everything),
            Err(BandRefusal::NothingToTakeAway)
        );
        let t = Table::band(2, RO);
        assert_eq!(
            survey(&t, BASE, 2 * K, Take::WriteOnly),
            Err(BandRefusal::NothingToTakeAway)
        );
    }

    #[test]
    fn a_read_only_band_can_still_be_guarded_though_it_cannot_be_quarantined() {
        // The one point in the domain where the two jobs differ: a band with
        // no WRITE has nothing for the quarantine to take, and everything for
        // the guard to take.
        let t = Table::band(2, RO);
        assert_eq!(survey(&t, BASE, 2 * K, Take::Everything), Ok(RO));
        assert_eq!(
            survey(&t, BASE, 2 * K, Take::WriteOnly),
            Err(BandRefusal::NothingToTakeAway)
        );
    }

    // ── what is written ──────────────────────────────────────────────────

    #[test]
    fn taking_everything_leaves_nothing_and_taking_write_leaves_the_rest() {
        assert_eq!(Take::Everything.applied_to(RW), MMUFlags::empty());
        assert_eq!(Take::WriteOnly.applied_to(RW), RO);
        // ...and taking WRITE off a band that has none is the band itself.
        assert_eq!(Take::WriteOnly.applied_to(RO), RO);
    }

    #[test]
    fn every_page_of_the_band_is_written_and_nothing_past_it() {
        let mut t = Table::band(5, RW);
        assert_eq!(set_band_flags(&mut t, BASE, 3 * K, RO), Ok(()));
        assert_eq!(t.written, alloc::vec![BASE, BASE + K, BASE + 2 * K]);
        assert_eq!(t.flags_at(0), RO);
        assert_eq!(t.flags_at(2 * K), RO);
        assert_eq!(t.flags_at(3 * K), RW, "the page past the band is untouched");
    }

    #[test]
    fn writing_stops_at_the_first_page_that_refuses() {
        // The caller rolls the whole band back on this, so it must report
        // rather than carry on and leave a half-edited band behind.
        let mut t = Table::band(4, RW);
        t.refuse_update.insert(BASE + K);
        assert_eq!(
            set_band_flags(&mut t, BASE, 4 * K, MMUFlags::empty()),
            Err(())
        );
        assert_eq!(t.written, alloc::vec![BASE, BASE + K]);
    }

    // ── and whether it took ──────────────────────────────────────────────

    #[test]
    fn the_read_back_is_what_says_the_permission_went_away() {
        let mut t = Table::band(3, RW);
        assert!(!took_effect(&t, BASE, 3 * K, Take::Everything));
        set_band_flags(&mut t, BASE, 3 * K, MMUFlags::empty()).unwrap();
        assert!(took_effect(&t, BASE, 3 * K, Take::Everything));

        let mut t = Table::band(3, RW);
        assert!(!took_effect(&t, BASE, 3 * K, Take::WriteOnly));
        set_band_flags(&mut t, BASE, 3 * K, RO).unwrap();
        assert!(took_effect(&t, BASE, 3 * K, Take::WriteOnly));
        // ...and read-only is not taken away, which is the whole point of the
        // quarantine: a stale read must stay silent.
        assert!(!took_effect(&t, BASE, 3 * K, Take::Everything));
    }

    #[test]
    fn one_page_that_did_not_take_refuses_the_whole_band() {
        let mut t = Table::band(4, RW);
        set_band_flags(&mut t, BASE, 4 * K, MMUFlags::empty()).unwrap();
        t.pages.get_mut(&(BASE + 3 * K)).unwrap().1 = RW;
        assert!(
            !took_effect(&t, BASE, 4 * K, Take::Everything),
            "a band with one page still readable is a guard band with a hole"
        );
    }

    #[test]
    fn a_page_that_vanished_is_guarded_but_is_not_quarantined() {
        // Nothing is left to fault through, so for a guard band it is gone;
        // for the quarantine the page has to still be there and merely
        // read-only, so a page that cannot be queried is not a quarantine.
        let mut t = Table::band(2, RW);
        set_band_flags(&mut t, BASE, 2 * K, MMUFlags::empty()).unwrap();
        t.pages.remove(&(BASE + K));
        assert!(took_effect(&t, BASE, 2 * K, Take::Everything));

        let mut t = Table::band(2, RW);
        set_band_flags(&mut t, BASE, 2 * K, RO).unwrap();
        t.pages.remove(&(BASE + K));
        assert!(!took_effect(&t, BASE, 2 * K, Take::WriteOnly));
    }

    // ── what the log says ────────────────────────────────────────────────

    #[test]
    fn every_refusal_reads_differently_for_the_two_jobs_where_it_has_to() {
        // The three that depend on what was being taken away say so; a log
        // line that says "already has no permissions" about a quarantine
        // sends whoever reads it to the wrong half of this module.
        for r in [
            BandRefusal::NothingToTakeAway,
            BandRefusal::EditFailed,
            BandRefusal::NoEffect,
        ] {
            assert_ne!(
                r.reason(Take::Everything),
                r.reason(Take::WriteOnly),
                "{r:?}"
            );
        }
        // ...and no reason is empty or starts with a capital, since the log
        // puts the caller's own noun in front of it.
        for r in [
            BandRefusal::NotPageAligned,
            BandRefusal::NotMapped,
            BandRefusal::NoSplitFrames,
            BandRefusal::Huge2M,
            BandRefusal::Huge1G,
            BandRefusal::NothingToTakeAway,
            BandRefusal::NotUniform,
            BandRefusal::RegistryFull,
            BandRefusal::EditFailed,
            BandRefusal::NoEffect,
        ] {
            for take in [Take::Everything, Take::WriteOnly] {
                let s = r.reason(take);
                assert!(!s.is_empty(), "{:?}", r);
                assert!(s.starts_with(char::is_lowercase), "{:?}: {}", r, s);
            }
        }
    }

    #[test]
    fn a_huge_entry_is_named_by_its_size() {
        assert_eq!(refuse_huge(PageSize::Size2M), BandRefusal::Huge2M);
        assert_eq!(refuse_huge(PageSize::Size1G), BandRefusal::Huge1G);
    }
}
