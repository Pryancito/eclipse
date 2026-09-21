//! N-to-one PCM summing, after SOF's mixer component.
//!
//! SOF (`src/audio/mixer.c`, `mixer_generic.c`) mixes several playback
//! streams into one by adding their samples: `mix_n_s16` walks the sources
//! innermost, accumulates each frame in a wider integer, and saturates once
//! at the end (`sat_int16`). There is no averaging and no per-source gain in
//! the base routine -- a stream that wants to be quieter runs through the
//! [`volume`](super::volume) component first. Summing is the whole job; the
//! only thing that can go wrong is overflow, which is why the accumulator is
//! `i32` and the clamp happens after every source has been added, never
//! between two of them (clamping pairwise would fold `+40000 - 20000` down to
//! `+32767 - 20000` and lose the quiet stream under the loud one).
//!
//! Eclipse has one unmixed S16LE ring per card, so today a second opener gets
//! `EBUSY` and userspace (PulseAudio) does the mixing. This component is what
//! a kernel-side mixer is built from: it is pure, allocates nothing on the
//! hot path, and is exercised without hardware. Wiring it into the ring so
//! more than one client can play at once is the follow-up, and changes a
//! device path, so it is its own change.

/// Clamp an `i32` mix accumulator to one S16 sample (SOF `sat_int16`).
#[inline]
fn sat_s16(acc: i32) -> i16 {
    acc.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// Add the S16LE samples of `src` into the `i32` accumulator `acc`, sample
/// for sample (SOF's inner `buf += src` step, before saturation). `src` short
/// of `acc` contributes silence past its end -- a stream that has run out
/// simply stops adding to the mix -- and any tail of `src` beyond `acc` is
/// dropped, so the caller sizes `acc` to the block it means to emit.
pub fn accumulate_s16(acc: &mut [i32], src: &[u8]) {
    let (samples, _) = src.as_chunks::<2>();
    for (a, s) in acc.iter_mut().zip(samples.iter()) {
        *a += i16::from_le_bytes(*s) as i32;
    }
}

/// Saturate an `i32` mix accumulator into interleaved S16LE `dst` and clear it
/// back to silence for the next block (SOF `sat_int16` on the way out). `dst`
/// takes two bytes per accumulator entry; a `dst` shorter than `acc` writes
/// only what fits, and the whole of `acc` is still cleared so no stale sum
/// carries into the next period.
pub fn drain_to_s16(acc: &mut [i32], dst: &mut [u8]) {
    let (out, _) = dst.as_chunks_mut::<2>();
    for (a, d) in acc.iter_mut().zip(out.iter_mut()) {
        *d = sat_s16(*a).to_le_bytes();
        *a = 0;
    }
    for a in acc.iter_mut() {
        *a = 0;
    }
}

/// Mix several interleaved S16LE streams into `dst` in one pass, saturating
/// (SOF `mix_n_s16`). Each source contributes over the samples it and `dst`
/// share; a shorter source is silence past its end. `acc` is caller-provided
/// `i32` scratch, at least `dst.len() / 2` long, so the hot path allocates
/// nothing; its contents on entry are ignored and it is left cleared.
pub fn mix_s16(sources: &[&[u8]], dst: &mut [u8], acc: &mut [i32]) {
    let out_samples = dst.len() / 2;
    let n = out_samples.min(acc.len());
    let acc = &mut acc[..n];
    for a in acc.iter_mut() {
        *a = 0;
    }
    for src in sources {
        accumulate_s16(acc, src);
    }
    drain_to_s16(acc, dst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    fn pcm(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    fn samples(bytes: &[u8]) -> Vec<i16> {
        bytes
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    fn mix(sources: &[&[u8]], out_samples: usize) -> Vec<i16> {
        let mut dst = vec![0u8; out_samples * 2];
        let mut acc = vec![0i32; out_samples];
        mix_s16(sources, &mut dst, &mut acc);
        samples(&dst)
    }

    #[test]
    fn two_streams_add() {
        let a = pcm(&[100, -200, 300, -400]);
        let b = pcm(&[1, 2, 3, 4]);
        assert_eq!(mix(&[&a, &b], 4), [101, -198, 303, -396]);
    }

    #[test]
    fn no_sources_is_silence() {
        assert_eq!(mix(&[], 4), [0, 0, 0, 0]);
    }

    #[test]
    fn one_source_is_a_copy() {
        let a = pcm(&[1, -1, 32767, -32768, 12345]);
        assert_eq!(mix(&[&a], 5), [1, -1, 32767, -32768, 12345]);
    }

    #[test]
    fn the_sum_saturates_once_at_the_end_not_pairwise() {
        // +40000 clips to +32767 on its own, but the third stream pulls the
        // true sum back into range. A pairwise clamp would lose that: it would
        // clamp 20000 + 20000 to 32767 first, then subtract, landing at 12767
        // instead of the correct 15000.
        let a = pcm(&[20000, -30000]);
        let b = pcm(&[20000, -30000]);
        let c = pcm(&[-25000, 25000]);
        assert_eq!(mix(&[&a, &b, &c], 2), [15000, -32768]);
    }

    #[test]
    fn positive_and_negative_overflow_clamp() {
        let a = pcm(&[30000, -30000]);
        let b = pcm(&[30000, -30000]);
        assert_eq!(mix(&[&a, &b], 2), [32767, -32768]);
    }

    #[test]
    fn a_shorter_source_is_silence_past_its_end() {
        // b runs out after two samples; the mix keeps going on a alone.
        let a = pcm(&[100, 100, 100, 100]);
        let b = pcm(&[50, 50]);
        assert_eq!(mix(&[&a, &b], 4), [150, 150, 100, 100]);
    }

    #[test]
    fn accumulate_then_drain_matches_one_shot() {
        let a = pcm(&[5000, -6000, 7000, -8000]);
        let b = pcm(&[1000, 2000, 3000, 4000]);
        let c = pcm(&[-500, -500, -500, -500]);

        let mut dst1 = vec![0u8; 8];
        let mut acc1 = vec![0i32; 4];
        mix_s16(&[&a, &b, &c], &mut dst1, &mut acc1);

        let mut acc2 = vec![0i32; 4];
        accumulate_s16(&mut acc2, &a);
        accumulate_s16(&mut acc2, &b);
        accumulate_s16(&mut acc2, &c);
        let mut dst2 = vec![0u8; 8];
        drain_to_s16(&mut acc2, &mut dst2);

        assert_eq!(dst1, dst2);
    }

    #[test]
    fn drain_clears_the_accumulator_for_the_next_block() {
        let mut acc = vec![0i32; 4];
        accumulate_s16(&mut acc, &pcm(&[10000, 10000, 10000, 10000]));
        let mut dst = vec![0u8; 8];
        drain_to_s16(&mut acc, &mut dst);
        assert!(acc.iter().all(|&a| a == 0), "acc not cleared: {:?}", acc);
        // A fresh block does not inherit the previous sum.
        accumulate_s16(&mut acc, &pcm(&[1, 2, 3, 4]));
        drain_to_s16(&mut acc, &mut dst);
        assert_eq!(samples(&dst), [1, 2, 3, 4]);
    }

    #[test]
    fn an_odd_trailing_byte_in_a_source_is_ignored() {
        // as_chunks::<2>() drops a dangling byte, matching a frame-aligned ring.
        let a = pcm(&[100, 200]);
        let mut ragged = pcm(&[1, 2]);
        ragged.push(0x7f);
        assert_eq!(mix(&[&a, &ragged], 2), [101, 202]);
    }
}
