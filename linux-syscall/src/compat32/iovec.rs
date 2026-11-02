//! The scatter/gather calls, whose list is a 32-bit `struct iovec`.
//!
//! ```text
//! native                          i386
//! struct iovec {                  struct iovec {
//!     void  *iov_base;  // 0, 8       void  *iov_base;  // 0, 4
//!     size_t iov_len;   // 8, 8       size_t iov_len;   // 4, 4
//! };  // 16 bytes                 };  // 8 bytes
//! ```
//!
//! Half the size, and that is the whole of it --- there is no reordering and
//! no sign to get wrong, because a length is unsigned and a 32-bit address is
//! an address in the low 4 GiB. What makes it worth a module is **what
//! happens when it is read at the wrong width**, which is not a failure:
//! reading the narrow list as the wide one pairs the halves wrongly. The
//! first buffer's address comes out as entry 0's base with entry 0's *length*
//! in its high half, and its length as the whole of entry 1 --- an address
//! nothing in the list named, and a length of hundreds of gigabytes. `writev`
//! walks it and reports a count.
//!
//! So the width is decided once, by the process's ABI, in
//! [`Syscall::read_iovecs`](crate::Syscall::read_iovecs) --- Linux's
//! `in_compat_syscall()`. The native handlers call that, which is why
//! `readv`(145) and `writev`(146) below are the native handlers unchanged:
//! the list they read is already the right one.
//!
//! `preadv`(333) and `pwritev`(334) need this module for the other reason:
//! their offset arrives split across two registers, **low first**, as
//! `pread64`'s does. See [`super::wide`] for the table of which call puts
//! which half where.
//!
//! Not here yet: `preadv2`(378) and `pwritev2`(379), whose split offset has
//! the extra rule that `(-1, -1)` means "use the file position"; and
//! `process_vm_readv`(347), whose *two* lists live in two different address
//! spaces, only one of which is 32-bit.

use super::*;

impl Syscall<'_> {
    /// `preadv(2)`, i386 333: the offset in two registers, **low first**.
    pub async fn sys_preadv32(
        &self,
        fd: usize,
        iov: usize,
        count: usize,
        offset_low: usize,
        offset_high: usize,
    ) -> SysResult {
        self.sys_preadv(
            (fd as i32).into(),
            iov.into(),
            count,
            super::wide::join(offset_low, offset_high),
        )
        .await
    }

    /// `pwritev(2)`, i386 334: the offset in two registers, **low first**.
    pub fn sys_pwritev32(
        &self,
        fd: usize,
        iov: usize,
        count: usize,
        offset_low: usize,
        offset_high: usize,
    ) -> SysResult {
        self.sys_pwritev(
            (fd as i32).into(),
            iov.into(),
            count,
            super::wide::join(offset_low, offset_high),
        )
    }
}

#[cfg(test)]
mod tests;
