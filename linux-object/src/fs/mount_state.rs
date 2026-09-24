//! Per-mount flags shared between the mount table and mounted filesystems.

use lock::Mutex;

/// Linux mount(2) flag bits used by Eclipse.
pub const MS_RDONLY: usize = 1;
pub const MS_NOSUID: usize = 2;
pub const MS_NODEV: usize = 4;
pub const MS_NOEXEC: usize = 8;
pub const MS_REMOUNT: usize = 32;
pub const MS_BIND: usize = 4096;
pub const MS_MOVE: usize = 8192;
#[allow(dead_code)]
pub const MS_REC: usize = 16384;

/// umount2(2) flags. `MNT_EXPIRE` and `UMOUNT_NOFOLLOW` are named so that the
/// word can be *validated*: `umount2` rejects anything outside these four with
/// `EINVAL`, which is how a program learns that the kernel it is talking to
/// does not know the option it asked for.
pub const MNT_FORCE: usize = 1;
pub const MNT_DETACH: usize = 2;
pub const MNT_EXPIRE: usize = 4;
pub const UMOUNT_NOFOLLOW: usize = 8;

/// Every `umount2(2)` flag this kernel will accept.
pub const UMOUNT_KNOWN_FLAGS: usize = MNT_FORCE | MNT_DETACH | MNT_EXPIRE | UMOUNT_NOFOLLOW;

/// `umount2(2)`'s own validation, before anything is unmounted.
///
/// An unknown bit is `EINVAL` (`sys_umount`: `if (flags & ~(MNT_FORCE |
/// MNT_DETACH | MNT_EXPIRE | UMOUNT_NOFOLLOW)) return -EINVAL`), and
/// `MNT_EXPIRE` is refused alongside either of the other two because expiry
/// means "unmount it if nobody has used it since I last asked", which is not a
/// thing one can also ask to be done forcibly or lazily.
pub fn check_umount_flags(flags: usize) -> Result<(), crate::error::LxError> {
    if flags & !UMOUNT_KNOWN_FLAGS != 0 {
        return Err(crate::error::LxError::EINVAL);
    }
    if flags & MNT_EXPIRE != 0 && flags & (MNT_FORCE | MNT_DETACH) != 0 {
        return Err(crate::error::LxError::EINVAL);
    }
    Ok(())
}

/// Mutable mount options checked on write paths.
#[derive(Debug)]
pub struct MountState {
    pub read_only: Mutex<bool>,
    /// `MS_NOSUID`: the set-user-ID and set-group-ID bits of a program on this
    /// mount are not honoured by `execve`. Linux asks this as `mnt_may_suid()`
    /// at the top of `bprm_fill_uid()`, and it is the only reason it is worth
    /// recording: a flag that reaches `/proc/mounts` and nothing else tells a
    /// program it is protected when it is not.
    nosuid: Mutex<bool>,
}

impl MountState {
    /// The state that goes with the `mount(2)` arguments, read from the same
    /// `(flags, data)` pair that [`build_options_string`] turns into the line
    /// in `/proc/mounts`. One source, so the line and the behaviour cannot
    /// disagree -- which they did, every boot: every pseudo-filesystem was
    /// registered as `nosuid` and given a state that had never heard of it.
    pub fn from_options(flags: usize, data: &str) -> Self {
        Self {
            read_only: Mutex::new(flags_read_only(flags, data)),
            nosuid: Mutex::new(flags_nosuid(flags, data)),
        }
    }

    pub fn is_read_only(&self) -> bool {
        *self.read_only.lock()
    }

    pub fn set_read_only(&self, read_only: bool) {
        *self.read_only.lock() = read_only;
    }

    /// Whether `execve` must ignore the set-user-ID bits of images here.
    pub fn is_nosuid(&self) -> bool {
        *self.nosuid.lock()
    }

    pub fn set_nosuid(&self, nosuid: bool) {
        *self.nosuid.lock() = nosuid;
    }

    /// Adopt a new set of options, the way `do_remount()` does: the flags it
    /// is given REPLACE what the mount had, which is why `mount -o remount`
    /// reads the current line out of `/proc/mounts` and passes the whole set
    /// back. Every option moves together, from the same pair
    /// [`build_options_string`] writes the new line from.
    pub fn apply_options(&self, flags: usize, data: &str) {
        self.set_read_only(flags_read_only(flags, data));
        self.set_nosuid(flags_nosuid(flags, data));
    }
}

pub fn flags_read_only(flags: usize, data: &str) -> bool {
    if flags & MS_RDONLY != 0 {
        return true;
    }
    parse_option_flag(data, "ro")
}

/// `MS_NOSUID`, from the flag word or from an `-o nosuid` in the data string.
/// Written like [`flags_read_only`] because it is the same question: both are
/// asked by a program that types the option one way or the other.
pub fn flags_nosuid(flags: usize, data: &str) -> bool {
    if flags & MS_NOSUID != 0 {
        return true;
    }
    parse_option_flag(data, "nosuid")
}

pub fn parse_option_flag(data: &str, key: &str) -> bool {
    for part in data.split(',') {
        let part = part.trim();
        if part == key {
            return true;
        }
        if let Some((k, v)) = part.split_once('=') {
            if k.trim() == key && (v.trim() == "1" || v.trim().eq_ignore_ascii_case("true")) {
                return true;
            }
        }
    }
    false
}

pub fn build_options_string(flags: usize, data: &str) -> alloc::string::String {
    use alloc::string::String;
    let mut opts = if flags_read_only(flags, data) {
        String::from("ro")
    } else {
        String::from("rw")
    };
    if flags & MS_NOSUID != 0 {
        opts.push_str(",nosuid");
    }
    if flags & MS_NODEV != 0 {
        opts.push_str(",nodev");
    }
    if flags & MS_NOEXEC != 0 {
        opts.push_str(",noexec");
    }
    if flags & MS_BIND != 0 {
        opts.push_str(",bind");
    }
    // The caller's own option string last, minus anything already said. The
    // flag-derived half above and `data` are two spellings of one request --
    // `mount -o ro,nosuid` arrives with both set -- so appending `data` whole
    // printed every shared option twice: a `mount -o remount,ro` left `ro,ro`
    // in `/proc/mounts`. Linux prints the mount's own flags and then lets the
    // filesystem add what is left (`show_mnt_opts` + `show_options`); neither
    // of them repeats the other.
    for part in data.split(',') {
        let part = part.trim();
        // `ro` and `rw` are one slot, and it has been filled: a `rw` copied
        // through next to the `ro` the flags asked for would say both.
        if part.is_empty() || part == "ro" || part == "rw" {
            continue;
        }
        if opts.split(',').any(|o| o == part) {
            continue;
        }
        opts.push(',');
        opts.push_str(part);
    }
    opts
}

#[cfg(test)]
mod mount_option_tests {
    //! The mount options this kernel records and the ones it acts on, which
    //! were not the same set. `/proc/mounts` has always said `nosuid` for
    //! `/dev`, `/tmp`, `/run`, `/proc` and `/sys`, and every one of those
    //! mounts was given a state that had never heard of the word -- so a
    //! program that reads the line and believes it was reading a promise.

    use super::*;
    use crate::error::LxError;

    #[test]
    fn the_flag_numbers_are_the_ones_linux_uses() {
        // From `include/uapi/linux/mount.h`. Pinned against the literals: a
        // wrong number here is not a missing option, it is a different one.
        assert_eq!(MS_RDONLY, 1);
        assert_eq!(MS_NOSUID, 2);
        assert_eq!(MS_NODEV, 4);
        assert_eq!(MS_NOEXEC, 8);
        assert_eq!(MS_REMOUNT, 32);
        assert_eq!(MS_BIND, 4096);
        assert_eq!(MS_MOVE, 8192);
        assert_eq!(MNT_FORCE, 1);
        assert_eq!(MNT_DETACH, 2);
        assert_eq!(MNT_EXPIRE, 4);
        assert_eq!(UMOUNT_NOFOLLOW, 8);
    }

    #[test]
    fn nosuid_can_be_asked_for_either_way() {
        // `mount -o nosuid` reaches the kernel as a flag from util-linux and
        // as a word in the data string from a caller that passes its options
        // through -- both are how it is typed, so both are how it is read.
        assert!(flags_nosuid(MS_NOSUID, ""));
        assert!(flags_nosuid(0, "rw,nosuid,nodev"));
        assert!(flags_nosuid(0, "nosuid"));
        assert!(!flags_nosuid(0, "rw,nodev"));
        assert!(!flags_nosuid(0, ""));
    }

    #[test]
    fn a_word_that_merely_contains_nosuid_is_not_nosuid() {
        // The options string is a comma-separated list, not prose, and a
        // substring match would turn an unrelated option into a promise that
        // set-user-ID bits are off.
        assert!(!flags_nosuid(0, "nosuidx"));
        assert!(!flags_nosuid(0, "xnosuid"));
        assert!(!flags_nosuid(0, "errors=nosuid"));
    }

    #[test]
    fn the_state_and_the_line_are_read_from_the_same_arguments() {
        // This is the whole of the fix: the string in `/proc/mounts` and the
        // behaviour behind it both come out of one `(flags, data)` pair, so
        // no mount can say one thing and do another.
        for (flags, data) in [
            (0usize, "rw"),
            (MS_NOSUID, ""),
            (0, "nosuid"),
            (MS_RDONLY | MS_NOSUID, ""),
            (0, "ro,nosuid,nodev"),
            (MS_NOSUID, "ro"),
        ] {
            let state = MountState::from_options(flags, data);
            let line = build_options_string(flags, data);
            assert_eq!(
                state.is_nosuid(),
                line.split(',').any(|o| o == "nosuid"),
                "state and line disagree for ({:#x}, {:?}): {:?}",
                flags,
                data,
                line
            );
            assert_eq!(
                state.is_read_only(),
                line.split(',').any(|o| o == "ro"),
                "state and line disagree for ({:#x}, {:?}): {:?}",
                flags,
                data,
                line
            );
        }
    }

    #[test]
    fn the_options_line_says_each_option_once() {
        // `mount -o remount,ro` reaches the kernel with MS_RDONLY set AND an
        // `ro` in the data string, because util-linux passes both. Appending
        // the data whole left `ro,ro` in `/proc/mounts`.
        assert_eq!(build_options_string(MS_RDONLY, "ro"), "ro");
        assert_eq!(build_options_string(0, "rw"), "rw");
        assert_eq!(build_options_string(MS_NOSUID, "nosuid"), "rw,nosuid");
        assert_eq!(
            build_options_string(MS_NOSUID | MS_NODEV, "nosuid,nodev,relatime"),
            "rw,nosuid,nodev,relatime"
        );
    }

    #[test]
    fn the_options_line_keeps_what_only_the_caller_knows() {
        // The filesystem's own options are the reason the data string is
        // carried at all; dropping them would leave `/proc/mounts` unable to
        // say which subvolume or which codepage a mount was given.
        assert_eq!(
            build_options_string(0, "subvol=/@,compress=zstd"),
            "rw,subvol=/@,compress=zstd"
        );
        assert_eq!(build_options_string(MS_RDONLY, ""), "ro");
        assert_eq!(build_options_string(0, ""), "rw");
    }

    #[test]
    fn a_read_only_mount_does_not_also_claim_to_be_writable() {
        // `ro` and `rw` are one slot. A data string saying `rw` next to a
        // flag word saying MS_RDONLY is a contradiction, and the flag is the
        // one the write path is enforcing, so it is the one that gets said.
        assert_eq!(build_options_string(MS_RDONLY, "rw"), "ro");
    }

    #[test]
    fn a_remount_replaces_every_option_at_once() {
        // `do_remount()` sets the mount's attributes from the flags it was
        // given; an option not named is an option turned off. A remount that
        // moved only the read-only bit would leave `nosuid` on a mount whose
        // new line no longer says it -- the same disagreement, arriving later.
        let state = MountState::from_options(MS_NOSUID | MS_RDONLY, "");
        assert!(state.is_nosuid());
        assert!(state.is_read_only());
        state.apply_options(0, "rw");
        assert!(!state.is_nosuid(), "nosuid outlived the remount");
        assert!(!state.is_read_only(), "ro outlived the remount");
        state.apply_options(0, "ro,nosuid");
        assert!(state.is_nosuid());
        assert!(state.is_read_only());
    }

    #[test]
    fn umount2_accepts_the_four_flags_it_knows() {
        for flags in [0, MNT_FORCE, MNT_DETACH, MNT_EXPIRE, UMOUNT_NOFOLLOW] {
            assert!(
                check_umount_flags(flags).is_ok(),
                "{:#x} should be accepted",
                flags
            );
        }
        assert!(check_umount_flags(MNT_FORCE | UMOUNT_NOFOLLOW).is_ok());
    }

    #[test]
    fn umount2_refuses_a_flag_it_does_not_know() {
        // `if (flags & ~(MNT_FORCE | MNT_DETACH | MNT_EXPIRE | UMOUNT_NOFOLLOW))
        // return -EINVAL`. Accepting an unknown bit is how a program comes to
        // believe the kernel did something it has never implemented.
        assert_eq!(check_umount_flags(16), Err(LxError::EINVAL));
        assert_eq!(check_umount_flags(usize::MAX), Err(LxError::EINVAL));
        assert_eq!(check_umount_flags(MNT_FORCE | 64), Err(LxError::EINVAL));
    }

    #[test]
    fn expiry_cannot_also_be_forced_or_lazy() {
        // `MNT_EXPIRE` means "unmount it if nobody has touched it since I last
        // asked", which is not a thing that can also be done forcibly or
        // lazily; Linux spells the pair out as EINVAL.
        assert_eq!(
            check_umount_flags(MNT_EXPIRE | MNT_FORCE),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            check_umount_flags(MNT_EXPIRE | MNT_DETACH),
            Err(LxError::EINVAL)
        );
        assert!(check_umount_flags(MNT_EXPIRE | UMOUNT_NOFOLLOW).is_ok());
    }
}
