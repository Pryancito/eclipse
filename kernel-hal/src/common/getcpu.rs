//! What `getcpu` publishes to userspace, as opposed to how it is published.
//!
//! `sched_getcpu()` cost a full trap here -- 13 µs against Linux's 83 ns under
//! the same emulator, 160x -- and the callers are allocators and thread pools
//! sharding per CPU, so every one of those traps sits on a hot path. Linux
//! answers it in userspace: the kernel writes each CPU's own id into
//! `IA32_TSC_AUX`, and `RDTSCP` (or `RDPID`) hands it back in `ECX` without
//! leaving ring 3. The vDSO's `__vdso_getcpu` is three instructions.
//!
//! The encoding is the part worth testing, and the part that cannot be guessed:
//! userspace reads one 32-bit word and has to split it the same way the kernel
//! packed it. Linux's layout, which musl and glibc both assume, is the CPU in
//! the low 12 bits and the NUMA node above them.

/// Bits of the `IA32_TSC_AUX` word that hold the CPU id.
pub const GETCPU_CPU_BITS: u32 = 12;

/// Highest CPU id this encoding can carry.
pub const GETCPU_MAX_CPU: u32 = (1 << GETCPU_CPU_BITS) - 1;

/// Pack a CPU id and a NUMA node into the `IA32_TSC_AUX` word `RDTSCP` returns.
///
/// `None` for a CPU or node the encoding cannot hold: a truncated id would be
/// *wrong* rather than absent, and a wrong answer is the one outcome a userspace
/// fast path must never produce. `MAX_CORE_NUM` is 64, so this is a guard
/// against a future that widens it, not a case that happens today.
pub const fn tsc_aux_word(cpu: u32, node: u32) -> Option<u32> {
    if cpu > GETCPU_MAX_CPU {
        return None;
    }
    match node.checked_shl(GETCPU_CPU_BITS) {
        Some(shifted) if node <= (u32::MAX >> GETCPU_CPU_BITS) => Some(shifted | cpu),
        _ => None,
    }
}

/// Split the word back, the way `__vdso_getcpu` does in userspace.
///
/// Only the tests use this -- the real reader is three lines of C in
/// `linux-vdso/vdso/vdso.c` -- and that is exactly why it is here: it is what
/// makes the round trip assertable at all, so the C and the kernel cannot drift
/// apart without something failing.
pub const fn split_tsc_aux_word(word: u32) -> (u32, u32) {
    (word & GETCPU_MAX_CPU, word >> GETCPU_CPU_BITS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The round trip, on every CPU the machine can have today.
    #[test]
    fn every_cpu_this_machine_can_have_survives_the_round_trip() {
        for cpu in 0..crate::config::MAX_CORE_NUM as u32 {
            let word = tsc_aux_word(cpu, 0).expect("una CPU de esta maquina cabe");
            assert_eq!(split_tsc_aux_word(word), (cpu, 0), "cpu {}", cpu);
        }
    }

    /// The node lands above the CPU, not mixed into it: packing them the other
    /// way round would read as a plausible CPU id and be silently wrong.
    #[test]
    fn the_node_lives_above_the_cpu() {
        let word = tsc_aux_word(5, 3).unwrap();
        assert_eq!(split_tsc_aux_word(word), (5, 3));
        assert_eq!(word, (3 << 12) | 5);
        // A node alone must not read as a CPU.
        assert_eq!(split_tsc_aux_word(tsc_aux_word(0, 7).unwrap()), (0, 7));
    }

    /// The edge of the field, and one past it.
    #[test]
    fn a_cpu_the_field_cannot_hold_is_refused_rather_than_truncated() {
        assert_eq!(GETCPU_MAX_CPU, 4095);
        assert!(tsc_aux_word(GETCPU_MAX_CPU, 0).is_some());
        assert_eq!(tsc_aux_word(GETCPU_MAX_CPU + 1, 0), None);
        assert_eq!(tsc_aux_word(u32::MAX, 0), None);
        assert_eq!(
            split_tsc_aux_word(tsc_aux_word(GETCPU_MAX_CPU, 1).unwrap()),
            (GETCPU_MAX_CPU, 1)
        );
    }

    /// A node that would run off the top of the word is refused too, for the
    /// same reason: it would wrap onto somebody else's node.
    #[test]
    fn a_node_that_would_overflow_the_word_is_refused() {
        let top = u32::MAX >> GETCPU_CPU_BITS;
        assert!(tsc_aux_word(0, top).is_some());
        assert_eq!(tsc_aux_word(0, top + 1), None);
        assert_eq!(tsc_aux_word(0, u32::MAX), None);
    }
}
