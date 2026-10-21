//! `struct sched_attr` as `sched_setattr(2)` reads it and `sched_getattr(2)`
//! writes it: version 1, 56 bytes, the last two words the util clamps.
//!
//! This kernel knew only version 0, the first 48 bytes. `sched_getattr`
//! reported a size of 48 to a caller that asked for 56, and `sched_setattr`
//! refused a 56-byte struct with `E2BIG` as soon as either util word was not
//! zero, even with no clamp flag set, where Linux ignores them.

use super::*;

/// Size of `struct sched_attr` version 1 (`SCHED_ATTR_SIZE_VER1`), the one
/// with `sched_util_min` and `sched_util_max`, and the whole of what this
/// kernel knows of that struct.
pub(crate) const SCHED_ATTR_SIZE_VER1: usize = 56;

/// `SCHED_FLAG_UTIL_CLAMP_MIN | SCHED_FLAG_UTIL_CLAMP_MAX`.
pub(crate) const SCHED_FLAG_UTIL_CLAMP: u64 = 0x60;

impl SchedAttr {
    /// The struct out of its bytes, in the uapi layout.
    pub(crate) fn from_bytes(b: &[u8; SCHED_ATTR_SIZE_VER1]) -> Self {
        let word = |at: usize| u32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
        let long = |at: usize| {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&b[at..at + 8]);
            u64::from_ne_bytes(bytes)
        };
        SchedAttr {
            size: word(0),
            sched_policy: word(4),
            sched_flags: long(8),
            sched_nice: word(16) as i32,
            sched_priority: word(20),
            sched_runtime: long(24),
            sched_deadline: long(32),
            sched_period: long(40),
            sched_util_min: word(48),
            sched_util_max: word(52),
        }
    }

    /// And its bytes, in the same layout.
    pub(crate) fn to_bytes(self) -> [u8; SCHED_ATTR_SIZE_VER1] {
        let mut b = [0u8; SCHED_ATTR_SIZE_VER1];
        b[0..4].copy_from_slice(&self.size.to_ne_bytes());
        b[4..8].copy_from_slice(&self.sched_policy.to_ne_bytes());
        b[8..16].copy_from_slice(&self.sched_flags.to_ne_bytes());
        b[16..20].copy_from_slice(&self.sched_nice.to_ne_bytes());
        b[20..24].copy_from_slice(&self.sched_priority.to_ne_bytes());
        b[24..32].copy_from_slice(&self.sched_runtime.to_ne_bytes());
        b[32..40].copy_from_slice(&self.sched_deadline.to_ne_bytes());
        b[40..48].copy_from_slice(&self.sched_period.to_ne_bytes());
        b[48..52].copy_from_slice(&self.sched_util_min.to_ne_bytes());
        b[52..56].copy_from_slice(&self.sched_util_max.to_ne_bytes());
        b
    }
}

/// `sched_copy_attr`: the struct the caller built, read as far as its `size`
/// says and zero past that, with the nice brought into range.
///
/// A size it cannot take, or a struct with a byte set past the 56 this
/// kernel knows, is `E2BIG`, and `size` is overwritten with 56 so that the
/// caller can tell which version to build: `put_user(sizeof(*attr),
/// &uattr->size)`, whose own failure nobody hears of. A clamp flag in a
/// struct too short to hold the clamps is `EINVAL`. Both come before the
/// task is looked up.
pub(crate) fn sched_copy_attr(addr: usize) -> LxResult<SchedAttr> {
    let size = match sched_setattr_size(UserInPtr::<u32>::from(addr).read()?) {
        Ok(size) => size,
        Err(e) => return Err(sched_attr_size_refused(addr, e)),
    };
    let known = size.min(SCHED_ATTR_SIZE_VER1);
    if size > known {
        let tail = UserInPtr::<u8>::from(addr + known).read_array(size - known)?;
        if !extensible_tail_is_empty(&tail) {
            return Err(sched_attr_size_refused(addr, LxError::E2BIG));
        }
    }
    let mut bytes = [0u8; SCHED_ATTR_SIZE_VER1];
    bytes[..known].copy_from_slice(&UserInPtr::<u8>::from(addr).read_array(known)?);
    let mut attr = SchedAttr::from_bytes(&bytes);
    if attr.sched_flags & SCHED_FLAG_UTIL_CLAMP != 0 && size < SCHED_ATTR_SIZE_VER1 {
        return Err(LxError::EINVAL);
    }
    // Linux is "lenient like existing syscalls" here: a nice out of range is
    // the nearest one in range, not an error.
    attr.sched_nice = attr.sched_nice.clamp(MIN_NICE as i32, MAX_NICE as i32);
    Ok(attr)
}

/// `err_size` in `sched_copy_attr`: tell the caller the size this kernel
/// takes, and refuse.
fn sched_attr_size_refused(addr: usize, error: LxError) -> LxError {
    let _ = UserOutPtr::<u32>::from(addr).write(SCHED_ATTR_SIZE_VER1 as u32);
    error
}

/// What `sched_getattr` writes into a buffer of `size` bytes, as
/// `copy_struct_to_user` does it: as much of `attr` as fits, `size` saying
/// how much that was, and zeros over the part of a bigger buffer that this
/// kernel has nothing to put in.
pub(crate) fn sched_attr_out(mut attr: SchedAttr, size: usize) -> Vec<u8> {
    let known = size.min(SCHED_ATTR_SIZE_VER1);
    attr.size = known as u32;
    let mut out = alloc::vec![0u8; size];
    out[..known].copy_from_slice(&attr.to_bytes()[..known]);
    out
}

/// The flags `sched_setattr` knows, checked where `__sched_setscheduler`
/// checks them: after the task has been found, so a pid that names nobody
/// is `ESRCH` whatever the flags say.
pub(crate) fn sched_flags_known(sched_flags: u64) -> LxResult<()> {
    if sched_flags & !SCHED_FLAG_ALL != 0 {
        return Err(LxError::EINVAL);
    }
    Ok(())
}
