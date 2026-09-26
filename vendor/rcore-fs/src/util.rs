/// Given a range and iterate sub-range for each block
pub struct BlockIter {
    pub begin: usize,
    pub end: usize,
    pub block_size_log2: u8,
}

#[derive(Debug, Eq, PartialEq)]
pub struct BlockRange {
    pub block: usize,
    pub begin: usize,
    pub end: usize,
    pub block_size_log2: u8,
}

impl BlockRange {
    pub fn is_empty(&self) -> bool {
        self.end == self.begin
    }
    pub fn len(&self) -> usize {
        self.end - self.begin
    }
    pub fn is_full(&self) -> bool {
        self.len() == (1usize << self.block_size_log2)
    }
    pub fn origin_begin(&self) -> usize {
        (self.block << self.block_size_log2) + self.begin
    }
    pub fn origin_end(&self) -> usize {
        (self.block << self.block_size_log2) + self.end
    }
}

impl Iterator for BlockIter {
    type Item = BlockRange;

    fn next(&mut self) -> Option<<Self as Iterator>::Item> {
        if self.begin >= self.end {
            return None;
        }
        let block_size_log2 = self.block_size_log2;
        let block_size = 1usize << self.block_size_log2;
        let block = self.begin / block_size;
        let begin = self.begin % block_size;
        let end = if block == self.end / block_size {
            self.end % block_size
        } else {
            block_size
        };
        self.begin += end - begin;
        Some(BlockRange {
            block,
            begin,
            end,
            block_size_log2,
        })
    }
}

// 声明一块未初始化的内存
/// Declares a block of uninitialized memory.
///
/// # Safety
///
/// Never read from uninitialized memory!
#[inline(always)]
pub unsafe fn uninit_memory<T>() -> T {
    // 这个写法十分恐怖，但实际上是死灵书的正牌写法
    #[allow(clippy::uninit_assumed_init)]
    core::mem::MaybeUninit::uninit().assume_init()
}

#[cfg(test)]
mod test {
    use super::*;
    use alloc::vec::Vec;

    /// Every sub-range the iterator produces, as `(block, begin, end)`.
    fn ranges(begin: usize, end: usize, log2: u8) -> Vec<(usize, usize, usize)> {
        BlockIter {
            begin,
            end,
            block_size_log2: log2,
        }
        // Bounded on purpose: an iterator that stops advancing would otherwise
        // hang the job with no hint inside it, and a run of identical
        // sub-ranges is a named failure in the assertion below.
        .take(64)
        .map(|r| (r.block, r.begin, r.end))
        .collect()
    }

    /// The bytes an iteration covers, taken from `origin_begin`/`origin_end`,
    /// which is how a caller maps a sub-range back onto its own buffer.
    fn covered(begin: usize, end: usize, log2: u8) -> Vec<usize> {
        BlockIter {
            begin,
            end,
            block_size_log2: log2,
        }
        .take(1024)
        .flat_map(|r| r.origin_begin()..r.origin_end())
        .collect()
    }

    #[test]
    fn an_empty_range_produces_nothing() {
        assert!(ranges(0, 0, 4).is_empty());
        assert!(ranges(0x40, 0x40, 4).is_empty());
        // And a backwards one does not run away.
        assert!(ranges(0x40, 0x10, 4).is_empty());
    }

    #[test]
    fn a_range_inside_one_block_is_one_sub_range() {
        assert_eq!(ranges(0x12, 0x18, 4), vec![(1, 2, 8)]);
    }

    #[test]
    fn a_range_that_is_exactly_one_block_is_full() {
        let r = BlockIter {
            begin: 0x20,
            end: 0x30,
            block_size_log2: 4,
        }
        .next()
        .unwrap();
        assert!(r.is_full(), "a whole block did not report itself full");
        assert!(!r.is_empty());
        assert_eq!(r.len(), 0x10);
        assert_eq!((r.origin_begin(), r.origin_end()), (0x20, 0x30));
    }

    #[test]
    fn a_range_shorter_than_a_block_is_not_full() {
        let r = BlockIter {
            begin: 0x21,
            end: 0x30,
            block_size_log2: 4,
        }
        .next()
        .unwrap();
        assert!(!r.is_full());
        assert_eq!(r.len(), 0xf);
    }

    #[test]
    fn the_sub_ranges_cover_the_whole_range_once_each() {
        for (begin, end) in [
            (0usize, 1usize),
            (0, 0x10),
            (0, 0x11),
            (1, 0x10),
            (1, 0x11),
            (0xf, 0x21),
            (0x10, 0x20),
            (0x123, 0x456),
        ] {
            let got = covered(begin, end, 4);
            let want: Vec<usize> = (begin..end).collect();
            assert_eq!(
                got, want,
                "{:#x}..{:#x} was not covered exactly",
                begin, end
            );
        }
    }

    #[test]
    fn a_range_ending_on_a_block_boundary_does_not_produce_an_empty_tail() {
        // `0x10..0x20` is one full block and no more: an empty sub-range at the
        // start of block 2 would make a caller ask the device for a block it
        // does not need.
        assert_eq!(ranges(0x10, 0x20, 4), vec![(1, 0, 0x10)]);
        assert_eq!(ranges(0x10, 0x30, 4), vec![(1, 0, 0x10), (2, 0, 0x10)]);
    }

    #[test]
    fn one_byte_at_the_end_of_a_block_is_its_own_sub_range() {
        assert_eq!(ranges(0x1f, 0x21, 4), vec![(1, 0xf, 0x10), (2, 0, 1)]);
    }

    #[test]
    fn a_block_size_of_one_byte_gives_one_sub_range_per_byte() {
        assert_eq!(ranges(2, 5, 0), vec![(2, 0, 1), (3, 0, 1), (4, 0, 1)]);
    }

    #[test]
    fn the_iterator_stops_and_stays_stopped() {
        let mut iter = BlockIter {
            begin: 0,
            end: 4,
            block_size_log2: 4,
        };
        assert!(iter.next().is_some());
        assert!(iter.next().is_none());
        assert!(iter.next().is_none());
    }

    #[test]
    fn every_sub_range_moves_the_cursor_forward() {
        // A sub-range of no bytes leaves `begin` where it was, so the iterator
        // hands out the same one for ever and its caller never returns.
        for (begin, end, log2) in [(0usize, 0usize, 4u8), (0x10, 0x10, 4), (5, 5, 0), (0, 1, 4)] {
            let iter = BlockIter {
                begin,
                end,
                block_size_log2: log2,
            };
            let mut at = begin;
            let mut n = 0;
            for r in iter {
                assert!(
                    r.origin_end() > at,
                    "{:#x}..{:#x} stood still at {:#x}",
                    begin,
                    end,
                    at
                );
                at = r.origin_end();
                n += 1;
                assert!(n <= 8, "{:#x}..{:#x} never ends", begin, end);
            }
        }
    }

    #[test]
    fn block_iter() {
        let mut iter = BlockIter {
            begin: 0x123,
            end: 0x2018,
            block_size_log2: 12,
        };
        assert_eq!(
            iter.next(),
            Some(BlockRange {
                block: 0,
                begin: 0x123,
                end: 0x1000,
                block_size_log2: 12
            })
        );
        assert_eq!(
            iter.next(),
            Some(BlockRange {
                block: 1,
                begin: 0,
                end: 0x1000,
                block_size_log2: 12
            })
        );
        assert_eq!(
            iter.next(),
            Some(BlockRange {
                block: 2,
                begin: 0,
                end: 0x18,
                block_size_log2: 12
            })
        );
        assert_eq!(iter.next(), None);
    }
}
