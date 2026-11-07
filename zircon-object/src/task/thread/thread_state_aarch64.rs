//! The AArch64 vector half of `zx_thread_read_state`/`zx_thread_write_state`:
//! `ZX_THREAD_STATE_VECTOR_REGS`, which on this architecture is `fpcr`,
//! `fpsr` and `v0`-`v31`.
//!
//! Pure functions over the thread's saved `q` registers (see
//! `UserContext::vector_state`, sixty-four doublewords, the low half of
//! each register first) and the caller's buffer, so every path here runs on
//! a host `cargo test`. The layout is Zircon's `zx_thread_state_vector_regs_t`
//! for arm64 in `zircon/syscalls/debug.h`: two 32-bit words and then thirty-two
//! `{low, high}` pairs, 520 bytes with no padding.
//!
//! There is no `ZX_THREAD_STATE_FP_REGS` here: on arm64 the floating-point
//! registers are the low halves of the vector ones, and Zircon's own
//! `arch/arm64/debugger.cc` answers `ZX_ERR_NOT_SUPPORTED` for that kind,
//! which the core tests expect.

use crate::{ZxError, ZxResult};
use core::convert::TryInto;

/// `zx_thread_state_vector_regs_t` on arm64: `fpcr`, `fpsr`, `v[32]`.
pub(super) const VECTOR_REGS_SIZE: usize = 4 + 4 + 32 * 16;

const FPCR: usize = 0;
const FPSR: usize = 4;
const V: usize = 8;

/// `ZX_THREAD_STATE_VECTOR_REGS` out of the saved registers.
pub(super) fn read_vector_regs(
    q: &[u64; 64],
    fpcr: usize,
    fpsr: usize,
    out: &mut [u8],
) -> ZxResult<usize> {
    let out = out
        .get_mut(..VECTOR_REGS_SIZE)
        .ok_or(ZxError::BUFFER_TOO_SMALL)?;
    out[FPCR..FPCR + 4].copy_from_slice(&(fpcr as u32).to_le_bytes());
    out[FPSR..FPSR + 4].copy_from_slice(&(fpsr as u32).to_le_bytes());
    for (word, bytes) in q.iter().zip(out[V..].as_chunks_mut::<8>().0) {
        *bytes = word.to_le_bytes();
    }
    Ok(VECTOR_REGS_SIZE)
}

/// `ZX_THREAD_STATE_VECTOR_REGS` into the saved registers. Every field is
/// the thread's to set, as in Zircon's `arch_set_vector_regs`: `fpcr` bits
/// the CPU does not implement are RES0 and the hardware ignores them.
pub(super) fn write_vector_regs(
    q: &mut [u64; 64],
    fpcr: &mut usize,
    fpsr: &mut usize,
    buf: &[u8],
) -> ZxResult {
    let buf = buf
        .get(..VECTOR_REGS_SIZE)
        .ok_or(ZxError::BUFFER_TOO_SMALL)?;
    *fpcr = u32::from_le_bytes(buf[FPCR..FPCR + 4].try_into().unwrap()) as usize;
    *fpsr = u32::from_le_bytes(buf[FPSR..FPSR + 4].try_into().unwrap()) as usize;
    for (word, bytes) in q.iter_mut().zip(buf[V..].as_chunks::<8>().0) {
        *word = u64::from_le_bytes(*bytes);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_zircons() {
        // `zx_thread_state_vector_regs_t` (arm64): two uint32_t, then
        // thirty-two `{uint64_t low, high}`.
        assert_eq!(VECTOR_REGS_SIZE, 520);
        assert_eq!(V, 8);
    }

    fn registers() -> [u64; 64] {
        let mut q = [0u64; 64];
        for (i, word) in q.iter_mut().enumerate() {
            *word = 0x0101_0101_0101_0101 * (i as u64 + 1);
        }
        q
    }

    #[test]
    fn read_lays_out_fpcr_fpsr_then_low_high_pairs() {
        let q = registers();
        let mut out = [0xaau8; VECTOR_REGS_SIZE + 8];
        assert_eq!(
            read_vector_regs(&q, 0x0040_0000, 0xf000_0010, &mut out),
            Ok(VECTOR_REGS_SIZE)
        );
        assert_eq!(&out[0..4], &0x0040_0000u32.to_le_bytes());
        assert_eq!(&out[4..8], &0xf000_0010u32.to_le_bytes());
        // v0.low is q[0], v0.high is q[1], v31.high is q[63].
        assert_eq!(&out[8..16], &q[0].to_le_bytes());
        assert_eq!(&out[16..24], &q[1].to_le_bytes());
        assert_eq!(&out[8 + 31 * 16 + 8..520], &q[63].to_le_bytes());
        // Nothing past the struct is touched.
        assert_eq!(&out[520..], &[0xaa; 8]);
    }

    #[test]
    fn write_then_read_round_trips() {
        let q = registers();
        let mut image = [0u8; VECTOR_REGS_SIZE];
        read_vector_regs(&q, 0x0100_0000, 0x1f, &mut image).unwrap();

        let mut back = [0u64; 64];
        let (mut fpcr, mut fpsr) = (usize::MAX, usize::MAX);
        assert_eq!(
            write_vector_regs(&mut back, &mut fpcr, &mut fpsr, &image),
            Ok(())
        );
        assert_eq!(back, q);
        assert_eq!(fpcr, 0x0100_0000);
        assert_eq!(fpsr, 0x1f);
    }

    #[test]
    fn write_only_reads_the_struct() {
        // A longer buffer is fine (Zircon takes the first struct's worth);
        // the bytes past it are not looked at.
        let mut image = [0u8; VECTOR_REGS_SIZE + 16];
        image[VECTOR_REGS_SIZE..].fill(0xff);
        image[8..16].copy_from_slice(&0x1234_5678_9abc_def0u64.to_le_bytes());
        let mut q = [0u64; 64];
        let (mut fpcr, mut fpsr) = (0, 0);
        write_vector_regs(&mut q, &mut fpcr, &mut fpsr, &image).unwrap();
        assert_eq!(q[0], 0x1234_5678_9abc_def0);
        assert_eq!(q[1..], [0; 63]);
    }

    #[test]
    fn short_buffers_are_refused_untouched() {
        let q = registers();
        let mut out = [0u8; VECTOR_REGS_SIZE - 1];
        assert_eq!(
            read_vector_regs(&q, 0, 0, &mut out),
            Err(ZxError::BUFFER_TOO_SMALL)
        );
        assert_eq!(out, [0u8; VECTOR_REGS_SIZE - 1]);

        let mut back = [7u64; 64];
        let (mut fpcr, mut fpsr) = (1, 2);
        assert_eq!(
            write_vector_regs(&mut back, &mut fpcr, &mut fpsr, &out),
            Err(ZxError::BUFFER_TOO_SMALL)
        );
        assert_eq!(back, [7u64; 64]);
        assert_eq!((fpcr, fpsr), (1, 2));
    }
}
