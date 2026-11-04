//! The `struct sigevent` of `timer_create(2)`, read at the caller's width.
//!
//! This one is neither a run of halved words nor the same bytes at a looser
//! alignment: **every field moves, and the first one also changes size.** Its
//! first member is a `union sigval`, which holds a pointer, so it is eight
//! bytes here and four to a 32-bit process --- and everything behind it
//! shifts by that difference, while `sigev_notify_thread_id` shifts by eight,
//! because natively it sits in a union that holds pointers and is therefore
//! 8-aligned.
//!
//! | field | native | i386 |
//! |---|---|---|
//! | `sigev_value` | 0, **8** bytes | 0, **4** bytes |
//! | `sigev_signo` | 8 | **4** |
//! | `sigev_notify` | 12 | **8** |
//! | `sigev_notify_thread_id` | 16 | **12** |
//!
//! The native column is measured, not read off a header: a C program on this
//! container's Linux printing `offsetof` gives 0 (8 bytes), 8, 12 and 16,
//! with `sizeof(struct sigevent) == 64` and `_Alignof == 8`. The i386 column
//! is Linux's `struct compat_sigevent`, whose four members are a
//! `compat_sigval_t` and three `compat_int_t` --- four bytes each, no padding,
//! because nothing in it is wider than an `int` on i386. The probe checks it
//! live, by passing bytes that are a valid request at one set of offsets and
//! an invalid one at the other.
//!
//! ## The pointer widens with ZEROS
//!
//! `sigev_value` is `compat_sigval_t`, whose `sival_ptr` is a `compat_uptr_t`,
//! and Linux's `get_compat_sigevent` reads it as `sival_int` into a
//! `kernel_siginfo` it has just `memset` to zero. So a `sival_ptr` of
//! `0x8000_0000` means two gigabytes up and arrives here as `0x0000_0000_8000_0000`,
//! not as the kernel address a sign extension would produce. That is the same
//! form of the trap as [`crate::narrow::siginfo`]'s incoming half and as the
//! `pselect6` of #2414 --- and it is the one thing the round trip cannot see,
//! because the value only ever comes back out through `to_i386_bytes`, which
//! truncates it again. It is checked here, on the widened value.
//!
//! ## Read UNALIGNED, in both paths
//!
//! `get_compat_sigevent` is four `__get_user` calls and the native
//! `do_timer_create` is one `copy_from_user` of the whole structure, and
//! **neither asks anything of the alignment**: a program may hand over a
//! `struct sigevent *` that points into a `char` buffer at any byte. A typed
//! `UserPtr::read` stores through a `*T`, so it has to insist on
//! `align_of::<T>()` --- four bytes for these fields, eight for the native
//! `sigev_value` --- and would answer `EFAULT` for a pointer Linux serves. So
//! every field here is read with `read_unaligned`, which is the same reason
//! [`crate::narrow`]'s own helpers do: and note it applies to the **native**
//! path too, because that one is a statement about the structure and not about
//! the process. Pointed out by Copilot on the #2422.
//!
//! ## What this does NOT do
//!
//! Linux's `get_compat_sigevent` runs one `access_ok` over all 64 bytes of the
//! structure before reading any field. Neither path here does: both read the
//! four fields they need, so a caller whose mapping ends after byte 16 is
//! served where Linux answers `EFAULT`. That is the **native** path's
//! behaviour as it already stood, and the 32-bit path is deliberately no
//! stricter than its native sibling rather than quietly diverging from it in
//! the other direction. Worth closing for both at once, which is a separate
//! change.

use kernel_hal::user::UserInPtr;
use linux_object::error::LxError;

/// The fields of `struct sigevent` a timer reads.
///
/// Its home is here rather than next to `timer_create` because every one of
/// these four numbers is an ABI offset, and the offsets are what this module
/// is about.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SigEvent {
    /// `sigev_value`, handed back to the handler as `si_value`. Widened with
    /// **zeros** from a 32-bit caller: see the module note.
    pub(crate) value: usize,
    /// `sigev_signo`.
    pub(crate) signo: i32,
    /// `sigev_notify`: one of `SIGEV_SIGNAL`, `SIGEV_NONE`, `SIGEV_THREAD`,
    /// `SIGEV_THREAD_ID`.
    pub(crate) notify: i32,
    /// `sigev_notify_thread_id`, read whatever the notify is --- the four
    /// reads are inside the structure either way, since it is 64 bytes to
    /// both ABIs.
    pub(crate) thread_id: i32,
}

/// Read the `struct sigevent` at `sevp` at the **calling process's** width.
///
/// `sevp` is never null here: a null `sevp` means "SIGALRM to the process"
/// and never reaches this function.
pub(crate) fn sigevent_get(abi32: bool, sevp: usize) -> Result<SigEvent, LxError> {
    if abi32 {
        let value_p: UserInPtr<u32> = sevp.into();
        let signo_p: UserInPtr<i32> = (sevp + 4).into();
        let notify_p: UserInPtr<i32> = (sevp + 8).into();
        let tid_p: UserInPtr<i32> = (sevp + 12).into();
        return Ok(SigEvent {
            // `as usize` of a `u32` is the zero extension, and it is the whole
            // of `compat_ptr()`.
            value: value_p.read_unaligned()? as usize,
            signo: signo_p.read_unaligned()?,
            notify: notify_p.read_unaligned()?,
            thread_id: tid_p.read_unaligned()?,
        });
    }
    let value_p: UserInPtr<usize> = sevp.into();
    let signo_p: UserInPtr<i32> = (sevp + 8).into();
    let notify_p: UserInPtr<i32> = (sevp + 12).into();
    let tid_p: UserInPtr<i32> = (sevp + 16).into();
    Ok(SigEvent {
        value: value_p.read_unaligned()?,
        signo: signo_p.read_unaligned()?,
        notify: notify_p.read_unaligned()?,
        thread_id: tid_p.read_unaligned()?,
    })
}
