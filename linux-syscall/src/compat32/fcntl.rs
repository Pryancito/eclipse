//! `fcntl64`, and the two `struct flock` that i386 has.
//!
//! Most of `fcntl` is a command number and a scalar: `F_SETFD`, `F_GETFL`,
//! `F_DUPFD`. Those need no conversion, and they go straight to the native
//! handler. Six commands are different, because their argument is not a
//! scalar but a pointer to a structure of file offsets --- and an offset is
//! the thing that is not the same width in the two ABIs.
//!
//! ## Two structures, not one
//!
//! i386 has both:
//!
//! ```text
//! struct flock {                   struct flock64 {
//!     short l_type;    //  0           short  l_type;    //  0
//!     short l_whence;  //  2           short  l_whence;  //  2
//!     long  l_start;   //  4           loff_t l_start;   //  4  (8 bytes)
//!     long  l_len;     //  8           loff_t l_len;     // 12  (8 bytes)
//!     pid_t l_pid;     // 12           pid_t  l_pid;     // 20
//! };  // 16 bytes                  };  // 24 bytes
//! ```
//!
//! Which one a call passes is decided **by the command number**, not by the
//! syscall number, and this is the part that bites:
//!
//! | command | number | structure |
//! |---|---|---|
//! | `F_GETLK` / `F_SETLK` / `F_SETLKW` | 5, 6, 7 | `struct flock`, 16 bytes |
//! | `F_GETLK64` / `F_SETLK64` / `F_SETLKW64` | 12, 13, 14 | `struct flock64`, 24 bytes |
//! | `F_OFD_GETLK` / `F_OFD_SETLK` / `F_OFD_SETLKW` | 36, 37, 38 | `struct flock64`, always |
//!
//! The `*64` numbers exist only on 32-bit: on x86_64 there is one `struct
//! flock` and it already holds 64-bit offsets, so 12, 13 and 14 mean nothing
//! there. The open-file-description commands were added after `flock64`
//! existed and never got a 16-byte form, so a 32-bit program must pass the
//! wide one --- which is what `_FILE_OFFSET_BITS=64` arranges, and why
//! `fcntl(2)` says those three are unusable without it.
//!
//! ## `fcntl`(55) and `fcntl64`(221) are the same call
//!
//! This one was written the other way round first, on the belief that the
//! older number refuses the wide commands --- `compat_sys_fcntl` answering
//! `-EINVAL` for `F_SETLK64` before handing the rest to
//! `compat_sys_fcntl64`. **Measured against the Linux of the build container,
//! that is not what happens**: `fcntl(fd, F_SETLK64, &flock64)` takes the
//! lock and returns 0, and `fcntl(fd, F_GETLK64, ...)` writes the answer back
//! into the 24-byte structure. The reverse holds too: `fcntl64` with the
//! narrow `F_GETLK` reads and writes 16 bytes.
//!
//! So the two numbers are interchangeable and the width is decided by the
//! command alone, which is also the simpler rule. Both are wired to the same
//! handler, and neither refuses anything the other answers. (There is no risk
//! either way: a caller that names a wide command passes 24 bytes and gets 24
//! bytes read, whichever number it came in on.)
//!
//! ## Why the 16-byte form is not simply "the same with smaller fields"
//!
//! `l_start` and `l_len` are a **signed** `long` there. `l_len` is negative to
//! name the range *before* `l_start` --- that is in `fcntl(2)`, not an
//! accident --- so the halves have to be sign-extended, where the split
//! offsets in [`super::wide`] are zero-extended. The same four bytes read the
//! two ways differ by four gigabytes, and the wrong reading does not fail: it
//! locks a range nobody asked about.

use super::*;
use crate::file::{Flock, F_GETLK, F_OFD_GETLK, F_OFD_SETLK, F_OFD_SETLKW, F_SETLK, F_SETLKW};
use core::convert::TryInto;
use linux_object::error::LxError;

/// `F_GETLK64`, `F_SETLK64`, `F_SETLKW64`: the three commands that exist only
/// in a 32-bit ABI, because only there is `struct flock` too narrow.
const F_GETLK64: usize = 12;
const F_SETLK64: usize = 13;
const F_SETLKW64: usize = 14;

/// `sizeof(struct flock)` on i386.
const FLOCK32_SIZE: usize = 16;
/// `sizeof(struct flock64)` on i386.
const FLOCK64_SIZE: usize = 24;

/// Which of the two structures a command's argument points at, or `None` for
/// a command whose argument is a scalar and needs no conversion at all.
fn flock_width(cmd: usize) -> Option<usize> {
    match cmd {
        F_GETLK | F_SETLK | F_SETLKW => Some(FLOCK32_SIZE),
        // The OFD commands never had a narrow form.
        F_GETLK64 | F_SETLK64 | F_SETLKW64 | F_OFD_GETLK | F_OFD_SETLK | F_OFD_SETLKW => {
            Some(FLOCK64_SIZE)
        }
        _ => None,
    }
}

/// Read a signed 64-bit offset out of two 32-bit words, **sign-extended**: a
/// negative `l_len` is how `fcntl(2)` names the range before `l_start`.
fn i64_at(bytes: &[u8], at: usize) -> i64 {
    i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// Read a signed 32-bit `long`, sign-extended to the native width for the
/// same reason.
fn i32_at(bytes: &[u8], at: usize) -> i64 {
    i64::from(i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()))
}

fn i16_at(bytes: &[u8], at: usize) -> i16 {
    i16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
}

/// Decode either of i386's two `struct flock` into the native one.
///
/// `width` says which, and it comes from the command number --- never from
/// the contents, which carry no clue.
fn flock_from_bytes(bytes: &[u8], width: usize) -> Result<Flock, LxError> {
    if bytes.len() < width {
        return Err(LxError::EFAULT);
    }
    let (l_start, l_len, l_pid) = if width == FLOCK64_SIZE {
        (i64_at(bytes, 4), i64_at(bytes, 12), 20)
    } else {
        (i32_at(bytes, 4), i32_at(bytes, 8), 12)
    };
    Ok(Flock {
        l_type: i16_at(bytes, 0),
        l_whence: i16_at(bytes, 2),
        l_start,
        l_len,
        l_pid: i32::from_le_bytes(bytes[l_pid..l_pid + 4].try_into().unwrap()),
    })
}

/// Lay a native `Flock` back out in whichever of the two shapes the caller
/// passed, for `F_GETLK`.
///
/// The narrow form can fail here, and that is not pedantry: `F_GETLK` reports
/// a *conflicting* lock, which some other process set and which may well sit
/// past two gigabytes. Linux answers `EOVERFLOW` for exactly this, and a
/// truncated offset would instead report a conflict at the wrong place.
fn flock_to_bytes(fl: &Flock, width: usize, out: &mut [u8]) -> Result<(), LxError> {
    out[..2].copy_from_slice(&fl.l_type.to_le_bytes());
    out[2..4].copy_from_slice(&fl.l_whence.to_le_bytes());
    if width == FLOCK64_SIZE {
        out[4..12].copy_from_slice(&fl.l_start.to_le_bytes());
        out[12..20].copy_from_slice(&fl.l_len.to_le_bytes());
        out[20..24].copy_from_slice(&fl.l_pid.to_le_bytes());
    } else {
        let start = i32::try_from(fl.l_start).map_err(|_| LxError::EOVERFLOW)?;
        let len = i32::try_from(fl.l_len).map_err(|_| LxError::EOVERFLOW)?;
        out[4..8].copy_from_slice(&start.to_le_bytes());
        out[8..12].copy_from_slice(&len.to_le_bytes());
        out[12..16].copy_from_slice(&fl.l_pid.to_le_bytes());
    }
    Ok(())
}

impl Syscall<'_> {
    /// `fcntl64(2)`, i386 221.
    pub async fn sys_fcntl64(&self, fd: usize, cmd: usize, arg: usize) -> SysResult {
        let fd = FileDesc::from(fd as i32);
        let Some(width) = flock_width(cmd) else {
            // Not a lock command: the argument is a scalar and means the same
            // on both sides. `F_SETFL`'s flag bits, `F_DUPFD`'s minimum
            // descriptor and `F_ADD_SEALS`' seal mask are all plain words.
            return self.sys_fcntl(fd, cmd, arg).await;
        };
        self.compat_record_lock(fd, cmd, arg, width).await
    }

    /// The six lock commands: convert in, do the same work the native path
    /// does, convert back.
    async fn compat_record_lock(
        &self,
        fd: FileDesc,
        cmd: usize,
        arg: usize,
        width: usize,
    ) -> SysResult {
        if arg == 0 {
            return Err(LxError::EFAULT);
        }
        let get = cmd == F_GETLK || cmd == F_GETLK64 || cmd == F_OFD_GETLK;
        let wait = cmd == F_SETLKW || cmd == F_SETLKW64 || cmd == F_OFD_SETLKW;

        let mut raw = [0u8; FLOCK64_SIZE];
        let input: UserInPtr<u8> = arg.into();
        let read = input.read_array(width)?;
        raw[..width].copy_from_slice(&read);
        let fl = flock_from_bytes(&raw, width)?;

        let file_like = self.linux_process().get_file_like(fd)?;
        let (ret, back) = self.do_lock_file_wait(&file_like, get, wait, fl).await?;
        if let Some(fl) = back {
            let mut raw = [0u8; FLOCK64_SIZE];
            flock_to_bytes(&fl, width, &mut raw)?;
            let mut out: UserOutPtr<u8> = arg.into();
            out.write_array(&raw[..width])?;
        }
        Ok(ret)
    }
}

#[cfg(test)]
mod tests;
