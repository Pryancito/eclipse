//! `struct rlimit`, whose narrow form is not a truncation but a **clamp**.
//!
//! ```text
//! native / struct rlimit64        i386 / struct rlimit
//! struct rlimit64 {               struct rlimit {
//!     __u64 rlim_cur;  // 0, 8        unsigned long rlim_cur;  // 0, 4
//!     __u64 rlim_max;  // 8, 8        unsigned long rlim_max;  // 4, 4
//! };  // 16 bytes                 };  // 8 bytes
//! ```
//!
//! Two words halved, which looks like every structure in [`crate::narrow`]
//! --- and is not one of them, because what a limit *means* changes with the
//! width. `RLIM64_INFINITY` is `~0ULL` and `RLIM_INFINITY` is `~0UL`, so the
//! same idea is a different number in each ABI, and a limit that is merely
//! very large is not the same thing as no limit at all.
//!
//! Measured against this container's Linux rather than reasoned about, with
//! an i386 program that sets a limit through `prlimit64` and reads it back
//! through the narrow calls:
//!
//! | 64-bit limit | `ugetrlimit` (191) | `getrlimit` (76) |
//! |---|---|---|
//! | 1000 | 1000 | 1000 |
//! | `1 << 33` | **`0xffffffff`** | `0x7fffffff` |
//! | `RLIM64_INFINITY` | `0xffffffff` | `0x7fffffff` |
//!
//! So reading out **clamps**, and `1 << 33` does not come back as 0, which
//! is what truncating it would give --- "no core dumps at all" for a process
//! whose limit is eight gigabytes. And the two narrow calls clamp at
//! different numbers: 191 at `0xffffffff` and 76 at `0x7fffffff`, which is
//! `compat_sys_old_getrlimit` keeping a limit inside a *signed* long for the
//! programs that read one into an `int`.
//!
//! Writing in goes the other way, also measured:
//!
//! | narrow `rlim_cur` | 64-bit limit afterwards |
//! |---|---|
//! | 1234 | 1234 |
//! | `0xffffffff` | **`RLIM64_INFINITY`** |
//!
//! `0xffffffff` is "infinity" and not the number 4294967295, so it widens to
//! `~0ULL`. Widening it as a plain number would give a process that asked for
//! no limit a limit of four gigabytes, which is the kind of thing that is
//! found years later as a program that works everywhere but here.
//!
//! `prlimit64` (340) is not here: its structure is `struct rlimit64` in both
//! ABIs, 16 bytes with the same two fields, so it is in `i386_abi_same.in`
//! with nothing to convert. It is also what musl's `getrlimit(3)` actually
//! calls, which makes the two below the compatibility path rather than the
//! common one.

use super::*;
use linux_object::process::{RLimit, RLIM_INFINITY};

/// `RLIM_INFINITY` as a 32-bit `unsigned long`: what `ugetrlimit` (191) says
/// for a limit that does not fit, and what it reads as "no limit".
const COMPAT_RLIM_INFINITY: u32 = u32::MAX;

/// And what the older `getrlimit` (76) clamps to instead: `RLIM_INFINITY`
/// kept inside a *signed* long. Measured; see the module documentation.
const OLD_RLIM_INFINITY: u32 = i32::MAX as u32;

/// The size of the narrow `struct rlimit`: two 32-bit words.
const RLIMIT32_SIZE: usize = 8;

/// One word of a narrow `struct rlimit`, widened: `0xffffffff` is infinity
/// and not the number it spells.
fn widen(word: u32) -> u64 {
    if word == COMPAT_RLIM_INFINITY {
        RLIM_INFINITY
    } else {
        u64::from(word)
    }
}

/// One 64-bit limit, narrowed: **clamped** at `ceiling`, never truncated.
fn narrow_word(value: u64, ceiling: u32) -> u32 {
    if value > u64::from(ceiling) {
        ceiling
    } else {
        value as u32
    }
}

impl Syscall<'_> {
    /// `ugetrlimit(2)`, i386 191: the limit as two 32-bit words, each
    /// clamped at `0xffffffff`.
    ///
    /// i386 calls it `ugetrlimit` because 76 was already taken by the
    /// version that clamps differently; x86_64, which never had either, has
    /// only `getrlimit`.
    pub fn sys_ugetrlimit(&self, resource: usize, rlim: usize) -> SysResult {
        self.getrlimit32(resource, rlim, COMPAT_RLIM_INFINITY)
    }

    /// `getrlimit(2)`, i386 76: the same two words, clamped at `0x7fffffff`.
    /// This is Linux's `compat_sys_old_getrlimit`, and the only difference
    /// between the two calls is that number.
    pub fn sys_old_getrlimit32(&self, resource: usize, rlim: usize) -> SysResult {
        self.getrlimit32(resource, rlim, OLD_RLIM_INFINITY)
    }

    fn getrlimit32(&self, resource: usize, rlim: usize, ceiling: u32) -> SysResult {
        info!(
            "getrlimit32: resource={}, rlim={:#x}, ceiling={:#x}",
            resource, rlim, ceiling
        );
        if rlim == 0 {
            return Err(LxError::EFAULT);
        }
        // The limit is read before the pointer is written, so a bad resource
        // is EINVAL and leaves the caller's buffer alone.
        let limit = self.do_prlimit(0, resource, None)?;
        let mut out = [0u8; RLIMIT32_SIZE];
        out[0..4].copy_from_slice(&narrow_word(limit.cur, ceiling).to_ne_bytes());
        out[4..8].copy_from_slice(&narrow_word(limit.max, ceiling).to_ne_bytes());
        // Bytes, not `UserOutPtr<Something>::write`: `UserPtr::user_bytes`
        // would require the destination be aligned to that type and
        // `copy_to_user` requires nothing of the sort. See
        // [`crate::narrow`].
        let mut ptr: UserOutPtr<u8> = rlim.into();
        ptr.write_array(&out)?;
        Ok(0)
    }

    /// `setrlimit(2)`, i386 75: the new limit as two 32-bit words, each
    /// widened with `0xffffffff` meaning infinity.
    ///
    /// There is no `usetrlimit`: the write direction never needed a second
    /// number, because the two clamps differ only in what is reported.
    pub fn sys_setrlimit32(&self, resource: usize, rlim: usize) -> SysResult {
        info!("setrlimit32: resource={}, rlim={:#x}", resource, rlim);
        if rlim == 0 {
            return Err(LxError::EFAULT);
        }
        let ptr: UserInPtr<u8> = rlim.into();
        let bytes = ptr.read_array(RLIMIT32_SIZE)?;
        let word = |at: usize| {
            u32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        let new = RLimit {
            cur: widen(word(0)),
            max: widen(word(4)),
        };
        // A soft limit above the hard one is EINVAL, and it is the shared
        // `do_prlimit` that says so rather than anything here: widening must
        // not be what decides a refusal. Measured: the narrow call answers
        // EINVAL for `{1000, 500}` exactly as the native one does.
        self.do_prlimit(0, resource, Some(new))?;
        Ok(0)
    }
}

#[cfg(test)]
mod tests;
