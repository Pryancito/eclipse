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
    /// `MS_NOEXEC`: nothing on this mount may be executed. Linux asks it as
    /// `path_noexec()` in `do_open_execat()` and answers `EACCES`.
    ///
    /// Recorded for the reason the line above gives. `/proc` and `/sys` have
    /// said `noexec` in `/proc/mounts` since this kernel first had a
    /// `/proc/mounts`, and the word did not reach anything: a hardening script
    /// that remounts a directory `noexec` and checks the line was reading a
    /// promise nobody had made.
    noexec: Mutex<bool>,
    /// `MS_NODEV`: a character or block device node on this mount cannot be
    /// opened. Linux asks it as `may_open_dev()` from `may_open()` and answers
    /// `EACCES`.
    ///
    /// The same story: `/tmp`, `/run` and `/dev/shm` are all `nodev` in
    /// `/proc/mounts`, so a `mknod` there followed by an open was refused on
    /// every other Unix and allowed here. `/dev` itself is deliberately NOT
    /// `nodev` -- it is where the nodes live -- and `/dev/pts` is a plain
    /// directory rather than a mount, so its slaves answer to `/dev`.
    nodev: Mutex<bool>,
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
            noexec: Mutex::new(flags_noexec(flags, data)),
            nodev: Mutex::new(flags_nodev(flags, data)),
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

    /// Whether `execve` must refuse an image here.
    pub fn is_noexec(&self) -> bool {
        *self.noexec.lock()
    }

    pub fn set_noexec(&self, noexec: bool) {
        *self.noexec.lock() = noexec;
    }

    /// Whether `open` must refuse a device node here.
    pub fn is_nodev(&self) -> bool {
        *self.nodev.lock()
    }

    pub fn set_nodev(&self, nodev: bool) {
        *self.nodev.lock() = nodev;
    }

    /// Adopt a new set of options, the way `do_remount()` does: the flags it
    /// is given REPLACE what the mount had, which is why `mount -o remount`
    /// reads the current line out of `/proc/mounts` and passes the whole set
    /// back. Every option moves together, from the same pair
    /// [`build_options_string`] writes the new line from.
    pub fn apply_options(&self, flags: usize, data: &str) {
        self.set_read_only(flags_read_only(flags, data));
        self.set_nosuid(flags_nosuid(flags, data));
        self.set_noexec(flags_noexec(flags, data));
        self.set_nodev(flags_nodev(flags, data));
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

/// `MS_NOEXEC`, from the flag word or from an `-o noexec` in the data string.
pub fn flags_noexec(flags: usize, data: &str) -> bool {
    if flags & MS_NOEXEC != 0 {
        return true;
    }
    parse_option_flag(data, "noexec")
}

/// `MS_NODEV`, from the flag word or from an `-o nodev` in the data string.
pub fn flags_nodev(flags: usize, data: &str) -> bool {
    if flags & MS_NODEV != 0 {
        return true;
    }
    parse_option_flag(data, "nodev")
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
    // Through the same `flags_*` predicates the behaviour is read from, not off
    // the flag bit alone: `mount -o nodev` puts the word in `data` and no bit in
    // `flags`, so the bit tests that used to be here printed `nodev` only for
    // half the ways of asking for it -- and the line reached `/proc/mounts`
    // anyway, because `data` is appended whole below. One question, one answer,
    // which is the rule [`MountState::from_options`] already states.
    if flags_nosuid(flags, data) {
        opts.push_str(",nosuid");
    }
    if flags_nodev(flags, data) {
        opts.push_str(",nodev");
    }
    if flags_noexec(flags, data) {
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
            (MS_NOEXEC, ""),
            (0, "noexec"),
            (MS_NODEV, ""),
            (0, "nodev"),
            (MS_NOSUID | MS_NODEV | MS_NOEXEC, "relatime"),
            (0, "rw,nosuid,nodev,noexec,relatime"),
        ] {
            let state = MountState::from_options(flags, data);
            let line = build_options_string(flags, data);
            // Every option the state carries, not just the two it started
            // with: `nodev` and `noexec` were printed and never recorded, so
            // this loop passed while the line promised what nothing enforced.
            for (word, held) in [
                ("ro", state.is_read_only()),
                ("nosuid", state.is_nosuid()),
                ("noexec", state.is_noexec()),
                ("nodev", state.is_nodev()),
            ] {
                assert_eq!(
                    held,
                    line.split(',').any(|o| o == word),
                    "state and line disagree about {} for ({:#x}, {:?}): {:?}",
                    word,
                    flags,
                    data,
                    line
                );
            }
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
    /// `noexec` and `nodev` were parsed, printed and asked of nobody.
    ///
    /// `/proc` and `/sys` are registered `rw,nosuid,nodev,noexec,relatime` and
    /// have said exactly that in `/proc/mounts` since this kernel first had
    /// one; `/tmp`, `/run` and `/dev/shm` are registered `nodev`. Executing out
    /// of `/proc` worked, and a device node `mknod`ed in `/tmp` opened, because
    /// the only thing either word reached was the string.
    fn the_two_options_that_only_reached_proc_mounts() {
        let pseudo = MountState::from_options(0, "rw,nosuid,nodev,noexec,relatime");
        assert!(pseudo.is_noexec(), "noexec did not reach the state");
        assert!(pseudo.is_nodev(), "nodev did not reach the state");
        assert!(pseudo.is_nosuid());
        assert!(!pseudo.is_read_only());

        // And `/dev`, which must NOT be `nodev`: it is where the nodes live.
        let dev = MountState::from_options(0, "rw,nosuid");
        assert!(!dev.is_nodev(), "/dev would refuse every device node");
        assert!(!dev.is_noexec());
    }

    #[test]
    /// Either spelling, like `ro` and `nosuid`: a program types the option one
    /// way or the other, and `mount -o nodev` puts the word in the data string
    /// with no bit in the flag word at all.
    fn noexec_and_nodev_can_be_asked_for_either_way() {
        assert!(flags_noexec(MS_NOEXEC, ""));
        assert!(flags_noexec(0, "noexec"));
        assert!(flags_noexec(0, "rw,nosuid,noexec,relatime"));
        assert!(!flags_noexec(0, "rw,nosuid"));
        assert!(!flags_noexec(0, ""));

        assert!(flags_nodev(MS_NODEV, ""));
        assert!(flags_nodev(0, "nodev"));
        assert!(flags_nodev(0, "rw,nosuid,nodev,relatime"));
        assert!(!flags_nodev(0, "rw,nosuid"));
    }

    #[test]
    /// And a word that merely contains one is not one. `/proc/mounts` is a
    /// comma-separated list and the option is a whole element of it.
    fn a_word_that_merely_contains_the_option_is_not_the_option() {
        for data in ["noexecute", "xnoexec", "no-exec", "nodevice", "xnodev"] {
            assert!(!flags_noexec(0, data), "{:?} read as noexec", data);
            assert!(!flags_nodev(0, data), "{:?} read as nodev", data);
        }
    }

    #[test]
    /// A remount moves all four together. One left behind is a mount doing
    /// something its own line no longer says.
    fn a_remount_moves_the_new_options_too() {
        let state = MountState::from_options(MS_NOEXEC | MS_NODEV, "");
        assert!(state.is_noexec());
        assert!(state.is_nodev());
        state.apply_options(0, "rw");
        assert!(!state.is_noexec(), "noexec outlived the remount");
        assert!(!state.is_nodev(), "nodev outlived the remount");
        state.apply_options(0, "noexec,nodev");
        assert!(state.is_noexec());
        assert!(state.is_nodev());
        // One at a time, because moving them together is a weaker statement:
        // a remount that wired both to the same question would pass every
        // assertion above and still be wrong.
        state.apply_options(0, "noexec");
        assert!(state.is_noexec());
        assert!(!state.is_nodev(), "nodev followed noexec");
        state.apply_options(MS_NODEV, "");
        assert!(!state.is_noexec(), "noexec followed nodev");
        assert!(state.is_nodev());
    }

    #[test]
    /// The line says each of them once, whichever way they were asked for.
    fn the_options_line_says_the_new_options_once() {
        assert_eq!(build_options_string(MS_NOEXEC, "noexec"), "rw,noexec");
        assert_eq!(build_options_string(MS_NODEV, "nodev"), "rw,nodev");
        assert_eq!(
            build_options_string(0, "rw,nosuid,nodev,noexec,relatime"),
            "rw,nosuid,nodev,noexec,relatime"
        );
        assert_eq!(
            build_options_string(MS_NOSUID | MS_NODEV | MS_NOEXEC, "relatime"),
            "rw,nosuid,nodev,noexec,relatime"
        );
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
