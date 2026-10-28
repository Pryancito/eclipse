//! Which directory a mount point's filesystem is mounted on, and `umount`,
//! `mount --move` and `mount --bind` end to end, on a tree and a table built
//! here: the kernel's are one each for the whole process, and every test that
//! makes a process resets the table.

use super::super::mount_state::{MS_BIND, MS_RDONLY, MS_REMOUNT};
use super::super::{line_entry, new_mount_entry, record_mount, remount_entry, MountEntry};
use super::{
    bind_from, mount_holder_from, move_from, resolve_from, umount_from, MNT_DETACH, MNT_FORCE,
};
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
        self.mount_as(source, at, fs, "rw");
    }

    /// [`Self::mount`], with `options` in its line and its flags.
    fn mount_as(&self, source: &str, at: &str, fs: Arc<dyn FileSystem>, options: &str) {
        resolve_from(&self.root, at).unwrap().mount(fs).unwrap();
        let entry = new_mount_entry(source, at, "tmpfs", options, None);
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
    bind_from(&w.root, &w.table, "/a", "/b").unwrap();
    assert!(w.shows("/b", "x"));
    assert_eq!(w.at("/b"), ["srcA"], "the line names the origin's source");

    umount_from(&w.root, &w.table, "/b", 0).unwrap();
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
    bind_from(&w.root, &w.table, "/c", "/a").unwrap();
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

    umount_from(&w.root, &w.table, "/a", 0).unwrap();
    assert!(w.shows("/a", "x") && !w.shows("/a", "y"));
    assert_eq!(w.at("/a"), ["first"]);
    umount_from(&w.root, &w.table, "/a", 0).unwrap();
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
    assert_eq!(umount_from(&w.root, &w.table, "/a", 0), Err(LxError::EIO));
    assert!(w.shows("/a", "x"));
    assert_eq!(w.at("/a"), ["disk"], "the line went and the mount stayed");
    // `umount -f` takes it down all the same, as Linux takes it without.
    umount_from(&w.root, &w.table, "/a", MNT_FORCE).unwrap();
    assert!(!w.shows("/a", "x"));
    assert!(w.at("/a").is_empty());
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
    assert_eq!(umount_from(&w.root, &w.table, "/a", 0), Err(LxError::EBUSY));
    assert!(
        w.shows("/a/sub", "z"),
        "a refused umount took something down"
    );
    assert_eq!(w.at("/a"), ["outer"]);

    umount_from(&w.root, &w.table, "/a", MNT_DETACH).unwrap();
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
    umount_from(&w.root, &w.table, "/c", 0).unwrap();
    assert!(w.at("/c").is_empty());
    // And so does an umount.
    w.line("init", "/a");
    umount_from(&w.root, &w.table, "/a", 0).unwrap();
    assert_eq!(w.at("/a"), ["kernel"]);
    assert!(
        w.shows("/a", "x"),
        "the line took the kernel's mount with it"
    );

    w.line("none", "/p");
    move_from(&w.root, &w.table, "/p", "/c").unwrap();
    assert!(w.at("/p").is_empty());
    assert_eq!(w.at("/c"), ["none"]);
    umount_from(&w.root, &w.table, "/c", 0).unwrap();
    assert!(w.at("/c").is_empty());
}

#[test]
fn what_is_not_a_mount_cannot_be_unmounted_or_moved() {
    let w = world();
    w.mount("srcA", "/a", RamFS::new());
    let inside = resolve_from(&w.root, "/a")
        .unwrap()
        .create("in", FileType::Dir, 0o755)
        .unwrap();
    drop(inside);
    for path in ["/p", "/a/in"].iter() {
        assert_eq!(
            umount_from(&w.root, &w.table, path, 0),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            move_from(&w.root, &w.table, path, "/c"),
            Err(LxError::EINVAL)
        );
    }
    assert_eq!(
        umount_from(&w.root, &w.table, "/", MNT_DETACH),
        Err(LxError::EBUSY)
    );
    for path in ["/nowhere", "/nowhere/x"].iter() {
        assert_eq!(
            umount_from(&w.root, &w.table, path, 0),
            Err(LxError::ENOENT)
        );
        assert_eq!(
            move_from(&w.root, &w.table, path, "/c"),
            Err(LxError::ENOENT)
        );
        assert_eq!(
            bind_from(&w.root, &w.table, path, "/c"),
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

/// The line at `at`, the top one.
fn line_at(w: &World, at: &str) -> MountEntry {
    let table = w.table.lock();
    table.iter().rev().find(|m| m.target == at).unwrap().clone()
}

/// `/a` mounted with a file `out` and a directory `d` holding `x` and the
/// directory `e`.
fn with_d() -> World {
    let w = world();
    w.mount("srcA", "/a", fs_with("out"));
    let d = resolve_from(&w.root, "/a")
        .unwrap()
        .create("d", FileType::Dir, 0o755)
        .unwrap();
    d.create("x", FileType::File, 0o644).unwrap();
    d.create("e", FileType::Dir, 0o755).unwrap();
    w
}

#[test]
fn a_bind_of_a_plain_directory_shows_it_and_nothing_else_of_its_mount() {
    let w = with_d();
    bind_from(&w.root, &w.table, "/a/d", "/b").unwrap();
    assert!(w.shows("/b", "x") && w.shows("/b", "e"));
    assert!(!w.shows("/b", "out"), "it shows the whole mount");
    let line = line_at(&w, "/b");
    assert_eq!(
        (
            line.source.as_str(),
            line.fstype.as_str(),
            line.root.as_str()
        ),
        ("srcA", "tmpfs", "/d")
    );
    // One directory, two paths: what is made through one is in the other.
    resolve_from(&w.root, "/b")
        .unwrap()
        .create("viab", FileType::File, 0o644)
        .unwrap();
    assert!(w.shows("/a/d", "viab"));
    resolve_from(&w.root, "/a/d/e")
        .unwrap()
        .create("made", FileType::File, 0o644)
        .unwrap();
    assert!(w.shows("/b/e", "made"));
}

#[test]
fn dot_dot_from_a_bind_of_a_directory_is_where_it_is_mounted() {
    let w = with_d();
    bind_from(&w.root, &w.table, "/a/d", "/b").unwrap();
    for up in ["/b/..", "/b/e/../.."].iter() {
        let at = resolve_from(&w.root, up).unwrap();
        assert!(at.is_same_node(&w.root), "{} is not /", up);
    }
    let b = resolve_from(&w.root, "/b").unwrap();
    assert_eq!(w.root.find_name_by_child(&b).unwrap(), "b");
    let e = resolve_from(&w.root, "/b/e").unwrap();
    assert_eq!(b.find_name_by_child(&e).unwrap(), "e");
}

#[test]
fn a_bind_of_a_directory_inside_a_bind_has_the_two_roots_joined() {
    let w = with_d();
    bind_from(&w.root, &w.table, "/a/d", "/b").unwrap();
    bind_from(&w.root, &w.table, "/b/e", "/c").unwrap();
    let line = line_at(&w, "/c");
    assert_eq!((line.source.as_str(), line.root.as_str()), ("srcA", "/d/e"));
    resolve_from(&w.root, "/c")
        .unwrap()
        .create("deep", FileType::File, 0o644)
        .unwrap();
    assert!(w.shows("/a/d/e", "deep"));
}

#[test]
fn a_plain_bind_does_not_carry_the_mounts_inside_the_directory() {
    let w = with_d();
    w.mount("inner", "/a/d/e", fs_with("z"));
    bind_from(&w.root, &w.table, "/a/d", "/b").unwrap();
    assert!(w.shows("/b", "e"));
    assert!(!w.shows("/b/e", "z"), "the mount at /a/d/e came along");
    assert!(w.shows("/a/d/e", "z"));
}

#[test]
fn umount_of_a_bind_of_a_directory_leaves_the_directory() {
    let w = with_d();
    bind_from(&w.root, &w.table, "/a/d", "/b").unwrap();
    umount_from(&w.root, &w.table, "/b", 0).unwrap();
    assert!(!w.shows("/b", "x"));
    assert!(w.at("/b").is_empty());
    assert!(w.shows("/a/d", "x") && w.shows("/a", "out"));
}

#[test]
fn a_moved_bind_of_a_directory_keeps_its_root() {
    let w = with_d();
    bind_from(&w.root, &w.table, "/a/d", "/b").unwrap();
    move_from(&w.root, &w.table, "/b", "/c").unwrap();
    assert!(w.shows("/c", "x") && !w.shows("/b", "x"));
    assert_eq!(line_at(&w, "/c").root, "/d");
}

#[test]
fn the_root_itself_can_be_bound_and_shows_nothing_mounted_in_it() {
    let w = with_d();
    bind_from(&w.root, &w.table, "/", "/p").unwrap();
    assert!(w.shows("/p", "a") && w.shows("/p", "b"));
    assert!(!w.shows("/p/a", "out"), "the mount at /a came along");
    let line = line_at(&w, "/p");
    assert_eq!((line.source.as_str(), line.root.as_str()), ("rootfs", "/"));
    assert!(resolve_from(&w.root, "/p/p/..")
        .unwrap()
        .is_same_node(&resolve_from(&w.root, "/p").unwrap()));
}

#[test]
fn a_directory_bound_onto_itself_becomes_a_mount_point_and_shows_the_same() {
    // `mount --bind dir dir`, the usual way to make a directory a mount point.
    let w = with_d();
    bind_from(&w.root, &w.table, "/a/d", "/a/d").unwrap();
    assert!(w.shows("/a/d", "x"));
    assert!(mount_holder_from(&w.root, "/a/d").unwrap().is_some());
    assert_eq!(line_at(&w, "/a/d").root, "/d");
    assert!(resolve_from(&w.root, "/a/d/..")
        .unwrap()
        .find(false, "out")
        .is_ok());
    umount_from(&w.root, &w.table, "/a/d", 0).unwrap();
    assert!(w.shows("/a/d", "x"));
    assert!(mount_holder_from(&w.root, "/a/d").unwrap().is_none());
}

#[test]
fn a_bind_needs_a_directory_on_both_sides() {
    let w = with_d();
    let f = resolve_from(&w.root, "/p")
        .unwrap()
        .create("f", FileType::File, 0o644)
        .unwrap();
    drop(f);
    assert_eq!(
        bind_from(&w.root, &w.table, "/a/d/x", "/b"),
        Err(LxError::ENOTDIR),
        "a file bound onto a directory"
    );
    assert_eq!(
        bind_from(&w.root, &w.table, "/a/d", "/p/f"),
        Err(LxError::ENOTDIR),
        "a directory bound onto a file"
    );
    assert_eq!(
        bind_from(&w.root, &w.table, "/a/d/nowhere", "/b"),
        Err(LxError::ENOENT)
    );
    assert!(!w.shows("/b", "x"));
    assert!(w.at("/b").is_empty() && w.at("/p/f").is_empty());
}

/// `/a` mounted with `options`, with the directory `d` in it.
fn with_d_as(options: &str) -> World {
    let w = world();
    w.mount_as("srcA", "/a", fs_with("out"), options);
    resolve_from(&w.root, "/a")
        .unwrap()
        .create("d", FileType::Dir, 0o755)
        .unwrap();
    w
}

/// SURVEY36 on Linux 6.18: a bind of a directory in a `nosuid,nodev,noexec`
/// tmpfs is all three, and its line has that mount's options.
#[test]
fn a_bind_is_what_the_mount_it_is_taken_from_is() {
    let w = with_d_as("rw,nosuid,nodev,noexec,size=1m");
    bind_from(&w.root, &w.table, "/a/d", "/b").unwrap();
    let line = line_at(&w, "/b");
    assert_eq!(line.options, "rw,nosuid,nodev,noexec,size=1m");
    assert!(line.state.is_nosuid(), "nosuid");
    assert!(line.state.is_nodev(), "nodev");
    assert!(line.state.is_noexec(), "noexec");
    assert!(!line.state.is_read_only(), "read-only");
}

#[test]
fn a_bind_of_a_read_only_mount_cannot_be_written_through() {
    let w = with_d_as("ro");
    bind_from(&w.root, &w.table, "/a/d", "/b").unwrap();
    assert_eq!(line_at(&w, "/b").options, "ro");
    let b = resolve_from(&w.root, "/b").unwrap();
    assert_eq!(
        b.create("new", FileType::File, 0o644).err(),
        Some(FsError::ReadOnly)
    );
}

/// The bind has flags of its own: `mount -o remount,bind,ro` on it leaves
/// the mount it was taken from as it was.
#[test]
fn a_remount_of_a_bind_changes_the_bind_and_not_its_mount() {
    let w = with_d_as("rw,nosuid");
    bind_from(&w.root, &w.table, "/a/d", "/b").unwrap();
    remount_entry(
        &mut w.table.lock(),
        "/b",
        MS_REMOUNT | MS_BIND | MS_RDONLY,
        "",
    )
    .unwrap();
    assert!(line_at(&w, "/b").state.is_read_only());
    let a = line_at(&w, "/a");
    assert!(!a.state.is_read_only(), "the remount reached the mount");
    assert_eq!(a.options, "rw,nosuid");
    let made = resolve_from(&w.root, "/b")
        .unwrap()
        .create("new", FileType::File, 0o644);
    assert_eq!(made.err(), Some(FsError::ReadOnly));
}

#[test]
fn a_bind_of_a_bind_takes_the_flags_on() {
    let w = with_d_as("rw,noexec");
    bind_from(&w.root, &w.table, "/a/d", "/b").unwrap();
    bind_from(&w.root, &w.table, "/b", "/c").unwrap();
    let line = line_at(&w, "/c");
    assert_eq!(
        (line.options.as_str(), line.root.as_str()),
        ("rw,noexec", "/d")
    );
    assert!(line.state.is_noexec());
}

#[test]
fn a_bind_of_a_whole_mount_takes_its_flags() {
    let w = with_d_as("rw,nosuid,nodev");
    bind_from(&w.root, &w.table, "/a", "/b").unwrap();
    let line = line_at(&w, "/b");
    assert_eq!(line.options, "rw,nosuid,nodev");
    assert!(line.state.is_nosuid() && line.state.is_nodev());
}

/// With no line for the mount the source is on, the bind names the path it
/// took, `none` and a plain `rw`.
#[test]
fn a_bind_from_where_no_mount_is_listed_is_a_plain_one() {
    let w = with_d();
    w.table.lock().clear();
    bind_from(&w.root, &w.table, "/a/d", "/b").unwrap();
    let line = line_at(&w, "/b");
    assert_eq!(
        (
            line.source.as_str(),
            line.fstype.as_str(),
            line.root.as_str(),
            line.options.as_str()
        ),
        ("/a/d", "none", "/", "rw")
    );
    assert!(!line.state.is_read_only() && !line.state.is_noexec());
}
