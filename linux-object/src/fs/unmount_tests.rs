//! What `umount` and `mount --move` do to the table, on tables built here:
//! the live one is cleared by every test that makes a process.

use super::*;
use alloc::vec::Vec;

fn table(mounts: &[(&str, &str)]) -> Vec<MountEntry> {
    let mut t = Vec::new();
    for (source, target) in mounts {
        record_mount(&mut t, new_mount_entry(source, target, "tmpfs", "rw", None));
    }
    t
}

fn targets(t: &[MountEntry]) -> Vec<&str> {
    t.iter().map(|m| m.target.as_str()).collect()
}

#[test]
fn an_umount_takes_the_top_one_and_leaves_the_one_under_it() {
    // Linux 6.18: a bind of `/dev` over a bind of `/tmp` on one directory,
    // and one `umount` uncovered `/tmp`'s. Here init's `mount -t tmpfs tmpfs
    // /tmp` is a line over the kernel's own mount.
    let mut t = table(&[("rootfs", "/"), ("kernel", "/tmp"), ("init", "/tmp")]);
    t[2].backed = false;
    let plan = unmount_in(&t, "/tmp/", false).unwrap();
    assert_eq!(plan.ids, [t[2].id]);
    assert!(!plan.backed, "a line only: there is no tree to touch");
    t.retain(|m| !plan.ids.contains(&m.id));
    let plan = unmount_in(&t, "/tmp", false).unwrap();
    assert_eq!(plan.ids, [t[1].id]);
    assert!(plan.backed);
}

#[test]
fn an_umount_of_a_mount_with_others_on_it_is_ebusy_unless_lazy() {
    // Linux 6.18: `umount` of a bind with a bind under it was `EBUSY`, and
    // `umount -l` took both lines.
    let t = table(&[
        ("rootfs", "/"),
        ("a", "/a"),
        ("b", "/a/b"),
        ("c", "/a/b/c"),
        ("ab", "/ab"),
    ]);
    assert_eq!(unmount_in(&t, "/a", false).err(), Some(LxError::EBUSY));
    let mut ids = unmount_in(&t, "/a", true).unwrap().ids;
    ids.sort_unstable();
    assert_eq!(
        ids,
        [t[1].id, t[2].id, t[3].id],
        "every one under it, however deep, and not /ab"
    );
    assert_eq!(unmount_in(&t, "/a/b/c", false).unwrap().ids, [t[3].id]);
}

#[test]
fn an_umount_of_the_root_is_ebusy_and_of_no_mount_einval() {
    let t = table(&[("rootfs", "/"), ("a", "/a")]);
    assert_eq!(unmount_in(&t, "/", true).err(), Some(LxError::EBUSY));
    assert_eq!(unmount_in(&t, "/a/x", false).err(), Some(LxError::EINVAL));
    assert_eq!(unmount_in(&[], "/", false).err(), Some(LxError::EBUSY));
}

#[test]
fn a_moved_mount_takes_the_ones_under_it_along() {
    // Linux 6.18: a bind of `/dev` under a bind of `/tmp` came along when the
    // bind of `/tmp` was moved, and hung from it at its new place.
    let mut t = table(&[
        ("rootfs", "/"),
        ("b", "/run/b"),
        ("d", "/run/b/d"),
        ("e", "/run/b/d/e"),
        ("c", "/run/c"),
    ]);
    let (b, d, c) = (t[1].id, t[2].id, t[4].id);
    move_entry(&mut t, "/run/b", "/run/c/x/").unwrap();
    assert_eq!(
        targets(&t),
        ["/", "/run/c/x", "/run/c/x/d", "/run/c/x/d/e", "/run/c"]
    );
    assert_eq!((t[1].parent, t[2].parent, t[3].parent), (c, b, d));
}

#[test]
fn a_move_leaves_what_hangs_from_the_mount_it_covered() {
    // The top one at `/a` covers `/a/b`, which is on the one under it, so it
    // stays where it is.
    let mut t = table(&[("rootfs", "/"), ("low", "/a"), ("b", "/a/b"), ("top", "/a")]);
    move_entry(&mut t, "/a", "/m").unwrap();
    assert_eq!(targets(&t), ["/", "/a", "/a/b", "/m"]);
}
