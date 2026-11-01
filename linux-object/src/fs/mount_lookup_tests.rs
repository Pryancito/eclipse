//! Which mount a path is on. It decides whether a set-user-ID bit on that
//! path is honoured, so getting it wrong in one direction hands privilege
//! away on a `nosuid` mount and in the other takes it from a mount that
//! never asked for the option.
//!
//! All of it is checked through [`mount_flag_from`], over mount points the
//! test supplies, so nothing here touches the one live mount table -- a
//! test that mounted something would leave it mounted for every test
//! after it, and `--test-threads=1` is exactly the setting under which
//! that goes unnoticed.

use super::*;

/// A plausible boot: the root plus the pseudo-filesystems the kernel
/// mounts itself, with the `nosuid` each of them is registered with.
const A_BOOT: &[(&str, bool)] = &[
    ("/", false),
    ("/dev", true),
    ("/dev/shm", true),
    ("/tmp", true),
    ("/run", true),
    ("/proc", true),
    ("/sys", true),
];

fn nosuid(path: &str) -> bool {
    mount_flag_from(path, A_BOOT.iter().copied())
}

#[test]
fn a_mount_with_no_state_of_its_own_gets_one_from_the_line_it_records() {
    // The pseudo-filesystems the kernel mounts itself have no wrapped
    // filesystem to carry a state, and they are exactly the ones
    // registered `nosuid`. Their state used to be built from nothing.
    let entry = new_mount_entry("proc", "/proc", "proc", "rw,nosuid,nodev,noexec", None);
    assert!(entry.state.is_nosuid());
    assert!(!entry.state.is_read_only());
    let plain = new_mount_entry("/dev/sda2", "/", "btrfs", "rw", None);
    assert!(!plain.state.is_nosuid());
}

#[test]
fn a_mount_that_brought_its_own_state_keeps_it() {
    // A real mount hands over the very state its filesystem wrapper is
    // already enforcing; rebuilding it here would give the table a second
    // copy that a remount could move without the wrapper noticing.
    let state = alloc::sync::Arc::new(mount_state::MountState::from_options(0, "rw"));
    let entry = new_mount_entry(
        "/dev/sdb1",
        "/mnt",
        "vfat",
        "rw,nosuid",
        Some(state.clone()),
    );
    assert!(
        !entry.state.is_nosuid(),
        "the line did not win over the state"
    );
    state.set_nosuid(true);
    assert!(entry.state.is_nosuid(), "the table holds the SAME state");
}

#[test]
fn a_remount_moves_the_state_the_filesystem_is_enforcing() {
    // Not a copy of it: the entry and the mounted filesystem share one
    // `MountState`, which is how a `mount -o remount,ro` reaches the
    // write path at all.
    let state = alloc::sync::Arc::new(mount_state::MountState::from_options(0, "rw,nosuid"));
    let mut table = alloc::vec![new_mount_entry(
        "tmpfs",
        "/tmp",
        "tmpfs",
        "rw,nosuid",
        Some(state.clone())
    )];
    remount_entry(&mut table, "/tmp", 0, "ro").unwrap();
    assert!(state.is_read_only(), "the filesystem still thinks it is rw");
    assert!(
        !state.is_nosuid(),
        "nosuid outlived a remount that dropped it"
    );
    assert_eq!(table[0].options, "ro");
}

#[test]
fn a_remount_where_two_are_stacked_moves_the_top_one() {
    // Linux 6.18, `mount -t tmpfs t1 c; mount -t tmpfs t2 c; mount -o
    // remount,ro c`: `t2` read `ro`, `t1` stayed `rw`. Here init's own
    // `mount -t tmpfs tmpfs /tmp` stacks one over the kernel's, and the
    // flags are read off the top one.
    let mut table = alloc::vec![
        new_mount_entry("t1", "/tmp", "tmpfs", "rw", None),
        new_mount_entry("t2", "/tmp", "tmpfs", "rw", None),
    ];
    remount_entry(&mut table, "/tmp", 0, "ro,noexec").unwrap();
    assert_eq!(
        (table[0].options.as_str(), table[1].options.as_str()),
        ("rw", "ro,noexec")
    );
    assert!(table[1].state.is_noexec() && !table[0].state.is_noexec());
    let flagged = table
        .iter()
        .map(|m| (m.target.as_str(), m.state.is_noexec()));
    assert!(
        mount_flag_from("/tmp/x", flagged),
        "the remount did nothing"
    );

    // A move takes the top one too, as the path leads to it.
    move_entry(&mut table, "/tmp", "/mnt").unwrap();
    assert_eq!(
        (table[0].target.as_str(), table[1].target.as_str()),
        ("/tmp", "/mnt")
    );
}

#[test]
fn a_remount_of_something_that_is_not_mounted_is_einval() {
    let mut table = alloc::vec![new_mount_entry("tmpfs", "/tmp", "tmpfs", "rw", None)];
    assert!(remount_entry(&mut table, "/nowhere", 0, "ro").is_err());
    // And a trailing slash names the same mount, because `/proc/mounts`
    // never carries one and the caller typed whatever they typed.
    assert!(remount_entry(&mut table, "/tmp/", 0, "ro").is_ok());
}

#[test]
fn a_program_on_the_root_filesystem_may_be_set_user_id() {
    // The case every program on this machine is in. Answering the other
    // way would turn off set-user-ID for the whole system.
    assert!(!nosuid("/bin/busybox"));
    assert!(!nosuid("/usr/bin/passwd"));
    assert!(!nosuid("/"));
}

#[test]
fn a_program_dropped_in_tmp_may_not() {
    // Which is the entire reason `/tmp` is mounted `nosuid`: it is the one
    // directory anybody can write to.
    assert!(nosuid("/tmp/x"));
    assert!(nosuid("/tmp/deep/er/still"));
    assert!(nosuid("/dev/shm/x"));
}

#[test]
fn the_mount_point_itself_is_on_its_own_mount() {
    assert!(nosuid("/tmp"));
    assert!(nosuid("/tmp/"));
}

#[test]
fn a_mount_point_is_not_a_prefix_of_a_name_that_merely_starts_with_it() {
    // `/tmpfiles` is a directory on the root filesystem, not something
    // inside the mount at `/tmp`. A plain `starts_with` puts it there and
    // silently disarms every set-user-ID program under it.
    assert!(!nosuid("/tmpfiles/x"));
    assert!(!nosuid("/tmpfoo"));
    assert!(!nosuid("/proc-backup/x"));
}

/// The pseudo-filesystems as `create_root_fs` actually registers them,
/// option strings and all, so the table these tests reason over is the one
/// the kernel builds rather than a guess at it.
const AS_REGISTERED: &[(&str, &str)] = &[
    ("/", "rw"),
    ("/dev", "rw,nosuid"),
    ("/dev/shm", "rw,nosuid,nodev"),
    ("/tmp", "rw,nosuid,nodev"),
    ("/run", "rw,nosuid,nodev"),
    ("/proc", "rw,nosuid,nodev,noexec,relatime"),
    ("/sys", "rw,nosuid,nodev,noexec,relatime"),
];

fn asked(path: &str, flag: fn(&mount_state::MountState) -> bool) -> bool {
    let states: Vec<(&str, bool)> = AS_REGISTERED
        .iter()
        .map(|(target, opts)| {
            (
                *target,
                flag(&mount_state::MountState::from_options(0, opts)),
            )
        })
        .collect();
    mount_flag_from(path, states.iter().copied())
}

#[test]
/// What the boot table means once the two options reach something. Both
/// directions matter equally: refusing where nothing asked for a refusal
/// breaks the desktop, and allowing where the line promised otherwise is
/// the bug.
fn the_boot_table_refuses_exactly_where_it_says_it_does() {
    let noexec = |p: &str| asked(p, |s| s.is_noexec());
    let nodev = |p: &str| asked(p, |s| s.is_nodev());

    // `noexec`: the two pseudo-filesystems that say so, and nowhere else.
    assert!(noexec("/proc/1/mem"));
    assert!(noexec("/sys/kernel/whatever"));
    assert!(!noexec("/bin/sh"), "the root filesystem is not noexec");
    // The pair that makes busybox work. `/proc/self/exe` is under a
    // `noexec` mount and would be refused if `execve` asked about the name
    // the caller wrote -- which is why it canonicalises the magic link to
    // the real binary FIRST, and the real binary is on the root.
    assert!(
        noexec("/proc/self/exe"),
        "the name itself is on a noexec mount"
    );
    assert!(
        !noexec("/bin/busybox"),
        "the path the magic link canonicalises to must stay executable"
    );
    assert!(!noexec("/tmp/script.sh"), "/tmp is not registered noexec");
    assert!(!noexec("/usr/lib/libc.so"));

    // `nodev`: the three tmpfs mounts, and NOT `/dev`.
    assert!(nodev("/tmp/fakedisk"));
    assert!(nodev("/run/x"));
    assert!(nodev("/dev/shm/x"));
    assert!(!nodev("/dev/null"), "/dev is where the nodes live");
    assert!(!nodev("/dev/sda"));
    // `/dev/pts` is a plain directory, not a mount, so a pty slave answers
    // to `/dev`. If this ever came back `true`, every terminal on the
    // machine would stop opening -- which is the whole reason it is pinned
    // here rather than left to be noticed on hardware.
    assert!(!nodev("/dev/pts/0"), "pty slaves must still open");
    assert!(!nodev("/dev/ptmx"));
}

#[test]
/// `/dev/shm` is inside `/dev`, and the two disagree about `nodev`. That is
/// the longest-match rule doing the only job it has, on the one pair of
/// real mounts where getting it wrong is the difference between a working
/// `/dev` and none.
fn dev_shm_is_nodev_without_making_dev_nodev() {
    let nodev = |p: &str| asked(p, |s| s.is_nodev());
    assert!(nodev("/dev/shm/sem.foo"));
    assert!(!nodev("/dev/tty0"));
    // And the boundary: a name that merely starts with `/dev/shm`.
    assert!(!nodev("/dev/shmfoo"), "a prefix is not a mount point");
}

#[test]
fn the_longest_mount_point_wins() {
    // Both directions, because a rule that took the first match would be
    // right about half of these by luck.
    let mounts = &[("/", false), ("/mnt", true), ("/mnt/usb", false)];
    assert!(mount_flag_from("/mnt/x", mounts.iter().copied()));
    assert!(
        !mount_flag_from("/mnt/usb/x", mounts.iter().copied()),
        "a mount inside a nosuid mount answers for itself"
    );
    let inverted = &[("/", false), ("/mnt", false), ("/mnt/usb", true)];
    assert!(!mount_flag_from("/mnt/x", inverted.iter().copied()));
    assert!(mount_flag_from("/mnt/usb/x", inverted.iter().copied()));
}

#[test]
fn the_last_mount_on_a_point_is_the_one_that_answers() {
    // Mounting over an existing mount point pushes a second entry with
    // the same target; a lookup reaches the newer filesystem, so the
    // newer entry is the one whose options apply.
    let stacked = &[("/", false), ("/mnt", false), ("/mnt", true)];
    assert!(mount_flag_from("/mnt/x", stacked.iter().copied()));
    let unstacked = &[("/", false), ("/mnt", true), ("/mnt", false)];
    assert!(!mount_flag_from("/mnt/x", unstacked.iter().copied()));
}

#[test]
fn a_path_on_no_mount_at_all_grants_nothing_extra() {
    // With no root entry there is nothing to be under. Fail open, which
    // is what the tree did before any of this existed -- the option is a
    // restriction, and inventing one nobody asked for is its own bug.
    assert!(!mount_flag_from("/bin/sh", core::iter::empty()));
    assert!(!mount_flag_from("relative/path", A_BOOT.iter().copied()));
}

#[test]
fn the_root_mount_covers_a_path_that_matches_nothing_else() {
    let ro_root = &[("/", true), ("/tmp", false)];
    assert!(mount_flag_from("/bin/sh", ro_root.iter().copied()));
    assert!(!mount_flag_from("/tmp/x", ro_root.iter().copied()));
}

#[test]
fn the_boundary_rule_on_its_own() {
    // `path_is_under` is what the rule above is built from, so it gets its
    // own test: the failures of the composite are hard to read.
    assert!(path_is_under("/anything", "/"));
    assert!(path_is_under("/tmp", "/tmp"));
    assert!(path_is_under("/tmp/x", "/tmp"));
    assert!(!path_is_under("/tmpx", "/tmp"));
    assert!(!path_is_under("/tm", "/tmp"));
    assert!(!path_is_under("/var/tmp/x", "/tmp"));
}
