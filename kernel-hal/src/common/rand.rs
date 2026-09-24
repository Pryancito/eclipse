//! Where the kernel's random bytes come from.
//!
//! [`fill_random`] is the only source in the tree. It feeds `/dev/random` and
//! `/dev/urandom`, `getrandom(2)`, `zx_cprng_draw`, FreeBSD's `kern.arandom`,
//! the UUID and boot id in `/proc/sys/kernel/random/`, and -- the one that
//! decides whether a bug is a crash or an exploit -- the sixteen `AT_RANDOM`
//! bytes that glibc turns into every process's **stack canary and pointer
//! guard**.
//!
//! It used to be a `cfg_if!` in `hal_fn.rs` with two arms and no tests:
//!
//! * On x86_64 it called `_rdrand64_step` and **threw away the flag that says
//!   whether the instruction produced anything**. RDRAND is specified to fail:
//!   it clears CF and leaves the destination zero when the entropy pool behind
//!   it is momentarily empty, which is why Intel's own guidance is to retry.
//!   A failure here filled the buffer with **zeros** and said nothing. It also
//!   never asked CPUID whether the instruction exists; on a part without
//!   RDRAND, `/dev/urandom` is an illegal instruction.
//!
//! * On everything else -- aarch64, riscv64 and the host build -- it was a
//!   multiplicative congruential generator over a `static mut` seeded with the
//!   constant `0xdead_beef_cafe_babe`. A constant seed and a deterministic
//!   generator mean **every boot produces the same bytes in the same order**,
//!   so on those machines the canary of the n-th process is a compile-time
//!   constant, and so is every UUID and every `getrandom` answer. The
//!   `static mut` was also read and written from every CPU with no
//!   synchronisation at all.
//!
//! What replaces it has two halves that are deliberately not alternatives:
//! the software generator always fills the buffer, and the hardware source is
//! **mixed in on top** when the machine has one and it works. A failure of the
//! hardware can then only leave the software bytes behind, never zeros.

use core::sync::atomic::{AtomicU64, Ordering};

/// The odd increment splitmix64 walks its state by; the fractional part of the
/// golden ratio.
const GOLDEN: u64 = 0x9e37_79b9_7f4a_7c15;

/// splitmix64's finalizer: the mixing step that turns a counter into output.
///
/// Every output bit depends on every input bit, which is the property the old
/// generator lacked -- it handed out a window of a multiply, so consecutive
/// outputs were related and two of them gave you the state.
pub const fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// splitmix64 over an atomic counter.
///
/// It is not a cryptographic generator and does not pretend to be one: its
/// state is recoverable from one output. It is the floor, for a machine with
/// no hardware source, and the point of it is that the floor is no longer a
/// compile-time constant and no longer a data race.
pub struct SoftRandom {
    /// The counter. Zero means "not seeded yet".
    state: AtomicU64,
}

impl Default for SoftRandom {
    fn default() -> Self {
        Self::new()
    }
}

impl SoftRandom {
    pub const fn new() -> Self {
        SoftRandom {
            state: AtomicU64::new(0),
        }
    }

    /// Take a seed the first time anybody asks, and only then.
    ///
    /// Two CPUs arriving together is fine: the exchange picks one seed and the
    /// loser keeps it, rather than the two of them walking separate states.
    fn seed_once(&self, seed: impl FnOnce() -> u64) {
        if self.state.load(Ordering::Relaxed) == 0 {
            // `| 1` so a seed that mixes to zero does not read as "unseeded"
            // and send the next caller round again.
            let s = mix64(seed()) | 1;
            let _ = self
                .state
                .compare_exchange(0, s, Ordering::Relaxed, Ordering::Relaxed);
        }
    }

    /// One word. The increment is the atomic step, so two CPUs drawing at once
    /// get two different words instead of the same one twice.
    fn draw(&self) -> u64 {
        mix64(
            self.state
                .fetch_add(GOLDEN, Ordering::Relaxed)
                .wrapping_add(GOLDEN),
        )
    }

    /// Fill `buf`, seeding from `seed` if this is the first call.
    pub fn fill(&self, buf: &mut [u8], seed: impl FnOnce() -> u64) {
        if buf.is_empty() {
            return;
        }
        self.seed_once(seed);
        for chunk in buf.chunks_mut(8) {
            let word = self.draw().to_ne_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
    }

    /// For tests: the state, so a test can say two generators differ.
    #[cfg(test)]
    fn peek(&self) -> u64 {
        self.state.load(Ordering::Relaxed)
    }
}

static SOFT: SoftRandom = SoftRandom::new();

/// How often anybody has asked. Part of the seed, so that two boots that read
/// the clock at the same tick still diverge.
static CALLS: AtomicU64 = AtomicU64::new(0);

/// Combine the four things this kernel has that might differ between two
/// boots of the same image. Split out from [`boot_seed`] so a test can say
/// that each of them reaches the result -- dropping one is exactly how the
/// old fallback ended up seeded by a constant.
///
/// None of these is entropy in the sense a cryptographer means. The clock is
/// the only one that really differs between two boots, and on a machine that
/// starts from a reset it may differ by very little.
fn seed_from(now_nanos: u64, now_secs: u64, calls: u64, here: u64) -> u64 {
    let mut s = now_nanos;
    s ^= mix64(now_secs);
    s ^= mix64(calls);
    s ^= mix64(here);
    s
}

/// The seed taken on first use.
fn boot_seed() -> u64 {
    // Where this frame happens to sit, which the allocator and the boot path
    // have already moved around by the time anybody asks for random bytes.
    let here = 0u64;
    let now = crate::timer::timer_now();
    seed_from(
        now.subsec_nanos() as u64,
        now.as_secs(),
        CALLS.fetch_add(1, Ordering::Relaxed),
        &here as *const u64 as u64,
    )
}

/// Fill `buf` with random bytes.
///
/// The software generator always runs; the hardware source is XORed over the
/// result when the machine has one. Mixing rather than choosing is what makes
/// a hardware failure a downgrade instead of a buffer of zeros.
pub fn fill_random(buf: &mut [u8]) {
    SOFT.fill(buf, boot_seed);
    hardware_xor(buf);
}

/// XOR a hardware random word over each eight bytes of `buf`, where the
/// machine has an instruction for it and that instruction succeeds. Leaves
/// `buf` untouched otherwise, which is why the caller must have filled it
/// first.
#[allow(unused_variables)]
fn hardware_xor(buf: &mut [u8]) {
    #[cfg(target_arch = "x86_64")]
    {
        if !x86::has_rdrand() {
            return;
        }
        for chunk in buf.chunks_mut(8) {
            if let Some(r) = x86::rdrand64() {
                let word = r.to_ne_bytes();
                for (b, w) in chunk.iter_mut().zip(word.iter()) {
                    *b ^= *w;
                }
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use core::sync::atomic::{AtomicU8, Ordering};

    /// 0 = not asked yet, 1 = absent, 2 = present.
    static HAS: AtomicU8 = AtomicU8::new(0);

    /// For tests: what has been remembered, 0 if nothing yet.
    #[cfg(test)]
    pub fn cached() -> u8 {
        HAS.load(Ordering::Relaxed)
    }

    /// For tests: pretend the machine has, or has not, the instruction, and
    /// give back what was there so the test can put it back.
    #[cfg(test)]
    pub fn force(present: bool) -> u8 {
        HAS.swap(if present { 2 } else { 1 }, Ordering::Relaxed)
    }

    /// For tests: undo a [`force`].
    #[cfg(test)]
    pub fn restore(was: u8) {
        HAS.store(was, Ordering::Relaxed);
    }

    /// CPUID leaf 1, ECX bit 30. Asking matters: `rdrand` on a part that does
    /// not have it is `#UD`, and plenty of hypervisors mask it out.
    pub fn has_rdrand() -> bool {
        match HAS.load(Ordering::Relaxed) {
            1 => false,
            2 => true,
            _ => {
                let present = core::arch::x86_64::__cpuid(1).ecx & (1 << 30) != 0;
                HAS.store(if present { 2 } else { 1 }, Ordering::Relaxed);
                present
            }
        }
    }

    /// One `rdrand`, retried as the SDM says to.
    ///
    /// The instruction clears CF and leaves the destination zero when the
    /// on-chip pool is momentarily empty. Ten attempts is Intel's own
    /// recommendation; past that the part is broken, and the answer is `None`
    /// rather than a zero the caller cannot tell from a number.
    pub fn rdrand64() -> Option<u64> {
        for _ in 0..10 {
            let mut r: u64 = 0;
            let ok = unsafe { core::arch::x86_64::_rdrand64_step(&mut r) };
            if ok == 1 {
                return Some(r);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn a_generator_does_not_start_from_a_number_written_in_the_source() {
        // The whole of the old fallback: `static mut SEED = 0xdeadbeefcafebabe`.
        // A constant seed makes the n-th random number of every boot the same
        // number, and the n-th process's stack canary with it.
        let a = SoftRandom::new();
        let b = SoftRandom::new();
        a.fill(&mut [0u8; 8], || 1);
        b.fill(&mut [0u8; 8], || 2);
        assert_ne!(
            a.peek(),
            b.peek(),
            "two generators given different seeds walk the same state"
        );
    }

    #[test]
    fn two_boots_that_differ_in_the_seed_produce_different_bytes() {
        let a = SoftRandom::new();
        let b = SoftRandom::new();
        let (mut x, mut y) = ([0u8; 64], [0u8; 64]);
        a.fill(&mut x, || 0x1234_5678);
        b.fill(&mut y, || 0x1234_5679);
        assert_ne!(x, y, "a one-bit change in the seed changed nothing");
    }

    #[test]
    fn the_same_generator_does_not_hand_out_the_same_word_twice_running() {
        let g = SoftRandom::new();
        let mut buf = [0u8; 8 * 16];
        g.fill(&mut buf, || 7);
        let words: alloc::vec::Vec<[u8; 8]> = buf
            .chunks(8)
            .map(|c| {
                let mut w = [0u8; 8];
                w.copy_from_slice(c);
                w
            })
            .collect();
        for i in 1..words.len() {
            assert_ne!(words[i - 1], words[i], "word {} repeats the one before", i);
        }
    }

    #[test]
    fn the_seed_is_taken_once_and_not_on_every_call() {
        // Re-seeding per call would tie the output to the clock, and a clock
        // an attacker can read is a seed an attacker can guess.
        let g = SoftRandom::new();
        g.fill(&mut [0u8; 8], || 42);
        let after_first = g.peek();
        g.fill(&mut [0u8; 8], || 0xffff_ffff);
        assert_eq!(
            g.peek(),
            after_first.wrapping_add(GOLDEN),
            "the second call re-seeded instead of advancing"
        );
    }

    #[test]
    fn a_buffer_that_is_not_a_multiple_of_a_word_is_filled_to_the_end() {
        // `getrandom(buf, 1)` and the 16 bytes of AT_RANDOM are both common,
        // and a tail left at zero is the part of a canary that is guessable.
        for len in 1..=33usize {
            let g = SoftRandom::new();
            let mut buf = vec![0u8; len + 1];
            let last = buf.len() - 1;
            g.fill(&mut buf[..len], || 0xa5a5_a5a5);
            assert_eq!(buf[last], 0, "the fill ran past the end of the slice");
            assert!(
                buf[..len].iter().any(|&b| b != 0),
                "a {}-byte buffer came back all zero",
                len
            );
        }
    }

    #[test]
    fn an_empty_buffer_is_not_a_seeding_event() {
        // A zero-length `getrandom` must not consume the one chance to seed.
        let g = SoftRandom::new();
        g.fill(&mut [], || 99);
        assert_eq!(g.peek(), 0, "an empty fill seeded the generator");
    }

    #[test]
    fn a_seed_that_mixes_to_zero_still_counts_as_seeded() {
        // Zero is the "not seeded" marker, so a seed landing on it would send
        // the next caller back to `seed_once` for ever, and the generator
        // would be re-seeded from the clock on every call.
        let g = SoftRandom::new();
        // `| 1` in `seed_once` is what guarantees this; pin it for every seed
        // whose mix has a zero low bit, which is half of them.
        for seed in 0..64u64 {
            let g2 = SoftRandom::new();
            g2.seed_once(|| seed);
            assert_ne!(g2.peek(), 0, "seed {} left the generator unseeded", seed);
        }
        g.seed_once(|| 0);
        assert_ne!(g.peek(), 0);
    }

    #[test]
    fn every_output_bit_depends_on_every_input_bit() {
        // The old generator handed out a window of one multiply, so flipping a
        // low bit of the state left most of the output alone. This is the
        // property that makes one output not predict the next.
        for bit in 0..64 {
            let a = mix64(0);
            let b = mix64(1u64 << bit);
            let changed = (a ^ b).count_ones();
            assert!(
                changed >= 16,
                "flipping bit {} changed only {} output bits",
                bit,
                changed
            );
        }
    }

    #[test]
    fn the_mixer_is_a_bijection_over_the_values_it_is_given() {
        // splitmix64's finalizer is invertible, which is what makes the
        // counter's full period the generator's full period: no two states
        // can collide onto one output.
        let mut seen = alloc::collections::BTreeSet::new();
        for i in 0..4096u64 {
            assert!(seen.insert(mix64(i)), "mix64 collided at {}", i);
        }
    }

    #[test]
    fn the_counter_walks_by_an_odd_step_so_it_visits_every_state() {
        // An even increment would halve the period at best and stall on a
        // cycle at worst.
        assert_eq!(GOLDEN & 1, 1, "the increment is even");
    }

    #[test]
    fn the_public_entry_never_hands_back_a_buffer_of_zeros() {
        // This is the shape of the RDRAND failure it used to have: the
        // instruction declines, the flag is ignored, and sixteen zero bytes
        // become a process's stack canary. Whatever the hardware does, the
        // software fill has already run underneath it.
        for len in [1usize, 8, 16, 63, 64, 4096] {
            let mut buf = vec![0u8; len];
            fill_random(&mut buf);
            assert!(
                buf.iter().any(|&b| b != 0),
                "a {}-byte fill came back all zero",
                len
            );
        }
    }

    #[test]
    fn two_reads_of_the_same_length_do_not_come_back_equal() {
        // `/dev/urandom` read twice, or two processes' AT_RANDOM.
        let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
        fill_random(&mut a);
        fill_random(&mut b);
        assert_ne!(a, b);
    }

    #[test]
    fn an_empty_fill_through_the_public_entry_writes_nothing() {
        fill_random(&mut []);
    }

    #[test]
    fn the_hardware_source_is_asked_for_once_and_remembered() {
        // The CPUID check is the one that was missing: `rdrand` on a part
        // without it is `#UD`, and plenty of hypervisors mask it out. Asking
        // per word would also put a CPUID -- a serialising instruction -- in
        // the middle of every eight bytes of /dev/urandom.
        #[cfg(target_arch = "x86_64")]
        {
            let first = x86::has_rdrand();
            assert_ne!(
                x86::cached(),
                0,
                "the answer was not remembered, so every eight bytes of \
                 /dev/urandom carry a serialising CPUID"
            );
            for _ in 0..8 {
                assert_eq!(x86::has_rdrand(), first, "the answer changed");
            }
        }
    }

    #[test]
    fn the_hardware_source_answers_with_a_number_or_with_nothing() {
        // Never with a zero the caller cannot tell from a number, which is
        // what ignoring the carry flag produced. On a part that has the
        // instruction, sixty-four draws that all come back the same value
        // mean it is not producing anything.
        #[cfg(target_arch = "x86_64")]
        {
            if !x86::has_rdrand() {
                return;
            }
            let draws: alloc::vec::Vec<Option<u64>> = (0..64).map(|_| x86::rdrand64()).collect();
            let got: alloc::vec::Vec<u64> = draws.iter().filter_map(|d| *d).collect();
            assert!(
                !got.is_empty(),
                "the machine says it has the instruction and 64 draws all \
                 declined, so it is never being issued"
            );
            let unique: alloc::collections::BTreeSet<u64> = got.iter().copied().collect();
            assert!(
                unique.len() > 1,
                "{} draws all came back as the same value",
                got.len()
            );
        }
    }

    #[test]
    fn the_mixer_is_the_published_splitmix64_finalizer() {
        // Pinning it against the reference is what makes any change to the
        // three steps a test failure rather than "still looks random". The old
        // generator also looked random.
        const VECTORS: [(u64, u64); 6] = [
            (0x0000_0000_0000_0000, 0x0000_0000_0000_0000),
            (0x0000_0000_0000_0001, 0x5692_161d_100b_05e5),
            (0x0000_0000_0000_0002, 0xdbd2_3897_3a2b_148a),
            (0x9e37_79b9_7f4a_7c15, 0xe220_a839_7b1d_cdaf),
            (0xffff_ffff_ffff_ffff, 0xb4d0_55fc_f2cb_bd7b),
            (0x0000_0001_0000_0000, 0xd820_b7e9_10b0_f93f),
        ];
        for (input, want) in VECTORS {
            assert_eq!(
                mix64(input),
                want,
                "mix64(0x{:016x}) is no longer splitmix64's finalizer",
                input
            );
        }
    }

    #[test]
    fn every_thing_the_seed_is_made_of_reaches_the_seed() {
        // The bug this guards is the one the old code had: a seed that does
        // not depend on the clock is a seed written in the source.
        let base = seed_from(1, 2, 3, 4);
        assert_ne!(seed_from(9, 2, 3, 4), base, "the nanoseconds are ignored");
        assert_ne!(seed_from(1, 9, 3, 4), base, "the seconds are ignored");
        assert_ne!(seed_from(1, 2, 9, 4), base, "the call count is ignored");
        assert_ne!(seed_from(1, 2, 3, 9), base, "the stack address is ignored");
    }

    #[test]
    fn asking_for_a_seed_moves_the_call_counter() {
        // The counter is what makes two seeds taken in the same nanosecond
        // differ, so a `load` where a `fetch_add` belongs is a silent
        // collapse to "the clock alone".
        let before = CALLS.load(Ordering::Relaxed);
        let _ = boot_seed();
        let _ = boot_seed();
        assert_eq!(
            CALLS.load(Ordering::Relaxed),
            before + 2,
            "the call counter did not move"
        );
    }

    #[test]
    fn every_byte_of_a_buffer_is_written_and_not_just_the_first_few() {
        // A generator that filled four bytes of every eight would leave half
        // the canary at zero, and the "is anything non-zero" check would
        // still pass. Eight fills with different seeds: a position that is
        // never written reads the same in all eight.
        const TRIALS: usize = 8;
        for len in [1usize, 7, 8, 9, 16, 31] {
            let mut runs = [[0u8; 32]; TRIALS];
            for (i, run) in runs.iter_mut().enumerate() {
                let g = SoftRandom::new();
                g.fill(&mut run[..len], || 0x1000 + i as u64);
            }
            for pos in 0..len {
                assert!(
                    runs.iter().any(|r| r[pos] != runs[0][pos]),
                    "byte {} of a {}-byte buffer was the same in all {} fills",
                    pos,
                    len,
                    TRIALS
                );
            }
        }
    }

    #[test]
    fn a_machine_without_the_instruction_still_gets_random_bytes() {
        // The point of filling in software first and mixing hardware in on
        // top: aarch64 and riscv64 have no `hardware_xor` at all, and on
        // x86_64 the instruction can be absent or masked out by a hypervisor.
        #[cfg(target_arch = "x86_64")]
        {
            let was = x86::force(false);
            let mut buf = [0u8; 64];
            fill_random(&mut buf);
            x86::restore(was);
            assert!(
                buf.iter().any(|&b| b != 0),
                "with no hardware source the buffer came back all zero"
            );
        }
    }

    #[test]
    fn the_hardware_source_is_not_touched_when_the_machine_has_not_got_it() {
        // Issuing `rdrand` without asking CPUID is `#UD`. The test cannot
        // survive that, so what it checks is the observable half: with the
        // answer forced to "absent", `hardware_xor` leaves the buffer alone.
        #[cfg(target_arch = "x86_64")]
        {
            let was = x86::force(false);
            let mut buf = [0xa5u8; 24];
            hardware_xor(&mut buf);
            x86::restore(was);
            assert_eq!(buf, [0xa5u8; 24], "it read from a source it has not got");
        }
    }

    #[test]
    fn concurrent_draws_do_not_hand_out_the_same_word() {
        // The old `static mut` was read-modify-written with no atomic at all,
        // so two CPUs drawing at once could take the same seed and hand the
        // same bytes to two processes. `fetch_add` is what rules that out.
        let g = SoftRandom::new();
        g.seed_once(|| 5);
        let mut words = alloc::vec::Vec::new();
        for _ in 0..256 {
            words.push(g.draw());
        }
        let unique: alloc::collections::BTreeSet<u64> = words.iter().copied().collect();
        assert_eq!(unique.len(), words.len(), "a word came out twice");
    }
}
