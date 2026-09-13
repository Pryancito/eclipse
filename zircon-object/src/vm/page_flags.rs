//! Per-page MMU flags of a `VmMapping`, run-length encoded.
//!
//! A mapping used to carry `Vec<MMUFlags>` with one entry per page: 8 bytes
//! of kernel heap for every 4 KiB of address space, whether or not anything
//! was ever mapped there. Firefox reserves address space by the gigabyte (its
//! JIT and wasm reservations are `mmap(PROT_NONE)` of 1-16 GiB), so a single
//! such `mmap` cost 2-32 MiB of heap and a browser session held ~350 MiB of
//! them -- the "5 x 64 MiB and 63 x 4 MiB blocks" of the OOM in #1135, named
//! by the `[bigalloc]` tripwire as `VmMapping::new` -> `vec![flags; pages]`.
//!
//! Protection is uniform over a mapping except where a partial `mprotect`
//! changed a sub-range, so the flags are stored as runs: a mapping with a
//! handful of `mprotect`s is a handful of runs whatever its size. A lookup
//! is a binary search over the run starts; a range update rebuilds the run
//! list once, in O(runs).
//!
//! Invariants: `starts` is strictly increasing and begins at 0 whenever the
//! set is non-empty; adjacent runs never carry equal flags; `len` is the
//! total page count and the last run extends to it.

use alloc::vec::Vec;
use core::ops::{Index, Range};
use kernel_hal::MMUFlags;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PageFlags {
    /// Page index at which each run starts.
    starts: Vec<usize>,
    /// Flags of each run.
    runs: Vec<MMUFlags>,
    /// Total number of pages.
    len: usize,
}

impl PageFlags {
    /// `pages` pages of `flags`.
    pub fn uniform(flags: MMUFlags, pages: usize) -> Self {
        let mut out = Self::default();
        out.extend_repeat(flags, pages);
        out
    }

    /// Empty, with room for `runs` runs so that receiving that many via
    /// [`split_off_into`](Self::split_off_into) does not allocate.
    pub fn with_run_capacity(runs: usize) -> Self {
        Self {
            starts: Vec::with_capacity(runs),
            runs: Vec::with_capacity(runs),
            len: 0,
        }
    }

    /// Number of pages.
    pub fn len(&self) -> usize {
        self.len
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of runs (distinct maximal stretches of equal flags).
    pub fn run_count(&self) -> usize {
        self.runs.len()
    }

    /// Index of the run holding `page`; `page < len` required.
    fn run_of(&self, page: usize) -> usize {
        debug_assert!(page < self.len);
        match self.starts.binary_search(&page) {
            Ok(r) => r,
            Err(r) => r - 1,
        }
    }

    /// First page after run `r`.
    fn run_end(&self, r: usize) -> usize {
        self.starts.get(r + 1).copied().unwrap_or(self.len)
    }

    /// Append one run, merging into the last one when the flags are equal.
    /// Does not touch `len`; the caller sets it.
    fn push_run(&mut self, flags: MMUFlags, start: usize) {
        if self.runs.last() == Some(&flags) {
            return;
        }
        self.starts.push(start);
        self.runs.push(flags);
    }

    pub fn get(&self, page: usize) -> Option<&MMUFlags> {
        (page < self.len).then(|| &self.runs[self.run_of(page)])
    }

    pub fn first(&self) -> Option<&MMUFlags> {
        self.runs.first()
    }

    pub fn last(&self) -> Option<&MMUFlags> {
        self.runs.last()
    }

    /// Append `pages` pages of `flags`.
    pub fn extend_repeat(&mut self, flags: MMUFlags, pages: usize) {
        if pages == 0 {
            return;
        }
        self.push_run(flags, self.len);
        self.len += pages;
    }

    /// Keep only the first `pages` pages.
    pub fn truncate(&mut self, pages: usize) {
        if pages >= self.len {
            return;
        }
        if pages == 0 {
            self.clear();
            return;
        }
        let r = self.run_of(pages - 1);
        self.starts.truncate(r + 1);
        self.runs.truncate(r + 1);
        self.len = pages;
    }

    pub fn clear(&mut self) {
        self.starts.clear();
        self.runs.clear();
        self.len = 0;
    }

    /// Drop the first `pages` pages; what follows shifts down to index 0.
    pub fn drain_front(&mut self, pages: usize) {
        if pages == 0 {
            return;
        }
        if pages >= self.len {
            self.clear();
            return;
        }
        let r = self.run_of(pages);
        self.starts.drain(..r);
        self.runs.drain(..r);
        for s in &mut self.starts {
            *s = s.saturating_sub(pages);
        }
        self.len -= pages;
    }

    /// Move the pages from `at` onwards to the end of `tail`, keeping
    /// `[0, at)` here. Only pushes onto `tail`'s vectors, so with enough
    /// capacity reserved there nothing allocates.
    pub fn split_off_into(&mut self, at: usize, tail: &mut Self) {
        if at >= self.len {
            return;
        }
        let r = self.run_of(at);
        let base = tail.len;
        let moved = self.len - at;
        for k in r..self.runs.len() {
            let start = self.starts[k].max(at) - at;
            tail.push_run(self.runs[k], base + start);
        }
        tail.len = base + moved;
        self.truncate(at);
    }

    #[cfg_attr(not(test), allow(dead_code))]
    /// Detach the pages from `at` onwards into a new set.
    pub fn split_off(&mut self, at: usize) -> Self {
        let mut tail = Self::default();
        self.split_off_into(at, &mut tail);
        tail
    }

    /// A copy of the pages in `range`.
    pub fn slice(&self, range: Range<usize>) -> Self {
        assert!(
            range.start <= range.end && range.end <= self.len,
            "page range {range:?} out of range for {} pages",
            self.len
        );
        let mut out = Self::default();
        if range.is_empty() {
            return out;
        }
        let r0 = self.run_of(range.start);
        let r1 = self.run_of(range.end - 1);
        for r in r0..=r1 {
            let start = self.starts[r].max(range.start) - range.start;
            out.push_run(self.runs[r], start);
        }
        out.len = range.end - range.start;
        out
    }

    /// Scratch space for [`update_range_in`](Self::update_range_in): two
    /// vectors with this much capacity make the update allocation-free.
    pub fn scratch_capacity(&self) -> usize {
        // A range update splits at most two runs, so the list grows by at
        // most two.
        self.runs.len() + 2
    }

    /// Replace the flags of every page in `range` with `f(old)`: one rebuild
    /// of the run list, however many pages the range spans. The new list is
    /// built in `scratch` and swapped in, so with `scratch` allocated up
    /// front (see [`scratch_capacity`](Self::scratch_capacity)) nothing here
    /// allocates -- `protect` calls this holding the mapping and page-table
    /// locks, where an allocation failure would halt the CPU with them held
    /// (#1136). On return `scratch` holds the previous list; drop it with
    /// the locks released.
    pub fn update_range_in(
        &mut self,
        range: Range<usize>,
        mut f: impl FnMut(MMUFlags) -> MMUFlags,
        scratch: &mut (Vec<usize>, Vec<MMUFlags>),
    ) {
        assert!(
            range.end <= self.len,
            "page range {range:?} out of range for {} pages",
            self.len
        );
        if range.is_empty() {
            return;
        }
        let (starts, runs) = scratch;
        starts.clear();
        runs.clear();
        let mut push = |start: usize, flags: MMUFlags| {
            if runs.last() != Some(&flags) {
                starts.push(start);
                runs.push(flags);
            }
        };
        for r in 0..self.runs.len() {
            let (s, e, g) = (self.starts[r], self.run_end(r), self.runs[r]);
            if s < range.start {
                push(s, g);
            }
            let (is, ie) = (s.max(range.start), e.min(range.end));
            if is < ie {
                push(is, f(g));
            }
            if e > range.end {
                push(s.max(range.end), g);
            }
        }
        core::mem::swap(&mut self.starts, starts);
        core::mem::swap(&mut self.runs, runs);
    }

    /// [`update_range_in`](Self::update_range_in) with its own scratch.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn update_range(&mut self, range: Range<usize>, f: impl FnMut(MMUFlags) -> MMUFlags) {
        let cap = self.scratch_capacity();
        let mut scratch = (Vec::with_capacity(cap), Vec::with_capacity(cap));
        self.update_range_in(range, f, &mut scratch);
    }

    #[cfg_attr(not(test), allow(dead_code))]
    /// Set one page's flags.
    pub fn set(&mut self, page: usize, flags: MMUFlags) {
        self.update_range(page..page + 1, |_| flags);
    }

    #[cfg_attr(not(test), allow(dead_code))]
    /// The flags of every page, in order.
    pub fn iter(&self) -> impl Iterator<Item = MMUFlags> + '_ {
        (0..self.len).map(move |i| self[i])
    }
}

impl Index<usize> for PageFlags {
    type Output = MMUFlags;

    fn index(&self, page: usize) -> &MMUFlags {
        assert!(
            page < self.len,
            "page index {page} out of range for {} pages",
            self.len
        );
        &self.runs[self.run_of(page)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const R: MMUFlags = MMUFlags::READ;
    const RW: MMUFlags =
        MMUFlags::from_bits_truncate(MMUFlags::READ.bits() | MMUFlags::WRITE.bits());
    const RX: MMUFlags =
        MMUFlags::from_bits_truncate(MMUFlags::READ.bits() | MMUFlags::EXECUTE.bits());
    const NONE: MMUFlags = MMUFlags::empty();

    fn check(pf: &PageFlags, model: &[MMUFlags]) {
        assert_eq!(pf.len(), model.len());
        assert_eq!(pf.iter().collect::<Vec<_>>(), model.to_vec());
        assert_eq!(pf.first(), model.first());
        assert_eq!(pf.last(), model.last());
        // invariants
        for w in pf.starts.windows(2) {
            assert!(w[0] < w[1]);
        }
        for w in pf.runs.windows(2) {
            assert_ne!(w[0], w[1], "adjacent runs must differ");
        }
        assert_eq!(pf.starts.len(), pf.runs.len());
        assert_eq!(pf.starts.first().copied(), (!model.is_empty()).then_some(0));
    }

    #[test]
    fn uniform_is_one_run() {
        let pf = PageFlags::uniform(RW, 1 << 22); // a 16 GiB reservation
        assert_eq!(pf.len(), 1 << 22);
        assert_eq!(pf.run_count(), 1);
        assert_eq!(pf[0], RW);
        assert_eq!(pf[(1 << 22) - 1], RW);
        assert_eq!(pf.get(1 << 22), None);
        assert!(PageFlags::uniform(RW, 0).is_empty());
    }

    #[test]
    fn set_splits_and_merges() {
        let mut pf = PageFlags::uniform(R, 10);
        pf.set(4, RW);
        check(&pf, &[R, R, R, R, RW, R, R, R, R, R]);
        assert_eq!(pf.run_count(), 3);
        pf.set(4, R);
        check(&pf, &[R; 10]);
        assert_eq!(pf.run_count(), 1);
        pf.set(0, NONE);
        pf.set(9, NONE);
        check(&pf, &[NONE, R, R, R, R, R, R, R, R, NONE]);
        pf.update_range(0..10, |_| RX);
        check(&pf, &[RX; 10]);
        assert_eq!(pf.run_count(), 1);
    }

    #[test]
    fn truncate_drain_split_slice() {
        let mut pf = PageFlags::default();
        pf.extend_repeat(R, 3);
        pf.extend_repeat(RW, 2);
        pf.extend_repeat(RW, 1); // merges
        pf.extend_repeat(NONE, 4);
        let model = [R, R, R, RW, RW, RW, NONE, NONE, NONE, NONE];
        check(&pf, &model);
        assert_eq!(pf.run_count(), 3);

        assert_eq!(
            pf.slice(2..7).iter().collect::<Vec<_>>(),
            model[2..7].to_vec()
        );
        assert!(pf.slice(3..3).is_empty());

        let mut a = pf.clone();
        let tail = a.split_off(4);
        check(&a, &model[..4]);
        check(&tail, &model[4..]);

        let mut b = pf.clone();
        b.drain_front(4);
        check(&b, &model[4..]);
        b.truncate(2);
        check(&b, &model[4..6]);
        b.truncate(0);
        check(&b, &[]);

        // split_off_into appends after what the tail already holds and merges
        // across the seam.
        let mut c = pf.clone();
        let mut tail = PageFlags::uniform(NONE, 2);
        c.split_off_into(6, &mut tail);
        check(&c, &model[..6]);
        check(&tail, &[NONE; 6]);
        assert_eq!(tail.run_count(), 1);
    }

    /// The scratch sized by `scratch_capacity` never has to grow.
    #[test]
    fn update_in_place_does_not_grow_scratch() {
        let mut pf = PageFlags::uniform(R, 100);
        for (i, f) in [(10usize, RW), (20, RX), (30, NONE), (40, RW)] {
            pf.set(i, f);
        }
        for range in [0..1, 5..50, 99..100, 0..100, 15..16, 45..46] {
            let cap = pf.scratch_capacity();
            let mut scratch = (Vec::with_capacity(cap), Vec::with_capacity(cap));
            pf.update_range_in(range.clone(), |old| old | MMUFlags::USER, &mut scratch);
            // the previous list came back in `scratch`; the new one grew by at most 2
            assert!(pf.run_count() <= cap);
            assert!(pf.starts.capacity() >= pf.run_count());
            for i in 0..100 {
                assert_eq!(pf[i].contains(MMUFlags::USER), range.contains(&i) || false);
            }
            pf.update_range(0..100, |old| old - MMUFlags::USER);
        }
    }

    /// Random operations against a plain `Vec<MMUFlags>` model.
    #[test]
    fn matches_vec_model() {
        let choices = [NONE, R, RW, RX];
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut rnd = |n: usize| -> usize {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n.max(1) as u64) as usize
        };
        for _ in 0..200 {
            let mut pf = PageFlags::default();
            let mut model: Vec<MMUFlags> = vec![];
            for _ in 0..60 {
                match rnd(7) {
                    0 => {
                        let (f, n) = (choices[rnd(4)], rnd(9));
                        pf.extend_repeat(f, n);
                        model.extend(core::iter::repeat_n(f, n));
                    }
                    1 => {
                        let n = rnd(model.len() + 3);
                        pf.truncate(n);
                        model.truncate(n);
                    }
                    2 => {
                        let n = rnd(model.len() + 3);
                        pf.drain_front(n);
                        model.drain(..n.min(model.len()));
                    }
                    3 if !model.is_empty() => {
                        let (i, f) = (rnd(model.len()), choices[rnd(4)]);
                        pf.set(i, f);
                        model[i] = f;
                    }
                    4 if !model.is_empty() => {
                        let a = rnd(model.len() + 1);
                        let b = a + rnd(model.len() + 1 - a);
                        let f = choices[rnd(4)];
                        pf.update_range(a..b, |old| old | f);
                        for m in &mut model[a..b] {
                            *m |= f;
                        }
                    }
                    5 if !model.is_empty() => {
                        let a = rnd(model.len() + 1);
                        let b = a + rnd(model.len() + 1 - a);
                        assert_eq!(
                            pf.slice(a..b).iter().collect::<Vec<_>>(),
                            model[a..b].to_vec()
                        );
                    }
                    6 => {
                        let at = rnd(model.len() + 2);
                        let tail = pf.split_off(at);
                        let mtail = model.split_off(at.min(model.len()));
                        check(&tail, &mtail);
                    }
                    _ => {}
                }
                check(&pf, &model);
            }
        }
    }
}
