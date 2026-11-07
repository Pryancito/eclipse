//! How a line of `/etc/fstab` is read: which lines boot skips, what the
//! option field means, and what reaches the filesystem that is being
//! mounted.
//!
//! These were four lines inside `mount_fstab`'s six-deep nesting and had no
//! test, which is how an `nfs` line could sit in the fstab doing nothing
//! for as long as it did. Each one is now a function with a name, and each
//! test here names the behaviour it would catch going wrong rather than the
//! shape of the code.

use super::*;

/// `noauto` is `mount -a`'s own rule: the line exists for a later `mount
/// /mnt`, and boot leaves it alone. It is the escape hatch that makes
/// [`FSTAB_NETWORK_BUDGET_MS`] something a user can avoid entirely.
#[test]
fn boot_skips_a_line_that_says_noauto() {
    assert!(fstab_has_option("noauto", "noauto"));
    assert!(fstab_has_option("rw,noauto", "noauto"));
    assert!(fstab_has_option("noauto,rw", "noauto"));
    assert!(fstab_has_option("rw,noauto,nosuid", "noauto"));
    assert!(fstab_has_option("rw, noauto ,nosuid", "noauto"));
}

/// The reason this is a split and not `options.contains(name)`.
///
/// `auto` is a real option and the opposite of the one being looked for, so
/// a substring match reads `noauto` out of a line that said `auto` — and
/// mounts nothing where the fstab asked for a mount. The other direction is
/// a longer name that merely starts the same way.
#[test]
fn an_option_is_a_whole_field_and_not_a_substring_of_one() {
    assert!(
        !fstab_has_option("auto", "noauto"),
        "`auto` is the opposite of `noauto`, not a line containing it"
    );
    assert!(
        !fstab_has_option("rw,auto", "noauto"),
        "`auto` is the opposite of `noauto`, not a line containing it"
    );
    assert!(
        !fstab_has_option("noauto_later", "noauto"),
        "a longer option name is a different option"
    );
    assert!(
        !fstab_has_option("x_noauto", "noauto"),
        "a longer option name is a different option"
    );
    assert!(!fstab_has_option("defaults", "noauto"));
    assert!(!fstab_has_option("", "noauto"));
}

/// Every flag the option field can set, so a dropped arm is a named
/// failure.
#[test]
fn the_option_field_sets_the_flags_mount_2_would_set() {
    assert_eq!(fstab_flags("ro") & mount_state::MS_RDONLY, {
        mount_state::MS_RDONLY
    });
    assert_eq!(fstab_flags("nosuid") & mount_state::MS_NOSUID, {
        mount_state::MS_NOSUID
    });
    assert_eq!(fstab_flags("nodev") & mount_state::MS_NODEV, {
        mount_state::MS_NODEV
    });
    assert_eq!(fstab_flags("noexec") & mount_state::MS_NOEXEC, {
        mount_state::MS_NOEXEC
    });
    assert_eq!(
        fstab_flags("ro,nosuid,nodev,noexec"),
        mount_state::MS_RDONLY
            | mount_state::MS_NOSUID
            | mount_state::MS_NODEV
            | mount_state::MS_NOEXEC
    );
}

/// `rw` **clears** `MS_RDONLY` rather than setting a flag of its own, which
/// is the one thing about this loop worth a test: it makes the field
/// order-sensitive exactly as `mount(8)` is. `ro,rw` is a read-write mount
/// and `rw,ro` is a read-only one, and a reading that treated `rw` as a
/// no-op would make both read-only and mount a writable home read-only.
#[test]
fn the_last_of_ro_and_rw_on_a_line_is_the_one_that_counts() {
    assert_eq!(
        fstab_flags("ro,rw") & mount_state::MS_RDONLY,
        0,
        "`rw` after `ro` is a writable mount"
    );
    assert_eq!(
        fstab_flags("rw,ro") & mount_state::MS_RDONLY,
        mount_state::MS_RDONLY,
        "`ro` after `rw` is a read-only mount"
    );
    assert_eq!(fstab_flags("rw") & mount_state::MS_RDONLY, 0);
    assert_eq!(fstab_flags("defaults") & mount_state::MS_RDONLY, 0);
}

/// An option name nothing knows is ignored rather than failing the line,
/// which is what `mount(8)` does with a filesystem-specific option.
#[test]
fn an_unknown_option_sets_no_flag_and_does_not_fail_the_line() {
    assert_eq!(fstab_flags("vers=3,rsize=32768"), 0);
    assert_eq!(
        fstab_flags("vers=3,ro,rsize=32768"),
        mount_state::MS_RDONLY,
        "an unknown option does not swallow the ones around it"
    );
}

/// `defaults` is `mount(8)`'s word for "nothing in particular" and is not
/// an option of any filesystem.
///
/// The block path never had to care, because `open_filesystem` ignores what
/// it does not recognise. `NfsOptions::parse` does not: passed through,
/// `defaults` is an unrecognised option on every `nfs` line in every fstab
/// that was written the ordinary way, which is to say it would fail the
/// mount it was supposed to describe.
#[test]
fn defaults_reaches_the_filesystem_as_no_options_at_all() {
    assert_eq!(fstab_mount_data("defaults"), "");
    assert_eq!(fstab_mount_data("rw,vers=3"), "rw,vers=3");
    assert_eq!(fstab_mount_data(""), "");
    assert_eq!(
        fstab_mount_data("defaults,ro"),
        "defaults,ro",
        "only the bare word is the placeholder; alongside others it is an \
         option like any other and the filesystem may say so"
    );
}

/// The whole run shares one budget, so the bound at boot is on the run and
/// not on the line.
///
/// The arithmetic the constant exists to stop: a minute per line means an
/// fstab with three unreachable servers holds the deferred task — and the
/// shell — for three minutes. Asserted against `greeting_budget`'s own
/// default so the two cannot drift apart silently.
#[test]
fn the_whole_fstab_run_is_bounded_and_not_each_of_its_lines() {
    let per_mount =
        crate::fs::nfs::greeting::greeting_budget(&crate::fs::nfs::source::NfsOptions::default());
    assert!(
        FSTAB_NETWORK_BUDGET_MS <= per_mount,
        "a run of network lines must not be allowed more time than one \
         mount(2) gets: {} vs {}",
        FSTAB_NETWORK_BUDGET_MS,
        per_mount
    );
}

/// A network line is routed by **type**, before anything looks its source
/// up.
///
/// The bug this whole batch exists to fix: `nas:/srv` is not a path, so a
/// line whose source is looked up as a file dies with `ENOENT` blamed on
/// the source, and the fstab entry silently does nothing. The source is not
/// even an argument here, which is the strongest form of that statement.
#[test]
fn an_nfs_line_is_routed_by_its_type_and_not_by_its_source() {
    assert_eq!(
        fstab_line_action("/mnt/nas", "nfs", "defaults"),
        FstabAction::Network("nfs")
    );
    assert_eq!(
        fstab_line_action("/mnt/nas", "nfs3", "defaults"),
        FstabAction::Network("nfs"),
        "the other spelling of the same type reaches the network path, and \
         reaches it under the one name the mount table records"
    );
    assert_eq!(
        fstab_line_action("/mnt/nas", "NFS", "rw"),
        FstabAction::Network("nfs"),
        "the type column is matched the way mount(8) matches it"
    );
}

/// And every other line still goes to the block path, so the branch cannot
/// be widened into one that swallows them.
#[test]
fn a_block_line_still_goes_to_the_block_path() {
    for fstype in ["vfat", "btrfs", "eclipsefs", "msdos"] {
        let action = fstab_line_action("/home", fstype, "defaults");
        assert!(
            matches!(action, FstabAction::Block(_)),
            "{} was routed to {:?}",
            fstype,
            action
        );
    }
    assert_eq!(
        fstab_line_action("/mnt/nas", "nfs4", "defaults"),
        FstabAction::Unsupported(LxError::ENODEV),
        "this is a v3 client: v4 is refused rather than mounted with it"
    );
    assert!(matches!(
        fstab_line_action("/mnt/x", "ntfs", "defaults"),
        FstabAction::Unsupported(_)
    ));
}

/// Which lines boot leaves alone, and the reason each one carries.
#[test]
fn boot_leaves_the_root_swap_and_noauto_lines_alone() {
    assert!(matches!(
        fstab_line_action("/", "eclipsefs", "defaults"),
        FstabAction::Skip(_)
    ));
    assert!(matches!(
        fstab_line_action("none", "swap", "sw"),
        FstabAction::Skip(_)
    ));
    assert!(matches!(
        fstab_line_action("/swapfile", "swap", "sw"),
        FstabAction::Skip(_)
    ));
    assert_eq!(
        fstab_line_action("/mnt/nas", "nfs", "noauto"),
        FstabAction::Skip("noauto"),
        "the escape hatch has to work for a network line above all, because \
         that is the line whose cost is a dial"
    );
    assert_eq!(
        fstab_line_action("/mnt/nas", "nfs", "rw,noauto,nosuid"),
        FstabAction::Skip("noauto")
    );
}

/// `noauto` is checked before the type is, so a `noauto` line of a type
/// this kernel has no driver for is a skip and not a warning on every boot.
#[test]
fn a_noauto_line_is_skipped_whatever_its_type_is() {
    assert_eq!(
        fstab_line_action("/mnt/x", "ntfs", "noauto"),
        FstabAction::Skip("noauto")
    );
    assert_eq!(
        fstab_line_action("/mnt/x", "nfs4", "noauto"),
        FstabAction::Skip("noauto")
    );
}

/// `_netdev` is mounted, not skipped, and that is deliberate.
///
/// In Linux `mount -a` skips it and a network-online target mounts it
/// later. Here the deferred fstab task **is** the later: it runs after init
/// is up, off the synchronous boot path. Skipping `_netdev` would make the
/// one option people put on network lines the option that stops them
/// mounting at all.
#[test]
fn a_netdev_line_is_mounted_because_this_run_is_already_the_late_one() {
    assert_eq!(
        fstab_line_action("/mnt/nas", "nfs", "_netdev"),
        FstabAction::Network("nfs")
    );
    assert_eq!(
        fstab_line_action("/mnt/nas", "nfs", "rw,_netdev,noauto"),
        FstabAction::Skip("noauto"),
        "`noauto` still wins, as it does in mount(8)"
    );
}

/// The run's deadline is built once and handed out again, so the second
/// network line gets what the first left of it.
///
/// Measured against the real clock, because that is the only clock a
/// `Budget` reads: a fresh deadline and a shared one are the same number
/// until time actually passes.
#[test]
fn the_runs_deadline_is_built_once_and_handed_out_again() {
    const BURN_MS: u64 = 30;
    let mut slot: Option<Budget> = None;
    assert!(slot.is_none(), "nothing is built before a line needs one");

    let first = run_budget(&mut slot).left().expect("time to start with");
    let start = kernel_hal::timer::timer_now().as_millis() as u64;
    while (kernel_hal::timer::timer_now().as_millis() as u64) < start + BURN_MS {
        core::hint::spin_loop();
    }
    let second = run_budget(&mut slot).left().expect("time still left");

    assert!(
        first >= second + BURN_MS / 2,
        "the second line got {} ms of a deadline that started at {} ms, \
         after {} ms had passed: a fresh minute per line, which is the \
         arithmetic the shared deadline exists to stop",
        second,
        first,
        BURN_MS
    );
}
