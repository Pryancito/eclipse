//! The `siginfo_t` that crosses in either direction, and the one structure of
//! this family whose **total size does not change at all**.
//!
//! It is 128 bytes to everybody (`SI_MAX_SIZE`). What moves is where its
//! union starts --- byte 12 to a 32-bit process, byte 16 natively, because
//! the union is 8-aligned here and 4-aligned there --- and how wide the live
//! member of that union is. The layout table that answers both questions is
//! [`linux_object::signal::siginfo32`], and it is in `linux-object` because
//! the 32-bit signal frame pushes the same bytes.
//!
//! So this module is only the **user-memory** half: the raw access that the
//! typed `UserPtr::read`/`write` cannot do here, for two separate reasons.
//!
//! 1. The bytes have to go through the conversion, which works on
//!    `[u8; SIGINFO_LEN]` and not on a `SigInfo`.
//! 2. On i386 a `siginfo_t` aligns to **4**, because its union does; the
//!    typed access demands the 8 that `SigInfo`'s own alignment asks for. The
//!    same alignment form as the `sigset_t` of `rt_sigprocmask`, and the
//!    reason both of these take the unaligned path even though a native
//!    caller's buffer would satisfy the typed one.
//!
//! The four callers are `rt_sigqueueinfo` (178) and `rt_tgsigqueueinfo`
//! (335), which pass one **inwards**, and `rt_sigtimedwait` (177) and
//! `waitid` (284), which pass one **outwards**.

use kernel_hal::user::{Policy, Read, UserInPtr, UserOutPtr, UserPtr, Write};
use linux_object::error::LxError;
use linux_object::signal::siginfo32::SIGINFO_LEN;
use linux_object::signal::SigInfo;

/// Read the `siginfo_t` a caller passed, at the caller's own width.
///
/// `abi32` is the **calling process's** ABI and not a property of the call:
/// both numbers that come through here exist at both widths and mean the same
/// thing, so unlike the `*_time64` family there is nothing in the number to
/// ask. (The fifth form of the trap is exactly the case where those two come
/// apart; see [`crate::narrow`].)
pub(crate) fn siginfo_get<P: Policy + Read>(
    abi32: bool,
    at: &UserPtr<SigInfo, P>,
) -> Result<SigInfo, LxError> {
    if !abi32 {
        return Ok(at.read()?);
    }
    let bytes: UserInPtr<u8> = at.as_addr().into();
    // `as_slice` and not `read_array`: both run the same `check_len_for`, but
    // the second allocates a `Vec` of 128 bytes that is then copied into the
    // array the conversion takes --- an allocation and a second copy on every
    // one of these calls, for a length that is fixed and known. Pointed out by
    // Copilot on the #2420.
    let mut narrow = [0u8; SIGINFO_LEN];
    narrow.copy_from_slice(bytes.as_slice(SIGINFO_LEN)?);
    Ok(SigInfo::from_i386_bytes(&narrow))
}

/// Write a `siginfo_t` out to a caller, at the caller's own width, when the
/// pointer is not null.
///
/// The null check is here because every caller has one: Linux's
/// `rt_sigtimedwait` copies out `if (info)`, and `sys_waitid` `if (infop)`.
pub(crate) fn siginfo_put_if_not_null<P: Policy + Write>(
    abi32: bool,
    at: &mut UserPtr<SigInfo, P>,
    info: SigInfo,
) -> Result<(), LxError> {
    if at.is_null() {
        return Ok(());
    }
    if !abi32 {
        at.write(info)?;
        return Ok(());
    }
    let mut bytes: UserOutPtr<u8> = at.as_addr().into();
    bytes.write_array(&info.to_i386_bytes())?;
    Ok(())
}
