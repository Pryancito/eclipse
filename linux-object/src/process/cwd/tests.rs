//! A working directory that has been removed is reported as gone, one that
//! has been renamed is followed, and either is still where relative names
//! are looked up from.
//!
//! Every test has a top directory of its own: a lookup from the root goes
//! through a path cache that every process in this binary shares.

use super::*;
use rcore_fs::vfs::FsError;
use rcore_fs_ramfs::RamFS;

fn a_process_in(top: &str) -> LinuxProcess {
    let proc = LinuxProcess::new(RamFS::new(), 0);
    let top_dir = proc.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    top_dir.create("d", FileType::Dir, 0o755).unwrap();
    top_dir.create("f", FileType::File, 0o644).unwrap();
    proc.change_directory(&format!("/{}/d", top));
    proc
}

#[test]
fn a_working_directory_that_is_there_is_its_own_link() {
    let proc = a_process_in("wdr_a");
    assert!(!proc.working_directory_removed());
    assert_eq!(proc.working_directory_link(), "/wdr_a/d");
    proc.change_directory("/");
    assert!(!proc.working_directory_removed(), "the root");
    assert_eq!(proc.working_directory_link(), "/");
}

#[test]
fn a_removed_working_directory_is_gone_and_its_link_says_deleted() {
    let proc = a_process_in("wdr_b");
    proc.root_inode()
        .lookup("wdr_b")
        .unwrap()
        .unlink("d")
        .unwrap();
    crate::fs::dcache_invalidate();
    assert!(proc.working_directory_removed());
    assert_eq!(proc.working_directory_link(), "/wdr_b/d (deleted)");
}

#[test]
fn a_working_directory_whose_name_is_now_a_file_is_gone() {
    let proc = a_process_in("wdr_c");
    let top = proc.root_inode().lookup("wdr_c").unwrap();
    top.unlink("d").unwrap();
    top.create("d", FileType::File, 0o644).unwrap();
    crate::fs::dcache_invalidate();
    assert!(proc.working_directory_removed());
}

/// Whether `a` and `b` are the same inode, by number (one filesystem).
fn same(a: &Arc<dyn INode>, b: &Arc<dyn INode>) -> bool {
    a.metadata().unwrap().inode == b.metadata().unwrap().inode
}

#[test]
fn a_renamed_working_directory_is_followed_to_its_new_name() {
    let proc = a_process_in("wdr_d");
    let top = proc.root_inode().lookup("wdr_d").unwrap();
    top.lookup("d")
        .unwrap()
        .create("inside", FileType::File, 0o644)
        .unwrap();
    top.move_("d", &top, "e").unwrap();
    crate::fs::dcache_invalidate();
    assert_eq!(proc.working_directory_path().as_deref(), Some("/wdr_d/e"));
    assert_eq!(proc.current_working_directory(), "/wdr_d/e");
    assert_eq!(proc.working_directory_link(), "/wdr_d/e");
    assert!(!proc.working_directory_removed());
    assert!(
        proc.lookup_inode("inside").is_ok(),
        "a relative name is still looked up in it"
    );
    // And once found, the new name is what it is known by.
    assert_eq!(proc.working_directory_as_entered(), "/wdr_d/e");
}

/// A rename further up, across a mount: `/tmp` is a filesystem of its own,
/// so the climb back to the root has to name the directory it is mounted on.
#[test]
fn a_directory_renamed_above_it_is_followed_through_a_mount() {
    let proc = LinuxProcess::new(RamFS::new(), 0);
    let tmp = proc.lookup_inode("/tmp").unwrap();
    let top = tmp.create("wdr_e", FileType::Dir, 0o755).unwrap();
    let p = top.create("p", FileType::Dir, 0o755).unwrap();
    p.create("q", FileType::Dir, 0o755).unwrap();
    proc.change_directory("/tmp/wdr_e/p/q");
    top.move_("p", &top, "r").unwrap();
    crate::fs::dcache_invalidate();
    assert_eq!(
        proc.working_directory_path().as_deref(),
        Some("/tmp/wdr_e/r/q")
    );
}

/// Linux keeps a removed working directory for whoever is in it: `.` is
/// still it, with no links left, `..` is still the directory it was in, and
/// nothing can be made in it.
#[test]
fn a_removed_working_directory_is_still_there_for_whoever_is_in_it() {
    let proc = a_process_in("wdr_f");
    let top = proc.root_inode().lookup("wdr_f").unwrap();
    top.unlink("d").unwrap();
    crate::fs::dcache_invalidate();
    assert_eq!(proc.working_directory_path(), None);
    let dot = proc.lookup_inode(".").unwrap();
    assert_eq!(dot.metadata().unwrap().nlinks, 0);
    assert!(same(&proc.lookup_inode("..").unwrap(), &top));
    let here = proc.lookup_parent_at(FileDesc::CWD, ".").unwrap();
    assert_eq!(
        here.create("new", FileType::File, 0o644).err(),
        Some(FsError::DirRemoved)
    );
    // The path it had is the one it keeps.
    assert_eq!(proc.current_working_directory(), "/wdr_f/d");
}

#[test]
fn a_directory_made_again_under_the_old_name_is_not_the_working_directory() {
    let proc = a_process_in("wdr_g");
    let top = proc.root_inode().lookup("wdr_g").unwrap();
    top.unlink("d").unwrap();
    let again = top.create("d", FileType::Dir, 0o755).unwrap();
    crate::fs::dcache_invalidate();
    assert!(proc.working_directory_removed());
    assert!(!same(&proc.lookup_inode(".").unwrap(), &again));
    assert_eq!(proc.working_directory_link(), "/wdr_g/d (deleted)");
}

/// `fchdir` into a directory that has been removed since it was opened: the
/// process is in it, by the path it had.
#[test]
fn entering_a_removed_directory_holds_it() {
    let proc = a_process_in("wdr_h");
    let top = proc.root_inode().lookup("wdr_h").unwrap();
    let gone = top.create("gone", FileType::Dir, 0o755).unwrap();
    top.unlink("gone").unwrap();
    crate::fs::dcache_invalidate();
    proc.enter_directory(gone.clone(), "/wdr_h/gone");
    assert!(proc.working_directory_removed());
    assert!(same(&proc.lookup_inode(".").unwrap(), &gone));
    assert!(same(&proc.working_directory().unwrap().0, &gone));
    assert_eq!(proc.working_directory().unwrap().1, "/wdr_h/gone");
}

/// A path that names nothing leaves nothing held, and the process is where
/// that path says: nowhere a relative name can be found.
#[test]
fn a_working_directory_named_by_nothing_is_gone() {
    let proc = a_process_in("wdr_i");
    proc.change_directory("/wdr_i/nothing");
    assert!(proc.working_directory_removed());
    assert_eq!(proc.current_working_directory(), "/wdr_i/nothing");
    assert!(proc.lookup_inode("f").is_err());
    proc.change_directory("..");
    assert_eq!(proc.working_directory_path().as_deref(), Some("/wdr_i"));
    assert!(proc.lookup_inode("f").is_ok());
}

/// In `/dev` and `/proc`, which the kernel walks straight into rather than
/// through the mounts, a relative name is the one an absolute path names,
/// and the working directory stays where it is when names change elsewhere.
#[test]
fn a_working_directory_in_dev_or_proc_is_the_one_absolute_paths_name() {
    let proc = LinuxProcess::new(RamFS::new(), 0);
    proc.change_directory("/dev");
    let relative = proc.lookup_inode("null").unwrap();
    let absolute = proc.lookup_inode("/dev/null").unwrap();
    assert_eq!(
        relative.metadata().unwrap().inode,
        absolute.metadata().unwrap().inode
    );
    crate::fs::dcache_invalidate();
    assert_eq!(proc.working_directory_path().as_deref(), Some("/dev"));
    proc.change_directory("/proc");
    assert!(proc.lookup_inode("meminfo").is_ok());
    crate::fs::dcache_invalidate();
    assert_eq!(proc.working_directory_path().as_deref(), Some("/proc"));
}

/// A directory reached through the mounts is seen as the `MNode` it is,
/// whatever its `as_any_ref` answers, and one of `/proc` is not one. Without
/// this every comparison would fall back on the inode number, which two
/// filesystems can share (see `MNode::is_same_node`).
#[test]
fn a_directory_reached_through_the_mounts_is_known_for_one() {
    let proc = LinuxProcess::new(RamFS::new(), 0);
    let tmp = proc.root_inode().lookup("tmp").unwrap();
    assert!(tmp.as_any_ref().downcast_ref::<MNode>().is_none());
    assert!(mnode(&tmp).is_some());
    assert!(mnode(&proc.root_inode()).is_some());
    assert!(mnode(&proc.lookup_inode("/proc").unwrap()).is_none());
    let again = proc.root_inode().lookup("tmp").unwrap();
    assert!(!same_object(&tmp, &again));
    assert!(same_node(&tmp, &again));
    assert!(!same_node(&tmp, &proc.root_inode()));
}

/// `change_directory` to a relative path goes from where the process is, and
/// holds the directory it gets to.
#[test]
fn a_relative_change_of_directory_holds_where_it_leads() {
    let proc = a_process_in("wdr_k");
    let top = proc.root_inode().lookup("wdr_k").unwrap();
    top.lookup("d")
        .unwrap()
        .create("e", FileType::Dir, 0o755)
        .unwrap();
    proc.change_directory("e");
    top.move_("d", &top, "moved").unwrap();
    crate::fs::dcache_invalidate();
    assert_eq!(
        proc.working_directory_path().as_deref(),
        Some("/wdr_k/moved/e")
    );
}

/// In `/dev/shm`, outside the mounts, a directory made again under the old
/// name is told apart by its number, and the climb from the removed one does
/// not take a directory of another filesystem for the root.
#[test]
fn a_directory_removed_in_dev_shm_is_gone_and_one_made_again_is_not_it() {
    let proc = LinuxProcess::new(RamFS::new(), 0);
    let shm = proc.lookup_inode("/dev/shm").unwrap();
    shm.create("wdr_l", FileType::Dir, 0o755).unwrap();
    proc.change_directory("/dev/shm/wdr_l");
    crate::fs::dcache_invalidate();
    assert_eq!(
        proc.working_directory_path().as_deref(),
        Some("/dev/shm/wdr_l")
    );
    shm.unlink("wdr_l").unwrap();
    let again = shm.create("wdr_l", FileType::Dir, 0o755).unwrap();
    crate::fs::dcache_invalidate();
    assert_eq!(proc.working_directory_path(), None);
    assert!(!same(&proc.lookup_inode(".").unwrap(), &again));
    shm.unlink("wdr_l").unwrap();
}
