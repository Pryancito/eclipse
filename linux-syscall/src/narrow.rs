//! Structures whose 32-bit image is the native one with every word halved.
//!
//! This is Linux's `put_timespec64`/`get_timespec64` and the handful of
//! `compat_*` conversions that sit next to them, and it lives here rather
//! than under [`compat32`](crate::compat32) for the reason Linux puts them in
//! `kernel/time/time.c`: **every** call site uses them, unconditionally, and
//! the one that decides which width to write is the helper, not the caller.
//! That is `in_compat_syscall()`: the question is asked of the *process*.
//!
//! ## The rule, measured rather than assumed
//!
//! Six structures, and one rule covers all six: a field that is eight bytes
//! natively is four bytes to a 32-bit process, in the same order, with no
//! padding anywhere -- because on i386 every one of these fields is a `long`
//! and a `long` is aligned to 4.
//!
//! | | native | i386 | words |
//! |---|---|---|---|
//! | `struct timespec` | 16 | **8** | 2 |
//! | `struct timeval` | 16 | **8** | 2 |
//! | `struct itimerval` | 32 | **16** | 4 |
//! | `struct itimerspec` | 32 | **16** | 4 |
//! | `struct tms` | 32 | **16** | 4 |
//! | `struct rusage` | 144 | **72** | 18 |
//!
//! Those are not read off a header: they were measured on this container's
//! Linux with an i386 program that poisons a buffer with `0xaa`, makes the
//! call and looks at where the poison starts again. `getrusage` is the one
//! worth having measured --- `ru_maxrss` lands at offset 16 (two 8-byte
//! `timeval`s), `ru_minflt` at 32, `ru_nvcsw` at 64, and byte 72 is still
//! `0xaa`. A `#[repr(C)]` with `u64` fields would have put them at 32, 64 and
//! 128, writing twice as far into the caller's buffer as it owns.
//!
//! ## Signed on the way in
//!
//! Narrowing is a truncation, which is what Linux's `put_old_timespec32`
//! does too. Widening is a **sign extension**: `old_time32_t` is
//! `__kernel_long_t`, a signed 32-bit integer, and `get_old_timespec32` is an
//! assignment from it. Zero-extending instead would turn a time before 1970,
//! or a `-1` meaning "no timeout", into a number near 4 billion that passes
//! every validity check. These fields are carried as `usize` here, so a
//! sign-extended negative arrives as a huge `usize` --- which is exactly what
//! the native path already sees when a 64-bit caller passes a negative, so
//! the two paths agree.
//!
//! ## Not through `UserOutPtr<T>::write`
//!
//! The narrow image is assembled as bytes and written through a
//! `UserOutPtr<u8>`, because `UserPtr::user_bytes` requires the destination
//! be aligned to `align_of::<T>()` and `copy_to_user` requires nothing of the
//! sort: a 32-bit program may pass a `struct timespec *` at any address, and
//! an aligned-only write would answer `EINVAL` to a program Linux serves.
//! Copilot found that on the `stat64` of #2380; it is the same trap here.

use crate::Syscall;
use kernel_hal::user::{Policy, Read, UserOutPtr, UserPtr, Write};
use linux_object::error::{LxError, LxResult};
use linux_object::time::{ITimerSpec, ITimerVal, RUsage, TimeSpec, TimeVal, Tms};

/// The most words any of these structures has: `struct rusage`, which is two
/// `timeval`s and fourteen `long`s.
const MAX_WORDS: usize = 18;

/// A structure laid out as a run of machine words: eight bytes each to a
/// 64-bit process, four bytes each to a 32-bit one.
///
/// Implementing it is a statement that the structure has **no padding and no
/// field that is not a word** in either ABI, which is what lets one pair of
/// helpers serve all six. The `WORDS` count is asserted against
/// `size_of::<Self>() / 8` in the tests, so a field added to one of these
/// structures without updating the count fails a named test rather than
/// writing the caller's buffer short.
pub(crate) trait Narrow: Copy {
    /// How many words the structure is.
    const WORDS: usize;

    /// The structure as its words, in ABI order. `out` is `WORDS` long.
    fn to_words(self, out: &mut [u64]);

    /// The structure back from its words. `words` is `WORDS` long.
    fn from_words(words: &[u64]) -> Self;
}

impl Narrow for TimeSpec {
    const WORDS: usize = 2;

    fn to_words(self, out: &mut [u64]) {
        out[0] = self.sec as u64;
        out[1] = self.nsec as u64;
    }

    fn from_words(words: &[u64]) -> Self {
        Self {
            sec: words[0] as usize,
            nsec: words[1] as usize,
        }
    }
}

impl Narrow for TimeVal {
    const WORDS: usize = 2;

    fn to_words(self, out: &mut [u64]) {
        out[0] = self.sec as u64;
        out[1] = self.usec as u64;
    }

    fn from_words(words: &[u64]) -> Self {
        Self {
            sec: words[0] as usize,
            usec: words[1] as usize,
        }
    }
}

impl Narrow for ITimerVal {
    const WORDS: usize = 4;

    fn to_words(self, out: &mut [u64]) {
        self.interval.to_words(&mut out[0..2]);
        self.value.to_words(&mut out[2..4]);
    }

    fn from_words(words: &[u64]) -> Self {
        Self {
            interval: TimeVal::from_words(&words[0..2]),
            value: TimeVal::from_words(&words[2..4]),
        }
    }
}

impl Narrow for ITimerSpec {
    const WORDS: usize = 4;

    fn to_words(self, out: &mut [u64]) {
        self.interval.to_words(&mut out[0..2]);
        self.value.to_words(&mut out[2..4]);
    }

    fn from_words(words: &[u64]) -> Self {
        Self {
            interval: TimeSpec::from_words(&words[0..2]),
            value: TimeSpec::from_words(&words[2..4]),
        }
    }
}

impl Narrow for Tms {
    const WORDS: usize = 4;

    fn to_words(self, out: &mut [u64]) {
        out[0] = self.tms_utime;
        out[1] = self.tms_stime;
        out[2] = self.tms_cutime;
        out[3] = self.tms_cstime;
    }

    fn from_words(words: &[u64]) -> Self {
        Self {
            tms_utime: words[0],
            tms_stime: words[1],
            tms_cutime: words[2],
            tms_cstime: words[3],
        }
    }
}

impl Narrow for RUsage {
    const WORDS: usize = 18;

    fn to_words(self, out: &mut [u64]) {
        self.utime.to_words(&mut out[0..2]);
        self.stime.to_words(&mut out[2..4]);
        for (slot, field) in out[4..18].iter_mut().zip([
            self.maxrss,
            self.ixrss,
            self.idrss,
            self.isrss,
            self.minflt,
            self.majflt,
            self.nswap,
            self.inblock,
            self.oublock,
            self.msgsnd,
            self.msgrcv,
            self.nsignals,
            self.nvcsw,
            self.nivcsw,
        ]) {
            *slot = field as u64;
        }
    }

    fn from_words(words: &[u64]) -> Self {
        let at = |i: usize| words[i] as i64;
        Self {
            utime: TimeVal::from_words(&words[0..2]),
            stime: TimeVal::from_words(&words[2..4]),
            maxrss: at(4),
            ixrss: at(5),
            idrss: at(6),
            isrss: at(7),
            minflt: at(8),
            majflt: at(9),
            nswap: at(10),
            inblock: at(11),
            oublock: at(12),
            msgsnd: at(13),
            msgrcv: at(14),
            nsignals: at(15),
            nvcsw: at(16),
            nivcsw: at(17),
        }
    }
}

impl Narrow for linux_object::process::RLimit {
    const WORDS: usize = 2;

    fn to_words(self, out: &mut [u64]) {
        out[0] = self.cur;
        out[1] = self.max;
    }

    fn from_words(words: &[u64]) -> Self {
        Self {
            cur: words[0],
            max: words[1],
        }
    }
}

/// The narrow image of `value`: `WORDS` little-endian `u32`s.
fn to_narrow_bytes<T: Narrow>(value: T) -> ([u8; MAX_WORDS * 4], usize) {
    let mut words = [0u64; MAX_WORDS];
    value.to_words(&mut words[..T::WORDS]);
    let mut bytes = [0u8; MAX_WORDS * 4];
    let (slots, _) = bytes.as_chunks_mut::<4>();
    for (slot, word) in slots.iter_mut().zip(words[..T::WORDS].iter()) {
        // A truncation, as `put_old_timespec32` is.
        *slot = (*word as u32).to_ne_bytes();
    }
    (bytes, T::WORDS * 4)
}

/// The same structure's **native** image, as bytes.
///
/// The pair above converts between a structure and its 32-bit image; this
/// pair does the other width. Both are needed where the value is already
/// bytes on both sides and never a user pointer of its own: an option value
/// of `setsockopt`, whose `optval` is a byte buffer whose meaning --- and so
/// whose width --- depends on `optname`. See [`sockopt`].
///
/// A word is `size_of::<usize>()` bytes and not a literal eight, so this
/// holds on a host of either width rather than only on the one the kernel
/// happens to be built for.
fn to_native_bytes<T: Narrow>(value: T) -> ([u8; MAX_WORDS * 8], usize) {
    let w = size_of::<usize>();
    let mut words = [0u64; MAX_WORDS];
    value.to_words(&mut words[..T::WORDS]);
    let mut bytes = [0u8; MAX_WORDS * 8];
    for (i, word) in words[..T::WORDS].iter().enumerate() {
        bytes[i * w..(i + 1) * w].copy_from_slice(&(*word as usize).to_ne_bytes());
    }
    (bytes, T::WORDS * w)
}

/// `value` back out of its native image.
///
/// No sign extension to do: a native word is already as wide as the word
/// these structures carry, which is what makes this the dull half of the
/// pair and [`from_narrow_bytes`] the one with a rule.
fn from_native_bytes<T: Narrow>(bytes: &[u8]) -> T {
    let w = size_of::<usize>();
    let mut words = [0u64; MAX_WORDS];
    for (i, word) in words[..T::WORDS].iter_mut().enumerate() {
        let mut slot = [0u8; 8];
        slot[..w].copy_from_slice(&bytes[i * w..(i + 1) * w]);
        *word = u64::from_ne_bytes(slot);
    }
    T::from_words(&words[..T::WORDS])
}

/// `value` back out of its narrow image, each word **sign**-extended.
fn from_narrow_bytes<T: Narrow>(bytes: &[u8]) -> T {
    let mut words = [0u64; MAX_WORDS];
    let (slots, _) = bytes.as_chunks::<4>();
    for (word, slot) in words[..T::WORDS].iter_mut().zip(slots.iter()) {
        *word = i32::from_ne_bytes(*slot) as i64 as u64;
    }
    T::from_words(&words[..T::WORDS])
}

/// Write `value` to a user pointer at the width `abi32` says the calling
/// process speaks: Linux's `put_timespec64` and the `compat_*` putters.
///
/// A free function, and not only a method on [`Syscall`], because two of the
/// call sites are free helpers of their own (`sleep_or_eintr` and the
/// `commit_and_report_old` of [`crate::outparams`]) and the alternative was
/// for each of them to grow its own copy of the conversion.
pub(crate) fn narrow_put<T: Narrow, P: Policy + Write>(
    abi32: bool,
    ptr: &mut UserPtr<T, P>,
    value: T,
) -> LxResult<()> {
    if !abi32 {
        ptr.write(value)?;
        return Ok(());
    }
    if ptr.is_null() {
        return Err(LxError::EFAULT);
    }
    let (bytes, len) = to_narrow_bytes(value);
    // Deliberately not `ptr.write(..)`: see the module documentation.
    let mut out: UserOutPtr<u8> = ptr.as_addr().into();
    out.write_array(&bytes[..len])?;
    Ok(())
}

/// [`narrow_put`], but a null pointer is "not asked for" rather than
/// `EFAULT`: `UserOutPtr::write_if_not_null`.
pub(crate) fn narrow_put_if_not_null<T: Narrow, P: Policy + Write>(
    abi32: bool,
    ptr: &mut UserPtr<T, P>,
    value: T,
) -> LxResult<()> {
    if ptr.is_null() {
        return Ok(());
    }
    narrow_put(abi32, ptr, value)
}

/// Read a user pointer at the width `abi32` says the calling process speaks:
/// Linux's `get_timespec64` and the `compat_*` getters.
pub(crate) fn narrow_get<T: Narrow, P: Policy + Read>(
    abi32: bool,
    ptr: &UserPtr<T, P>,
) -> LxResult<T> {
    if !abi32 {
        return Ok(ptr.read()?);
    }
    if ptr.is_null() {
        return Err(LxError::EFAULT);
    }
    let raw: UserPtr<u8, P> = ptr.as_addr().into();
    let bytes = raw.read_array(T::WORDS * 4)?;
    Ok(from_narrow_bytes(&bytes))
}

/// [`narrow_get`], but a null pointer is `None` rather than `EFAULT`:
/// `UserInPtr::read_if_not_null`.
pub(crate) fn narrow_get_if_not_null<T: Narrow, P: Policy + Read>(
    abi32: bool,
    ptr: &UserPtr<T, P>,
) -> LxResult<Option<T>> {
    if ptr.is_null() {
        return Ok(None);
    }
    narrow_get(abi32, ptr).map(Some)
}

/// Write `value` in its **native** layout, but without requiring the
/// destination be aligned to it.
///
/// Not every structure a 32-bit process passes is halved. `struct rlimit64`
/// and `struct statx` were designed to be the same bytes in both ABIs --- and
/// they are, except for one thing: on i386 a `long long` is aligned to 4, so
/// such a structure is **4-aligned** in a 32-bit program and 8-aligned here.
/// `UserPtr::write` stores through a `*T`, so it has to insist on
/// `align_of::<T>()`; `copy_to_user` insists on nothing, and a 32-bit caller
/// that passes its own correctly-aligned buffer would be answered `EFAULT`
/// for a pointer Linux accepts.
///
/// Found by the i386 probe, in QEMU and not in a review: `prlimit64` failed
/// on a buffer at `0x0804b09c`, which is a perfectly good
/// `struct rlimit64 *` to a 32-bit program.
///
/// Unlike [`narrow_put`] this needs nothing of `T` --- there is no
/// conversion, only a store that does not demand an alignment the caller's
/// ABI never promised --- so it serves `struct statx` too, which is no run of
/// machine words.
pub(crate) fn wide_put_unaligned<T, P: Policy + Write>(
    abi32: bool,
    ptr: &mut UserPtr<T, P>,
    value: T,
) -> LxResult<()> {
    if abi32 {
        ptr.write_unaligned(value)?;
    } else {
        ptr.write(value)?;
    }
    Ok(())
}

/// [`wide_put_unaligned`], but a null pointer is "not asked for".
pub(crate) fn wide_put_unaligned_if_not_null<T, P: Policy + Write>(
    abi32: bool,
    ptr: &mut UserPtr<T, P>,
    value: T,
) -> LxResult<()> {
    if ptr.is_null() {
        return Ok(());
    }
    wide_put_unaligned(abi32, ptr, value)
}

/// [`wide_put_unaligned`] the other way round.
pub(crate) fn wide_get_unaligned<T, P: Policy + Read>(
    abi32: bool,
    ptr: &UserPtr<T, P>,
) -> LxResult<T> {
    Ok(if abi32 {
        ptr.read_unaligned()?
    } else {
        ptr.read()?
    })
}

/// [`wide_get_unaligned`], but a null pointer is `None`.
pub(crate) fn wide_get_unaligned_if_not_null<T, P: Policy + Read>(
    abi32: bool,
    ptr: &UserPtr<T, P>,
) -> LxResult<Option<T>> {
    if ptr.is_null() {
        return Ok(None);
    }
    wide_get_unaligned(abi32, ptr).map(Some)
}

impl Syscall<'_> {
    /// Whether the calling process reads and writes these structures at half
    /// the native width: Linux's `in_compat_syscall()`.
    pub(crate) fn narrow_abi(&self) -> bool {
        self.linux_process().abi().is_32bit()
    }

    /// [`narrow_put`] for the calling process.
    pub(crate) fn put_narrow<T: Narrow, P: Policy + Write>(
        &self,
        ptr: &mut UserPtr<T, P>,
        value: T,
    ) -> LxResult<()> {
        narrow_put(self.narrow_abi(), ptr, value)
    }

    /// [`narrow_get`] for the calling process.
    pub(crate) fn get_narrow<T: Narrow, P: Policy + Read>(
        &self,
        ptr: &UserPtr<T, P>,
    ) -> LxResult<T> {
        narrow_get(self.narrow_abi(), ptr)
    }

    /// [`narrow_get_if_not_null`] for the calling process.
    pub(crate) fn get_narrow_if_not_null<T: Narrow, P: Policy + Read>(
        &self,
        ptr: &UserPtr<T, P>,
    ) -> LxResult<Option<T>> {
        narrow_get_if_not_null(self.narrow_abi(), ptr)
    }

    /// [`wide_put_unaligned_if_not_null`] for the calling process.
    pub(crate) fn put_wide_if_not_null<T, P: Policy + Write>(
        &self,
        ptr: &mut UserPtr<T, P>,
        value: T,
    ) -> LxResult<()> {
        wide_put_unaligned_if_not_null(self.narrow_abi(), ptr, value)
    }

    /// [`wide_get_unaligned_if_not_null`] for the calling process.
    pub(crate) fn get_wide_if_not_null<T, P: Policy + Read>(
        &self,
        ptr: &UserPtr<T, P>,
    ) -> LxResult<Option<T>> {
        wide_get_unaligned_if_not_null(self.narrow_abi(), ptr)
    }
}

/// The option value of `setsockopt`/`getsockopt`, whose width depends not on
/// the structure but on **which option** it is.
pub(crate) mod sockopt;

/// The envelope of `sendmsg`/`recvmsg`, which belongs to this family and
/// breaks its one rule: the native `struct msghdr` has padding, so it takes a
/// table of offsets rather than a count of words.
pub(crate) mod msghdr;

#[cfg(test)]
mod tests;
