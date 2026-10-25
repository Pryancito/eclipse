//! Whether the machine boots with a PID 1, and how it decides.
//!
//! `run_init_if_present` is the one place that answers "is there an init to
//! start?", and `program_exists` is how it answers. Neither had a test, and
//! the three ways the answer can be no all end the same way: the machine
//! boots with only the per-terminal shells, and the only evidence is a
//! `warn!` line at a log level a desktop does not print.
//!
//! The fixture is two filesystems and nothing else: an initramfs, and
//! optionally a primary root mounted over it. That is the real shape of the
//! question -- `INIT=/sbin/init` is resolved against whichever of the two
//! has it, and `/sbin/init` is normally a symlink to a busybox that may or
//! may not have been installed.

use super::*;
use linux_object::fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;

/// An empty filesystem to put an initramfs or a root on.
fn an_fs() -> Arc<dyn FileSystem> {
    RamFS::new()
}

/// Create `dir/name` as an ordinary file, making `dir` on the way.
fn file(fs: &Arc<dyn FileSystem>, dir: &str, name: &str) {
    let d = fs
        .root_inode()
        .create(dir, FileType::Dir, 0o755)
        .expect("a directory of its own");
    d.create(name, FileType::File, 0o755).expect("a file in it");
}

/// Create the symlink `/name` pointing at `target`, which need not exist.
fn symlink(fs: &Arc<dyn FileSystem>, name: &str, target: &str) {
    let l = fs
        .root_inode()
        .create(name, FileType::SymLink, 0o777)
        .expect("a symlink");
    l.write_at(0, target.as_bytes())
        .expect("a symlink holds its target as its contents");
}

/// `/l0` -> `/l1` -> ... -> `/l<hops-1>` -> `/bin/busybox`, so resolving
/// `/l0` costs `hops` symlink hops.
fn a_chain_of(hops: usize) -> Arc<dyn FileSystem> {
    let fs = an_fs();
    file(&fs, "bin", "busybox");
    for i in 0..hops {
        let target = if i + 1 == hops {
            String::from("/bin/busybox")
        } else {
            alloc::format!("/l{}", i + 1)
        };
        symlink(&fs, &alloc::format!("l{}", i), &target);
    }
    fs
}

/// The plain case, and the one the boot takes: the binary is on the
/// initramfs and no primary root is mounted yet.
#[test]
fn a_binary_on_the_initramfs_is_found() {
    let fs = an_fs();
    file(&fs, "bin", "busybox");
    assert!(program_exists("/bin/busybox", &None, &fs));
}

/// And a path neither filesystem has is not found, which is the answer that
/// decides the machine boots without a PID 1.
#[test]
fn a_path_nothing_has_is_not_found() {
    let fs = an_fs();
    file(&fs, "bin", "busybox");
    assert!(!program_exists("/sbin/init", &None, &fs));
    assert!(!program_exists("/bin/busybos", &None, &fs));
}

/// The primary root is asked first and can answer on its own: an init
/// installed on the real root is found even when the initramfs has nothing.
#[test]
fn the_primary_root_answers_on_its_own() {
    let root = an_fs();
    file(&root, "sbin", "init");
    let initramfs = an_fs();
    assert!(program_exists(
        "/sbin/init",
        &Some(root.root_inode()),
        &initramfs
    ));
}

/// And it is asked FIRST, not instead: a binary only the initramfs has is
/// still found with a primary root mounted. This is the fall-through, and
/// without it mounting a root would hide the initramfs busybox that the
/// machine boots from.
#[test]
fn a_binary_only_the_initramfs_has_is_still_found_under_a_mounted_root() {
    let root = an_fs();
    file(&root, "etc", "fstab");
    let initramfs = an_fs();
    file(&initramfs, "bin", "busybox");
    assert!(program_exists(
        "/bin/busybox",
        &Some(root.root_inode()),
        &initramfs
    ));
}

/// `/sbin/init` is normally a symlink to busybox, and the symlink existing
/// is not the same as the program existing. A dangling one has to count as
/// not installed, or the machine tries to exec a name that resolves to
/// nothing -- which is exactly what a rootfs built without busybox leaves
/// behind. This is what the follow in `lookup_follow` is for: a plain
/// lookup finds the link itself and would answer yes.
#[test]
fn a_dangling_init_symlink_counts_as_not_installed() {
    let fs = an_fs();
    symlink(&fs, "init", "/bin/busybox");
    assert!(
        !program_exists("/init", &None, &fs),
        "the link is there and its target is not"
    );

    let fs = an_fs();
    file(&fs, "bin", "busybox");
    symlink(&fs, "init", "/bin/busybox");
    assert!(
        program_exists("/init", &None, &fs),
        "and with busybox installed the same link resolves"
    );
}

/// The hop bound is reachable: a chain as long as the budget still
/// resolves, so the bound is not spent before a real `/sbin/init` ->
/// `/bin/busybox` -> ... chain is walked.
#[test]
fn a_chain_of_links_up_to_the_hop_bound_resolves() {
    let fs = a_chain_of(FOLLOW_LINK_DEPTH);
    assert!(
        program_exists("/l0", &None, &fs),
        "{} hops is within the budget",
        FOLLOW_LINK_DEPTH
    );
}

/// And it binds: one hop more is not followed. Together with the test above
/// this pins `FOLLOW_LINK_DEPTH` to a number the walk really uses, rather
/// than to a constant nobody reads.
#[test]
fn a_chain_one_hop_longer_than_the_bound_does_not_resolve() {
    let fs = a_chain_of(FOLLOW_LINK_DEPTH + 1);
    assert!(
        !program_exists("/l0", &None, &fs),
        "{} hops is one too many",
        FOLLOW_LINK_DEPTH + 1
    );
}

/// What the bound is really there for: a loop answers no instead of walking
/// forever. A boot that hangs here hangs before anything has been printed.
#[test]
fn a_symlink_loop_answers_no_instead_of_walking_forever() {
    let fs = an_fs();
    symlink(&fs, "init", "/sbin-init");
    symlink(&fs, "sbin-init", "/init");
    assert!(!program_exists("/init", &None, &fs));
}

/// An empty command line is no init at all, and the refusal comes before
/// anything is spawned: there is no process to return and none is created.
#[test]
fn an_empty_command_line_is_not_an_init() {
    let fs = an_fs();
    file(&fs, "sbin", "init");
    assert!(run_init_if_present(Vec::new(), Vec::new(), fs, None, true).is_none());
}

/// Nor is an empty program name, which is what an `INIT=` with nothing
/// after it in `rboot.conf` leaves behind. Without this guard the path goes
/// on to look up `""`.
#[test]
fn an_empty_program_name_is_not_an_init() {
    let fs = an_fs();
    file(&fs, "sbin", "init");
    assert!(run_init_if_present(alloc::vec![String::new()], Vec::new(), fs, None, true).is_none());
}

/// And the case the whole function exists for: the init named is not
/// installed, so the machine boots WITHOUT a PID 1 instead of panicking.
/// The initramfs here has a busybox but no `/sbin/init` pointing at it,
/// which is a rootfs built with `INIT=` left at its default.
#[test]
fn an_init_that_is_not_installed_boots_the_machine_without_a_pid_1() {
    let fs = an_fs();
    file(&fs, "bin", "busybox");
    assert!(run_init_if_present(
        alloc::vec![String::from("/sbin/init")],
        Vec::new(),
        fs,
        None,
        true
    )
    .is_none());
}

/// The same refusal with a primary root mounted: the root is consulted, and
/// not having it there either is still no init.
#[test]
fn an_init_missing_from_both_the_root_and_the_initramfs_is_not_an_init() {
    let root = an_fs();
    file(&root, "etc", "fstab");
    let initramfs = an_fs();
    file(&initramfs, "bin", "busybox");
    assert!(run_init_if_present(
        alloc::vec![String::from("/sbin/init")],
        Vec::new(),
        initramfs,
        Some(root.root_inode()),
        false
    )
    .is_none());
}
