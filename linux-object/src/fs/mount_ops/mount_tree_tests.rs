//! Which directory a mount point's filesystem is mounted on, and `umount`,
//! `mount --move` and `mount --bind` end to end, on a tree and a table built
//! here: the kernel's are one each for the whole process, and every test that
//! makes a process resets the table.

use super::super::{line_entry, new_mount_entry, record_mount, MountEntry};
use super::{bind_from, mount_holder_from, move_from, resolve_from, umount_from, MS_BIND};
use crate::error::LxError;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use lock::Mutex;
use rcore_fs::vfs::{FileSystem, FileType, FsError, FsInfo, INode};
use rcore_fs_mountfs::{MNode, MountFS};
use rcore_fs_ramfs::RamFS;

/// A ramfs holding one empty file called `name`.
fn fs_with(name: &str) -> Arc<dyn FileSystem> {
    let fs = RamFS::new();
    fs.root_inode().create(name, FileType::File, 0o644).unwrap();
    fs
}

/// A root with one directory, `/d`, in it.
fn root_and_d() -> (Arc<MNode>, Arc<MNode>) {
    let root = MountFS::new(RamFS::new()).mountpoint_root_inode();
    let d = root.create("d", FileType::Dir, 0o755).unwrap();
    (root, d)
}

#[test]
fn a_mount_is_held_by_the_directory_it_is_on_not_the_root_it_shows() {
    let (root, d) = root_and_d();
    d.mount(fs_with("x")).unwrap();
    let shown = resolve_from(&root, "/d").unwrap();
    assert!(shown.find(false, "x").is_ok());
    assert!(
        !shown.is_mountpoint(),
        "what a lookup lands on is the mounted root, which nothing is on"
    );
    let holder = mount_holder_from(&root, "//d/")
        .unwrap()
        .expect("no holder");
    assert!(holder.is_same_node(&d));
    assert!(holder.is_mountpoint());
}

#[test]
fn a_stack_is_held_by_the_root_under_its_top() {
    let (root, d) = root_and_d();
    d.mount(fs_with("first")).unwrap();
    resolve_from(&root, "/d")
        .unwrap()
        .mount(fs_with("second"))
        .unwrap();
    assert!(resolve_from(&root, "/d")
        .unwrap()
        .find(false, "second")
        .is_ok());
    let holder = mount_holder_from(&root, "/d").unwrap().unwrap();
    assert!(holder.find(false, "first").is_ok(), "held by the first one");
    holder.umount().unwrap();
    assert!(resolve_from(&root, "/d")
        .unwrap()
        .find(false, "first")
        .is_ok());
    let holder = mount_holder_from(&root, "/d").unwrap().unwrap();
    assert!(holder.is_same_node(&d));
}

#[test]
fn a_mount_inside_a_mount_is_held_inside_it() {
    let (root, d) = root_and_d();
    d.mount(RamFS::new()).unwrap();
    let inner = resolve_from(&root, "/d")
        .unwrap()
        .create("in", FileType::Dir, 0o755)
        .unwrap();
    assert!(mount_holder_from(&root, "/d/in").unwrap().is_none());
    inner.mount(fs_with("y")).unwrap();
    let holder = mount_holder_from(&root, "/d/in").unwrap().unwrap();
    assert!(holder.is_same_node(&inner));
    assert!(mount_holder_from(&root, "/d")
        .unwrap()
        .unwrap()
        .is_same_node(&d));
}

#[test]
fn nothing_holds_a_directory_nothing_is_mounted_on() {
    let (root, _d) = root_and_d();
    assert!(mount_holder_from(&root, "/d").unwrap().is_none());
    assert!(mount_holder_from(&root, "/").unwrap().is_none());
    assert_eq!(
        mount_holder_from(&root, "/nowhere").err(),
        Some(LxError::ENOENT)
    );
    assert_eq!(
        mount_holder_from(&root, "/nowhere/x").err(),
        Some(LxError::ENOENT)
    );
}

/// A root with the directories `/a`, `/b`, `/c` and `/p`, and a table that
/// lists the root.
struct World {
    root: Arc<MNode>,
    table: Mutex<Vec<MountEntry>>,
}

fn world() -> World {
    let root = MountFS::new(RamFS::new()).mountpoint_root_inode();
    for name in ["a", "b", "c", "p"].iter() {
        root.create(name, FileType::Dir, 0o755).unwrap();
    }
    let mut table = Vec::new();
    record_mount(
        &mut table,
        new_mount_entry("rootfs", "/", "rootfs", "rw", None),
    );
    World {
        root,
        table: Mutex::new(table),
    }
}

impl World {
    /// Mount `fs` at `at` as the kernel does, and list it as `source`.
    fn mount(&self, source: &str, at: &str, fs: Arc<dyn FileSystem>) {
        resolve_from(&self.root, at).unwrap().mount(fs).unwrap();
        let entry = new_mount_entry(source, at, "tmpfs", "rw", None);
        record_mount(&mut self.table.lock(), entry);
    }

    /// List a mount at `at` that nothing was mounted for.
    fn line(&self, source: &str, at: &str) {
        record_mount(
            &mut self.table.lock(),
            line_entry(source, at, "tmpfs", "rw"),
        );
    }

    fn shows(&self, dir: &str, name: &str) -> bool {
        resolve_from(&self.root, dir).is_ok_and(|d| d.find(false, name).is_ok())
    }

    /// The sources listed at `at`, bottom first.
    fn at(&self, at: &str) -> Vec<String> {
        let table = self.table.lock();
        table
            .iter()
            .filter(|m| m.target == at)
            .map(|m| m.source.clone())
            .collect()
    }
}

#[test]
fn a_bind_of_a_mount_shows_its_files_and_umount_takes_it_down() {
    let w = world();
    w.mount("srcA", "/a", fs_with("x"));
    bind_from(&w.root, &w.table, "/a", "/b", MS_BIND, "").unwrap();
    assert!(w.shows("/b", "x"));
    assert_eq!(w.at("/b"), ["srcA"], "the line names the origin's source");

    umount_from(&w.root, &w.table, "/b", false).unwrap();
    assert!(!w.shows("/b", "x"), "the directory still shows the bind");
    assert!(w.at("/b").is_empty());
    assert!(w.shows("/a", "x"), "the origin went with it");
    assert_eq!(w.at("/a"), ["srcA"]);
}

#[test]
fn a_mount_on_a_mount_goes_on_top_and_umount_uncovers_the_one_below() {
    let w = world();
    w.mount("first", "/a", fs_with("x"));
    w.mount("second", "/c", fs_with("y"));
    bind_from(&w.root, &w.table, "/c", "/a", MS_BIND, "").unwrap();
    assert!(w.shows("/a", "y") && !w.shows("/a", "x"));
    assert_eq!(w.at("/a"), ["first", "second"]);
    {
        let table = w.table.lock();
        let at_a: Vec<&MountEntry> = table.iter().filter(|m| m.target == "/a").collect();
        assert_eq!(
            at_a[1].parent, at_a[0].id,
            "the top one hangs from the one below"
        );
    }

    umount_from(&w.root, &w.table, "/a", false).unwrap();
    assert!(w.shows("/a", "x") && !w.shows("/a", "y"));
    assert_eq!(w.at("/a"), ["first"]);
    umount_from(&w.root, &w.table, "/a", false).unwrap();
    assert!(!w.shows("/a", "x"));
    assert!(w.at("/a").is_empty());
    assert!(w.shows("/c", "y"), "a bind's origin stays where it was");
}

/// A filesystem whose device is gone by the time it is synced.
struct Unsyncable(Arc<dyn FileSystem>);

impl FileSystem for Unsyncable {
    fn sync(&self) -> rcore_fs::vfs::Result<()> {
        Err(FsError::DeviceError)
    }
    fn root_inode(&self) -> Arc<dyn INode> {
        self.0.root_inode()
    }
    fn info(&self) -> FsInfo {
        self.0.info()
    }
}

#[test]
fn an_umount_that_cannot_write_out_leaves_the_mount_and_its_line() {
    let w = world();
    w.mount("disk", "/a", Arc::new(Unsyncable(fs_with("x"))));
    assert_eq!(
        umount_from(&w.root, &w.table, "/a", false),
        Err(LxError::EIO)
    );
    assert!(w.shows("/a", "x"));
    assert_eq!(w.at("/a"), ["disk"], "the line went and the mount stayed");
}

/// `/a` with a directory `sub` in it and a mount on that.
fn nested() -> World {
    let w = world();
    w.mount("outer", "/a", RamFS::new());
    resolve_from(&w.root, "/a")
        .unwrap()
        .create("sub", FileType::Dir, 0o755)
        .unwrap();
    w.mount("inner", "/a/sub", fs_with("z"));
    w
}

#[test]
fn an_umount_with_a_mount_under_it_is_ebusy_and_a_lazy_one_takes_both() {
    let w = nested();
    assert_eq!(
        umount_from(&w.root, &w.table, "/a", false),
        Err(LxError::EBUSY)
    );
    assert!(
        w.shows("/a/sub", "z"),
        "a refused umount took something down"
    );
    assert_eq!(w.at("/a"), ["outer"]);

    umount_from(&w.root, &w.table, "/a", true).unwrap();
    assert!(!w.shows("/a", "sub"));
    assert!(w.at("/a").is_empty() && w.at("/a/sub").is_empty());
}

#[test]
fn a_move_takes_the_mount_and_what_is_inside_it() {
    let w = nested();
    move_from(&w.root, &w.table, "/a", "/b").unwrap();
    assert!(w.shows("/b/sub", "z"), "what was inside it stayed behind");
    assert!(!w.shows("/a", "sub"));
    assert_eq!(
        (w.at("/b"), w.at("/b/sub")),
        (alloc::vec!["outer".into()], alloc::vec!["inner".into()])
    );
    assert!(w.at("/a").is_empty() && w.at("/a/sub").is_empty());
    let holder = mount_holder_from(&w.root, "/b/sub")
        .unwrap()
        .expect("no holder");
    assert!(holder.is_mountpoint());
}

#[test]
fn a_mount_that_is_only_a_line_goes_as_a_line() {
    let w = world();
    w.mount("kernel", "/a", fs_with("x"));
    w.line("init", "/a");
    // A move takes the line on top and nothing of the mount under it.
    move_from(&w.root, &w.table, "/a", "/c").unwrap();
    assert_eq!(w.at("/a"), ["kernel"]);
    assert_eq!(w.at("/c"), ["init"]);
    assert!(w.shows("/a", "x") && !w.shows("/c", "x"));
    umount_from(&w.root, &w.table, "/c", false).unwrap();
    assert!(w.at("/c").is_empty());
    // And so does an umount.
    w.line("init", "/a");
    umount_from(&w.root, &w.table, "/a", false).unwrap();
    assert_eq!(w.at("/a"), ["kernel"]);
    assert!(
        w.shows("/a", "x"),
        "the line took the kernel's mount with it"
    );

    w.line("none", "/p");
    move_from(&w.root, &w.table, "/p", "/c").unwrap();
    assert!(w.at("/p").is_empty());
    assert_eq!(w.at("/c"), ["none"]);
    umount_from(&w.root, &w.table, "/c", false).unwrap();
    assert!(w.at("/c").is_empty());
}

#[test]
fn what_is_not_a_mount_cannot_be_unmounted_moved_or_bound() {
    let w = world();
    w.mount("srcA", "/a", RamFS::new());
    let inside = resolve_from(&w.root, "/a")
        .unwrap()
        .create("in", FileType::Dir, 0o755)
        .unwrap();
    drop(inside);
    for path in ["/p", "/a/in"].iter() {
        assert_eq!(
            umount_from(&w.root, &w.table, path, false),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            move_from(&w.root, &w.table, path, "/c"),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            bind_from(&w.root, &w.table, path, "/c", MS_BIND, ""),
            Err(LxError::EINVAL)
        );
    }
    assert_eq!(
        umount_from(&w.root, &w.table, "/", true),
        Err(LxError::EBUSY)
    );
    for path in ["/nowhere", "/nowhere/x"].iter() {
        assert_eq!(
            umount_from(&w.root, &w.table, path, false),
            Err(LxError::ENOENT)
        );
        assert_eq!(
            move_from(&w.root, &w.table, path, "/c"),
            Err(LxError::ENOENT)
        );
        assert_eq!(
            bind_from(&w.root, &w.table, path, "/c", MS_BIND, ""),
            Err(LxError::ENOENT)
        );
    }
    // A mount the table does not list is not moved either: the tree would
    // be left saying one thing and the table another.
    resolve_from(&w.root, "/b")
        .unwrap()
        .mount(fs_with("y"))
        .unwrap();
    assert_eq!(
        move_from(&w.root, &w.table, "/b", "/c"),
        Err(LxError::EINVAL)
    );
    assert!(w.shows("/b", "y") && !w.shows("/c", "y"));
    assert!(w.shows("/a", "in"));
    assert_eq!(w.at("/a"), ["srcA"]);
    assert!(w.at("/c").is_empty());
}
