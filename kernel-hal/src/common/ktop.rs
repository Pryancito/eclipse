//! Kernel-mode CPU profile: which kernel code the timer interrupts.
//!
//! `/proc/perf/top` samples the interrupted *user* PC, and `/proc/stat` says
//! how the time splits between user and system. Neither says where the system
//! half goes. Firefox on the desktop (30-sep-2026) had a 10-second window of
//! 50% system, 4.5% user and 45% idle across six CPUs: three cores in the
//! kernel, and nothing in the tree that could name the function they were in.
//!
//! So the APIC timer, when it interrupts ring 0 on a CPU that is not idle,
//! records the interrupted RIP and the return addresses of the frames above it.
//! The kernel is built with frame pointers (`"frame-pointer": "always"` in the
//! target spec), so `[rbp]` is the caller's `rbp` and `[rbp + 8]` its return
//! address. The leaf alone would not be enough: `lock::Mutex` holds interrupts
//! off for its whole critical section, so a tick that lands while a lock is
//! held is delivered at the `pop_off` that releases it, and every contended
//! lock in the kernel would read as the same three-instruction function. The
//! callers are what name the lock.
//!
//! Every address is folded to the start of its function before it is counted,
//! so one row is one call path through functions, not through instructions.
//!
//! The table is fixed-size and never allocates: it is written from the timer
//! interrupt. It is emptied by [`take`], which is what makes a profile mean "since
//! the previous read" instead of "since boot".

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// The interrupted function plus this many frames above it.
pub const DEPTH: usize = 6;

/// Table size. A power of two (the probe masks with it).
pub const SLOTS: usize = 2048;

/// Distinct call paths kept before new ones are counted as lost. A quarter of
/// the table stays free so a probe for a missing key always ends on an empty
/// slot within a few steps.
const MAX_LIVE: usize = SLOTS - SLOTS / 4;

/// One sample's call path: `[leaf, caller, caller's caller, ...]`, each folded
/// to its function's start, 0 past the last frame the walk could trust.
pub type Path = [u64; DEPTH];

/// Walk the frame-pointer chain from an interrupted `rip`/`rbp`.
///
/// `readable` must say whether a kernel address can be read without faulting;
/// `read` reads the qword there; `is_text` says whether a word is a kernel
/// code address. The walk stops at the first frame that fails any check, and
/// insists that frames strictly climb the stack, so a corrupt chain ends the
/// walk instead of looping or wandering.
///
/// The interrupted function may not have pushed its own frame yet (a tick on
/// its first instruction, or a frameless assembly leaf); the first caller is
/// then its caller's caller. That costs a missing row in one path, never a
/// wrong function at the leaf.
pub fn walk(
    rip: u64,
    rbp: u64,
    readable: impl Fn(u64) -> bool,
    read: impl Fn(u64) -> u64,
    is_text: impl Fn(u64) -> bool,
) -> Path {
    let mut path = [0u64; DEPTH];
    path[0] = rip;
    let mut fp = rbp;
    for slot in path.iter_mut().skip(1) {
        if fp == 0 || !fp.is_multiple_of(8) || !readable(fp) || !readable(fp + 8) {
            break;
        }
        let saved = read(fp);
        let ret = read(fp + 8);
        if !is_text(ret) {
            break;
        }
        *slot = ret;
        if saved <= fp {
            break;
        }
        fp = saved;
    }
    path
}

/// Fold every address of `path` to the start of its function.
///
/// A return address points just *after* its `call`, which for a call that is
/// the last instruction of a function (a `noreturn` callee) is the first byte
/// of the next one; looking up `ret - 1` keeps it in the caller. The leaf is a
/// real instruction address and is looked up as is. `start_of` returns `None`
/// for an address it cannot name, and that address is kept raw: still a
/// distinct row, printed as a number.
pub fn fold(path: &Path, start_of: impl Fn(u64) -> Option<u64>) -> Path {
    let mut out = [0u64; DEPTH];
    for (i, &a) in path.iter().enumerate() {
        if a == 0 {
            break;
        }
        let probe = if i == 0 { a } else { a - 1 };
        out[i] = start_of(probe).unwrap_or(a);
    }
    out
}

#[derive(Clone, Copy)]
struct Slot {
    key: Path,
    count: u32,
}

const EMPTY: Slot = Slot {
    key: [0; DEPTH],
    count: 0,
};

/// The histogram of call paths. Open addressing, never evicts, never
/// allocates.
pub struct Profile {
    slots: [Slot; SLOTS],
    len: usize,
    /// Samples offered, including the ones in `full`.
    samples: u64,
    /// Samples whose path was new while the table already held `MAX_LIVE`.
    full: u64,
}

impl Default for Profile {
    fn default() -> Self {
        Self::new()
    }
}

impl Profile {
    pub const fn new() -> Self {
        Self {
            slots: [EMPTY; SLOTS],
            len: 0,
            samples: 0,
            full: 0,
        }
    }

    fn index(key: &Path) -> usize {
        let mut h: u64 = 0;
        for &w in key.iter() {
            h = (h ^ w).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
        ((h >> 32) as usize) & (SLOTS - 1)
    }

    /// Count one sample. A path of all zeroes is not a sample.
    pub fn add(&mut self, key: Path) {
        if key[0] == 0 {
            return;
        }
        self.samples += 1;
        let mut i = Self::index(&key);
        for _ in 0..SLOTS {
            let slot = &mut self.slots[i];
            if slot.count == 0 {
                if self.len >= MAX_LIVE {
                    break;
                }
                slot.key = key;
                slot.count = 1;
                self.len += 1;
                return;
            }
            if slot.key == key {
                slot.count = slot.count.saturating_add(1);
                return;
            }
            i = (i + 1) & (SLOTS - 1);
        }
        self.full += 1;
    }

    /// Move every row into `out` and empty the table. `out` should already
    /// have room for [`SLOTS`] rows so this does not allocate while a caller
    /// holds the lock around it. Returns `(samples, full)` for the period.
    pub fn drain_into(&mut self, out: &mut Vec<(Path, u32)>) -> (u64, u64) {
        for slot in self.slots.iter_mut() {
            if slot.count != 0 {
                out.push((slot.key, slot.count));
                *slot = EMPTY;
            }
        }
        let totals = (self.samples, self.full);
        self.len = 0;
        self.samples = 0;
        self.full = 0;
        totals
    }
}

static PROFILE: spin::Mutex<Profile> = spin::Mutex::new(Profile::new());
/// Ticks dropped because the table was locked (a reader draining it, or the
/// same tick racing on another CPU).
static LOCK_MISSES: AtomicU64 = AtomicU64::new(0);
/// Ring-0 ticks on a CPU that was idle: not profiled, counted so the report
/// can say how much of the machine the profile covers.
static IDLE_TICKS: AtomicU64 = AtomicU64::new(0);

/// One profile period, as [`take`] hands it over.
pub struct Taken {
    pub rows: Vec<(Path, u32)>,
    pub samples: u64,
    pub full: u64,
    pub lock_misses: u64,
    pub idle_ticks: u64,
}

/// Everything recorded since the previous call, and start a new period.
pub fn take() -> Taken {
    let mut rows = Vec::with_capacity(SLOTS);
    let (samples, full) = PROFILE.lock().drain_into(&mut rows);
    Taken {
        rows,
        samples,
        full,
        lock_misses: LOCK_MISSES.swap(0, Relaxed),
        idle_ticks: IDLE_TICKS.swap(0, Relaxed),
    }
}

/// A timer tick interrupted ring 0 on an idle CPU.
pub fn note_idle_tick() {
    IDLE_TICKS.fetch_add(1, Relaxed);
}

/// A timer tick interrupted ring 0 on a busy CPU at `rip` with frame pointer
/// `rbp`. Runs in the interrupt handler: it reads the stack only through the
/// page table, and gives the sample up rather than wait for the lock.
#[cfg(target_os = "none")]
pub fn note_kernel_tick(rip: u64, rbp: u64) {
    use crate::vm::{GenericPageTable, PageTable};
    let pt = PageTable::from_current();
    // One page-table walk per page, not per word: a frame chain mostly stays
    // on one or two pages of the stack.
    let last_page = core::cell::Cell::new(u64::MAX);
    let readable = |a: u64| {
        if !crate::kaddr::is_kernel_addr(a) {
            return false;
        }
        let page = a & !0xfff;
        if page == last_page.get() {
            return true;
        }
        let ok = pt
            .query(a as usize)
            .map(|(_, flags, _)| !flags.is_empty())
            .unwrap_or(false);
        if ok {
            last_page.set(page);
        }
        ok
    };
    // SAFETY: `walk` only reads an address after `readable` confirmed it is a
    // kernel address on a present page.
    let read = |a: u64| unsafe { core::ptr::read_volatile(a as *const u64) };
    let path = walk(rip, rbp, readable, read, crate::kaddr::is_kernel_text);
    let path = fold(&path, |a| crate::ksyms::lookup(a).map(|(_, off)| a - off));
    match PROFILE.try_lock() {
        Some(mut p) => p.add(path),
        None => {
            LOCK_MISSES.fetch_add(1, Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use alloc::collections::BTreeMap;

    const TEXT: core::ops::Range<u64> = 0x1000..0x9000;

    /// A fake stack: address -> qword.
    fn stack(words: &[(u64, u64)]) -> BTreeMap<u64, u64> {
        words.iter().copied().collect()
    }

    fn walk_on(mem: &BTreeMap<u64, u64>, rip: u64, rbp: u64) -> Path {
        walk(
            rip,
            rbp,
            |a| mem.contains_key(&a),
            |a| mem[&a],
            |a| TEXT.contains(&a),
        )
    }

    #[test]
    fn the_walk_follows_the_frame_chain_to_each_return_address() {
        // Three frames climbing the stack: [saved rbp, return address].
        let mem = stack(&[
            (0x100, 0x140),
            (0x108, 0x2222),
            (0x140, 0x180),
            (0x148, 0x3333),
            (0x180, 0x0),
            (0x188, 0x4444),
        ]);
        assert_eq!(
            walk_on(&mem, 0x1111, 0x100),
            [0x1111, 0x2222, 0x3333, 0x4444, 0, 0]
        );
    }

    #[test]
    fn the_walk_stops_at_most_depth_frames_up() {
        let mut words = Vec::new();
        for i in 0..10u64 {
            let fp = 0x100 + i * 0x20;
            words.push((fp, fp + 0x20));
            words.push((fp + 8, 0x2000 + i));
        }
        let mem = stack(&words);
        assert_eq!(
            walk_on(&mem, 0x1111, 0x100),
            [0x1111, 0x2000, 0x2001, 0x2002, 0x2003, 0x2004]
        );
    }

    #[test]
    fn the_walk_never_reads_what_is_not_readable() {
        // The second frame points at a page that is not mapped: the walk must
        // stop there, not ask `read` for it (which would fault in the kernel).
        let mem = stack(&[(0x100, 0x5000), (0x108, 0x2222)]);
        assert_eq!(walk_on(&mem, 0x1111, 0x100), [0x1111, 0x2222, 0, 0, 0, 0]);
        // Both words of a frame are checked: a frame pointer on the last qword
        // of an unmapped page has its return address on the next, mapped one.
        let mem = stack(&[(0x100, 0x5ff8), (0x108, 0x2222), (0x6000, 0x3333)]);
        assert_eq!(walk_on(&mem, 0x1111, 0x100), [0x1111, 0x2222, 0, 0, 0, 0]);
        // And the other way round: the return address is past the end of the
        // last mapped page.
        let mem = stack(&[(0x100, 0x6ff8), (0x108, 0x2222), (0x6ff8, 0x0)]);
        assert_eq!(walk_on(&mem, 0x1111, 0x100), [0x1111, 0x2222, 0, 0, 0, 0]);
        // A misaligned or null frame pointer is not followed either, even
        // where the words it would read are there.
        let mem = stack(&[(0x0, 0x40), (0x8, 0x2222), (0x104, 0x140), (0x10c, 0x2222)]);
        assert_eq!(walk_on(&mem, 0x1111, 0x104), [0x1111, 0, 0, 0, 0, 0]);
        assert_eq!(walk_on(&mem, 0x1111, 0), [0x1111, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn a_chain_that_does_not_climb_the_stack_ends_the_walk() {
        // A frame whose saved rbp points back down (or at itself) is corrupt
        // or a loop: its own return address is kept, nothing above it is.
        let mem = stack(&[
            (0x100, 0x140),
            (0x108, 0x2222),
            (0x140, 0x100),
            (0x148, 0x3333),
        ]);
        assert_eq!(
            walk_on(&mem, 0x1111, 0x100),
            [0x1111, 0x2222, 0x3333, 0, 0, 0]
        );
    }

    #[test]
    fn a_return_address_outside_kernel_text_ends_the_walk() {
        let mem = stack(&[
            (0x100, 0x140),
            (0x108, 0x2222),
            (0x140, 0x180),
            (0x148, 0xdead_0000),
        ]);
        assert_eq!(walk_on(&mem, 0x1111, 0x100), [0x1111, 0x2222, 0, 0, 0, 0]);
    }

    #[test]
    fn folding_names_the_caller_even_when_its_call_is_the_last_instruction() {
        // Functions start at 0x1000, 0x2000 and 0x3000. A return address of
        // exactly 0x3000 follows a call that ended the function at 0x2000.
        let start_of = |a: u64| TEXT.contains(&a).then_some(a & !0xfff);
        let path = [0x1234, 0x3000, 0x2010, 0, 0, 0];
        assert_eq!(fold(&path, start_of), [0x1000, 0x2000, 0x2000, 0, 0, 0]);
        // An address the table cannot name is kept as it is.
        let path = [0xdead_0001, 0x2010, 0, 0, 0, 0];
        assert_eq!(fold(&path, start_of), [0xdead_0001, 0x2000, 0, 0, 0, 0]);
    }

    #[test]
    fn the_profile_counts_each_path_and_drains_to_empty() {
        let mut p = Box::new(Profile::new());
        let a = [0x1000, 0x2000, 0, 0, 0, 0];
        let b = [0x1000, 0x3000, 0, 0, 0, 0];
        for _ in 0..3 {
            p.add(a);
        }
        p.add(b);
        p.add([0; DEPTH]);
        let mut rows = Vec::new();
        assert_eq!(p.drain_into(&mut rows), (4, 0));
        rows.sort();
        assert_eq!(rows, alloc::vec![(a, 3), (b, 1)]);
        // Drained means a new period: nothing carries over.
        let mut again = Vec::new();
        assert_eq!(p.drain_into(&mut again), (0, 0));
        assert!(again.is_empty());
        p.add(b);
        assert_eq!(p.drain_into(&mut again), (1, 0));
        assert_eq!(again, alloc::vec![(b, 1)]);
    }

    #[test]
    fn a_full_profile_still_counts_known_paths_and_reports_the_new_ones_lost() {
        let mut p = Box::new(Profile::new());
        for i in 0..MAX_LIVE as u64 {
            p.add([0x1000 + i, 0, 0, 0, 0, 0]);
        }
        p.add([0x1000, 0, 0, 0, 0, 0]);
        p.add([0x9999_9999, 0, 0, 0, 0, 0]);
        let mut rows = Vec::new();
        assert_eq!(
            p.drain_into(&mut rows),
            (MAX_LIVE as u64 + 2, 1),
            "one new path lost"
        );
        assert_eq!(rows.len(), MAX_LIVE);
        assert!(rows.contains(&([0x1000, 0, 0, 0, 0, 0], 2)));
        // And emptied, a full table takes new paths again.
        p.add([0x9999_9999, 0, 0, 0, 0, 0]);
        let mut rows = Vec::new();
        assert_eq!(p.drain_into(&mut rows), (1, 0));
    }
}
