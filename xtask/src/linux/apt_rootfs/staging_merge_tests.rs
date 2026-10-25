//! What a `.deb` leaves in the rootfs once the staging root is merged in.
//!
//! The bootstrap never installs anything. It unpacks every `.deb` into a
//! throwaway staging root with `dpkg-deb -x` and then copies selected trees
//! out of it, and four functions decide what survives that copy:
//! `merge_usrmerge_tree`, `merge_stage`, `copy_tree` and `copy_one_bin`.
//! None of them had a test.
//!
//! Each is there for a failure that has already happened. Ubuntu ships
//! `/bin` as a symlink to `usr/bin`, so unpacking two packages into one
//! directory dies with "Cannot create symlink to 'usr/bin': File exists"
//! unless the top-level names are folded by hand. And the Eclipse rootfs has
//! a `/bin` of its own: busybox owns those applets, so a merge that took
//! `bin` wholesale would replace the shell the machine boots with.

use super::*;
use std::os::unix::fs::PermissionsExt;

/// A directory of its own for one test, named after it.
fn a_dir(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("eclipse-apt-merge-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).expect("a directory of its own");
    p
}

/// `path`, with the parents it needs, holding `body`.
fn file_with(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("a parent directory");
    }
    fs::write(path, body).expect("a file");
}

/// The symlink `path` -> `target`, which need not exist.
fn link(path: &Path, target: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("a parent directory");
    }
    std::os::unix::fs::symlink(target, path).expect("a symlink");
}

fn body(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A top-level `bin` symlink is Ubuntu's usrmerge marker and carries no
/// payload, so it is skipped rather than recreated. Recreating it is what
/// makes the next package's unpack fail: by then `bin` is a real directory
/// on one side and a link on the other.
#[test]
fn a_top_level_usrmerge_symlink_is_skipped_instead_of_recreated() {
    let tmp = a_dir("usrmerge-symlink");
    let from = tmp.join("unpacked");
    let stage = tmp.join("stage");
    file_with(&from.join("usr/bin/dpkg"), "the real binary");
    link(&from.join("bin"), "usr/bin");
    fs::create_dir_all(stage.join("usr/bin")).expect("a staging root");

    merge_usrmerge_tree(&from, &stage);

    assert_eq!(body(&stage.join("usr/bin/dpkg")), "the real binary");
    let marker = stage.join("bin");
    assert!(
        !marker.exists() && !marker.is_symlink(),
        "the marker has no payload, and recreating it fights the next package"
    );
    let _ = fs::remove_dir_all(&tmp);
}

/// A package from before usrmerge has those names as real directories, and
/// each one is folded under `usr` -- into its own destination. A `lib` that
/// landed in `usr/lib64`, or the other way round, puts the loader where no
/// ELF looks for it.
#[test]
fn a_real_top_level_directory_is_folded_into_its_own_place_under_usr() {
    let tmp = a_dir("usrmerge-real");
    let from = tmp.join("unpacked");
    let stage = tmp.join("stage");
    file_with(&from.join("bin/dash"), "a shell");
    file_with(&from.join("sbin/ldconfig"), "the cache builder");
    file_with(&from.join("lib/libc.so.6"), "libc");
    file_with(&from.join("lib64/ld-linux-x86-64.so.2"), "the loader");
    fs::create_dir_all(&stage).expect("a staging root");

    merge_usrmerge_tree(&from, &stage);

    assert_eq!(body(&stage.join("usr/bin/dash")), "a shell");
    assert_eq!(body(&stage.join("usr/sbin/ldconfig")), "the cache builder");
    assert_eq!(body(&stage.join("usr/lib/libc.so.6")), "libc");
    assert_eq!(
        body(&stage.join("usr/lib64/ld-linux-x86-64.so.2")),
        "the loader"
    );
    for folded in ["bin", "sbin", "lib", "lib64"] {
        assert!(
            !stage.join(folded).exists(),
            "{folded} was left at the top level as well"
        );
    }
    let _ = fs::remove_dir_all(&tmp);
}

/// Everything else keeps the name it came with, `usr` included -- which is
/// where the whole payload of a usrmerged package lives.
#[test]
fn a_tree_that_is_not_one_of_the_four_keeps_the_name_it_came_with() {
    let tmp = a_dir("other-trees");
    let from = tmp.join("unpacked");
    let stage = tmp.join("stage");
    file_with(&from.join("etc/dpkg/origins/ubuntu"), "Vendor: Ubuntu");
    file_with(&from.join("var/lib/dpkg/available"), "");
    file_with(&from.join("usr/share/doc/dpkg/copyright"), "GPL-2");
    fs::create_dir_all(&stage).expect("a staging root");

    merge_usrmerge_tree(&from, &stage);

    assert_eq!(
        body(&stage.join("etc/dpkg/origins/ubuntu")),
        "Vendor: Ubuntu"
    );
    assert!(stage.join("var/lib/dpkg/available").is_file());
    assert_eq!(body(&stage.join("usr/share/doc/dpkg/copyright")), "GPL-2");
    let _ = fs::remove_dir_all(&tmp);
}

/// A symlink never replaces a directory that is already in the staging root.
/// One package shipping `share -> usr/share` would otherwise throw away
/// everything an earlier package put in `share`.
#[test]
fn a_symlink_does_not_replace_a_directory_already_in_the_staging_root() {
    let tmp = a_dir("link-over-dir");
    let from = tmp.join("unpacked");
    let stage = tmp.join("stage");
    link(&from.join("share"), "usr/share");
    file_with(
        &stage.join("share/keep-me"),
        "an earlier package put this here",
    );

    merge_usrmerge_tree(&from, &stage);

    let share = stage.join("share");
    assert!(share.is_dir() && !share.is_symlink(), "the directory went");
    assert_eq!(
        body(&share.join("keep-me")),
        "an earlier package put this here"
    );
    let _ = fs::remove_dir_all(&tmp);
}

/// But over a file it does, and it arrives as a symlink. Following it
/// instead would copy nothing at all here: the target is a path inside the
/// finished rootfs and does not exist in the staging root.
#[test]
fn a_symlink_arrives_as_a_symlink_and_replaces_the_file_that_was_there() {
    let tmp = a_dir("link-over-file");
    let from = tmp.join("unpacked");
    let stage = tmp.join("stage");
    link(&from.join("initrd.img"), "boot/initrd.img-generic");
    file_with(&stage.join("initrd.img"), "a stale file");

    merge_usrmerge_tree(&from, &stage);

    let made = stage.join("initrd.img");
    assert!(made.is_symlink(), "a symlink must not become a copy");
    assert_eq!(
        fs::read_link(&made).expect("a target"),
        Path::new("boot/initrd.img-generic")
    );
    let _ = fs::remove_dir_all(&tmp);
}

/// A plain file replaces the one already there. Packages are unpacked one
/// after another into the same staging root, so the later one has to win --
/// that is how a dependency's stub gets overwritten by the real thing.
#[test]
fn a_file_replaces_the_one_the_staging_root_already_had() {
    let tmp = a_dir("file-over-file");
    let from = tmp.join("unpacked");
    let stage = tmp.join("stage");
    file_with(&from.join("usr-is-not-here.conf"), "the new one");
    file_with(&stage.join("usr-is-not-here.conf"), "the old one");

    merge_usrmerge_tree(&from, &stage);

    assert_eq!(body(&stage.join("usr-is-not-here.conf")), "the new one");
    let _ = fs::remove_dir_all(&tmp);
}

/// An unpack that produced nothing merges to nothing, rather than taking the
/// build down. Every `.deb` goes through this, and one bad archive in a
/// closure of nine hundred is not a reason to stop.
#[test]
fn an_unpacked_package_that_is_not_there_merges_to_nothing() {
    let tmp = a_dir("missing-unpacked");
    let stage = tmp.join("stage");
    fs::create_dir_all(&stage).expect("a staging root");

    merge_usrmerge_tree(&tmp.join("never-unpacked"), &stage);

    assert_eq!(
        fs::read_dir(&stage).expect("a staging root").count(),
        0,
        "nothing was unpacked, so nothing is staged"
    );
    let _ = fs::remove_dir_all(&tmp);
}

/// Given a file rather than a directory, the copy makes the parent the
/// destination needs. The dpkg status file is copied this way and its
/// `var/lib/dpkg` does not exist yet on a fresh rootfs.
#[test]
fn a_copy_of_a_single_file_makes_the_parent_it_needs() {
    let tmp = a_dir("copy-one-file");
    file_with(&tmp.join("src/status"), "install ok installed");
    let dst = tmp.join("dst/var/lib/dpkg/status");

    copy_tree(&tmp.join("src/status"), &dst);

    assert_eq!(body(&dst), "install ok installed");
    let _ = fs::remove_dir_all(&tmp);
}

/// A tree is walked to the bottom, and a symlink inside it stays a symlink.
/// That is not cosmetic: a shared library is found through its soname link,
/// and a copy of the file under the link's name would double the size of
/// every library in the rootfs.
#[test]
fn a_copied_tree_keeps_its_symlinks_as_symlinks() {
    let tmp = a_dir("copy-tree");
    let from = tmp.join("src");
    file_with(&from.join("x86_64-linux-gnu/libz.so.1.3.1"), "zlib");
    link(&from.join("x86_64-linux-gnu/libz.so.1"), "libz.so.1.3.1");
    link(&from.join("libnotbuilt.so"), "../never/built.so");
    let to = tmp.join("dst/usr/lib");

    copy_tree(&from, &to);

    assert_eq!(body(&to.join("x86_64-linux-gnu/libz.so.1.3.1")), "zlib");
    let soname = to.join("x86_64-linux-gnu/libz.so.1");
    assert!(soname.is_symlink(), "the soname link became a second copy");
    assert_eq!(
        fs::read_link(&soname).expect("a target"),
        Path::new("libz.so.1.3.1")
    );

    let dangling = to.join("libnotbuilt.so");
    assert!(
        dangling.is_symlink(),
        "a target that is not there yet is still a target"
    );
    assert!(
        !dangling.exists(),
        "and following it would have copied nothing"
    );
    let _ = fs::remove_dir_all(&tmp);
}

/// A tree that is not there leaves no empty directory behind. An empty
/// `usr/libexec` in the rootfs is not harmless: the next question asked of
/// the staging root is whether a tree exists.
#[test]
fn a_copy_from_nowhere_leaves_no_empty_directory_behind() {
    let tmp = a_dir("copy-tree-missing");
    let to = tmp.join("dst/usr/libexec");

    copy_tree(&tmp.join("never-unpacked"), &to);

    assert!(
        !to.exists(),
        "an absent tree must not be announced as an empty one"
    );
    let _ = fs::remove_dir_all(&tmp);
}

/// The merge into the real rootfs takes the named trees and nothing else.
/// `bin` and `sbin` are deliberately not among them: busybox owns those
/// applets, and the shell the machine boots is not an apt package's to
/// replace. `etc` is not among them either, only named subtrees of it, so a
/// package's `/etc/passwd` cannot take the rootfs's own accounts with it.
#[test]
fn the_merge_takes_usr_and_leaves_the_busybox_applets_alone() {
    let tmp = a_dir("merge-stage");
    let stage = tmp.join("stage");
    let root = tmp.join("root");
    file_with(&stage.join("usr/bin/dpkg"), "ubuntu dpkg");
    file_with(&stage.join("usr/lib/x86_64-linux-gnu/libc.so.6"), "glibc");
    file_with(&stage.join("etc/dpkg/origins/ubuntu"), "Vendor: Ubuntu");
    file_with(&stage.join("var/lib/dpkg/status"), "install ok installed");
    file_with(&stage.join("lib/ld-linux-aarch64.so.1"), "the arm64 loader");
    file_with(&stage.join("bin/sh"), "ubuntu dash");
    file_with(&stage.join("etc/passwd"), "ubuntu:x:0:0::/root:/bin/sh");
    file_with(&root.join("bin/sh"), "busybox");
    file_with(&root.join("etc/passwd"), "root:x:0:0::/root:/bin/sh");

    merge_stage(&stage, &root);

    assert_eq!(body(&root.join("usr/bin/dpkg")), "ubuntu dpkg");
    assert_eq!(
        body(&root.join("usr/lib/x86_64-linux-gnu/libc.so.6")),
        "glibc"
    );
    assert_eq!(
        body(&root.join("etc/dpkg/origins/ubuntu")),
        "Vendor: Ubuntu"
    );
    assert_eq!(
        body(&root.join("var/lib/dpkg/status")),
        "install ok installed"
    );
    assert_eq!(
        body(&root.join("lib/ld-linux-aarch64.so.1")),
        "the arm64 loader",
        "arm64 names its loader at the top level and nowhere else"
    );
    assert_eq!(
        body(&root.join("bin/sh")),
        "busybox",
        "the shell the machine boots is not an apt package's to replace"
    );
    assert_eq!(
        body(&root.join("etc/passwd")),
        "root:x:0:0::/root:/bin/sh",
        "only the named etc subtrees are merged, never etc itself"
    );
    let _ = fs::remove_dir_all(&tmp);
}

/// And a tree the staging root never got is skipped, not created empty.
#[test]
fn a_tree_the_staging_root_never_got_is_skipped_rather_than_created() {
    let tmp = a_dir("merge-stage-sparse");
    let stage = tmp.join("stage");
    let root = tmp.join("root");
    file_with(&stage.join("usr/bin/apt"), "apt");
    fs::create_dir_all(&root).expect("a rootfs");

    merge_stage(&stage, &root);

    assert!(root.join("usr/bin/apt").is_file());
    for absent in ["usr/libexec", "usr/share", "var/cache/apt"] {
        assert!(
            !root.join(absent).exists(),
            "{absent} was invented out of nothing"
        );
    }
    let _ = fs::remove_dir_all(&tmp);
}

/// The handful of binaries copied one by one after the merge land
/// executable, and only if the staging root really has them. A 0644 copy is
/// the interesting case: `fs::copy` carries the source's mode over, and
/// `dpkg-deb -x` is not the only thing that ever wrote these files.
#[test]
fn a_binary_copied_one_by_one_lands_executable_and_only_if_it_is_there() {
    let tmp = a_dir("copy-one-bin");
    let stage = tmp.join("stage");
    let root = tmp.join("root");
    file_with(&stage.join("usr/bin/dpkg"), "ubuntu dpkg");
    fs::set_permissions(
        stage.join("usr/bin/dpkg"),
        fs::Permissions::from_mode(0o644),
    )
    .expect("a mode of its own");
    fs::create_dir_all(stage.join("bin/bash")).expect("a directory where a binary is looked for");

    copy_one_bin(&stage, &root, "usr/bin/dpkg");
    copy_one_bin(&stage, &root, "bin/bash");
    copy_one_bin(&stage, &root, "usr/bin/gpgv");

    let dpkg = root.join("usr/bin/dpkg");
    assert_eq!(body(&dpkg), "ubuntu dpkg");
    assert_eq!(
        fs::metadata(&dpkg)
            .expect("a copied binary")
            .permissions()
            .mode()
            & 0o777,
        0o755,
        "the mode comes with the copy, and a 0644 dpkg does not run"
    );
    assert!(
        !root.join("bin/bash").exists(),
        "a directory of that name is not a binary"
    );
    assert!(
        !root.join("usr/bin/gpgv").exists(),
        "and a package that was never downloaded leaves nothing"
    );
    let _ = fs::remove_dir_all(&tmp);
}
