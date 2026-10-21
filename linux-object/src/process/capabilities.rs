//! The capability sets of a process, and the rules of `security/commoncap.c`
//! that move them.
//!
//! Linux keeps the sets in `struct cred`, and every privilege check asks the
//! effective one. This kernel used to keep none: a process held every
//! capability while its effective uid was 0 and none otherwise. `capset(2)`
//! checked what it was asked and threw it away, `PR_CAPBSET_DROP` answered 0
//! and dropped nothing, and `PR_SET_KEEPCAPS` was recorded with nothing to
//! keep. So a root daemon that put `CAP_DAC_OVERRIDE` down with `capset` still
//! read every user's files, and one that asked to keep its capabilities across
//! `setuid` was left with none.
//!
//! The ambient set is not here: there is no `PR_CAP_AMBIENT`, so it is always
//! empty, which is what a process that never asked for one has on Linux.

use super::{Credentials, ROOT_UID};
use crate::error::LxResult;

// The capability numbers this kernel names, from `include/uapi/linux/
// capability.h`. Only the ones a gate actually asks about are here: a number
// nobody asks about is a number nobody can get wrong, and is exactly the kind
// of write-only constant this change exists to remove.

/// `CAP_CHOWN`: give a file away, or to a group you are not in.
pub const CAP_CHOWN: u32 = 0;
/// `CAP_DAC_OVERRIDE`: read, write and search past the permission bits
/// (`generic_permission`).
pub const CAP_DAC_OVERRIDE: u32 = 1;
/// `CAP_DAC_READ_SEARCH`: read a file and read or search a directory past
/// the permission bits, but not write either.
pub const CAP_DAC_READ_SEARCH: u32 = 2;
/// `CAP_FOWNER`: what the owner of a file may do, done to anybody's file
/// (`inode_owner_or_capable`): chmod, explicit times, the sticky bit,
/// `O_NOATIME`, a hard link.
pub const CAP_FOWNER: u32 = 3;
/// `CAP_FSETID`: keep `S_ISGID` on a file of a group you are not in.
pub const CAP_FSETID: u32 = 4;
/// `CAP_KILL`: signal a process none of whose ids you hold.
pub const CAP_KILL: u32 = 5;
/// `CAP_SETGID`: change group ids, and set the supplementary group list.
pub const CAP_SETGID: u32 = 6;
/// `CAP_SETUID`: change user ids, `setfsuid(2)` included.
pub const CAP_SETUID: u32 = 7;
/// `CAP_SETPCAP`: drop capabilities from the bounding set
/// (`prctl(PR_CAPBSET_DROP)`), among other things.
pub const CAP_SETPCAP: u32 = 8;
/// `CAP_LINUX_IMMUTABLE`: set the immutable and append-only flags. Here only
/// for [`CAP_FS_SET`].
pub const CAP_LINUX_IMMUTABLE: u32 = 9;
/// `CAP_SYS_PTRACE`: trace, inspect or reach into the innards of a process
/// that is not yours -- `pidfd_getfd(2)` included.
pub const CAP_SYS_PTRACE: u32 = 19;
/// `CAP_SYS_ADMIN`: the catch-all -- mount, `sethostname`, and much else.
pub const CAP_SYS_ADMIN: u32 = 21;
/// `CAP_SYS_BOOT`: `reboot(2)` and `kexec_load(2)`.
pub const CAP_SYS_BOOT: u32 = 22;
/// `CAP_SYS_NICE`: raise a task's priority, and set the scheduling
/// parameters or CPU affinity of a task that is not yours.
pub const CAP_SYS_NICE: u32 = 23;
/// `CAP_SYS_RESOURCE`: raise a hard resource limit, and reach into another
/// process's limits.
pub const CAP_SYS_RESOURCE: u32 = 24;
/// `CAP_SYS_TIME`: set the system clock and discipline it.
pub const CAP_SYS_TIME: u32 = 25;
/// `CAP_MKNOD`: make a character or block device node with `mknod(2)`.
pub const CAP_MKNOD: u32 = 27;
/// `CAP_MAC_OVERRIDE`: there is no MAC policy to override. Here only for
/// [`CAP_FS_SET`].
pub const CAP_MAC_OVERRIDE: u32 = 32;
/// `CAP_SYSLOG`: the `syslog(2)` actions that change the kernel log or the
/// console (clear, read-and-clear, console on/off/level) -- and, under
/// `dmesg_restrict`, reading it at all.
pub const CAP_SYSLOG: u32 = 34;

/// The highest capability number this kernel reports. 40 is Linux 5.15's
/// `CAP_LAST_CAP` (`CAP_CHECKPOINT_RESTORE`); `capget`, `prctl`'s bounding
/// set and [`Capabilities::has`] all measure against this one number.
pub const CAP_LAST_CAP: u32 = 40;

/// Every capability there is, `CAP_VALID_MASK`: root's permitted set, and
/// the bounding set of a process nothing has narrowed. `capset(2)` keeps
/// only these bits of what it is handed.
pub const CAP_FULL_SET: u64 = (1u64 << (CAP_LAST_CAP + 1)) - 1;

/// `CAP_FS_SET` (`include/linux/capability.h`): the capabilities that follow
/// the filesystem uid. `setfsuid(2)` away from 0 takes them out of the
/// effective set and back to 0 puts the permitted ones back, so a root file
/// server acting for a user is checked as that user.
pub const CAP_FS_SET: u64 = (1 << CAP_CHOWN)
    | (1 << CAP_DAC_OVERRIDE)
    | (1 << CAP_DAC_READ_SEARCH)
    | (1 << CAP_FOWNER)
    | (1 << CAP_FSETID)
    | (1 << CAP_LINUX_IMMUTABLE)
    | (1 << CAP_MKNOD)
    | (1 << CAP_MAC_OVERRIDE);

/// A process's capability sets, the bitmaps `capget(2)` and the `Cap*:` lines
/// of `/proc/<pid>/status` show. They are part of the [`Credentials`], so a
/// fork copies them and every check handed the credentials can ask them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// What the checks ask: [`Capabilities::has`] is a bit of this one.
    pub effective: u64,
    /// The most `effective` may hold. `capset` may only shrink it.
    pub permitted: u64,
    /// What an exec as root adds to the bounding set to make the new
    /// permitted set.
    pub inheritable: u64,
    /// The most an exec gives a root image. `PR_CAPBSET_DROP` shrinks it,
    /// nothing grows it, and it does not touch the permitted set.
    pub bounding: u64,
}

impl Capabilities {
    /// `init_cred`: every capability in every set but the inheritable one.
    /// What the first process starts with.
    pub const ROOT: Self = Self {
        effective: CAP_FULL_SET,
        permitted: CAP_FULL_SET,
        inheritable: 0,
        bounding: CAP_FULL_SET,
    };

    /// The sets a process started as root holds once `set*uid` calls have
    /// taken it to these three uids, with `PR_SET_KEEPCAPS` off: all of them
    /// permitted while one of the three is 0, effective only while the
    /// effective one is, and none at all once none is.
    pub fn for_uids(ruid: u32, euid: u32, suid: u32) -> Self {
        let mut caps = Self::ROOT;
        caps.follow_setuid([ROOT_UID; 3], [ruid, euid, suid], false);
        caps
    }

    /// Whether the effective set holds `cap`. A number past
    /// [`CAP_LAST_CAP`] is held by nobody, root included: `capget` never told
    /// anyone they had it.
    pub fn has(&self, cap: u32) -> bool {
        cap <= CAP_LAST_CAP && self.effective & (1 << cap) != 0
    }

    /// `PR_CAPBSET_READ`: whether `cap` is still in the bounding set.
    pub fn bounds(&self, cap: u32) -> bool {
        cap <= CAP_LAST_CAP && self.bounding & (1 << cap) != 0
    }

    /// `cap_emulate_setxuid()`, which `setuid`, `setreuid` and `setresuid`
    /// run once the real, effective and saved uids have moved from `old` to
    /// `new`:
    ///
    /// ```c
    /// if ((uid_eq(old->uid, root_uid) || uid_eq(old->euid, root_uid) ||
    ///      uid_eq(old->suid, root_uid)) &&
    ///     (!uid_eq(new->uid, root_uid) && !uid_eq(new->euid, root_uid) &&
    ///      !uid_eq(new->suid, root_uid))) {
    ///         if (!issecure(SECURE_KEEP_CAPS)) {
    ///                 cap_clear(new->cap_permitted);
    ///                 cap_clear(new->cap_effective);
    ///         }
    ///         ...
    /// }
    /// if (uid_eq(old->euid, root_uid) && !uid_eq(new->euid, root_uid))
    ///         cap_clear(new->cap_effective);
    /// if (!uid_eq(old->euid, root_uid) && uid_eq(new->euid, root_uid))
    ///         new->cap_effective = new->cap_permitted;
    /// ```
    ///
    /// `keep_caps` is `SECURE_KEEP_CAPS`, `prctl(PR_SET_KEEPCAPS)`. A call
    /// that moves no user id changes nothing, so the group setters may run it
    /// too.
    pub fn follow_setuid(&mut self, old: [u32; 3], new: [u32; 3], keep_caps: bool) {
        if old.contains(&ROOT_UID) && !new.contains(&ROOT_UID) && !keep_caps {
            self.permitted = 0;
            self.effective = 0;
        }
        let (old_euid, new_euid) = (old[1], new[1]);
        if old_euid == ROOT_UID && new_euid != ROOT_UID {
            self.effective = 0;
        }
        if old_euid != ROOT_UID && new_euid == ROOT_UID {
            self.effective = self.permitted;
        }
    }

    /// `cap_task_fix_setuid(LSM_SETID_FS)`, which only `setfsuid(2)` runs:
    /// moving the filesystem uid away from 0 drops [`CAP_FS_SET`] from the
    /// effective set, and moving it back raises the part of it that is
    /// permitted. Every other capability stays where it was.
    pub fn follow_setfsuid(&mut self, old_fsuid: u32, new_fsuid: u32) {
        if old_fsuid == ROOT_UID && new_fsuid != ROOT_UID {
            self.effective &= !CAP_FS_SET;
        }
        if old_fsuid != ROOT_UID && new_fsuid == ROOT_UID {
            self.effective |= self.permitted & CAP_FS_SET;
        }
    }

    /// `cap_bprm_creds_from_file()` for an image without file capabilities,
    /// which is every image here. `ruid` and `euid` are the ids the image
    /// runs with, a set-user-ID bit already honoured:
    ///
    /// - `get_file_caps()` starts the new permitted set empty;
    /// - `handle_privileged_root()`: when either id is 0 it is the bounding
    ///   set together with the inheritable one, and only an effective 0 makes
    ///   it effective;
    /// - under `no_new_privs` the image gets "no more than they had, and
    ///   maybe less": the new permitted set is cut to the old one.
    ///
    /// The inheritable and bounding sets cross the exec as they are.
    pub fn follow_exec(&mut self, ruid: u32, euid: u32, no_new_privs: bool) {
        let old_permitted = self.permitted;
        self.permitted = 0;
        if ruid == ROOT_UID || euid == ROOT_UID {
            self.permitted = self.bounding | self.inheritable;
        }
        if no_new_privs {
            self.permitted &= old_permitted;
        }
        self.effective = if euid == ROOT_UID { self.permitted } else { 0 };
    }

    /// `access_override_creds()`: `access(2)` checks as the real uid, and so
    /// with the capabilities the real uid would have -- the permitted set
    /// when it is 0, none otherwise -- not with the effective set.
    pub fn for_access(&self, ruid: u32) -> Self {
        let effective = if ruid == ROOT_UID { self.permitted } else { 0 };
        Self { effective, ..*self }
    }
}

impl Credentials {
    /// `capable(cap)` for whoever holds these credentials: whether `cap` is in
    /// their effective set.
    pub fn capable(&self, cap: u32) -> bool {
        self.caps.has(cap)
    }
}

/// Keep what `change` leaves in `caps` only when it answers `Ok`, so a call
/// that decides against the sets and then changes them (`capset(2)`,
/// `PR_CAPBSET_DROP`) leaves them as they were when it refuses.
pub(super) fn change(
    caps: &mut Capabilities,
    change: impl FnOnce(&mut Capabilities) -> LxResult,
) -> LxResult {
    let mut new = *caps;
    change(&mut new)?;
    *caps = new;
    Ok(())
}

#[cfg(test)]
mod tests;
