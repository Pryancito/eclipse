//! A bump arena over the firmware's page allocator.
//!
//! The loader used to call `BootServices::allocate_pages` **once per 4 KiB
//! frame**. The kernel's last `PT_LOAD` has `FileSiz` 0 and `MemSiz` 548 MiB
//! (512 MiB of it is `zcore::memory_x86_64::init::HEAP`, a `static mut` in
//! `.bss`), so loading the kernel alone made about 134,000 of those calls, and
//! a UEFI implementation answers each one by walking its memory-descriptor list
//! and splitting a descriptor in two — a list that gets longer with every
//! allocation. That is the shape of the boot's second-biggest stretch.
//!
//! So frames come from here instead: one firmware allocation per [`CHUNK_PAGES`]
//! frames, handed out by bumping a pointer.
//!
//! **Nothing in here may panic**, same as the rest of the loader: by the time a
//! frame request goes wrong the only output left is the progress bar.
//!
//! The arena never frees, and its unused tail stays `LOADER_DATA` in the memory
//! map the kernel is handed — so the kernel counts up to one chunk of memory as
//! used that nobody is using. That is the price of the change, it is bounded by
//! the chunk size, and it is why the chunk is megabytes rather than gigabytes.

/// How many 4 KiB pages one firmware allocation asks for.
///
/// Four mebibytes. Large enough that the per-call cost stops mattering (the
/// 134,000 calls above become 137), small enough that the memory the arena
/// never hands out is a rounding error against a machine's RAM.
pub const CHUNK_PAGES: u64 = 1024;

/// The size of one 4 KiB frame. (`page_table::PAGE_SIZE` is the same number;
/// this module is deliberately free of `x86_64` types so it can be read and
/// tested on its own.)
const PAGE_SIZE: u64 = 4096;

/// A bump pointer over one firmware allocation at a time.
#[derive(Debug, Default, Clone, Copy)]
pub struct Arena {
    next: u64,
    end: u64,
}

impl Arena {
    pub const fn new() -> Self {
        Arena { next: 0, end: 0 }
    }

    /// Physical address of one fresh 4 KiB frame.
    ///
    /// `refill(pages)` must allocate `pages` contiguous 4 KiB pages from the
    /// firmware and return the first one's physical address, or `None`. It is
    /// called at most twice per frame: once for a whole chunk, and — only if
    /// that fails — once for a single page, so that a machine too short of
    /// memory for a chunk still boots the way it did before this module
    /// existed, one page at a time, rather than failing outright.
    pub fn frame(&mut self, mut refill: impl FnMut(u64) -> Option<u64>) -> Option<u64> {
        if self.next >= self.end {
            // A chunk, or failing that a single page. `0` is not a usable
            // address here: it would make a handed-out frame
            // indistinguishable from "nothing left", and the firmware does not
            // place LOADER_DATA at physical zero.
            let (start, pages) = match refill(CHUNK_PAGES).filter(|&a| a != 0) {
                Some(start) => (start, CHUNK_PAGES),
                None => (refill(1).filter(|&a| a != 0)?, 1),
            };
            // A chunk whose end does not fit in a u64 is not a chunk any
            // firmware can have given us; refusing it beats wrapping.
            self.end = start.checked_add(pages.checked_mul(PAGE_SIZE)?)?;
            self.next = start;
        }
        let frame = self.next;
        // Cannot overflow: `next < end` and `end` is a checked sum.
        self.next = frame + PAGE_SIZE;
        Some(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    /// A fake firmware that hands out ascending chunks and counts its calls,
    /// which is the whole point of the module: the test that matters is how
    /// MANY times the firmware was asked, not what it answered.
    #[derive(Default)]
    struct Fw {
        next: u64,
        calls: Vec<u64>,
        budget: Option<usize>,
        refuse_chunks: bool,
    }

    impl Fw {
        fn new(base: u64) -> Self {
            Fw {
                next: base,
                ..Default::default()
            }
        }
        fn alloc(&mut self, pages: u64) -> Option<u64> {
            self.calls.push(pages);
            if let Some(left) = self.budget {
                if self.calls.len() > left {
                    return None;
                }
            }
            if self.refuse_chunks && pages > 1 {
                return None;
            }
            let at = self.next;
            self.next += pages * PAGE_SIZE;
            Some(at)
        }
    }

    #[test]
    fn a_whole_chunk_of_frames_costs_the_firmware_one_call() {
        let mut fw = Fw::new(0x10_0000);
        let mut a = Arena::new();
        for i in 0..CHUNK_PAGES {
            let got = a.frame(|p| fw.alloc(p)).expect("a frame");
            assert_eq!(got, 0x10_0000 + i * PAGE_SIZE, "frames must be ascending");
        }
        assert_eq!(fw.calls, std::vec![CHUNK_PAGES]);
    }

    #[test]
    fn the_frame_after_a_chunk_asks_for_the_next_one() {
        let mut fw = Fw::new(0x10_0000);
        let mut a = Arena::new();
        for _ in 0..CHUNK_PAGES + 1 {
            a.frame(|p| fw.alloc(p)).expect("a frame");
        }
        assert_eq!(fw.calls, std::vec![CHUNK_PAGES, CHUNK_PAGES]);
    }

    #[test]
    fn a_hundred_thousand_frames_are_not_a_hundred_thousand_firmware_calls() {
        // This is the number the kernel's `.bss` actually asks for.
        const FRAMES: u64 = 133_936;
        let mut fw = Fw::new(0x10_0000);
        let mut a = Arena::new();
        for _ in 0..FRAMES {
            a.frame(|p| fw.alloc(p)).expect("a frame");
        }
        assert_eq!(fw.calls.len(), FRAMES.div_ceil(CHUNK_PAGES) as usize);
        assert!(fw.calls.len() < 200, "{} calls", fw.calls.len());
    }

    #[test]
    fn no_two_frames_are_the_same_frame() {
        let mut fw = Fw::new(0x10_0000);
        let mut a = Arena::new();
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..3 * CHUNK_PAGES {
            let f = a.frame(|p| fw.alloc(p)).expect("a frame");
            assert!(seen.insert(f), "frame {f:#x} handed out twice");
            assert!(f.is_multiple_of(PAGE_SIZE), "frame {f:#x} is not aligned");
        }
    }

    #[test]
    fn a_firmware_that_will_not_give_a_whole_chunk_still_gives_frames() {
        // The old behaviour, as a fallback: a machine too short of memory for a
        // 4 MiB chunk must still boot one page at a time rather than fail.
        let mut fw = Fw::new(0x10_0000);
        fw.refuse_chunks = true;
        let mut a = Arena::new();
        for i in 0..4u64 {
            assert_eq!(
                a.frame(|p| fw.alloc(p)),
                Some(0x10_0000 + i * PAGE_SIZE),
                "frame {i}"
            );
        }
        assert_eq!(
            fw.calls,
            std::vec![
                CHUNK_PAGES,
                1,
                CHUNK_PAGES,
                1,
                CHUNK_PAGES,
                1,
                CHUNK_PAGES,
                1
            ]
        );
    }

    #[test]
    fn a_firmware_with_nothing_left_returns_none_instead_of_zero() {
        let mut fw = Fw::new(0x10_0000);
        fw.budget = Some(0);
        let mut a = Arena::new();
        assert_eq!(a.frame(|p| fw.alloc(p)), None);
    }

    #[test]
    fn an_allocation_at_physical_zero_is_not_handed_out() {
        // A frame of 0 would be indistinguishable from "nothing left" to every
        // caller that checks for it, so it is refused and the single-page path
        // tried instead.
        let mut fw = Fw::new(0);
        let mut a = Arena::new();
        // Both calls answer 0 (the chunk at 0, then one page at 0+4MiB... no:
        // the fake advances, so the second answer is past zero).
        let got = a.frame(|p| fw.alloc(p));
        assert_eq!(got, Some(CHUNK_PAGES * PAGE_SIZE));
        assert_eq!(fw.calls, std::vec![CHUNK_PAGES, 1]);
    }

    #[test]
    fn a_chunk_whose_end_would_wrap_is_refused_rather_than_wrapped() {
        let mut a = Arena::new();
        // A "firmware" answering with an address so high that the chunk does
        // not fit in 64 bits. Nothing is handed out and the arena is left
        // untouched: a wrapped `end` would make it hand out addresses it does
        // not own for ever after.
        assert_eq!(a.frame(|_| Some(u64::MAX - 1)), None);
    }

    #[test]
    fn an_arena_that_ran_out_can_be_asked_again() {
        // `allocate_frame` returning `None` is a recoverable error upstream
        // (`MapError::OutOfFrames`), so a later call must not find the arena in
        // a state that hands out a frame it does not own.
        let mut fw = Fw::new(0x10_0000);
        fw.budget = Some(0);
        let mut a = Arena::new();
        assert_eq!(a.frame(|p| fw.alloc(p)), None);
        fw.budget = None;
        let got = a.frame(|p| fw.alloc(p)).expect("a frame");
        assert_eq!(got, 0x10_0000);
    }
}
