use super::*;

/// The most one `zx_cprng_draw_once` will produce, `ZX_CPRNG_DRAW_MAX_LEN`.
///
/// This is the syscall's contract, and the vDSO's `zx_cprng_draw` loops over
/// chunks of it. It is also what keeps the buffer below from being an
/// allocation whose size userspace picks: `len` arrives raw from the caller
/// and `vec![0u8; len]` cannot fail, so without the limit
/// `zx_cprng_draw_once(buf, 1 << 40)` asks the kernel for a terabyte and
/// panics it. No handle and no right is needed to make that call.
const CPRNG_DRAW_MAX_LEN: usize = 256;

impl Syscall<'_> {
    /// Draw random bytes from the kernel CPRNG.
    ///
    /// This data should be suitable for cryptographic applications.
    ///
    /// Clients that require a large volume of randomness should consider using these bytes to seed a user-space random number generator for better performance.
    pub fn sys_cprng_draw_once(&self, mut buf: UserOutPtr<u8>, len: usize) -> ZxResult {
        info!("cprng_draw_once: buf=({:?}; {:?})", buf, len);
        let mut res = vec![0u8; cprng_draw_len(len)?];
        // Fill random bytes to the buffer
        kernel_hal::rand::fill_random(&mut res);
        buf.write_array(&res)?;
        Ok(())
    }
}

/// The buffer a draw of `len` bytes is allowed to allocate.
///
/// Refusing rather than clamping: the syscall reports no count of its own, so a
/// short answer would leave the caller reading whatever was already in its
/// buffer as if the kernel had made it random.
fn cprng_draw_len(len: usize) -> ZxResult<usize> {
    if len > CPRNG_DRAW_MAX_LEN {
        return Err(ZxError::INVALID_ARGS);
    }
    Ok(len)
}

#[cfg(test)]
mod cprng_len_tests {
    use super::*;

    /// The bug: there was no limit at all, so the length userspace passed was
    /// the size of an infallible kernel allocation.
    #[test]
    fn a_hostile_length_is_refused_and_never_allocated() {
        for len in [CPRNG_DRAW_MAX_LEN + 1, 4096, 1 << 20, 1 << 40, usize::MAX] {
            assert_eq!(
                cprng_draw_len(len),
                Err(ZxError::INVALID_ARGS),
                "{:#x} was accepted",
                len
            );
        }
    }

    /// `ZX_CPRNG_DRAW_MAX_LEN` bytes is what the contract promises, so it is a
    /// length the syscall must still serve in full.
    #[test]
    fn the_documented_maximum_is_served_whole() {
        assert_eq!(cprng_draw_len(CPRNG_DRAW_MAX_LEN), Ok(CPRNG_DRAW_MAX_LEN));
        assert_eq!(CPRNG_DRAW_MAX_LEN, 256);
        assert_eq!(cprng_draw_len(0), Ok(0));
        assert_eq!(cprng_draw_len(1), Ok(1));
    }
}
