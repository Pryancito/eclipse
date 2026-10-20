//! What `.` and `..` name, and what `unlink` must refuse.
//!
//! Every directory on an SFS carries the two entries `init_direntry` writes,
//! and the 28 tests beside this file go through `lookup` and `move_`, which
//! read `..` (entry 1) but never ask for `.`. Nothing looked at entry 0, and
//! nothing tried to remove either of them.

extern crate std;

use crate::*;
use rcore_fs::vfs::{FileSystem, FileType, Result};
use std::sync::{Arc, Mutex};

fn _new_sfs() -> Arc<SimpleFileSystem> {
    let file = tempfile::tempfile().expect("failed to create file");
    SimpleFileSystem::create(Arc::new(Mutex::new(file)), 32 * 4096 * 4096)
        .expect("failed to create SFS")
}

/// `.` is the directory itself and `..` is its parent, at every depth.
///
/// Entry 0 is written with the new directory's own inode id and entry 1 with
/// the parent's. Swapping the two leaves `..` right, so a test that only walks
/// upwards cannot tell: `open(".")` would hand out the parent, and every
/// relative path through `./` would resolve one level too high.
#[test]
fn dot_is_the_directory_itself_and_dotdot_is_its_parent() -> Result<()> {
    let sfs = _new_sfs();
    let root = sfs.root_inode();
    let root_ino = root.metadata()?.inode;

    let dir = root.create("dir", FileType::Dir, 0o777)?;
    let dir_ino = dir.metadata()?.inode;
    assert_ne!(dir_ino, root_ino, "a new directory is not the root");
    assert_eq!(
        dir.find(".")?.metadata()?.inode,
        dir_ino,
        "`.` of /dir must be /dir"
    );
    assert_eq!(
        dir.find("..")?.metadata()?.inode,
        root_ino,
        "`..` of /dir must be the root"
    );

    // One level further down, where the parent is no longer the root: this is
    // what tells "entry 0 holds my id" apart from "entry 0 holds my parent's".
    let sub = dir.create("sub", FileType::Dir, 0o777)?;
    let sub_ino = sub.metadata()?.inode;
    assert_eq!(
        sub.find(".")?.metadata()?.inode,
        sub_ino,
        "`.` of /dir/sub must be /dir/sub"
    );
    assert_eq!(
        sub.find("..")?.metadata()?.inode,
        dir_ino,
        "`..` of /dir/sub must be /dir"
    );

    // And the root's own two entries both name the root.
    assert_eq!(root.find(".")?.metadata()?.inode, root_ino);
    assert_eq!(root.find("..")?.metadata()?.inode, root_ino);

    sfs.sync()
}

/// `unlink` refuses `.` and `..` before it touches a single link count.
///
/// Linux answers `rmdir(".")` with `EINVAL`; what matters here is that the
/// name is refused at the top of `unlink`, because the code below it would
/// take the directory's links away twice over -- `nlinks_dec` for the entry,
/// again for `.`, and once more on the parent, which is the same inode -- and
/// the third one would land on a count already at zero and trip
/// `nlinks_dec`'s `assert!(disk_inode.nlinks > 0)`. In the kernel that
/// assertion is a panic while a syscall holds the namespace lock, which is
/// how the riscv64 job died once already.
#[test]
fn unlink_of_dot_or_dotdot_is_refused() -> Result<()> {
    let sfs = _new_sfs();
    let root = sfs.root_inode();

    let dir = root.create("dir", FileType::Dir, 0o777)?;
    let dir_links = dir.metadata()?.nlinks;
    let root_links = root.metadata()?.nlinks;

    assert_eq!(
        dir.unlink(".").unwrap_err(),
        FsError::IsDir,
        "rmdir of `.` must be refused"
    );
    assert_eq!(
        dir.unlink("..").unwrap_err(),
        FsError::IsDir,
        "rmdir of `..` must be refused"
    );
    // An empty directory is the dangerous case: `size / DIRENT_SIZE` is 2, so
    // the DirNotEmpty guard below does not stop it either.
    assert_eq!(root.unlink(".").unwrap_err(), FsError::IsDir);
    assert_eq!(root.unlink("..").unwrap_err(), FsError::IsDir);

    // Nothing was spent on the refusals.
    assert_eq!(dir.metadata()?.nlinks, dir_links, "/dir kept its links");
    assert_eq!(root.metadata()?.nlinks, root_links, "/ kept its links");
    assert_eq!(dir.find(".")?.metadata()?.inode, dir.metadata()?.inode);
    assert!(root.find("dir").is_ok(), "/dir is still there");

    // And a real rmdir of the same directory still works afterwards.
    root.unlink("dir")?;
    assert_eq!(root.find("dir").err(), Some(FsError::EntryNotFound));
    assert_eq!(root.metadata()?.nlinks, root_links - 1);

    sfs.sync()
}
