//! The first tests of SEFS.
//!
//! This crate was vendored and put in the `[patch]` table, so every build of
//! the tree compiles it, but it was never a workspace member: no `cargo test`,
//! `cargo fmt` or `clippy` line named it, and it had no tests at all. What
//! follows runs the filesystem on ordinary memory.

use crate::dev::{DevResult, DeviceError, File, Storage};
use crate::*;
use alloc::vec;
use rcore_fs::vfs;
use spin::Mutex;

// --- the seam: a `Storage` of ordinary memory

/// One file of the storage: a growable buffer, plus a count of the flushes it
/// was asked for.
///
/// The flush count is here because `sync_data` is a single forwarding line and
/// a wrapper that forwards nothing looks exactly like one that does.
#[derive(Default)]
struct MemFile {
    bytes: Mutex<Vec<u8>>,
    flushes: Mutex<usize>,
}

impl MemFile {
    fn len(&self) -> usize {
        self.bytes.lock().len()
    }

    fn snapshot(&self) -> Vec<u8> {
        self.bytes.lock().clone()
    }

    fn flushes(&self) -> usize {
        *self.flushes.lock()
    }
}

/// The same semantics `std::fs::File` has, which is what the trait's own
/// doc comment promises: a read stops at the end of the file and returns how
/// much it got, a write past the end grows the file and zero-fills the gap,
/// and `set_len` both truncates and extends.
impl File for Arc<MemFile> {
    fn read_at(&self, buf: &mut [u8], offset: usize) -> DevResult<usize> {
        let bytes = self.bytes.lock();
        if offset >= bytes.len() {
            return Ok(0);
        }
        let len = core::cmp::min(buf.len(), bytes.len() - offset);
        buf[..len].copy_from_slice(&bytes[offset..offset + len]);
        Ok(len)
    }

    fn write_at(&self, buf: &[u8], offset: usize) -> DevResult<usize> {
        let mut bytes = self.bytes.lock();
        if bytes.len() < offset + buf.len() {
            bytes.resize(offset + buf.len(), 0);
        }
        bytes[offset..offset + buf.len()].copy_from_slice(buf);
        Ok(buf.len())
    }

    fn set_len(&self, len: usize) -> DevResult<()> {
        self.bytes.lock().resize(len, 0);
        Ok(())
    }

    fn flush(&self) -> DevResult<()> {
        *self.flushes.lock() += 1;
        Ok(())
    }
}

/// The whole collection, by file id.
///
/// `Storage` and `File` are traits, so nothing in SEFS wants a disk. The one
/// implementation the crate shipped with, `StdStorage`, writes a real
/// directory, which is part of why none of this had ever been run.
#[derive(Default)]
struct MemStorage {
    files: Mutex<BTreeMap<usize, Arc<MemFile>>>,
}

impl MemStorage {
    /// The buffer behind a file id, whether or not the filesystem still holds
    /// it open.
    fn peek(&self, file_id: usize) -> Option<Arc<MemFile>> {
        self.files.lock().get(&file_id).cloned()
    }

    fn ids(&self) -> Vec<usize> {
        self.files.lock().keys().copied().collect()
    }
}

impl Storage for Arc<MemStorage> {
    fn open(&self, file_id: usize) -> DevResult<Box<dyn File>> {
        match self.files.lock().get(&file_id) {
            Some(file) => Ok(Box::new(file.clone())),
            None => Err(DeviceError),
        }
    }

    fn create(&self, file_id: usize) -> DevResult<Box<dyn File>> {
        let file = Arc::new(MemFile::default());
        self.files.lock().insert(file_id, file.clone());
        Ok(Box::new(file))
    }

    fn remove(&self, file_id: usize) -> DevResult<()> {
        match self.files.lock().remove(&file_id) {
            Some(_) => Ok(()),
            None => Err(DeviceError),
        }
    }
}

/// A clock that does not move, so every timestamp in a test is exactly this.
struct FixedClock;

const NOW: i64 = 1_700_000_000;

impl rcore_fs::dev::TimeProvider for FixedClock {
    fn current_time(&self) -> vfs::Timespec {
        vfs::Timespec { sec: NOW, nsec: 0 }
    }
}

static CLOCK: FixedClock = FixedClock;

/// A brand new filesystem, with the storage behind it kept to hand.
fn new_fs() -> (Arc<SEFS>, Arc<MemStorage>) {
    let storage = Arc::new(MemStorage::default());
    let fs = SEFS::create(Box::new(storage.clone()), &CLOCK).expect("failed to create SEFS");
    (fs, storage)
}

/// The root directory of a brand new filesystem.
fn new_root() -> (Arc<SEFS>, Arc<MemStorage>, Arc<dyn vfs::INode>) {
    let (fs, storage) = new_fs();
    let root = fs.root_inode();
    (fs, storage, root)
}

/// The `INodeImpl` behind a `dyn INode` of this filesystem.
fn as_impl(inode: &Arc<dyn vfs::INode>) -> &INodeImpl {
    inode
        .as_any_ref()
        .downcast_ref::<INodeImpl>()
        .expect("not an inode of this filesystem")
}

/// The names in a directory, in the order its entries are stored.
fn entries(dir: &Arc<dyn vfs::INode>) -> Vec<String> {
    let mut names = Vec::new();
    let mut id = 0;
    while let Ok(name) = dir.get_entry(id) {
        names.push(name);
        id += 1;
    }
    names
}

// --- the superblock, and reading back what was written

#[test]
fn a_new_filesystem_reports_the_geometry_it_was_built_with() {
    let (fs, _storage) = new_fs();
    let info = fs.info();
    assert_eq!(info.bsize, BLKSIZE, "the block size is not the one on disk");
    assert_eq!(info.frsize, BLKSIZE, "frsize parted ways with bsize");
    assert_eq!(info.blocks, BLKBITS, "a group is one block of freemap bits");
    // The superblock starts at `blocks - 2` free (itself and the freemap), and
    // the root inode takes one more.
    assert_eq!(
        info.bfree,
        BLKBITS - 3,
        "the free count is not what create left"
    );
    assert_eq!(info.bavail, info.bfree, "bavail drifted from bfree");
    assert_eq!(info.namemax, MAX_FNAME_LEN, "the name limit moved");
    // This field is the one that broke three vendored filesystems of this tree
    // in a row: `FsInfo` grew `ftype` and a struct literal that did not name it
    // stopped compiling. A test on it is cheap and the breakage was not.
    assert_eq!(
        info.ftype,
        vfs::EXT4_SUPER_MAGIC,
        "statfs would report this filesystem as something else"
    );
}

#[test]
fn what_one_filesystem_writes_the_next_one_reads_back() {
    let storage = Arc::new(MemStorage::default());
    {
        let fs = SEFS::create(Box::new(storage.clone()), &CLOCK).unwrap();
        let root = fs.root_inode();
        let file = root.create("hello", vfs::FileType::File, 0o644).unwrap();
        assert_eq!(file.write_at(0, b"on disk").unwrap(), 7);
        fs.sync().unwrap();
    }
    // Nothing of the first filesystem is left alive: what `open` has to work
    // from is the bytes in the storage.
    let fs = SEFS::open(Box::new(storage.clone()), &CLOCK).expect("failed to re-open SEFS");
    let root = fs.root_inode();
    assert_eq!(entries(&root), vec![".", "..", "hello"]);
    let file = root
        .find("hello")
        .expect("the file did not survive the reopen");
    let mut buf = [0u8; 7];
    assert_eq!(file.read_at(0, &mut buf).unwrap(), 7);
    assert_eq!(&buf, b"on disk");
    assert_eq!(
        file.metadata().unwrap().size,
        7,
        "the size was not written back"
    );
}

#[test]
fn opening_a_storage_whose_magic_is_not_ours_is_refused() {
    let (fs, storage) = new_fs();
    fs.sync().unwrap();
    drop(fs);
    // The magic is the first four bytes of block 0 of the meta file.
    let meta = storage.peek(0).expect("the meta file is file 0");
    assert_eq!(
        &meta.snapshot()[..4],
        &MAGIC.to_le_bytes(),
        "the magic is not where the superblock says it is"
    );
    meta.bytes.lock()[0] ^= 0xff;
    let err = SEFS::open(Box::new(storage.clone()), &CLOCK)
        .err()
        .expect("it opened a storage that is not a SEFS");
    assert_eq!(err, FsError::WrongFs);
}

#[test]
fn the_magic_is_the_only_thing_the_superblock_check_looks_at() {
    // `check()` is one comparison, so the mistake it invites is comparing the
    // wrong field: a superblock with absurd counts and the right magic has to
    // pass, and the right counts with a wrong magic has to fail.
    let odd = SuperBlock {
        magic: MAGIC,
        blocks: 0,
        unused_blocks: 0xffff_ffff,
        groups: 0,
    };
    assert!(odd.check(), "the check looked past the magic");
    let wrong = SuperBlock {
        magic: MAGIC + 1,
        blocks: BLKBITS as u32,
        unused_blocks: 1,
        groups: 1,
    };
    assert!(!wrong.check(), "a magic one off from ours was accepted");
}

// --- the root directory

#[test]
fn the_root_of_a_new_filesystem_is_its_own_parent() {
    let (_fs, _storage, root) = new_root();
    assert_eq!(entries(&root), vec![".", ".."]);
    let info = root.metadata().unwrap();
    assert_eq!(info.inode, BLKN_ROOT, "the root is not at its own block");
    // For a directory the metadata size is the number of entries, not bytes.
    assert_eq!(info.size, 2, "the root does not count its two entries");
    assert_eq!(info.type_, vfs::FileType::Dir);
    assert_eq!(info.nlinks, 2, "the root has to link to itself twice");
    for name in [".", ".."] {
        let found = root.find(name).unwrap();
        assert_eq!(
            found.metadata().unwrap().inode,
            BLKN_ROOT,
            "{} of the root does not point at the root",
            name
        );
    }
}

#[test]
fn an_entry_past_the_last_one_is_not_found() {
    let (_fs, _storage, root) = new_root();
    assert_eq!(root.get_entry(1).unwrap(), "..");
    assert_eq!(root.get_entry(2), Err(FsError::EntryNotFound));
    assert_eq!(root.get_entry(usize::MAX), Err(FsError::EntryNotFound));
}

#[test]
fn a_file_is_not_read_as_a_directory() {
    let (_fs, _storage, root) = new_root();
    let file = root.create("plain", vfs::FileType::File, 0o644).unwrap();
    assert_eq!(file.get_entry(0), Err(FsError::NotDir));
    assert_eq!(file.find("plain").err(), Some(FsError::NotDir));
    assert_eq!(file.unlink("plain"), Err(FsError::NotDir));
    assert_eq!(
        file.create("under", vfs::FileType::File, 0o644).err(),
        Some(FsError::NotDir)
    );
}

// --- creating

#[test]
fn a_created_file_is_found_by_name_and_counted_in_its_directory() {
    let (fs, _storage, root) = new_root();
    let before = fs.info().bfree;
    let file = root.create("one", vfs::FileType::File, 0o640).unwrap();
    assert_eq!(entries(&root), vec![".", "..", "one"]);
    assert_eq!(
        root.metadata().unwrap().size,
        3,
        "the entry was not counted"
    );
    let info = file.metadata().unwrap();
    assert_eq!(info.nlinks, 1, "a new file has exactly one name");
    assert_eq!(info.mode, 0o640, "the mode asked for was not stored");
    assert_eq!(info.size, 0);
    assert_eq!(info.type_, vfs::FileType::File);
    assert_eq!(info.atime.sec, NOW, "the clock was not the one asked for");
    assert_eq!(info.mtime.sec, NOW);
    assert_eq!(info.ctime.sec, NOW);
    assert_eq!(fs.info().bfree, before - 1, "an inode did not cost a block");
    assert_eq!(
        root.find("one").unwrap().metadata().unwrap().inode,
        info.inode,
        "find gave back a different inode than create made"
    );
    // The root's own link count does not move for a file, only for a directory.
    assert_eq!(root.metadata().unwrap().nlinks, 2);
}

#[test]
fn the_same_name_is_not_created_twice() {
    let (_fs, _storage, root) = new_root();
    root.create("once", vfs::FileType::File, 0o644).unwrap();
    assert_eq!(
        root.create("once", vfs::FileType::File, 0o644).err(),
        Some(FsError::EntryExist)
    );
    // And neither are the two names every directory already has.
    for name in [".", ".."] {
        assert_eq!(
            root.create(name, vfs::FileType::Dir, 0o755).err(),
            Some(FsError::EntryExist),
            "it created a second {}",
            name
        );
    }
    assert_eq!(entries(&root), vec![".", "..", "once"]);
}

#[test]
fn a_created_directory_points_back_at_the_one_it_was_made_in() {
    let (_fs, _storage, root) = new_root();
    let dir = root.create("sub", vfs::FileType::Dir, 0o755).unwrap();
    assert_eq!(entries(&dir), vec![".", ".."]);
    assert_eq!(
        dir.find("..").unwrap().metadata().unwrap().inode,
        BLKN_ROOT,
        "the new directory's .. does not reach its parent"
    );
    assert_eq!(
        dir.find(".").unwrap().metadata().unwrap().inode,
        dir.metadata().unwrap().inode,
        "the new directory's . does not reach itself"
    );
    // Two for the directory (its name and its own `.`), and the parent gains
    // one for the child's `..`.
    assert_eq!(dir.metadata().unwrap().nlinks, 2);
    assert_eq!(
        root.metadata().unwrap().nlinks,
        3,
        "the parent was not told about the child's .."
    );
}

#[test]
fn only_the_three_types_this_filesystem_stores_can_be_created() {
    let (_fs, _storage, root) = new_root();
    for (name, type_) in [
        ("f", vfs::FileType::File),
        ("d", vfs::FileType::Dir),
        ("l", vfs::FileType::SymLink),
    ] {
        let made = root.create(name, type_, 0o644).unwrap();
        assert_eq!(
            made.metadata().unwrap().type_,
            type_,
            "{} came back wrong",
            name
        );
    }
    for (name, type_) in [
        ("c", vfs::FileType::CharDevice),
        ("b", vfs::FileType::BlockDevice),
        ("p", vfs::FileType::NamedPipe),
        ("s", vfs::FileType::Socket),
    ] {
        assert_eq!(
            root.create(name, type_, 0o644).err(),
            Some(FsError::InvalidParam),
            "it made a {:?} it has no on-disk type for",
            type_
        );
    }
    // And the refusal left nothing behind, neither an entry nor a block.
    assert_eq!(entries(&root), vec![".", "..", "f", "d", "l"]);
}

// --- unlinking, and the trick that fills the hole

#[test]
fn unlinking_an_entry_in_the_middle_moves_the_last_one_into_its_place() {
    let (_fs, _storage, root) = new_root();
    for name in ["a", "b", "c"] {
        root.create(name, vfs::FileType::File, 0o644).unwrap();
    }
    assert_eq!(entries(&root), vec![".", "..", "a", "b", "c"]);
    root.unlink("b").unwrap();
    // `dirent_remove` does not shift the tail down: it copies the last entry
    // over the hole and shortens by one. So "c" is now where "b" was, and the
    // mistake the comment in the code warns about -- leaving the stale copy at
    // the end -- would show up as a fourth name here.
    assert_eq!(
        entries(&root),
        vec![".", "..", "a", "c"],
        "the hole was not filled with the last entry"
    );
    assert_eq!(root.metadata().unwrap().size, 4);
    assert_eq!(root.find("b").err(), Some(FsError::EntryNotFound));
    assert!(root.find("c").is_ok(), "the entry that was moved got lost");
}

#[test]
fn unlinking_the_last_entry_leaves_the_others_where_they_were() {
    let (_fs, _storage, root) = new_root();
    for name in ["a", "b", "c"] {
        root.create(name, vfs::FileType::File, 0o644).unwrap();
    }
    root.unlink("c").unwrap();
    assert_eq!(entries(&root), vec![".", "..", "a", "b"]);
    // The same again, to show that removing the last one is not a special case
    // that forgets to shorten.
    root.unlink("b").unwrap();
    assert_eq!(entries(&root), vec![".", "..", "a"]);
    assert_eq!(root.metadata().unwrap().size, 3);
}

#[test]
fn the_two_names_every_directory_has_cannot_be_unlinked() {
    let (_fs, _storage, root) = new_root();
    assert_eq!(root.unlink("."), Err(FsError::IsDir));
    assert_eq!(root.unlink(".."), Err(FsError::IsDir));
    assert_eq!(root.unlink("missing"), Err(FsError::EntryNotFound));
    assert_eq!(entries(&root), vec![".", ".."]);
}

#[test]
fn a_directory_with_something_in_it_is_not_unlinked() {
    let (_fs, _storage, root) = new_root();
    let dir = root.create("sub", vfs::FileType::Dir, 0o755).unwrap();
    dir.create("inside", vfs::FileType::File, 0o644).unwrap();
    assert_eq!(root.unlink("sub"), Err(FsError::DirNotEmpty));
    // Emptying it first is what makes the unlink go through, and the parent
    // gets back the link the child's `..` was holding.
    dir.unlink("inside").unwrap();
    root.unlink("sub").unwrap();
    assert_eq!(entries(&root), vec![".", ".."]);
    assert_eq!(
        root.metadata().unwrap().nlinks,
        2,
        "the parent kept the link the removed child's .. had"
    );
}

#[test]
fn an_inode_with_no_names_left_gives_its_block_and_its_file_back() {
    let (fs, storage, root) = new_root();
    let before_free = fs.info().bfree;
    let before_ids = storage.ids().len();
    let file = root.create("doomed", vfs::FileType::File, 0o644).unwrap();
    let id = file.metadata().unwrap().inode;
    assert!(storage.peek(id).is_some(), "the inode got no backing file");
    assert_eq!(fs.info().bfree, before_free - 1);
    assert_eq!(storage.ids().len(), before_ids + 1);
    root.unlink("doomed").unwrap();
    // The unlink only drops the link count. The block and the backing file come
    // back when the last handle goes, which is what `INodeImpl::drop` is for.
    assert!(
        storage.peek(id).is_some(),
        "the file went away while a handle was still open"
    );
    drop(file);
    assert_eq!(fs.info().bfree, before_free, "the block was not freed");
    assert!(
        storage.peek(id).is_none(),
        "the backing file outlived the inode"
    );
    assert_eq!(storage.ids().len(), before_ids);
}

// --- reading and writing

#[test]
fn writing_past_the_end_grows_the_file_and_leaves_zeros_in_the_gap() {
    let (_fs, _storage, root) = new_root();
    let file = root.create("holes", vfs::FileType::File, 0o644).unwrap();
    assert_eq!(file.write_at(0, b"abc").unwrap(), 3);
    assert_eq!(file.metadata().unwrap().size, 3);
    assert_eq!(file.write_at(10, b"z").unwrap(), 1);
    assert_eq!(
        file.metadata().unwrap().size,
        11,
        "the write past the end did not resize the inode"
    );
    let mut buf = [0xaau8; 11];
    assert_eq!(file.read_at(0, &mut buf).unwrap(), 11);
    assert_eq!(&buf, b"abc\0\0\0\0\0\0\0z");
    // A read that starts past the end gets nothing rather than an error.
    assert_eq!(file.read_at(11, &mut buf).unwrap(), 0);
    // And a write inside the file does not move the size.
    assert_eq!(file.write_at(1, b"B").unwrap(), 1);
    assert_eq!(file.metadata().unwrap().size, 11);
}

#[test]
fn resize_moves_the_size_in_the_inode_in_both_directions() {
    let (_fs, _storage, root) = new_root();
    let file = root.create("resized", vfs::FileType::File, 0o644).unwrap();
    file.write_at(0, b"0123456789").unwrap();
    file.resize(4).unwrap();
    assert_eq!(file.metadata().unwrap().size, 4);
    let mut buf = [0u8; 8];
    assert_eq!(file.read_at(0, &mut buf).unwrap(), 4, "the bytes were kept");
    assert_eq!(&buf[..4], b"0123");
    file.resize(6).unwrap();
    assert_eq!(file.metadata().unwrap().size, 6);
    assert_eq!(file.read_at(0, &mut buf).unwrap(), 6);
    assert_eq!(&buf[..6], b"0123\0\0", "growing did not zero the new tail");
}

#[test]
fn a_symlink_is_read_and_written_like_a_file_and_a_directory_is_not() {
    let (_fs, _storage, root) = new_root();
    // Both gates in `read_at`, `write_at` and `resize` read
    // `type_ != File && type_ != SymLink`, so a symlink has to go through and a
    // directory has to be turned away. Dropping either half of the condition
    // changes exactly one of these two.
    let link = root.create("link", vfs::FileType::SymLink, 0o777).unwrap();
    assert_eq!(link.write_at(0, b"/target").unwrap(), 7);
    let mut buf = [0u8; 7];
    assert_eq!(link.read_at(0, &mut buf).unwrap(), 7);
    assert_eq!(&buf, b"/target");
    assert_eq!(link.metadata().unwrap().size, 7, "a symlink counts bytes");
    link.resize(2).unwrap();
    assert_eq!(link.metadata().unwrap().size, 2);

    let mut buf = [0u8; 4];
    assert_eq!(root.read_at(0, &mut buf), Err(FsError::NotFile));
    assert_eq!(root.write_at(0, b"x"), Err(FsError::NotFile));
    assert_eq!(root.resize(0), Err(FsError::NotFile));
}

#[test]
fn set_metadata_writes_back_every_field_it_is_given_and_no_other() {
    let (_fs, _storage, root) = new_root();
    let file = root.create("owned", vfs::FileType::File, 0o600).unwrap();
    file.write_at(0, b"keep me").unwrap();
    let mut wanted = file.metadata().unwrap();
    wanted.mode = 0o751;
    wanted.uid = 1000;
    wanted.gid = 50;
    wanted.atime = vfs::Timespec { sec: 11, nsec: 0 };
    wanted.mtime = vfs::Timespec { sec: 22, nsec: 0 };
    wanted.ctime = vfs::Timespec { sec: 33, nsec: 0 };
    // Fields the on-disk inode has nowhere to put: they must come back as they
    // were, not as these.
    wanted.size = 999;
    wanted.inode = 7;
    wanted.nlinks = 9;
    file.set_metadata(&wanted).unwrap();

    let got = file.metadata().unwrap();
    assert_eq!(got.mode, 0o751);
    assert_eq!(got.uid, 1000);
    assert_eq!(got.gid, 50);
    assert_eq!(got.atime.sec, 11, "atime and mtime were crossed");
    assert_eq!(got.mtime.sec, 22);
    assert_eq!(got.ctime.sec, 33);
    assert_eq!(got.size, 7, "the size was taken from the metadata given");
    assert_eq!(got.nlinks, 1, "the link count was taken from outside");
    assert_ne!(got.inode, 7, "the inode number was overwritten");
}

#[test]
fn the_only_calls_this_filesystem_turns_down_outright_are_ioctl_and_mmap() {
    let (_fs, _storage, root) = new_root();
    let file = root.create("plain", vfs::FileType::File, 0o644).unwrap();
    assert_eq!(file.io_control(0, 0), Err(FsError::NotSupported));
    assert_eq!(
        file.mmap(vfs::MMapArea {
            start_vaddr: 0,
            end_vaddr: 0x1000,
            prot: 0,
            flags: 0,
            offset: 0,
        }),
        Err(FsError::NotSupported)
    );
    // `poll` is the other constant answer, and a file of this filesystem is
    // always ready and never hung up.
    let status = file.poll().unwrap();
    assert!(status.read && status.write);
    assert!(!status.error, "a plain file reported an error condition");
    assert!(!status.hangup, "a plain file reported a hangup");
}

#[test]
fn syncing_a_file_reaches_the_storage_underneath_it() {
    let (fs, storage, root) = new_root();
    let file = root.create("flushed", vfs::FileType::File, 0o644).unwrap();
    let id = file.metadata().unwrap().inode;
    let backing = storage.peek(id).unwrap();
    let before = backing.flushes();
    file.sync_data().unwrap();
    assert_eq!(
        backing.flushes(),
        before + 1,
        "sync_data did not reach the file underneath"
    );
    file.sync_all().unwrap();
    assert_eq!(
        backing.flushes(),
        before + 2,
        "sync_all does not include the data"
    );
    // And the filesystem's own sync reaches the meta file.
    let meta = storage.peek(0).unwrap();
    let before_meta = meta.flushes();
    fs.sync().unwrap();
    assert_eq!(meta.flushes(), before_meta + 1);
}

// --- hard links

#[test]
fn a_hard_link_is_a_second_name_for_one_inode() {
    let (_fs, _storage, root) = new_root();
    let file = root.create("first", vfs::FileType::File, 0o644).unwrap();
    file.write_at(0, b"shared").unwrap();
    root.link("second", &file).unwrap();
    assert_eq!(entries(&root), vec![".", "..", "first", "second"]);
    let id = file.metadata().unwrap().inode;
    assert_eq!(root.find("second").unwrap().metadata().unwrap().inode, id);
    assert_eq!(
        file.metadata().unwrap().nlinks,
        2,
        "the second name was not counted"
    );
    // Unlinking one name leaves the other, and the inode with it.
    root.unlink("first").unwrap();
    assert_eq!(file.metadata().unwrap().nlinks, 1);
    let mut buf = [0u8; 6];
    let still = root.find("second").unwrap();
    assert_eq!(still.read_at(0, &mut buf).unwrap(), 6);
    assert_eq!(&buf, b"shared");
}

#[test]
fn a_directory_is_not_hard_linked_and_a_taken_name_is_not_reused() {
    let (_fs, _storage, root) = new_root();
    let dir = root.create("sub", vfs::FileType::Dir, 0o755).unwrap();
    let file = root.create("f", vfs::FileType::File, 0o644).unwrap();
    assert_eq!(root.link("another", &dir), Err(FsError::IsDir));
    assert_eq!(root.link("f", &file), Err(FsError::EntryExist));
    assert_eq!(dir.link("x", &file), Ok(()), "a link into a subdirectory");
    assert_eq!(file.metadata().unwrap().nlinks, 2);
    assert_eq!(entries(&root), vec![".", "..", "sub", "f"]);
}

#[test]
fn an_inode_of_another_filesystem_is_not_linked_in() {
    let (_fs, _storage, root) = new_root();
    let (_other_fs, _other_storage, other_root) = new_root();
    let stranger = other_root
        .create("theirs", vfs::FileType::File, 0o644)
        .unwrap();
    // Both are `INodeImpl`, so the downcast succeeds and the only thing left to
    // tell them apart is which `Arc<SEFS>` they point at.
    assert_eq!(root.link("ours", &stranger), Err(FsError::NotSameFs));
    assert_eq!(
        root.move_("nothing", &other_root, "there"),
        Err(FsError::NotSameFs)
    );
    assert_eq!(entries(&root), vec![".", ".."]);
}

// --- renaming and moving

#[test]
fn renaming_inside_one_directory_writes_over_the_entry_that_was_there() {
    let (_fs, _storage, root) = new_root();
    for name in ["a", "b"] {
        root.create(name, vfs::FileType::File, 0o644).unwrap();
    }
    let id = root.find("a").unwrap().metadata().unwrap().inode;
    root.move_("a", &root, "renamed").unwrap();
    // A rename is the one case that does not append and remove: it overwrites
    // the entry in place, so "renamed" stays at index 2 and "b" does not move.
    assert_eq!(
        entries(&root),
        vec![".", "..", "renamed", "b"],
        "the rename did not happen where the old entry was"
    );
    assert_eq!(root.metadata().unwrap().size, 4, "the entry count moved");
    assert_eq!(root.find("renamed").unwrap().metadata().unwrap().inode, id);
    assert_eq!(root.find("a").err(), Some(FsError::EntryNotFound));
}

#[test]
fn moving_a_file_to_another_directory_takes_its_entry_with_it() {
    let (_fs, _storage, root) = new_root();
    let dir = root.create("sub", vfs::FileType::Dir, 0o755).unwrap();
    let file = root.create("travels", vfs::FileType::File, 0o644).unwrap();
    file.write_at(0, b"same inode").unwrap();
    let id = file.metadata().unwrap().inode;
    root.move_("travels", &dir, "arrived").unwrap();
    assert_eq!(entries(&root), vec![".", "..", "sub"]);
    assert_eq!(entries(&dir), vec![".", "..", "arrived"]);
    assert_eq!(dir.find("arrived").unwrap().metadata().unwrap().inode, id);
    assert_eq!(
        file.metadata().unwrap().nlinks,
        1,
        "a move is not a link: the count must not grow"
    );
    // A file carries no `..`, so neither directory's link count moves.
    assert_eq!(root.metadata().unwrap().nlinks, 3);
    assert_eq!(dir.metadata().unwrap().nlinks, 2);
}

#[test]
fn moving_a_directory_hands_its_parents_link_over_too() {
    let (_fs, _storage, root) = new_root();
    let home = root.create("home", vfs::FileType::Dir, 0o755).unwrap();
    let moved = root.create("moved", vfs::FileType::Dir, 0o755).unwrap();
    assert_eq!(root.metadata().unwrap().nlinks, 4, "two children, two ..");
    root.move_("moved", &home, "moved").unwrap();
    assert_eq!(
        root.metadata().unwrap().nlinks,
        3,
        "the old parent kept the link of the .. that left"
    );
    assert_eq!(
        home.metadata().unwrap().nlinks,
        3,
        "the new parent was not given the link of the .. it gained"
    );
    assert_eq!(moved.metadata().unwrap().nlinks, 2, "the child itself");
    assert_eq!(entries(&home), vec![".", "..", "moved"]);
}

#[test]
fn a_move_that_would_bury_a_name_or_a_dot_is_refused() {
    let (_fs, _storage, root) = new_root();
    let dir = root.create("sub", vfs::FileType::Dir, 0o755).unwrap();
    root.create("a", vfs::FileType::File, 0o644).unwrap();
    dir.create("taken", vfs::FileType::File, 0o644).unwrap();
    assert_eq!(root.move_("a", &dir, "taken"), Err(FsError::EntryExist));
    assert_eq!(root.move_(".", &dir, "dot"), Err(FsError::IsDir));
    assert_eq!(root.move_("..", &dir, "dotdot"), Err(FsError::IsDir));
    assert_eq!(root.move_("gone", &dir, "x"), Err(FsError::EntryNotFound));
    // A directory is not a destination unless it is one.
    let file = root.find("a").unwrap();
    assert_eq!(root.move_("a", &file, "x"), Err(FsError::NotDir));
    assert_eq!(entries(&root), vec![".", "..", "sub", "a"]);
    assert_eq!(entries(&dir), vec![".", "..", "taken"]);
}

// --- the block allocator and the second group

#[test]
fn the_freemap_of_a_new_group_is_reserved_before_anything_is_handed_out() {
    let (fs, _storage) = new_fs();
    // One group holds `BLKBITS` blocks: the superblock, one freemap block, the
    // root inode, and the rest free.
    let free_now = fs.info().bfree;
    assert_eq!(free_now, BLKBITS - 3);
    let mut handed = Vec::new();
    for _ in 0..free_now {
        handed.push(fs.alloc_block().expect("a block the free count promised"));
    }
    assert_eq!(fs.info().bfree, 0, "the free count did not reach the floor");
    assert!(
        !handed.contains(&BLKN_SUPER) && !handed.contains(&BLKN_FREEMAP),
        "a block the superblock lives in was handed out"
    );

    // The next one has nowhere to come from, so a group is grown for it.
    let first_of_group = fs.alloc_block().expect("the second group was not grown");
    let info = fs.info();
    assert_eq!(info.blocks, 2 * BLKBITS, "the new group's blocks");
    assert_eq!(
        first_of_group, BLKBITS,
        "the first block of the new group is the one right after the old group"
    );
    // The new group needs a block for its own freemap, and it is the one at
    // `BLKBITS * 1 + BLKN_FREEMAP`. Getting that offset wrong is the mistake
    // this test is here for: the next block handed out must skip it.
    let next = fs.alloc_block().unwrap();
    assert_eq!(
        next,
        BLKBITS + BLKN_FREEMAP + 1,
        "the freemap block of the new group was handed out as storage"
    );
    // Of the group's `BLKBITS` blocks, one went to its freemap and two were
    // just handed out.
    assert_eq!(info.blocks - info.bfree, BLKBITS + 2);
    for id in handed {
        fs.free_block(id);
    }
    fs.free_block(first_of_group);
    fs.free_block(next);
    // Everything handed out is back, and what stays in use is the superblock,
    // one freemap block per group, and the root inode.
    assert_eq!(
        fs.info().bfree,
        2 * BLKBITS - 4,
        "freeing did not give every block back"
    );
}

#[test]
fn the_freemap_block_of_each_group_is_one_block_into_it() {
    // Three groups' worth, without allocating anything: the offset is pure
    // arithmetic and the whole on-disk layout depends on it.
    assert_eq!(SEFS::get_freemap_block_id_of_group(0), BLKN_FREEMAP);
    assert_eq!(
        SEFS::get_freemap_block_id_of_group(1),
        BLKBITS + BLKN_FREEMAP
    );
    assert_eq!(
        SEFS::get_freemap_block_id_of_group(2),
        2 * BLKBITS + BLKN_FREEMAP
    );
}

#[test]
fn the_filesystem_forgets_the_inodes_whose_last_handle_is_gone() {
    let (fs, _storage, root) = new_root();
    let file = root.create("kept", vfs::FileType::File, 0o644).unwrap();
    let id = file.metadata().unwrap().inode;
    assert_eq!(fs.inodes.read().len(), 2, "the root and the new file");
    drop(file);
    // The map keeps a `Weak`, so the entry is still there, dangling, until
    // something sweeps it. A `find` has to rebuild the inode from the meta
    // file rather than hand back the dead one.
    let again = root.find("kept").unwrap();
    assert_eq!(again.metadata().unwrap().inode, id);
    drop(again);
    fs.sync().unwrap();
    assert_eq!(
        fs.inodes.read().len(),
        1,
        "the dangling entries were not swept"
    );
    assert!(fs.inodes.read().contains_key(&BLKN_ROOT));
}

// --- names on disk

#[test]
fn a_name_of_the_longest_length_survives_the_round_trip() {
    let (_fs, storage, root) = new_root();
    let long = "n".repeat(MAX_FNAME_LEN);
    root.create(&long, vfs::FileType::File, 0o644).unwrap();
    assert_eq!(root.get_entry(2).unwrap(), long, "the name came back short");
    assert!(root.find(&long).is_ok());
    // On disk an entry is four bytes of inode number and then the name, and the
    // name field is 256 bytes however long the name is.
    let dir_file = storage.peek(BLKN_ROOT).unwrap();
    assert_eq!(
        dir_file.len(),
        3 * DIRENT_SIZE,
        "three entries do not take three entries' worth of bytes"
    );
    let bytes = dir_file.snapshot();
    assert_eq!(
        &bytes[2 * DIRENT_SIZE + 4..][..MAX_FNAME_LEN],
        long.as_bytes()
    );
    assert_eq!(
        bytes[2 * DIRENT_SIZE + 4 + MAX_FNAME_LEN],
        0,
        "the longest name left no terminator"
    );
}

#[test]
fn a_short_name_written_over_a_long_one_does_not_keep_its_tail() {
    let (_fs, _storage, root) = new_root();
    let long = "x".repeat(MAX_FNAME_LEN);
    root.create(&long, vfs::FileType::File, 0o644).unwrap();
    // A rename writes the new entry over the old one in place, so the 256-byte
    // name field still holds the old name's bytes unless it is rewritten whole.
    root.move_(&long, &root, "s").unwrap();
    assert_eq!(
        root.get_entry(2).unwrap(),
        "s",
        "the old name's tail came back with the new one"
    );
    assert_eq!(root.find(&long).err(), Some(FsError::EntryNotFound));
    assert!(root.find("s").is_ok());
}

#[test]
fn an_entry_name_is_read_up_to_its_first_zero_byte() {
    let short = Str256::from("ab");
    assert_eq!(short.as_ref(), "ab");
    assert_eq!(format!("{:?}", short), "ab", "Debug is not the name itself");
    let empty = Str256::from("");
    assert_eq!(empty.as_ref(), "");
    let longest = Str256::from("y".repeat(MAX_FNAME_LEN).as_str());
    assert_eq!(longest.as_ref().len(), MAX_FNAME_LEN);
}

#[test]
fn the_debug_line_of_an_inode_names_its_number_and_what_is_on_disk() {
    let (_fs, _storage, root) = new_root();
    let printed = format!("{:?}", as_impl(&root));
    // `INodeImpl`'s `Debug` is written by hand and the only two things it says
    // are the id and the on-disk inode, which carries its own clean-or-dirty
    // tag from `Dirty`.
    assert!(
        printed.contains(&format!("id: {}", BLKN_ROOT)),
        "the inode number is missing: {}",
        printed
    );
    assert!(
        printed.contains("Dir"),
        "the file type is missing: {}",
        printed
    );
    assert!(
        printed.contains("Clean") || printed.contains("Dirty"),
        "the dirty tag is missing: {}",
        printed
    );
}

// --- the storage the crate shipped with: a real directory

#[test]
fn a_filesystem_on_a_real_directory_is_read_back_from_its_files() {
    let dir = std::env::temp_dir().join(format!(
        "sefs-test-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("failed to make the test directory");

    let id = {
        let fs = SEFS::create(Box::new(dev::StdStorage::new(&dir)), &CLOCK).unwrap();
        let root = fs.root_inode();
        let file = root.create("on-disk", vfs::FileType::File, 0o644).unwrap();
        file.write_at(0, b"written through std::fs").unwrap();
        fs.sync().unwrap();
        file.metadata().unwrap().inode
    };
    // One host file per inode, named by its id, plus the meta file at 0.
    for name in ["0", &id.to_string()] {
        assert!(
            dir.join(name).is_file(),
            "the storage has no file {} in it",
            name
        );
    }

    let fs = SEFS::open(Box::new(dev::StdStorage::new(&dir)), &CLOCK)
        .expect("the directory it just wrote is not a SEFS");
    let root = fs.root_inode();
    let file = root
        .find("on-disk")
        .expect("the entry did not reach the disk");
    let mut buf = [0u8; 23];
    assert_eq!(file.read_at(0, &mut buf).unwrap(), 23);
    assert_eq!(&buf, b"written through std::fs");

    // And removing the last name takes the host file with it.
    root.unlink("on-disk").unwrap();
    drop(file);
    assert!(
        !dir.join(id.to_string()).exists(),
        "the host file outlived the inode it belonged to"
    );
    drop(root);
    drop(fs);
    std::fs::remove_dir_all(&dir).ok();
}
