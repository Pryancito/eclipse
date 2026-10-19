//! `struct rlimit` as `getrlimit(2)` and `prlimit64(2)` hand it over.
//!
//! The soft limit comes first and the hard limit second, both the same
//! width -- so a swap compiles clean and reports the hard limit where
//! the soft one belongs. Which is not hypothetical here: a stack limit
//! that announced 8 MiB while the loader mapped 512 KiB is what killed
//! halloy in `libvulkan`, and the number userspace believed came out of
//! this struct.
//!
//! Against `include/uapi/linux/resource.h`.

use super::*;
use core::mem::{align_of, offset_of, size_of};

#[test]
fn rlimit_is_the_two_rlim_t_of_struct_rlimit_the_soft_one_first() {
    assert_eq!(size_of::<RLimit>(), 16);
    assert_eq!(align_of::<RLimit>(), 8);
    assert_eq!(offset_of!(RLimit, cur), 0);
    assert_eq!(offset_of!(RLimit, max), 8);
    assert_eq!(offset_of!(RLimit, max) + 8, 16);
}
