//! `ipc(2)`, i386 117: the eleven System V calls behind one number.
//!
//! i386 never got the separate `semget`/`msgsnd`/`shmat` syscalls x86_64
//! has. It has one, whose first argument names which of them is meant.
//!
//! ## Why it is not a row in either allowlist
//!
//! Two reasons, and either alone would be enough.
//!
//! The first is the obvious one: there is no native `ipc`. 117 would have to
//! translate to eleven different numbers depending on a value this layer
//! would have to look at anyway.
//!
//! The second is the one with teeth: the multiplexer is not a thin shuffle
//! of registers. Linux's `ksys_ipc` itself **reads and writes the caller's
//! memory** in three places, each a `long` or a pointer --- four bytes to a
//! 32-bit program and eight here:
//!
//! | sub-call | what the multiplexer touches |
//! |---|---|
//! | `SEMCTL` | reads the `union semun` through `ptr`: one word |
//! | `MSGRCV` (version 0) | reads a `struct ipc_kludge`: two words |
//! | `SHMAT` (version 0) | **writes** the attach address to `third`: one word |
//!
//! None of those is in any handler. They are in the multiplexer, so they are
//! here.
//!
//! ## The version in the high half
//!
//! `call >> 16` is a version, and `call & 0xffff` the sub-call. It is not
//! decoration: two sub-calls answer differently for it.
//!
//! - `MSGRCV` version 0 is the OLD form, where `ptr` points at a
//!   `struct ipc_kludge { void *msgp; long msgtyp; }` instead of being the
//!   buffer. That is the form glibc's i386 `msgrcv` has always used, so
//!   ignoring the version would send the kernel the ADDRESS OF THE KLUDGE as
//!   the message buffer and the first four bytes of a pointer as the type.
//! - `SHMAT` version 1 is `EINVAL` in Linux (`ipc/syscall.c`): it was an
//!   interface that never shipped, and the number is refused rather than
//!   guessed at.
//!
//! ## `struct msgbuf`, which is not the multiplexer's at all
//!
//! `msgsnd` and `msgrcv` take `{ long mtype; char mtext[]; }`, so on i386
//! the type is four bytes and **the payload starts at `msgp + 4`**. Read
//! eight and two things go wrong at once: `mtype` is the real type with the
//! first four payload bytes on top of it --- which for any ordinary small
//! type is a large positive number and not an error --- and then every byte
//! of the message is taken from four bytes too far along. See
//! [`crate::Syscall::msgsnd_at`].
//!
//! ## What needs nothing
//!
//! `struct sembuf` is `{ unsigned short, short, short }`: six bytes with no
//! `long` and no pointer, so a `semop` array is the same at both widths and
//! the native handler reads it as it stands.
//!
//! ## What is still missing
//!
//! The `*ctl` structures. `struct ipc64_perm` is 36 bytes on i386 and 48
//! here, and the three `*id64_ds` that embed it are 64/104, 88/120 and
//! 84/112 --- and on a 32-bit architecture Linux splits each of their
//! `time_t` members into a `_low`/`_high` PAIR of `unsigned long`, so the
//! native `__pad` slots are not padding there at all. Until that is written,
//! `IPC_STAT` and `IPC_SET` through 117 hand the native layout to a 32-bit
//! caller, so they answer `ENOSYS` here rather than writing it.
//!
//! `SEMTIMEDOP` answers `ENOSYS` for a different reason: this kernel has no
//! `semtimedop` at either width yet.

use super::*;

/// i386's number for `ipc`.
pub(super) const I386_IPC: u32 = 117;

/// The sub-call numbers, from `include/uapi/linux/ipc.h`. Not contiguous:
/// the three families were numbered 1, 11 and 21 so that a new one could be
/// added to any of them.
const SEMOP: u32 = 1;
const SEMGET: u32 = 2;
const SEMCTL: u32 = 3;
const SEMTIMEDOP: u32 = 4;
const MSGSND: u32 = 11;
const MSGRCV: u32 = 12;
const MSGGET: u32 = 13;
const MSGCTL: u32 = 14;
const SHMAT: u32 = 21;
const SHMDT: u32 = 22;
const SHMGET: u32 = 23;
const SHMCTL: u32 = 24;

/// The `IPC_64` bit, which a libc ORs into every `*ctl` command to ask for
/// the modern structure. Kept out of the command before it is judged.
const IPC_64: usize = 0x100;

/// The `*ctl` commands whose structure is not the same at both widths **and
/// is not converted yet**, per family. Everything else goes through, because
/// its structure is the same bytes either way, because there is no structure
/// at all, or because [`crate::narrow::ipcbuf`] converts it.
///
/// Worked out from the structures, not guessed at a family level:
///
/// - `IPC_SET` and `IPC_STAT` carry a `*id64_ds` everywhere, which embeds
///   `struct ipc64_perm` --- 36 bytes to a 32-bit caller, 48 here --- and
///   whose `time_t` members Linux splits into a `_low`/`_high` PAIR of
///   `unsigned long` on a 32-bit architecture. So do the per-family
///   `*_STAT`s, which are the same structure reached by index. **These are
///   converted now**, which is why two of the three lists below are empty:
///   the conversion is in the handlers, not here, because `semctl`, `msgctl`
///   and `shmctl` have no separate i386 number and are reached directly as
///   well as through this multiplexer.
/// - The `seminfo` and `msginfo` of `IPC_INFO`/`SEM_INFO`/`MSG_INFO` are runs
///   of `int` (and one `unsigned short`), the same at both widths, so those
///   are **not** held back.
/// - Shared memory's are the exception, and the only thing left: `struct
///   shminfo64` (`IPC_INFO`) and `struct shm_info` (`SHM_INFO`) are runs of
///   `__kernel_ulong_t`, so both of those differ and both are still held
///   back. `shm_info` leads with an `int used_ids`, so it is not a pure run
///   and the word-halving of [`crate::narrow`] does not serve it either.
/// - `GETALL`/`SETALL` carry `unsigned short[]`, and `GETVAL`/`SETVAL` and
///   friends are scalars: nothing to convert.
///
/// A list stays here, empty, rather than being deleted with its call site:
/// the next structure of its family that turns out to differ is a number in
/// it, and the check is one `contains` either way.
const SEM_NEEDS_LAYOUT: &[usize] = &[];
/// See [`SEM_NEEDS_LAYOUT`]: nothing left in this family either.
const MSG_NEEDS_LAYOUT: &[usize] = &[];
/// See [`SEM_NEEDS_LAYOUT`]: `IPC_INFO` (`shminfo64`) and `SHM_INFO`
/// (`shm_info`), the two that are still not converted.
const SHM_NEEDS_LAYOUT: &[usize] = &[3, 14];

/// `struct ipc_kludge`, the old `msgrcv` argument block: a pointer and a
/// `long`, both four bytes to a 32-bit program.
struct IpcKludge {
    msgp: usize,
    msgtyp: isize,
}

impl IpcKludge {
    /// Read it from `ptr`, as Linux's `ksys_ipc` does for `MSGRCV` version 0.
    fn read(ptr: usize) -> LxResult<Self> {
        let words: UserInPtr<u32> = ptr.into();
        let raw = words.read_array(2)?;
        Ok(Self {
            msgp: raw[0] as usize,
            // SIGNED: `msgtyp` of a `msgrcv` is a `long`, and a negative one
            // means "the lowest type not above |msgtyp|". Read unsigned, that
            // whole mode of `msgrcv` becomes a type of four thousand million
            // and nothing ever matches.
            msgtyp: raw[1] as i32 as isize,
        })
    }
}

impl Syscall<'_> {
    /// `ipc` (117).
    #[allow(clippy::too_many_arguments)]
    pub async fn sys_ipc32(
        &mut self,
        call: u32,
        first: usize,
        second: usize,
        third: usize,
        ptr: usize,
        fifth: usize,
    ) -> SysResult {
        let version = call >> 16;
        let sub = call & 0xffff;
        trace!(
            "ipc: call={}, version={}, first={}, second={}, third={:#x}, ptr={:#x}, fifth={:#x}",
            sub,
            version,
            first,
            second,
            third,
            ptr,
            fifth
        );
        match sub {
            // `semop` is `semtimedop` with no timeout. The array of
            // `struct sembuf` is six-byte entries at both widths, so the
            // native handler reads the caller's own memory as it stands.
            SEMOP => self.sys_semop(first, ptr.into(), second).await,
            // Not implemented at EITHER width yet, so there is nothing to
            // multiplex into. Said by name rather than silently becoming an
            // untimed `semop`, which would park for ever where the caller
            // asked for a deadline.
            SEMTIMEDOP => {
                warn!("ipc: semtimedop is not implemented at any width");
                Err(LxError::ENOSYS)
            }
            SEMGET => self.sys_semget(first, second, third),
            // The multiplexer's own read. `ptr` does not point at the
            // `union semun`: it points at a word HOLDING it, which is what
            // makes this one different from every other sub-call.
            SEMCTL => {
                if ptr == 0 {
                    return Err(LxError::EINVAL);
                }
                let arg = ctl_arg(ptr)?;
                let cmd = ctl_cmd("semctl", third, SEM_NEEDS_LAYOUT)?;
                self.sys_semctl(first, second, cmd, arg)
            }
            MSGSND => self.msgsnd_at(first, ptr, second, third, true).await,
            MSGRCV => {
                let (msgp, msgtyp) = if version == 0 {
                    if ptr == 0 {
                        return Err(LxError::EINVAL);
                    }
                    let k = IpcKludge::read(ptr)?;
                    (k.msgp, k.msgtyp)
                } else {
                    (ptr, fifth as i32 as isize)
                };
                self.msgrcv_at(first, msgp, second, msgtyp, third, true)
                    .await
            }
            MSGGET => self.sys_msgget(first, second),
            MSGCTL => {
                let cmd = ctl_cmd("msgctl", second, MSG_NEEDS_LAYOUT)?;
                self.sys_msgctl(first, cmd, ptr)
            }
            SHMAT => {
                // Version 1 was an interface that never shipped. Linux
                // refuses the number rather than guessing (`ipc/syscall.c`).
                if version == 1 {
                    return Err(LxError::EINVAL);
                }
                let raddr = self.sys_shmat(first, ptr, second)?;
                // And the multiplexer's own WRITE. `shmat` through 117 does
                // not return the address: it returns 0 and stores the
                // address at `third`, in FOUR bytes. Eight would put a zero
                // high half into the caller's next variable along.
                let mut out: UserOutPtr<u32> = third.into();
                out.write(raddr as u32)?;
                Ok(0)
            }
            SHMDT => self.sys_shmdt(ptr),
            SHMGET => self.sys_shmget(first, second, third),
            SHMCTL => {
                let cmd = ctl_cmd("shmctl", second, SHM_NEEDS_LAYOUT)?;
                self.sys_shmctl(first, cmd, ptr)
            }
            _ => {
                warn!("ipc: no such sub-call: {}", sub);
                Err(LxError::ENOSYS)
            }
        }
    }
}

/// Take the `IPC_64` bit off a `*ctl` command and refuse the ones whose
/// structure is not written yet.
///
/// **The bit has to come off.** An i386 glibc ORs `__IPC_64` into the
/// command of EVERY `*ctl` it sends through 117 --- it is how the caller
/// asks for the modern structure, and on i386 there is an older one it could
/// have meant. The handlers here know only the modern layout and match the
/// command by its bare number, so left on, the bit makes every single
/// `*ctl` through 117 `EINVAL`: not just `IPC_STAT` but `IPC_RMID`,
/// `SETVAL`, `GETVAL`, the lot.
///
/// Found by the test of `IPC_RMID`, which is the one command in this whole
/// family that carries nothing at all and so had no other reason to fail.
fn ctl_cmd(which: &str, raw: usize, needs_layout: &[usize]) -> LxResult<usize> {
    let cmd = raw & !IPC_64;
    if needs_layout.contains(&cmd) {
        warn!(
            "ipc: {} cmd={} needs the i386 layout of its structure, which is not written yet",
            which, cmd
        );
        return Err(LxError::ENOSYS);
    }
    Ok(cmd)
}

/// Read the one word `SEMCTL`'s `ptr` holds, at the caller's width.
fn ctl_arg(ptr: usize) -> LxResult<usize> {
    let word: UserInPtr<u32> = ptr.into();
    Ok(word.read()? as usize)
}

#[cfg(test)]
mod tests;
