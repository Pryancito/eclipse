//! What a cache-maintenance loop decides, apart from the instruction it runs.
//!
//! AArch64 cache maintenance is by virtual address, one *cache line* at a
//! time, and the line length is not a constant of the architecture: it is read
//! out of `CTR_EL0`, a register only an AArch64 machine has. So the decision --
//! how long a line is, and which addresses a range of bytes spans -- is soldered
//! to a register no host test can read, and the two mistakes it invites are the
//! ones that cost the most:
//!
//! * a stride **larger** than the real line silently **skips** lines, so some of
//!   the data is never cleaned and the reader sees stale bytes;
//! * a stride of zero, or an end computed without care, spins for ever with
//!   interrupts off, which on a boot path is a machine that never prints
//!   anything again.
//!
//! Both are decisions, not hardware, so they live here where every build
//! compiles them and a test can drive them. The instruction itself
//! (`dc cvac`, `ic ivau`) stays in the architecture file, which passes it in as
//! a closure. This is the same split as [`crate::common::rtc`] and
//! [`crate::common::cpu_power`].

/// A `CTR_EL0` "minimum line" field counts 32-bit **words**, not bytes.
pub const WORD: usize = 4;

/// `CTR_EL0.DminLine`, bits `[19:16]`: log2 of the number of words in the
/// smallest **data** cache line of any cache the PE can control.
const DMINLINE_SHIFT: u32 = 16;
/// `CTR_EL0.IminLine`, bits `[3:0]`: the same for the **instruction** side.
const IMINLINE_SHIFT: u32 = 0;
const MINLINE_MASK: u64 = 0b1111;

/// Length in bytes of the smallest data cache line this PE controls.
///
/// Data maintenance must use `DminLine` and not its instruction-side twin:
/// the two fields are different numbers on real parts (Apple's M1 reports a
/// 64-byte instruction line and a 128-byte data line), and the failure is
/// asymmetric -- too small a stride only costs time, too large a stride leaves
/// lines uncleaned.
pub fn dcache_line_size(ctr_el0: u64) -> usize {
    WORD << ((ctr_el0 >> DMINLINE_SHIFT) & MINLINE_MASK)
}

/// Length in bytes of the smallest instruction cache line this PE controls.
pub fn icache_line_size(ctr_el0: u64) -> usize {
    WORD << ((ctr_el0 >> IMINLINE_SHIFT) & MINLINE_MASK)
}

/// A stride that can be used as an alignment mask and cannot stall the loop.
///
/// [`dcache_line_size`] can only return a power of two, so this changes nothing
/// for it; it exists because the loop below is the one place a bad number turns
/// into an unbounded spin on a boot path, and because the cache size may one
/// day come from somewhere less well behaved (a device tree, a firmware table).
/// Rounding **down** to a power of two keeps the failure on the safe side: a
/// shorter stride cleans every line the longer one would have, and more.
fn usable_stride(line: usize) -> usize {
    if line < WORD {
        WORD
    } else if line.is_power_of_two() {
        line
    } else {
        // `1 << floor(log2(line))`, i.e. the largest power of two that fits.
        1usize << (usize::BITS - 1 - line.leading_zeros())
    }
}

/// Run `op` once on every cache line that holds any byte of
/// `[base, base + len)`, and answer how many lines that was.
///
/// The first line is the one holding `base`, **aligned down**: a range that
/// starts in the middle of a line still needs that whole line cleaned, and an
/// implementation that started at `base` itself would leave the leading bytes
/// dirty on every object that is not line-aligned -- which is most of them.
/// The last line is the one holding the range's last byte, so a range that ends
/// mid-line is covered too.
///
/// An empty range touches nothing, and a range that runs off the top of the
/// address space stops at the last line that exists rather than wrapping to
/// zero and starting again.
pub fn for_each_line(base: usize, len: usize, line: usize, mut op: impl FnMut(usize)) -> usize {
    if len == 0 {
        return 0;
    }
    let line = usable_stride(line);
    let first = base & !(line - 1);
    // The last byte, not one past it: `base + len` would name the next line
    // whenever the range ends exactly on a boundary, and clean one line too
    // many on every aligned object.
    let last = match base.checked_add(len - 1) {
        Some(end) => end & !(line - 1),
        None => usize::MAX & !(line - 1),
    };
    let mut addr = first;
    let mut count = 0;
    loop {
        op(addr);
        count += 1;
        if addr >= last {
            break;
        }
        addr += line;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// A `CTR_EL0` with the two minimum-line fields set as asked.
    fn ctr(dmin: u64, imin: u64) -> u64 {
        (dmin << DMINLINE_SHIFT) | (imin << IMINLINE_SHIFT)
    }

    fn touched(base: usize, len: usize, line: usize) -> Vec<usize> {
        let mut v = Vec::new();
        let n = for_each_line(base, len, line, |a| v.push(a));
        assert_eq!(n, v.len(), "the count and the calls disagree");
        v
    }

    #[test]
    fn the_line_length_is_counted_in_words_not_bytes() {
        // `CTR_EL0` stores log2 of a count of 32-bit words. Reading it as a
        // count of bytes gives a quarter of the real line, which is slow but
        // safe; reading the exponent as the length outright gives 4 bytes for
        // a 64-byte line, which is not.
        assert_eq!(dcache_line_size(ctr(4, 4)), 64);
        assert_eq!(dcache_line_size(ctr(2, 2)), 16);
        assert_eq!(dcache_line_size(ctr(0, 0)), 4);
    }

    #[test]
    fn the_data_side_is_read_from_its_own_field() {
        // Apple's M1: a 64-byte instruction line and a 128-byte data line. A
        // data clean that strode by the instruction line would still work
        // there (shorter is safe), but a part with the fields the other way
        // round would skip every other line.
        assert_eq!(dcache_line_size(ctr(5, 4)), 128);
        assert_eq!(icache_line_size(ctr(5, 4)), 64);
        assert_eq!(dcache_line_size(ctr(2, 6)), 16);
        assert_eq!(icache_line_size(ctr(2, 6)), 256);
    }

    #[test]
    fn the_line_fields_are_four_bits_and_nothing_above_them_leaks_in() {
        // Bits [23:20] are `CTR_EL0.ERG` and [27:24] `CWG`, and [15:14] the
        // cache-type bits. None of them is a line length.
        assert_eq!(dcache_line_size(u64::MAX), WORD << 0b1111);
        assert_eq!(icache_line_size(u64::MAX), WORD << 0b1111);
        assert_eq!(dcache_line_size(0xFFFF_FFFF_FFF0_FFFF), WORD);
    }

    #[test]
    fn a_range_inside_one_line_is_one_line() {
        assert_eq!(touched(0x1000, 8, 64), [0x1000]);
        assert_eq!(touched(0x1004, 8, 64), [0x1000]);
    }

    #[test]
    fn the_first_line_is_aligned_down_so_a_head_is_not_left_dirty() {
        // An object at 0x1038 of 16 bytes lives in the lines at 0x1000 and
        // 0x1040. Starting the walk at the object's own address would clean
        // 0x1038 -- which `dc cvac` resolves to the line at 0x1000 anyway --
        // but the point is the rule, because the count and the end depend on
        // it: see the next test.
        assert_eq!(touched(0x1038, 16, 64), [0x1000, 0x1040]);
    }

    #[test]
    fn the_last_byte_decides_the_end_not_the_byte_after_it() {
        // Exactly one line: the range ends on the boundary, and an end taken
        // from `base + len` would clean the next line as well -- harmless
        // here, fatal at the top of the address space.
        assert_eq!(touched(0x1000, 64, 64), [0x1000]);
        assert_eq!(touched(0x1000, 65, 64), [0x1000, 0x1040]);
        assert_eq!(touched(0x1000, 128, 64), [0x1000, 0x1040]);
    }

    #[test]
    fn an_empty_range_touches_nothing() {
        // `len - 1` below would wrap, so this is the guard that keeps the
        // whole address space from being cleaned one line at a time.
        assert_eq!(touched(0x1000, 0, 64), []);
        assert_eq!(touched(0, 0, 64), []);
    }

    #[test]
    fn a_range_that_reaches_the_top_of_memory_stops_there() {
        // The last line exists; the one after it does not, and computing it
        // wraps to zero, where the loop would start again and never end.
        let line = 64;
        let base = usize::MAX - 7;
        let v = touched(base, 8, line);
        assert_eq!(v, [usize::MAX & !(line - 1)]);
    }

    #[test]
    fn a_length_that_runs_off_the_end_stops_at_the_last_line() {
        let line = 64;
        let v = touched(usize::MAX - 127, usize::MAX, line);
        assert_eq!(
            v,
            [(usize::MAX - 127) & !(line - 1), usize::MAX & !(line - 1)]
        );
    }

    #[test]
    fn a_stride_of_zero_does_not_spin_for_ever() {
        // A zero stride is an infinite loop with interrupts off on a boot
        // path: the machine stops without printing anything. Fall back to the
        // architectural word instead.
        assert_eq!(touched(0x1000, 8, 0), [0x1000, 0x1004]);
    }

    #[test]
    fn a_stride_that_is_not_a_power_of_two_is_rounded_down() {
        // The alignment mask `!(line - 1)` is only a mask for a power of two.
        // Rounding down keeps the failure safe: 48 becomes 32, and every line
        // a 48-byte stride would have cleaned is still cleaned.
        assert_eq!(touched(0x1000, 64, 48), [0x1000, 0x1020]);
        assert_eq!(touched(0x1000, 96, 48), [0x1000, 0x1020, 0x1040]);
        assert_eq!(touched(0x1000, 8, 3), [0x1000, 0x1004]);
    }

    #[test]
    fn every_byte_of_the_range_lands_in_some_line_that_was_cleaned() {
        // The property the loop exists for, checked directly rather than by
        // example: whatever the base, the length and the line, no byte of the
        // range is left outside a line `op` was called on.
        for line in [4usize, 16, 32, 64, 128] {
            for base in [0usize, 1, 63, 64, 0x1000, 0x1001, 0x2FFF] {
                for len in [1usize, 2, 7, 63, 64, 65, 200] {
                    let v = touched(base, len, line);
                    for b in base..base + len {
                        let want = b & !(line - 1);
                        assert!(
                            v.contains(&want),
                            "byte {:#x} of {:#x}+{} (line {}) was never cleaned",
                            b,
                            base,
                            len,
                            line
                        );
                    }
                    // ...and nothing outside the range was cleaned either, so
                    // the loop is not simply walking all of memory.
                    let first = base & !(line - 1);
                    let last = (base + len - 1) & !(line - 1);
                    assert_eq!(v.len(), (last - first) / line + 1);
                }
            }
        }
    }
}
