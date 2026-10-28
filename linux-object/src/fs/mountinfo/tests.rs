//! The two mount files against what Linux 6.18 printed for the same mounts,
//! made in a private namespace with `unshare -m`.
//!
//! Every table here is built locally: the live one is cleared by every test
//! that makes a process, so nothing can be asserted about its content.

use super::super::mount_state::{build_options_string, MS_BIND, MS_RDONLY};
use super::super::{move_entry, new_mount_entry, origin_in, record_mount};
use super::*;
use crate::error::LxError;
use alloc::vec::Vec;
use rcore_fs_ramfs::RamFS;

/// A table built by `record_mount`, as `register_mount` builds the live one.
fn table(mounts: &[(&str, &str, &str, &str)]) -> Vec<MountEntry> {
    let mut t = Vec::new();
    for (source, target, fstype, options) in mounts {
        record_mount(
            &mut t,
            new_mount_entry(source, target, fstype, options, None),
        );
    }
    t
}

fn lines(text: &str) -> Vec<&str> {
    text.lines().collect()
}

#[test]
fn a_line_is_the_one_linux_prints_for_the_same_mount() {
    let t = table(&[
        ("/dev/vda", "/", "ext4", "rw,relatime,discard"),
        (
            "src1",
            "/tmp/p55/a b",
            "tmpfs",
            "rw,nosuid,nodev,noexec,relatime,size=1024k,mode=700",
        ),
        ("tmpfs", "/tmp/p55/t\tx\\y", "tmpfs", "ro,noatime"),
        ("tmpfs", "/tmp/p55/c", "tmpfs", "rw,sync,dirsync,relatime"),
        (
            "my src",
            "/tmp/mnt a",
            "tmpfs",
            "rw,nodev,relatime,size=1024k",
        ),
    ]);
    let dev = |target: &str| match target {
        "/" => 65024,
        "/tmp/p55/a b" => 42,
        "/tmp/p55/t\tx\\y" => 43,
        "/tmp/p55/c" => 44,
        _ => 45,
    };
    let root = t[0].id;
    assert_eq!(
        lines(&render(&t, dev)),
        [
            alloc::format!("{root} 1 254:0 / / rw,relatime - ext4 /dev/vda rw,discard"),
            alloc::format!(
                "{} {root} 0:42 / /tmp/p55/a\\040b rw,nosuid,nodev,noexec,relatime \
                 - tmpfs src1 rw,size=1024k,mode=700",
                t[1].id
            ),
            alloc::format!(
                "{} {root} 0:43 / /tmp/p55/t\\011x\\134y ro,noatime - tmpfs tmpfs ro",
                t[2].id
            ),
            alloc::format!(
                "{} {root} 0:44 / /tmp/p55/c rw,relatime - tmpfs tmpfs rw,sync,dirsync",
                t[3].id
            ),
            alloc::format!(
                "{} {root} 0:45 / /tmp/mnt\\040a rw,nodev,relatime - tmpfs my\\040src rw,size=1024k",
                t[4].id
            ),
        ]
    );
    assert_eq!(
        lines(&mounts_text(&t)),
        [
            "/dev/vda / ext4 rw,relatime,discard 0 0",
            "src1 /tmp/p55/a\\040b tmpfs rw,nosuid,nodev,noexec,relatime,size=1024k,mode=700 0 0",
            "tmpfs /tmp/p55/t\\011x\\134y tmpfs ro,noatime 0 0",
            "tmpfs /tmp/p55/c tmpfs rw,sync,dirsync,relatime 0 0",
            "my\\040src /tmp/mnt\\040a tmpfs rw,nodev,relatime,size=1024k 0 0",
        ]
    );
}

#[test]
fn an_option_with_a_space_cannot_split_the_line() {
    // Not measured: Linux refuses an option tmpfs does not know. This kernel
    // records the string it was given, so a space in it is escaped as one in
    // a name is (`seq_show_option` escapes an option's value the same way).
    let t = table(&[("none", "/mnt", "tmpfs", "rw,x=a b")]);
    assert_eq!(
        lines(&mounts_text(&t)),
        ["none /mnt tmpfs rw,x=a\\040b 0 0"]
    );
    let info = render(&t, |_| 0);
    assert!(info.ends_with(" - tmpfs none rw,x=a\\040b\n"), "{}", info);
}

#[test]
fn the_mount_s_own_flags_and_the_filesystem_s_options_are_two_fields() {
    assert_eq!(
        split_options("rw"),
        ("rw".into(), "rw".into()),
        "with nothing else, both say how it is open"
    );
    assert_eq!(
        split_options("rw,nosuid,nodev,noexec,noatime,nodiratime,relatime,nosymfollow"),
        (
            "rw,nosuid,nodev,noexec,noatime,nodiratime,relatime,nosymfollow".into(),
            "rw".into()
        )
    );
    assert_eq!(
        split_options("ro,size=1k,nosuid,lazytime"),
        ("ro,nosuid".into(), "ro,size=1k,lazytime".into()),
        "ro reaches both halves wherever it is"
    );
    assert_eq!(split_options("nodev,ro"), ("ro,nodev".into(), "ro".into()));
}

#[test]
fn a_mount_goes_on_the_top_mount_at_the_longest_mount_point_that_holds_it() {
    let t = table(&[
        ("rootfs", "/", "rootfs", "rw"),
        ("devfs", "/dev", "devtmpfs", "rw"),
        ("tmpfs", "/dev/shm", "tmpfs", "rw"),
        ("proc", "/proc", "proc", "rw"),
        // init's `mount -t proc proc /proc`, over the kernel's.
        ("proc", "/proc", "proc", "rw"),
        ("tmpfs", "/devx", "tmpfs", "rw"),
        ("tmpfs", "/proc/x", "tmpfs", "rw"),
    ]);
    let parents: Vec<usize> = t.iter().map(|m| m.parent).collect();
    assert_eq!(
        parents,
        [
            ROOT_PARENT,
            t[0].id,
            t[1].id,
            t[0].id,
            t[3].id,
            t[0].id,
            t[4].id
        ]
    );
    let ids: Vec<usize> = t.iter().map(|m| m.id).collect();
    assert!(
        ids.windows(2).all(|w| w[0] < w[1]),
        "each mount a new number, in order: {:?}",
        ids
    );
    assert!(ids[0] > ROOT_PARENT);
    assert_eq!(parent_of("/", [].iter()), ROOT_PARENT);
}

#[test]
fn a_moved_mount_hangs_from_what_is_at_its_new_place() {
    let mut t = table(&[
        ("rootfs", "/", "rootfs", "rw"),
        ("tmpfs", "/mnt", "tmpfs", "rw"),
        ("tmpfs", "/a", "tmpfs", "rw"),
    ]);
    let (root, mnt, a) = (t[0].id, t[1].id, t[2].id);
    assert_eq!(t[2].parent, root);
    move_entry(&mut t, "/a/", "/mnt/a").unwrap();
    assert_eq!((t[2].id, t[2].target.as_str()), (a, "/mnt/a"));
    assert_eq!(t[2].parent, mnt);
    // Never on itself, even where nothing else is.
    move_entry(&mut t, "/mnt/a", "/mnt/a").unwrap();
    assert_eq!(t[2].parent, mnt);
    assert_eq!(move_entry(&mut t, "/nowhere", "/b"), Err(LxError::EINVAL));
}

#[test]
fn escape_is_mangle() {
    assert_eq!(escape("my src"), "my\\040src");
    assert_eq!(escape("/tmp/p55/t\tx\\y"), "/tmp/p55/t\\011x\\134y");
    assert_eq!(escape("a\nb"), "a\\012b");
    assert_eq!(escape("size=1024k,mode=700"), "size=1024k,mode=700");
    assert_eq!(escape("/mnt/año"), "/mnt/año");
}

#[test]
fn the_device_is_taken_apart_as_glibc_takes_it() {
    // `stat -c %d /` and its line, `254:0`, on the machine measured.
    assert_eq!(major_minor(65024), (254, 0));
    assert_eq!(major_minor(42), (0, 42));
    assert_eq!(major_minor(0x801), (8, 1));
    // `os.makedev(0x12345, 0x6789a)`: both halves past their old 8 bits.
    assert_eq!(major_minor(316_661_085_455_770), (0x12345, 0x6789a));
}

#[test]
fn the_device_is_what_stat_says_at_the_mount_point() {
    let proc = LinuxProcess::new(RamFS::new(), 0);
    assert_eq!(dev_at(Some(&proc), "/sys"), 2, "sysfs's own");
    assert_eq!(dev_at(Some(&proc), "/nowhere"), 0);
    assert_eq!(dev_at(None, "/sys"), 0);
    let t = table(&[("sysfs", "/sys", "sysfs", "rw")]);
    assert!(
        render(&t, |target| dev_at(Some(&proc), target)).contains(" 0:2 / /sys "),
        "{}",
        render(&t, |target| dev_at(Some(&proc), target))
    );
}

#[test]
fn a_bind_shows_the_source_and_type_of_what_it_took() {
    // The top one at the directory, as Linux binds it: `mount --bind
    // /tmp/p55/c /tmp/p55/e` over two mounts there showed `tmpfs2`.
    let t = table(&[
        ("tmpfs", "/tmp/p55/c", "tmpfs", "rw"),
        ("tmpfs2", "/tmp/p55/c", "tmpfs", "rw,nosuid"),
    ]);
    // And its options, which are the bind's (SURVEY36 on Linux 6.18).
    assert_eq!(
        origin_in(&t, "/tmp/p55/c/"),
        Some((
            "tmpfs2".into(),
            "tmpfs".into(),
            "/".into(),
            "rw,nosuid".into()
        ))
    );
    assert_eq!(origin_in(&t, "/tmp/p55"), None);
    // A directory inside it: the same mount, rooted at the directory.
    assert_eq!(
        origin_in(&t, "/tmp/p55/c/d/e"),
        Some((
            "tmpfs2".into(),
            "tmpfs".into(),
            "/d/e".into(),
            "rw,nosuid".into()
        ))
    );
    assert_eq!(origin_in(&t, "/tmp/p55/cd"), None, "not inside /tmp/p55/c");
    // And no word of its own: `bind` is what the call did.
    assert_eq!(build_options_string(MS_BIND | MS_RDONLY, ""), "ro");
    assert_eq!(build_options_string(MS_BIND, "nosuid"), "rw,nosuid");
}

#[test]
fn a_bind_of_a_directory_inside_a_bind_has_the_two_roots_joined() {
    // SURVEY34 on Linux 6.18: `/tmp/s59d` bound at `/run/s59/b`, then
    // `/run/s59/b/x` bound at `/run/s59/c`.
    let mut t = table(&[
        ("/dev/vda", "/", "ext4", "rw"),
        ("s59a", "/tmp", "tmpfs", "rw"),
    ]);
    assert_eq!(
        origin_in(&t, "/etc/x"),
        Some((
            "/dev/vda".into(),
            "ext4".into(),
            "/etc/x".into(),
            "rw".into()
        ))
    );
    assert_eq!(origin_in(&t, "/").unwrap().2, "/");
    let (source, fstype, root, _) = origin_in(&t, "/tmp/s59d").unwrap();
    assert_eq!((source.as_str(), root.as_str()), ("s59a", "/s59d"));
    let bind = MountEntry {
        root,
        ..new_mount_entry(&source, "/run/s59/b", &fstype, "rw", None)
    };
    record_mount(&mut t, bind);
    assert_eq!(
        origin_in(&t, "/run/s59/b/x"),
        Some(("s59a".into(), "tmpfs".into(), "/s59d/x".into(), "rw".into()))
    );
    assert_eq!(origin_in(&t, "/run/s59/b").unwrap().2, "/s59d");
    // A move takes the root with it.
    move_entry(&mut t, "/run/s59/b", "/run/s59/c").unwrap();
    assert_eq!(t[2].root, "/s59d");
    assert!(render(&t, |_| 0)
        .lines()
        .any(|l| l.contains(" 0:0 /s59d /run/s59/c rw - tmpfs s59a rw")));
}

#[test]
fn a_root_with_a_space_in_it_cannot_split_the_line() {
    let mut t = table(&[("tmpfs", "/tmp", "tmpfs", "rw")]);
    t[0].root = "/a b".into();
    assert!(
        render(&t, |_| 0).contains(" /a\\040b /tmp "),
        "{}",
        render(&t, |_| 0)
    );
}
