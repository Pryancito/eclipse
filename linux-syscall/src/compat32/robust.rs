//! `struct robust_list_head`, whose every field is a pointer or a `long` ---
//! so the 32-bit one is not the native one with something taken off the end,
//! it is the native one at half the stride, and its LENGTH is an argument the
//! kernel checks.
//!
//! ```text
//! native                              i386
//! struct robust_list_head {           struct robust_list_head {
//!     struct robust_list *list;  //  0    compat_uptr_t list;       //  0
//!     long futex_offset;         //  8    compat_long_t futex_offset; // 4
//!     struct robust_list *pending; // 16   compat_uptr_t pending;    //  8
//! };  // 24 bytes                     };  // 12 bytes
//! ```
//!
//! `set_robust_list(head, len)` takes `len` and refuses anything that is not
//! `sizeof(*head)`, so the native call and this one refuse each other's
//! length. Measured in this container's Linux with an i386 program:
//!
//! | `len` | i386 311 |
//! |---|---|
//! | 12 | 0 |
//! | 24 | **`EINVAL`** |
//! | 0, 13 | `EINVAL` |
//!
//! Which is why the number cannot be translated and forwarded: the native
//! handler would answer `EINVAL` to the only length a 32-bit libc ever
//! passes. Both glibc and musl call this on every thread they start, so it is
//! not a corner: before this, every thread of every 32-bit program got
//! `ENOSYS` where Linux answers 0 --- and then died holding its locks, which
//! is the half nobody notices until a program hangs.
//!
//! ## The half that is not a system call
//!
//! A robust list is read by the kernel at thread EXIT, and the stride is a
//! property of the list, not of the kernel: see
//! [`linux_object::thread::walk_robust_list`], which takes the width, and
//! [`linux_object::thread::robust_word_bytes`], which reads it off the length
//! that was registered. That is Linux's `compat_exit_robust_list`, the
//! companion of `compat_sys_set_robust_list`.
//!
//! Measured too, with a 32-bit thread that registers a list naming a futex
//! word it owns and then exits: Linux marks the word `FUTEX_OWNER_DIED`
//! (`0x40000000`) and clears the owner's tid, exactly as it does for a native
//! thread.
//!
//! ## And one deliberate divergence
//!
//! Linux keeps a 32-bit thread's list in a SECOND field
//! (`task->compat_robust_list`), so a native `get_robust_list` asked about a
//! 32-bit thread answers its empty native field. This kernel has one field,
//! so it answers the list the thread really registered. Nothing in userspace
//! can tell the difference without being a debugger of another ABI's threads,
//! and a second field would have to be kept in step at every exit and exec.

use super::*;
use linux_object::thread::{ThreadExt, ROBUST_LIST32_SIZE};

/// `set_robust_list`, i386 311.
pub(super) const I386_SET_ROBUST_LIST: u32 = 311;

/// `get_robust_list`, i386 312.
pub(super) const I386_GET_ROBUST_LIST: u32 = 312;

impl Syscall<'_> {
    /// `set_robust_list(2)`, i386 311: the head of this thread's list of
    /// robust mutexes, whose structure is twelve bytes.
    ///
    /// Nothing is read from the head here, by Linux or by this: only the
    /// address is kept, and the walk at thread exit is what reads it. So an
    /// unaligned head is accepted -- measured, Linux answers 0 for a head at
    /// 1 modulo 4 -- and a null one is as well, which is how a libc says
    /// "forget the list I registered".
    pub fn sys_set_robust_list32(&self, head: usize, len: usize) -> SysResult {
        info!("set_robust_list32: head={:#x}, len={}", head, len);
        if len != ROBUST_LIST32_SIZE {
            return Err(LxError::EINVAL);
        }
        self.thread.set_robust_list(head.into(), len);
        Ok(0)
    }

    /// `get_robust_list(2)`, i386 312: the head and its length, as two
    /// 4-byte words.
    ///
    /// The length answered is this ABI's `sizeof(*head)` and not whatever
    /// the thread registered, which is what the native call does too: Linux
    /// answers 12 here even for a thread that never called 311.
    pub fn sys_get_robust_list32(&self, pid: i32, head_ptr: usize, len_ptr: usize) -> SysResult {
        info!(
            "get_robust_list32: pid={}, head_ptr={:#x}, len_ptr={:#x}",
            pid, head_ptr, len_ptr
        );
        let task = self.robust_list_target(pid)?;
        let head = task.robust_list_head() as u32;
        // Four bytes, and only four: a 32-bit caller's `size_t *` names a
        // word of its own width, and writing eight would scribble over
        // whatever follows it. Written as bytes rather than through a typed
        // `UserOutPtr`, which would also require an alignment the real call
        // does not: see [`crate::narrow`].
        let mut head_out: UserOutPtr<u8> = head_ptr.into();
        head_out.write_array(&head.to_ne_bytes())?;
        let mut len_out: UserOutPtr<u8> = len_ptr.into();
        len_out.write_array(&(ROBUST_LIST32_SIZE as u32).to_ne_bytes())?;
        Ok(0)
    }
}

#[cfg(test)]
mod tests;
