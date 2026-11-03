extern crate std;

use crate::*;
use rcore_fs::{
    util::uninit_memory,
    vfs::{FileSystem, FileType, Metadata, Result, Timespec},
};
use std::{
    fs::{self, OpenOptions},
    sync::{Arc, Mutex},
};

fn _open_sample_file() -> Arc<SimpleFileSystem> {
    fs::copy("sfs.img", "test.img").expect("failed to open sfs.img");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open("test.img")
        .expect("failed to open test.img");
    SimpleFileSystem::open(Arc::new(Mutex::new(file))).expect("failed to open SFS")
}

fn _create_new_sfs() -> Arc<SimpleFileSystem> {
    let file = tempfile::tempfile().expect("failed to create file");
    SimpleFileSystem::create(Arc::new(Mutex::new(file)), 32 * 4096 * 4096)
        .expect("failed to create SFS")
}

#[test]
#[ignore]
fn open_sample_file() {
    _open_sample_file();
}

#[test]
fn create_new_sfs() {
    let sfs = _create_new_sfs();
    let _root = sfs.root_inode();
}

#[test]
fn directory_names_at_storage_limit_do_not_panic_or_mutate() -> Result<()> {
    let sfs = _create_new_sfs();
    let root = sfs.root_inode();
    let max_name = "x".repeat(255);
    let too_long = "x".repeat(256);
    let source = root.create("source", FileType::File, 0o777)?;
    let max_file = root.create(&max_name, FileType::File, 0o777)?;
    assert_eq!(
        root.find(&max_name)?.metadata()?.inode,
        max_file.metadata()?.inode
    );

    let entries_before = root.list()?;
    let links_before = source.metadata()?.nlinks;
    assert!(matches!(
        root.create(&too_long, FileType::File, 0o777),
        Err(FsError::NameTooLong)
    ));
    assert_eq!(root.link(&too_long, &source), Err(FsError::NameTooLong));
    assert_eq!(
        root.move_("source", &root, &too_long),
        Err(FsError::NameTooLong)
    );
    assert_eq!(root.list()?, entries_before);
    assert_eq!(source.metadata()?.nlinks, links_before);
    assert_eq!(
        root.find("source")?.metadata()?.inode,
        source.metadata()?.inode
    );
    Ok(())
}

#[test]
fn rename_over_existing_file_preserves_other_entries() -> Result<()> {
    let sfs = _create_new_sfs();
    let root = sfs.root_inode();
    let source = root.create("source", FileType::File, 0o777)?;
    let other = root.create("other", FileType::File, 0o777)?;
    let replaced = root.create("replaced", FileType::File, 0o777)?;
    source.write_at(0, b"source data")?;

    assert_eq!(
        root.move_("missing", &root, "replaced"),
        Err(FsError::EntryNotFound)
    );
    assert_eq!(
        root.find("replaced")?.metadata()?.inode,
        replaced.metadata()?.inode
    );
    root.move_("source", &root, "source")?;
    assert_eq!(source.metadata()?.nlinks, 1);
    root.move_("source", &root, "replaced")?;
    assert!(root.find("source").is_err());
    assert_eq!(
        root.find("replaced")?.metadata()?.inode,
        source.metadata()?.inode
    );
    assert_eq!(
        root.find("other")?.metadata()?.inode,
        other.metadata()?.inode
    );
    assert_eq!(replaced.metadata()?.nlinks, 0);
    let mut data = [0; 11];
    root.find("replaced")?.read_at(0, &mut data)?;
    assert_eq!(&data, b"source data");
    Ok(())
}

#[test]
fn move_directory_updates_parent_and_replaced_links() -> Result<()> {
    let sfs = _create_new_sfs();
    let root = sfs.root_inode();
    let source_parent = root.create("from", FileType::Dir, 0o777)?;
    let dest_parent = root.create("to", FileType::Dir, 0o777)?;
    let source = source_parent.create("source", FileType::Dir, 0o777)?;
    let replaced = dest_parent.create("dest", FileType::Dir, 0o777)?;
    source_parent.move_("source", &dest_parent, "dest")?;
    assert!(source_parent.find("source").is_err());
    assert_eq!(
        dest_parent.find("dest")?.metadata()?.inode,
        source.metadata()?.inode
    );
    assert_eq!(
        source.find("..")?.metadata()?.inode,
        dest_parent.metadata()?.inode
    );
    assert_eq!(replaced.metadata()?.nlinks, 0);
    assert_eq!(source_parent.metadata()?.nlinks, 2);
    assert_eq!(dest_parent.metadata()?.nlinks, 3);
    Ok(())
}

#[test]
fn moving_directory_into_descendant_keeps_tree_reachable() -> Result<()> {
    let sfs = _create_new_sfs();
    let root = sfs.root_inode();
    let parent = root.create("parent", FileType::Dir, 0o777)?;
    let child = parent.create("child", FileType::Dir, 0o777)?;
    assert_eq!(
        root.move_("parent", &child, "moved"),
        Err(FsError::InvalidParam)
    );
    assert_eq!(
        root.find("parent")?.metadata()?.inode,
        parent.metadata()?.inode
    );
    assert_eq!(
        child.find("..")?.metadata()?.inode,
        parent.metadata()?.inode
    );
    Ok(())
}

/// Each SFS is a device of its own, and everything in it is on it: `/` was
/// device 0, as `/proc` and `/dev` were.
#[test]
fn an_sfs_is_a_device_of_its_own() -> Result<()> {
    let (one, two) = (_create_new_sfs(), _create_new_sfs());
    let root = one.root_inode();
    let dev = root.metadata()?.dev;
    assert_ne!(dev, 0);
    assert_ne!(dev, two.root_inode().metadata()?.dev);
    let dir = root.create("d", FileType::Dir, 0o755)?;
    let file = dir.create("f", FileType::File, 0o644)?;
    assert_eq!(dir.metadata()?.dev, dev);
    assert_eq!(file.metadata()?.dev, dev);
    Ok(())
}

#[test]
fn create_file() -> Result<()> {
    let sfs = _create_new_sfs();
    let root = sfs.root_inode();
    let file1 = root.create("file1", FileType::File, 0o777)?;

    assert_eq!(
        file1.metadata()?,
        Metadata {
            inode: 7,
            size: 0,
            type_: FileType::File,
            mode: 0o777,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            nlinks: 1,
            uid: 0,
            ctime: Timespec { sec: 0, nsec: 0 },
            gid: 0,
            blk_size: 4096,
            // The filesystem's, which every inode of it reports.
            dev: root.metadata()?.dev,
            rdev: 100, // dummo why 100 here, maybe legacy data?
        }
    );

    sfs.sync()?;
    Ok(())
}

#[test]
fn resize() -> Result<()> {
    let sfs = _create_new_sfs();
    let root = sfs.root_inode();
    let file1 = root.create("file1", FileType::File, 0o777)?;
    assert_eq!(file1.metadata()?.size, 0, "empty file size != 0");

    const SIZE1: usize = 0x1234;
    const SIZE2: usize = 0x1250;
    file1.resize(SIZE1)?;
    assert_eq!(file1.metadata()?.size, SIZE1, "wrong size after resize");
    let mut data1: [u8; SIZE2] = unsafe { uninit_memory() };
    let len = file1.read_at(0, data1.as_mut())?;
    assert_eq!(len, SIZE1, "wrong size returned by read_at()");
    assert_eq!(
        &data1[..SIZE1],
        &[0u8; SIZE1][..],
        "expanded data should be 0"
    );

    sfs.sync()?;
    Ok(())
}

#[test]
fn resize_on_dir_should_panic() -> Result<()> {
    let sfs = _create_new_sfs();
    let root = sfs.root_inode();
    assert!(root.resize(4096).is_err());
    sfs.sync()?;

    Ok(())
}

#[test]
fn resize_too_large_should_panic() -> Result<()> {
    let sfs = _create_new_sfs();
    let root = sfs.root_inode();
    let file1 = root.create("file1", FileType::File, 0o777)?;
    assert!(file1.resize(1 << 40).is_err());
    sfs.sync()?;

    Ok(())
}

#[test]
fn create_then_lookup() -> Result<()> {
    let sfs = _create_new_sfs();
    let root = sfs.root_inode();

    assert!(Arc::ptr_eq(&root.lookup(".")?, &root), "failed to find .");
    assert!(Arc::ptr_eq(&root.lookup("..")?, &root), "failed to find ..");

    let file1 = root
        .create("file1", FileType::File, 0o777)
        .expect("failed to create file1");
    assert!(
        Arc::ptr_eq(&root.lookup("file1")?, &file1),
        "failed to find file1"
    );
    assert!(root.lookup("file2").is_err(), "found non-existent file");

    let dir1 = root
        .create("dir1", FileType::Dir, 0o777)
        .expect("failed to create dir1");
    let file2 = dir1
        .create("file2", FileType::File, 0o777)
        .expect("failed to create /dir1/file2");
    assert!(
        Arc::ptr_eq(&root.lookup("dir1/file2")?, &file2),
        "failed to find dir1/file2"
    );
    assert!(
        Arc::ptr_eq(&root.lookup("/")?.lookup("dir1/file2")?, &file2),
        "failed to find dir1/file2"
    );
    assert!(
        Arc::ptr_eq(&dir1.lookup("..")?, &root),
        "failed to find .. from dir1"
    );

    assert!(
        Arc::ptr_eq(&dir1.lookup("../dir1/file2")?, &file2),
        "failed to find dir1/file2 by relative"
    );
    assert!(
        Arc::ptr_eq(&dir1.lookup("/dir1/file2")?, &file2),
        "failed to find dir1/file2 by absolute"
    );
    assert!(
        Arc::ptr_eq(&dir1.lookup("/dir1/../dir1/file2")?, &file2),
        "failed to find dir1/file2 by absolute"
    );
    assert!(
        Arc::ptr_eq(&dir1.lookup("../../..//dir1/../dir1/file2")?, &file2),
        "failed to find dir1/file2 by more than one .."
    );
    assert!(
        Arc::ptr_eq(&dir1.lookup("..//dir1/file2")?, &file2),
        "failed to find dir1/file2 by weird relative"
    );

    assert!(
        root.lookup("./dir1/../file2").is_err(),
        "found non-existent file"
    );
    assert!(
        root.lookup("./dir1/../file3").is_err(),
        "found non-existent file"
    );
    assert!(
        root.lookup("/dir1/../dir1/../file3").is_err(),
        "found non-existent file"
    );
    assert!(
        root.lookup("/dir1/../../../dir1/../file3").is_err(),
        "found non-existent file"
    );
    assert!(
        root.lookup("/").unwrap().lookup("dir1/../file2").is_err(),
        "found non-existent file"
    );

    sfs.sync()?;
    Ok(())
}

#[test]
fn test_symlinks() -> Result<()> {
    let sfs = _create_new_sfs();
    let root = sfs.root_inode();

    let file1 = root
        .create("file1", FileType::File, 0o777)
        .expect("failed to create file1");
    assert!(
        Arc::ptr_eq(&root.lookup("file1")?, &file1),
        "failed to find file1"
    );

    let link1 = root
        .create("link1", FileType::SymLink, 0o777)
        .expect("failed to create link1");
    let data = "file1".as_bytes();
    link1.resize(data.len())?;
    link1.write_at(0, data)?;

    let link2 = root
        .create("link2", FileType::SymLink, 0o777)
        .expect("failed to create link2");
    let data = "link1".as_bytes();
    link2.resize(data.len())?;
    link2.write_at(0, data)?;

    assert!(
        Arc::ptr_eq(&root.lookup("link1")?, &link1),
        "failed to find link1 by relative"
    );
    assert!(
        Arc::ptr_eq(&root.lookup_follow("link1", 1)?, &file1),
        "failed to find file1 by link1"
    );
    assert!(
        Arc::ptr_eq(&root.lookup_follow("link2", 0)?, &link2),
        "failed to find link2 by link2"
    );
    // Running out of budget on the way is ELOOP, as on Linux, not the link
    // where the walk stopped: a loop used to come back as a file.
    assert_eq!(
        root.lookup_follow("link2", 1).err(),
        Some(FsError::SymLoop),
        "link2 with one hop left stops at link1, which is a symlink"
    );
    assert!(
        Arc::ptr_eq(&root.lookup_follow("link2", 2)?, &file1),
        "failed to find file1 by link2"
    );

    let link3 = root
        .create("link3", FileType::SymLink, 0o777)
        .expect("failed to create link3");
    let data = "/link2".as_bytes();
    link3.resize(data.len())?;
    link3.write_at(0, data)?;

    assert!(
        Arc::ptr_eq(&root.lookup_follow("link3", 0)?, &link3),
        "failed to find link3 by link3"
    );
    assert_eq!(root.lookup_follow("link3", 1).err(), Some(FsError::SymLoop));
    assert_eq!(root.lookup_follow("link3", 2).err(), Some(FsError::SymLoop));
    assert!(
        Arc::ptr_eq(&root.lookup_follow("link3", 3)?, &file1),
        "failed to find file1 by link2"
    );

    let dir1 = root
        .create("dir1", FileType::Dir, 0o777)
        .expect("failed to create dir1");
    let file2 = dir1
        .create("file2", FileType::File, 0o777)
        .expect("failed to create /dir1/file2");

    let link_dir = root
        .create("link_dir", FileType::SymLink, 0o777)
        .expect("failed to create link2");
    let data = "dir1".as_bytes();
    link_dir.resize(data.len())?;
    link_dir.write_at(0, data)?;

    assert!(
        Arc::ptr_eq(&root.lookup_follow("link_dir/file2", 1)?, &file2),
        "failed to find file2"
    );

    sfs.sync()?;
    Ok(())
}

#[test]
fn test_double_indirect_blocks() -> Result<()> {
    let sfs = _create_new_sfs();
    let root = sfs.root_inode();

    let file1 = root
        .create("file1", FileType::File, 0o777)
        .expect("failed to create file1");
    assert!(
        Arc::ptr_eq(&root.lookup("file1")?, &file1),
        "failed to find file1"
    );

    // resize to direct maximum size
    file1.resize(MAX_NBLOCK_DIRECT * BLKSIZE).unwrap();
    // force usage of indirect block
    file1.resize((MAX_NBLOCK_DIRECT + 1) * BLKSIZE).unwrap();
    file1.resize(MAX_NBLOCK_INDIRECT * BLKSIZE).unwrap();
    // force usage of double indirect block
    file1.resize((MAX_NBLOCK_INDIRECT + 1) * BLKSIZE).unwrap();
    file1.resize((MAX_NBLOCK_INDIRECT + 2) * BLKSIZE).unwrap();

    // resize up and down
    file1.resize(0).unwrap();
    file1.resize((MAX_NBLOCK_INDIRECT + 2) * BLKSIZE).unwrap();
    file1.resize(MAX_NBLOCK_DIRECT * BLKSIZE).unwrap();
    file1.resize((MAX_NBLOCK_DIRECT + 1) * BLKSIZE).unwrap();
    file1.resize(MAX_NBLOCK_DIRECT * BLKSIZE).unwrap();
    file1.resize(0).unwrap();
    file1.resize((MAX_NBLOCK_INDIRECT + 1) * BLKSIZE).unwrap();
    file1.resize(MAX_NBLOCK_DIRECT * BLKSIZE).unwrap();
    file1.resize((MAX_NBLOCK_INDIRECT + 2) * BLKSIZE).unwrap();
    file1.resize((MAX_NBLOCK_INDIRECT + 1) * BLKSIZE).unwrap();
    file1.resize(0).unwrap();

    sfs.sync()?;
    Ok(())
}

#[test]
fn arc_layout() {
    // [usize, usize, T]
    //  ^ start       ^ Arc::into_raw
    let p = Arc::new([2u8; 5]);
    let ptr = Arc::into_raw(p);
    let start = unsafe { (ptr as *const usize).offset(-2) };
    let ns = unsafe { &*(start as *const [usize; 2]) };
    assert_eq!(ns, &[1usize, 1]);
}

#[test]
#[ignore]
fn kernel_image_file_create() -> Result<()> {
    let sfs = _open_sample_file();
    let root = sfs.root_inode();
    let files_count_before = root.list()?.len();
    root.create("hello2", FileType::File, 0o777)?;
    let files_count_after = root.list()?.len();
    assert_eq!(files_count_before + 1, files_count_after);
    assert!(root.lookup("hello2").is_ok());

    sfs.sync()?;
    Ok(())
}

#[test]
#[ignore]
fn kernel_image_file_unlink() -> Result<()> {
    let sfs = _open_sample_file();
    let root = sfs.root_inode();
    let files_count_before = root.list()?.len();
    root.unlink("hello")?;
    let files_count_after = root.list()?.len();
    assert_eq!(files_count_before, files_count_after + 1);
    assert!(root.lookup("hello").is_err());

    sfs.sync()?;
    Ok(())
}

#[test]
#[ignore]
fn kernel_image_file_rename() -> Result<()> {
    let sfs = _open_sample_file();
    let root = sfs.root_inode();
    let files_count_before = root.list()?.len();
    root.move_("hello", &root, "hello2")?;
    let files_count_after = root.list()?.len();
    assert_eq!(files_count_before, files_count_after);
    assert!(root.lookup("hello").is_err());
    assert!(root.lookup("hello2").is_ok());

    sfs.sync()?;
    Ok(())
}

#[test]
#[ignore]
fn kernel_image_file_move() -> Result<()> {
    let sfs = _open_sample_file();
    let root = sfs.root_inode();
    let files_count_before = root.list()?.len();
    root.unlink("divzero")?;
    let rust_dir = root.create("rust", FileType::Dir, 0o777)?;
    root.move_("hello", &rust_dir, "hello_world")?;
    let files_count_after = root.list()?.len();
    assert_eq!(files_count_before, files_count_after + 1);
    assert!(root.lookup("hello").is_err());
    assert!(root.lookup("divzero").is_err());
    assert!(root.lookup("rust").is_ok());
    assert!(rust_dir.lookup("hello_world").is_ok());

    sfs.sync()?;
    Ok(())
}

#[test]
fn hard_link() -> Result<()> {
    let sfs = _create_new_sfs();
    let root = sfs.root_inode();
    let file1 = root.create("file1", FileType::File, 0o777)?;
    root.link("file2", &file1)?;
    let file2 = root.lookup("file2")?;
    file1.resize(100)?;
    assert_eq!(file2.metadata()?.size, 100);

    sfs.sync()?;
    Ok(())
}

#[test]
fn nlinks() -> Result<()> {
    let sfs = _create_new_sfs();
    let root = sfs.root_inode();
    // -root
    assert_eq!(root.metadata()?.nlinks, 2);

    let file1 = root.create("file1", FileType::File, 0o777)?;
    // -root
    //   `-file1 <f1>
    assert_eq!(file1.metadata()?.nlinks, 1);
    assert_eq!(root.metadata()?.nlinks, 2);

    let dir1 = root.create("dir1", FileType::Dir, 0o777)?;
    // -root
    //   +-dir1
    //   `-file1 <f1>
    assert_eq!(dir1.metadata()?.nlinks, 2);
    assert_eq!(root.metadata()?.nlinks, 3);

    root.move_("dir1", &root, "dir_1")?;
    // -root
    //   +-dir_1
    //   `-file1 <f1>
    assert_eq!(dir1.metadata()?.nlinks, 2);
    assert_eq!(root.metadata()?.nlinks, 3);

    dir1.link("file1_", &file1)?;
    // -root
    //   +-dir_1
    //   |  `-file1_ <f1>
    //   `-file1 <f1>
    assert_eq!(dir1.metadata()?.nlinks, 2);
    assert_eq!(root.metadata()?.nlinks, 3);
    assert_eq!(file1.metadata()?.nlinks, 2);

    let dir2 = root.create("dir2", FileType::Dir, 0o777)?;
    // -root
    //   +-dir_1
    //   |  `-file1_ <f1>
    //   +-dir2
    //   `-file1 <f1>
    assert_eq!(dir1.metadata()?.nlinks, 2);
    assert_eq!(dir2.metadata()?.nlinks, 2);
    assert_eq!(root.metadata()?.nlinks, 4);
    assert_eq!(file1.metadata()?.nlinks, 2);

    root.move_("file1", &root, "file_1")?;
    // -root
    //   +-dir_1
    //   |  `-file1_ <f1>
    //   +-dir2
    //   `-file_1 <f1>
    assert_eq!(dir1.metadata()?.nlinks, 2);
    assert_eq!(dir2.metadata()?.nlinks, 2);
    assert_eq!(root.metadata()?.nlinks, 4);
    assert_eq!(file1.metadata()?.nlinks, 2);

    root.move_("file_1", &dir2, "file__1")?;
    // -root
    //   +-dir_1
    //   |  `-file1_ <f1>
    //   `-dir2
    //      `-file__1 <f1>
    assert_eq!(dir1.metadata()?.nlinks, 2);
    assert_eq!(dir2.metadata()?.nlinks, 2);
    assert_eq!(root.metadata()?.nlinks, 4);
    assert_eq!(file1.metadata()?.nlinks, 2);

    root.move_("dir_1", &dir2, "dir__1")?;
    // -root
    //   `-dir2
    //      +-dir__1
    //      |  `-file1_ <f1>
    //      `-file__1 <f1>
    assert_eq!(dir1.metadata()?.nlinks, 2);
    assert_eq!(dir2.metadata()?.nlinks, 3);
    assert_eq!(root.metadata()?.nlinks, 3);
    assert_eq!(file1.metadata()?.nlinks, 2);

    dir2.unlink("file__1")?;
    // -root
    //   `-dir2
    //      `-dir__1
    //         `-file1_ <f1>
    assert_eq!(file1.metadata()?.nlinks, 1);
    assert_eq!(dir1.metadata()?.nlinks, 2);
    assert_eq!(dir2.metadata()?.nlinks, 3);
    assert_eq!(root.metadata()?.nlinks, 3);

    dir1.unlink("file1_")?;
    // -root
    //   `-dir2
    //      `-dir__1
    assert_eq!(file1.metadata()?.nlinks, 0);
    assert_eq!(dir1.metadata()?.nlinks, 2);
    assert_eq!(dir2.metadata()?.nlinks, 3);
    assert_eq!(root.metadata()?.nlinks, 3);

    dir2.unlink("dir__1")?;
    // -root
    //   `-dir2
    assert_eq!(file1.metadata()?.nlinks, 0);
    assert_eq!(dir1.metadata()?.nlinks, 0);
    assert_eq!(root.metadata()?.nlinks, 3);
    assert_eq!(dir2.metadata()?.nlinks, 2);

    root.unlink("dir2")?;
    // -root
    assert_eq!(file1.metadata()?.nlinks, 0);
    assert_eq!(dir1.metadata()?.nlinks, 0);
    assert_eq!(root.metadata()?.nlinks, 2);
    assert_eq!(dir2.metadata()?.nlinks, 0);

    sfs.sync()?;
    Ok(())
}

#[test]
fn create_then_get_entry() -> Result<()> {
    let sfs = _create_new_sfs();
    let root = sfs.root_inode();

    assert!(root.get_entry(0).unwrap() == *".", "entry 0 is .");
    assert!(
        root.get_entry_with_metadata(0).unwrap().1 == *".",
        "entry 0 is ."
    );
    assert!(root.get_entry(1).unwrap() == *"..", "entry 1 is ..");
    assert!(
        root.get_entry_with_metadata(1).unwrap().1 == *"..",
        "entry 1 is .."
    );

    let _file1 = root.create("file1", FileType::File, 0o777)?;
    assert!(
        root.get_entry_with_metadata(2).unwrap().1 == *"file1",
        "entry 2 is file1"
    );

    sfs.sync()?;
    Ok(())
}

/// Growing past the end of the device must fail with `NoDeviceSpace` and
/// leave the inode exactly as it was: same size and block count, no phantom
/// block ids, and every block the failed attempt took handed back. 2000
/// blocks needs the indirect block, the double-indirect block and one of its
/// tables, none of which an 8-block file has, so the rollback of all three
/// is exercised on a 64-block device that cannot hold them.
#[test]
fn resize_out_of_space_rolls_back() -> Result<()> {
    let file = tempfile::tempfile().expect("failed to create file");
    let sfs = SimpleFileSystem::create(Arc::new(Mutex::new(file)), 64 * BLKSIZE)?;
    let root = sfs.root_inode();
    let free0 = sfs.info().bfree;

    let f = root.create("big", FileType::File, 0o644)?;
    let free_created = sfs.info().bfree;
    f.resize(8 * BLKSIZE)?;
    f.write_at(7 * BLKSIZE, &[0x5a; BLKSIZE])?;
    let free1 = sfs.info().bfree;
    assert_eq!(free_created - free1, 8);

    // resize(2) past the device
    let err = f.resize(2000 * BLKSIZE).unwrap_err();
    assert_eq!(err, FsError::NoDeviceSpace);
    let meta = f.metadata()?;
    assert_eq!(meta.size, 8 * BLKSIZE);
    assert_eq!(meta.blocks, 8);
    assert_eq!(
        sfs.info().bfree,
        free1,
        "failed growth must give every block back"
    );

    // write(2) past the device (auto-extend goes through the same path)
    let err = f.write_at(3000 * BLKSIZE, &[1u8; 16]).unwrap_err();
    assert_eq!(err, FsError::NoDeviceSpace);
    assert_eq!(f.metadata()?.size, 8 * BLKSIZE);
    assert_eq!(sfs.info().bfree, free1);

    // the surviving blocks are the real ones
    let mut buf = [0u8; BLKSIZE];
    f.read_at(7 * BLKSIZE, &mut buf)?;
    assert!(buf.iter().all(|&b| b == 0x5a));

    // and the space is usable again: a growth that fits succeeds
    f.resize(10 * BLKSIZE)?;
    assert_eq!(sfs.info().bfree, free1 - 2);

    // freeing walks every block id the inode holds; a bogus one would trip
    // free_block's assert
    root.unlink("big")?;
    drop(f);
    assert_eq!(sfs.info().bfree, free0);
    Ok(())
}

/// A name created on an SFS must still resolve after a remount that nothing
/// synced in between.
///
/// The inode and the directory entry naming it are written straight through by
/// `sync_all`, but the bitmap that records the inode's block used to stay in
/// RAM until someone called `FileSystem::sync` -- and the kernel, which holds
/// its root filesystem in a static for the life of the machine, never does. On
/// the one architecture QEMU hands a raw read-write image to (aarch64), the two
/// `mkdir`s `create_root_fs` does for `/var/cache/apk` were written back into
/// `aarch64.img` without their bitmap bits, and the next boot panicked in
/// `get_inode`'s `assert!(!self.free_map.read()[id])` before reaching its test.
///
/// `mem::forget` rather than `drop` on purpose: `SimpleFileSystem::drop` syncs,
/// which is exactly the thing that does not happen when a machine goes away.
#[test]
fn a_name_created_without_sync_survives_a_remount() -> Result<()> {
    let file = tempfile::tempfile().expect("failed to create file");
    let device = Arc::new(Mutex::new(file));
    let size = 32 * 4096 * 4096;

    let sfs = SimpleFileSystem::create(device.clone(), size).expect("failed to create SFS");
    sfs.sync()?;

    // What `create_root_fs` does to the initramfs root on every boot. `apk` is
    // deliberately still alive at the remount below, the way a mount point is:
    // `INodeImpl::drop` was the only thing that ever wrote an inode back, so an
    // inode the kernel keeps forever stayed all zeroes on disk under a
    // directory entry that named it, and reading it back gave
    // `FileType::Invalid`.
    let root = sfs.root_inode();
    let cache = root.create("cache", FileType::Dir, 0o755)?;
    let apk = cache.create("apk", FileType::Dir, 0o755)?;
    drop(cache);
    drop(root);
    std::mem::forget(sfs);

    let sfs = SimpleFileSystem::open(device).expect("failed to reopen SFS");
    let root = sfs.root_inode();
    let cache = root.find("cache")?;
    assert_eq!(cache.metadata()?.type_, FileType::Dir);
    let found = cache.find("apk")?;
    assert_eq!(found.metadata()?.type_, FileType::Dir);
    std::mem::forget(apk);
    std::mem::forget(sfs);
    Ok(())
}

/// Run `op` on `threads` threads released at once, `rounds` times over,
/// each told which thread it is, and hand back every round's results. A thread that panics comes back as
/// `None`: that is the kernel's `assert!` firing, in a test.
fn race<T: Send + 'static>(
    threads: usize,
    rounds: usize,
    setup: impl Fn(usize) -> Arc<dyn INode>,
    op: impl Fn(&Arc<dyn INode>, usize) -> T + Send + Sync + 'static,
) -> std::vec::Vec<std::vec::Vec<Option<T>>> {
    let op = Arc::new(op);
    (0..rounds)
        .map(|round| {
            let dir = setup(round);
            let barrier = Arc::new(std::sync::Barrier::new(threads));
            let handles: std::vec::Vec<_> = (0..threads)
                .map(|thread| {
                    let (dir, barrier, op) = (dir.clone(), barrier.clone(), op.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        op(&dir, thread)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().ok()).collect()
        })
        .collect()
}

/// Two `mkdir` of one name at once: one of them makes it, the other is told
/// it exists. It is the lock three of the boot scripts take around labwc's
/// `environment` (`mkdir "$ENVF.lock"`), so a lock two of them both get is
/// two read-modify-writes of one file at once -- and two `rmdir` of it.
#[test]
fn two_creates_of_one_name_at_once_make_one_entry() -> Result<()> {
    let sfs = _create_new_sfs();
    let root = sfs.root_inode();
    let results = race(
        4,
        200,
        |round| {
            root.create(&format!("d{}", round), FileType::Dir, 0o755)
                .unwrap()
        },
        |dir, _| dir.create("lock", FileType::Dir, 0o755).map(|_| ()),
    );
    for (round, results) in results.iter().enumerate() {
        let made = results.iter().filter(|r| matches!(r, Some(Ok(())))).count();
        assert_eq!(made, 1, "round {}: {:?}", round, results);
        let dir = root.find(&format!("d{}", round))?;
        let entries = dir.list()?;
        let locks = entries.iter().filter(|e| e.as_str() == "lock").count();
        assert_eq!(locks, 1, "round {}: {:?}", round, entries);
    }
    Ok(())
}

/// Two `rmdir` of one directory at once: one of them removes it, the other
/// is told it is not there. Both used to find it, both took its two links
/// away, and the second `nlinks_dec` fired the `nlinks > 0` assertion -- the
/// kernel panic in `INodeImpl::nlinks_dec` (lib.rs) that the riscv64 libc
/// test job dies of while init is starting.
#[test]
fn two_unlinks_of_one_directory_at_once_remove_it_once() -> Result<()> {
    let sfs = _create_new_sfs();
    let root = sfs.root_inode();
    let results = race(
        4,
        200,
        |round| {
            let dir = root
                .create(&format!("d{}", round), FileType::Dir, 0o755)
                .unwrap();
            dir.create("lock", FileType::Dir, 0o755).unwrap();
            dir
        },
        |dir, _| dir.unlink("lock"),
    );
    for (round, results) in results.iter().enumerate() {
        assert!(
            results.iter().all(|r| r.is_some()),
            "round {}: an unlink panicked: {:?}",
            round,
            results
        );
        let removed = results.iter().filter(|r| matches!(r, Some(Ok(())))).count();
        assert_eq!(removed, 1, "round {}: {:?}", round, results);
        let dir = root.find(&format!("d{}", round))?;
        assert_eq!(
            dir.metadata()?.nlinks,
            2,
            "round {}: the parent lost a link per unlink, not per directory",
            round
        );
    }
    Ok(())
}

/// The same for a file: two `unlink` of one name at once take one link.
#[test]
fn two_unlinks_of_one_file_at_once_take_one_link() -> Result<()> {
    let sfs = _create_new_sfs();
    let root = sfs.root_inode();
    let results = race(
        4,
        200,
        |round| {
            let dir = root
                .create(&format!("d{}", round), FileType::Dir, 0o755)
                .unwrap();
            let file = dir.create("f", FileType::File, 0o644).unwrap();
            dir.link("g", &file).unwrap();
            dir
        },
        |dir, _| dir.unlink("f"),
    );
    for (round, results) in results.iter().enumerate() {
        assert!(results.iter().all(|r| r.is_some()), "round {}", round);
        let removed = results.iter().filter(|r| matches!(r, Some(Ok(())))).count();
        assert_eq!(removed, 1, "round {}: {:?}", round, results);
        let dir = root.find(&format!("d{}", round))?;
        assert_eq!(dir.find("g")?.metadata()?.nlinks, 1, "round {}", round);
    }
    Ok(())
}

/// Two `link` to one new name at once: one entry, one link more.
#[test]
fn two_links_to_one_name_at_once_make_one_entry() -> Result<()> {
    let sfs = _create_new_sfs();
    let root = sfs.root_inode();
    let results = race(
        4,
        200,
        |round| {
            let dir = root
                .create(&format!("d{}", round), FileType::Dir, 0o755)
                .unwrap();
            dir.create("f", FileType::File, 0o644).unwrap();
            dir
        },
        |dir, _| {
            let f = dir.find("f").unwrap();
            dir.link("g", &f)
        },
    );
    for (round, results) in results.iter().enumerate() {
        let made = results.iter().filter(|r| matches!(r, Some(Ok(())))).count();
        assert_eq!(made, 1, "round {}: {:?}", round, results);
        let dir = root.find(&format!("d{}", round))?;
        let entries = dir.list()?;
        assert_eq!(
            entries.iter().filter(|e| e.as_str() == "g").count(),
            1,
            "round {}: {:?}",
            round,
            entries
        );
        assert_eq!(dir.find("f")?.metadata()?.nlinks, 2, "round {}", round);
    }
    Ok(())
}

/// Two renames onto one existing name at once -- `mv "$tmp" "$ENVF"` from
/// two writers whose lock was stolen. Each replaces the name once, so the
/// file that was there loses its one link once, not once per rename.
#[test]
fn two_renames_onto_one_name_at_once_replace_it_once_each() -> Result<()> {
    let sfs = _create_new_sfs();
    let root = sfs.root_inode();
    let results = race(
        4,
        200,
        |round| {
            let dir = root
                .create(&format!("d{}", round), FileType::Dir, 0o755)
                .unwrap();
            dir.create("env", FileType::File, 0o644).unwrap();
            for t in 0..4 {
                dir.create(&format!("t{}", t), FileType::File, 0o644)
                    .unwrap();
            }
            dir
        },
        |dir, thread| dir.move_(&format!("t{}", thread), dir, "env"),
    );
    for (round, results) in results.iter().enumerate() {
        assert!(
            results.iter().all(|r| matches!(r, Some(Ok(())))),
            "round {}: {:?}",
            round,
            results
        );
        let dir = root.find(&format!("d{}", round))?;
        let mut entries = dir.list()?;
        entries.sort();
        assert_eq!(entries, [".", "..", "env"], "round {}", round);
        assert_eq!(dir.find("env")?.metadata()?.nlinks, 1, "round {}", round);
    }
    Ok(())
}

/// Several threads writing interleaved chunks of ONE file. Each write past
/// EOF grows the file; a writer that read the size before another writer
/// grew it must not "grow" the file back down to its own end offset. That
/// used to truncate the other writers' blocks and, with two such truncates
/// racing, free the same block twice (`!free_map[block_id]` panic).
#[test]
fn concurrent_writers_never_shrink_the_file() -> Result<()> {
    const THREADS: usize = 4;
    const CHUNKS: usize = 64;
    // Block-aligned on purpose: two writers sharing one device block race in
    // the device's read-modify-write, which is a different bug.
    const CHUNK: usize = 3 * BLKSIZE;
    for _round in 0..8 {
        let sfs = _create_new_sfs();
        let root = sfs.root_inode();
        let file = root.create("shared", FileType::File, 0o777)?;
        let handles: std::vec::Vec<_> = (0..THREADS)
            .map(|t| {
                let file = file.clone();
                std::thread::spawn(move || {
                    let buf = [t as u8 + 1; CHUNK];
                    for k in 0..CHUNKS {
                        let off = (k * THREADS + t) * CHUNK;
                        assert_eq!(file.write_at(off, &buf).unwrap(), CHUNK);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("writer thread panicked");
        }
        let total = THREADS * CHUNKS * CHUNK;
        assert_eq!(file.metadata()?.size, total, "a writer shrank the file");
        let mut back = std::vec![0u8; total];
        assert_eq!(file.read_at(0, &mut back)?, total);
        for (i, c) in back.chunks(CHUNK).enumerate() {
            let want = (i % THREADS) as u8 + 1;
            assert!(
                c.iter().all(|&b| b == want),
                "chunk {} lost its data: first={} want={} bad={}",
                i,
                c[0],
                want,
                c.iter().filter(|&&b| b != want).count()
            );
        }
        sfs.sync()?;
    }
    Ok(())
}

/// Threads creating, writing, looking up and unlinking files in one
/// directory. A lookup racing an inode's `Drop` used to load a second copy of
/// it from disk, and dropping that copy freed its blocks again.
#[test]
fn concurrent_lookup_and_unlink_do_not_double_free() -> Result<()> {
    for _round in 0..4 {
        let sfs = _create_new_sfs();
        let root = sfs.root_inode();
        let hs: std::vec::Vec<_> = (0..4)
            .map(|t| {
                let root = root.clone();
                std::thread::spawn(move || {
                    for i in 0..300 {
                        let name = std::format!("f{}_{}", t, i);
                        let f = root.create(&name, FileType::File, 0o644).unwrap();
                        f.write_at(0, &[1u8; 5000]).unwrap();
                        drop(f);
                        let _ = root.find(&std::format!("f{}_{}", (t + 1) % 4, i));
                        root.unlink(&name).unwrap();
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().expect("thread panicked");
        }
        sfs.sync()?;
    }
    Ok(())
}

/// Offline consistency check of an SFS image: every block referenced once and
/// marked used. `SFS_FSCK=img cargo test -p rcore-fs-sfs --features std
/// fsck_image -- --ignored --nocapture`.
#[test]
#[ignore]
fn fsck_image() {
    let path = match std::env::var("SFS_FSCK") {
        Ok(p) => p,
        Err(_) => return,
    };
    let file = OpenOptions::new()
        .read(true)
        .write(false)
        .open(&path)
        .unwrap();
    let sfs = SimpleFileSystem::open(Arc::new(Mutex::new(file))).expect("open");
    let mut owner: std::collections::BTreeMap<usize, (usize, &'static str)> = Default::default();
    let mut dup = 0usize;
    let mut freeref = 0usize;
    let mut stack = std::vec![BLKN_ROOT];
    let mut seen = std::collections::BTreeSet::new();
    let fm: BitVec<Lsb0, u8> = (**sfs.free_map.read()).clone();
    let mut claim =
        |b: usize, ino: usize, what: &'static str, dup: &mut usize, freeref: &mut usize| {
            if fm[b] {
                *freeref += 1;
                if *freeref < 10 {
                    std::println!("ino {} {} block {} marked FREE", ino, what, b);
                }
            }
            if let Some(o) = owner.insert(b, (ino, what)) {
                *dup += 1;
                if *dup < 10 {
                    std::println!(
                        "block {} shared: ino {} {} and ino {} {}",
                        b,
                        o.0,
                        o.1,
                        ino,
                        what
                    );
                }
            }
        };
    while let Some(ino) = stack.pop() {
        if !seen.insert(ino) {
            continue;
        }
        let inode = sfs.get_inode(ino);
        claim(ino, ino, "inode", &mut dup, &mut freeref);
        let (dblocks, dind, ddb, dtype, dsize) = {
            let d = inode.disk_inode.read();
            (d.blocks, d.indirect, d.db_indirect, d.type_, d.size)
        };
        let blocks = dblocks as usize;
        if blocks > NDIRECT {
            claim(dind as usize, ino, "indirect", &mut dup, &mut freeref);
        }
        if blocks > MAX_NBLOCK_INDIRECT {
            claim(ddb as usize, ino, "dbind", &mut dup, &mut freeref);
        }
        for i in 0..blocks {
            let b = inode.get_disk_block_id(i).unwrap();
            claim(b, ino, "data", &mut dup, &mut freeref);
        }
        if dtype == crate::structs::FileType::Dir {
            for e in 0..(dsize as usize / DIRENT_SIZE) {
                let ent = inode.read_direntry(e).unwrap();
                let n: &str = ent.name.as_ref();
                if n != "." && n != ".." {
                    stack.push(ent.id as usize);
                }
            }
        }
    }
    std::println!(
        "inodes {} blocks used {} shared {} referenced-but-free {}",
        seen.len(),
        owner.len(),
        dup,
        freeref
    );
    std::mem::forget(sfs);
}

/// The name index must answer exactly what a scan of the entries answers,
/// through creates, unlinks, renames, moves and replacing renames.
#[test]
fn dir_index_agrees_with_a_scan() -> Result<()> {
    let sfs = _create_new_sfs();
    let root = sfs.root_inode();
    root.create("a", FileType::Dir, 0o755)?;
    root.create("b", FileType::Dir, 0o755)?;
    let mut seed: u64 = 0x9e3779b97f4a7c15;
    let mut rnd = |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    for _ in 0..3000 {
        // Fresh handles each round: the directories' INodeImpls are dropped
        // and reloaded, so the index also has to survive being parked.
        let dirs = [root.find("a")?, root.find("b")?];
        let d = &dirs[rnd(2) as usize];
        let name = std::format!("f{}", rnd(60));
        match rnd(4) {
            0 => {
                let _ = d.create(&name, FileType::File, 0o644);
            }
            1 => {
                let _ = d.unlink(&name);
            }
            2 => {
                let to = &dirs[rnd(2) as usize];
                let _ = d.move_(&name, to, &std::format!("f{}", rnd(60)));
            }
            _ => {
                let _ = d.find(&name);
            }
        }
        for dir in dirs.iter() {
            let imp = dir.downcast_ref::<INodeImpl>().unwrap();
            for i in 0..60 {
                let n = std::format!("f{}", i);
                assert_eq!(
                    imp.get_file_inode_and_entry_id(&n),
                    imp.scan_for_entry(&n),
                    "{}",
                    n
                );
            }
            for n in [".", ".."] {
                assert_eq!(imp.get_file_inode_and_entry_id(n), imp.scan_for_entry(n));
            }
        }
    }
    sfs.sync()?;
    Ok(())
}

struct RamDev(spin::RwLock<std::vec::Vec<u8>>);
impl rcore_fs::dev::Device for RamDev {
    fn read_at(&self, off: usize, buf: &mut [u8]) -> rcore_fs::dev::Result<usize> {
        let v = self.0.read();
        let n = buf.len().min(v.len().saturating_sub(off));
        buf[..n].copy_from_slice(&v[off..off + n]);
        Ok(n)
    }
    fn write_at(&self, off: usize, buf: &[u8]) -> rcore_fs::dev::Result<usize> {
        let mut v = self.0.write();
        let n = buf.len().min(v.len().saturating_sub(off));
        v[off..off + n].copy_from_slice(&buf[..n]);
        Ok(n)
    }
    fn sync(&self) -> rcore_fs::dev::Result<()> {
        Ok(())
    }
}

/// `find` timing on a real image held in RAM: `SFS_IMG=img cargo test --release
/// -p rcore-fs-sfs --features std find_cost_on_image -- --ignored --nocapture`.
#[test]
#[ignore]
fn find_cost_on_image() {
    let img = std::fs::read(std::env::var("SFS_IMG").unwrap()).unwrap();
    let sfs = SimpleFileSystem::open(Arc::new(RamDev(spin::RwLock::new(img)))).unwrap();
    let root = sfs.root_inode();
    for dir in ["", "bin", "etc", "lib", "usr", "sbin"] {
        let d = if dir.is_empty() {
            root.clone()
        } else {
            match root.lookup(dir) {
                Ok(d) => d,
                Err(_) => continue,
            }
        };
        let n = d.metadata().unwrap().size;
        let names: std::vec::Vec<_> = (0..).map_while(|i| d.get_entry(i).ok()).collect();
        let n = names.len();
        let _ = n;
        let last = names.last().unwrap().clone();
        let first = names[2].clone();
        let iters = 2000;
        let t = std::time::Instant::now();
        for _ in 0..iters {
            let _ = d.find(&last).unwrap();
        }
        let tl = t.elapsed().as_nanos() as f64 / iters as f64;
        let t = std::time::Instant::now();
        for _ in 0..iters {
            let _ = d.find(&first).unwrap();
        }
        let tf = t.elapsed().as_nanos() as f64 / iters as f64;
        std::println!(
            "/{:<10} {:5} entries  find(first) {:8.0} ns  find(last) {:8.0} ns",
            dir,
            n,
            tf,
            tl
        );
    }
}
