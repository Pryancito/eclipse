//! The v0.3 driver through its public calls: what the integration tests
//! leave out (attributes, growing, refusals, a failing device), and fsck
//! fed the damage it looks for.

use core::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::mkfs;
use crate::testdisk::{clock, opts, Ram, MIB};

/// A RAM disk whose writes at or past byte `fail_from` fail, and whose
/// reads of block `fail_read` fail.
struct Faulty {
    ram: Arc<Ram>,
    fail_from: AtomicU64,
    fail_read: AtomicU64,
}

impl BlockDevice for Faulty {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let b = self.fail_read.load(Ordering::Relaxed) * BLOCK_SIZE;
        if offset < b + BLOCK_SIZE && offset + buf.len() as u64 > b {
            return Err(Error::Io);
        }
        self.ram.read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        if offset + buf.len() as u64 > self.fail_from.load(Ordering::Relaxed) {
            return Err(Error::Io);
        }
        self.ram.write_at(offset, buf)
    }
    fn sync(&self) -> Result<()> {
        Ok(())
    }
    fn size(&self) -> u64 {
        self.ram.size()
    }
}

fn formatted(mib: usize) -> Arc<Ram> {
    let d = Ram::new(mib * MIB);
    mkfs::format_v3(&*d, &opts()).unwrap();
    d
}

fn mounted(mib: usize) -> (Arc<Ram>, EclipseFsV3) {
    let d = formatted(mib);
    let mut fs = EclipseFsV3::mount(d.clone(), false).unwrap();
    fs.set_clock(clock);
    (d, fs)
}

fn file(fs: &mut EclipseFsV3, dir: u64, name: &str) -> u64 {
    fs.create(dir, name, FileKind::Regular, 0o644, 0).unwrap()
}

fn dir(fs: &mut EclipseFsV3, parent: u64, name: &str) -> u64 {
    fs.create(parent, name, FileKind::Dir, 0o755, 0).unwrap()
}

/// The image of `from` on a device of `bytes`.
fn copied(from: &Ram, bytes: usize) -> Arc<Ram> {
    let to = Ram::new(bytes);
    let n = from.size().min(bytes as u64) as usize;
    to.write_at(0, &from.peek(0, n)).unwrap();
    to
}

#[test]
fn accessors_and_attributes() {
    let (_, mut fs) = mounted(8);
    assert_eq!(fs.label(), "unit");
    assert_ne!(fs.uuid(), [0; 16]);
    assert_eq!(fs.nodesize(), BLOCK_SIZE as usize);
    assert_eq!(fs.sectorsize(), BLOCK_SIZE as usize);
    let f = file(&mut fs, ROOT_INO, "f");
    fs.set_attr(f, Some(0o600), Some(7), Some(8), Some((1, 2)), Some((3, 4)))
        .unwrap();
    let st = fs.stat(f).unwrap();
    assert_eq!((st.mode & 0o7777, st.uid, st.gid), (0o600, 7, 8));
    assert_eq!((st.atime, st.mtime), ((1, 2), (3, 4)));
    fs.set_attr(f, None, None, None, None, None).unwrap();
    fs.check().unwrap();
}

/// Growing fills the last group, adds groups while they are big enough
/// to be one, and does nothing when there is nothing to add.
#[test]
fn growing_to_the_device() {
    let small = formatted(8);
    let group = (GROUP_BLOCKS * BLOCK_SIZE) as usize;
    let d = copied(&small, group + 100 * BLOCK_SIZE as usize);
    let mut fs = EclipseFsV3::mount(d.clone(), false).unwrap();
    assert!(fs.grow_to_device().unwrap());
    assert_eq!(fs.sb.total_blocks, GROUP_BLOCKS + 100);
    assert!(!fs.grow_to_device().unwrap());
    fs.check().unwrap();
    drop(fs);

    let d = copied(&small, group + 10 * BLOCK_SIZE as usize);
    let mut fs = EclipseFsV3::mount(d, false).unwrap();
    assert!(fs.grow_to_device().unwrap());
    assert_eq!(fs.sb.total_blocks, GROUP_BLOCKS);
    assert!(!fs.grow_to_device().unwrap());
    fs.check().unwrap();
}

#[test]
fn mounts_that_cannot_be() {
    let d = formatted(8);
    assert_eq!(
        EclipseFsV3::mount(Ram::new(100), false).err(),
        Some(Error::BadSuperblock)
    );
    assert_eq!(
        EclipseFsV3::mount(copied(&d, 4 * MIB), false).err(),
        Some(Error::Corrupt("filesystem larger than device"))
    );
    let mut ro = EclipseFsV3::mount(d, true).unwrap();
    assert!(matches!(
        ro.create(ROOT_INO, "x", FileKind::Regular, 0o644, 0),
        Err(Error::Unsupported(_))
    ));
    ro.sync().unwrap();
}

/// Calls given the wrong kind of inode or a name that is taken.
#[test]
fn calls_that_do_not_fit_the_inode() {
    let (_, mut fs) = mounted(8);
    let f = file(&mut fs, ROOT_INO, "f");
    let d = dir(&mut fs, ROOT_INO, "d");
    let full = dir(&mut fs, ROOT_INO, "full");
    file(&mut fs, full, "x");
    let l = fs.symlink(ROOT_INO, "l", b"f").unwrap();

    assert_eq!(fs.symlink(ROOT_INO, "e", b""), Err(Error::Invalid));
    assert_eq!(fs.write_symlink(f, 0, b"x"), Err(Error::Invalid));
    assert_eq!(fs.read_link(f), Err(Error::Invalid));
    assert_eq!(fs.parent(ROOT_INO), Ok(ROOT_INO));
    assert_eq!(fs.link(ROOT_INO, "d2", d), Err(Error::IsDir));
    assert_eq!(fs.link(ROOT_INO, "d", f), Err(Error::Exists));
    assert_eq!(fs.read(d, 0, &mut [0; 4]), Err(Error::IsDir));
    assert_eq!(fs.write(f, 0, &[]), Ok(0));
    assert_eq!(fs.write(d, 0, b"x"), Err(Error::IsDir));
    assert_eq!(fs.write(l, 0, b"x"), Err(Error::Invalid));
    assert_eq!(fs.truncate(d, 0), Err(Error::IsDir));
    assert_eq!(fs.truncate(l, 0), Err(Error::Invalid));
    assert_eq!(fs.stat(1 << 40).err(), Some(Error::NotFound));
    assert_eq!(fs.lookup(f, "x"), Err(Error::NotDir));

    // Renames onto what is there.
    fs.link(ROOT_INO, "f2", f).unwrap();
    fs.rename(ROOT_INO, "f", ROOT_INO, "f2").unwrap();
    assert_eq!(fs.rename(ROOT_INO, "f2", ROOT_INO, "d"), Err(Error::IsDir));
    assert_eq!(
        fs.rename(ROOT_INO, "d", ROOT_INO, "full"),
        Err(Error::NotEmpty)
    );
    fs.check().unwrap();
}

/// A symlink rewritten from a block-sized target to a short one gives its
/// block back.
#[test]
fn a_long_symlink_target_rewritten() {
    let (_, mut fs) = mounted(8);
    let long = vec![b'a'; 1000];
    let l = fs.symlink(ROOT_INO, "l", &long).unwrap();
    fs.write_symlink(l, 0, &[b'b'; 2000]).unwrap();
    assert_eq!(fs.read_link(l).unwrap().len(), 2000);
    fs.check().unwrap();
}

/// More inodes and directories than the caches keep.
#[test]
fn caches_evict_and_reload() {
    let (_, mut fs) = mounted(32);
    let mut dirs = Vec::new();
    for i in 0..300 {
        let d = dir(&mut fs, ROOT_INO, &alloc::format!("d{}", i));
        file(&mut fs, d, "f");
        dirs.push(d);
    }
    for &d in &dirs {
        assert_eq!(fs.readdir(d).unwrap().len(), 1);
    }
    for i in 0..INODE_CACHE {
        file(&mut fs, dirs[0], &alloc::format!("g{}", i));
    }
    assert!(fs.inodes.len() <= INODE_CACHE);
    fs.check().unwrap();
}

/// A full disk: entries and symlink targets that do not fit.
#[test]
fn names_on_a_full_disk() {
    let (_, mut fs) = mounted(8);
    let big = file(&mut fs, ROOT_INO, "big");
    let chunk = vec![1u8; 256 * 1024];
    let mut off = 0;
    while let Ok(n) = fs.write(big, off, &chunk) {
        off += n as u64;
    }
    while let Ok(n) = fs.write(big, off, &[1; BLOCK_SIZE as usize]) {
        off += n as u64;
    }
    let mut failed = false;
    for i in 0..2000 {
        let name = alloc::format!("{:0>200}", i);
        if fs
            .create(ROOT_INO, &name, FileKind::Regular, 0o644, 0)
            .is_err()
        {
            failed = true;
            break;
        }
    }
    assert!(failed);
    assert!(fs.symlink(ROOT_INO, "s", &[b'x'; 3000]).is_err());
    fs.check().unwrap();
}

/// Writes the device refuses: data, a commit kept for the next one, the
/// commit on unmount, and a journal replayed by a read-only mount.
#[test]
fn a_device_that_fails_writes() {
    let ram = formatted(8);
    let dev = Arc::new(Faulty {
        ram: ram.clone(),
        fail_from: AtomicU64::new(u64::MAX),
        fail_read: AtomicU64::new(u64::MAX / BLOCK_SIZE),
    });
    let mut fs = EclipseFsV3::mount(dev.clone(), false).unwrap();
    fs.set_clock(clock);
    let f = file(&mut fs, ROOT_INO, "f");
    fs.sync().unwrap();
    let j = fs.journal.unwrap();
    dev.fail_from
        .store((j.start + j.blocks) * BLOCK_SIZE, Ordering::Relaxed);
    assert_eq!(fs.write(f, 10, b"x"), Err(Error::Io));
    assert_eq!(fs.write(f, 0, &[1; BLOCK_SIZE as usize]), Err(Error::Io));
    fs.set_attr(f, Some(0o600), None, None, None, None).unwrap();
    assert_eq!(fs.sync(), Err(Error::Io));
    drop(fs);
    // The journal holds the last commit.
    let mut ro = EclipseFsV3::mount(ram.clone(), true).unwrap();
    assert_eq!(ro.stat(f).unwrap().mode & 0o7777, 0o600);
    drop(ro);
    let mut fs = EclipseFsV3::mount(ram, false).unwrap();
    fs.check().unwrap();
}

/// A superblock replayed from the journal that the device cannot hold.
#[test]
fn a_journal_that_grows_the_filesystem_past_the_device() {
    let d = formatted(8);
    let mut buf = vec![0u8; BLOCK_SIZE as usize];
    d.read_at(0, &mut buf).unwrap();
    let sb = Superblock::decode(&buf).unwrap();
    let j = JournalArea {
        start: sb.journal_start,
        blocks: sb.journal_blocks as u64,
    };
    let mut big = sb.clone();
    big.total_blocks += 1;
    j.write(&*d, &sb.uuid, 1, &[(0, big.encode())]).unwrap();
    assert_eq!(
        EclipseFsV3::mount(d, true).err(),
        Some(Error::Corrupt("filesystem larger than device"))
    );
}

#[test]
fn a_root_that_is_not_a_directory() {
    let (d, mut fs) = mounted(8);
    let mut root = fs.take(ROOT_INO).unwrap();
    root.kind = FileKind::Regular;
    fs.put(ROOT_INO, root).unwrap();
    fs.sync().unwrap();
    drop(fs);
    assert_eq!(
        EclipseFsV3::mount(d, false).err(),
        Some(Error::Corrupt("root is not a directory"))
    );
}

/// Space freed in the open transaction is committed early once it is what
/// the next call needs.
#[test]
fn freed_space_commits_when_it_runs_short() {
    let (_, mut fs) = mounted(8);
    let big = file(&mut fs, ROOT_INO, "big");
    let chunk = vec![1u8; MIB];
    let mut at = 0;
    while let Ok(n) = fs.write(big, at, &chunk) {
        at += n as u64;
    }
    fs.sync().unwrap();
    fs.unlink(ROOT_INO, "big").unwrap();
    assert!(fs.alloc.deferred_blocks() > 0);
    file(&mut fs, ROOT_INO, "next");
    assert_eq!(fs.alloc.deferred_blocks(), 0);
    fs.check().unwrap();
}

/// Inodes whose sizes or parents cannot be.
#[test]
fn inodes_out_of_bounds() {
    let (_, mut fs) = mounted(8);
    let d = dir(&mut fs, ROOT_INO, "d");
    let f = file(&mut fs, ROOT_INO, "f");
    let l = fs.symlink(ROOT_INO, "l", b"f").unwrap();
    fs.dirs.remove(&d);
    fs.inodes.get_mut(&d).unwrap().size = MAX_DIR_BYTES + 1;
    assert_eq!(fs.lookup(d, "x"), Err(Error::Corrupt("directory size")));
    fs.inodes.get_mut(&l).unwrap().size = MAX_SYMLINK_LEN as u64 + 1;
    assert_eq!(fs.read_link(l), Err(Error::Corrupt("symlink size")));
    fs.inodes.get_mut(&f).unwrap().parent = 0;
    assert_eq!(fs.parent(f), Err(Error::NotFound));
}

/// A compaction that cannot write drops the cached directory.
#[test]
fn a_compaction_that_fails() {
    let ram = formatted(8);
    let dev = Arc::new(Faulty {
        ram,
        fail_from: AtomicU64::new(u64::MAX),
        fail_read: AtomicU64::new(u64::MAX / BLOCK_SIZE),
    });
    let mut fs = EclipseFsV3::mount(dev.clone(), false).unwrap();
    let d = dir(&mut fs, ROOT_INO, "d");
    let mut i = 0;
    // Three blocks and more than half of a fourth.
    while fs.inode(d).unwrap().size < 3 * BLOCK_SIZE + BLOCK_SIZE / 2 {
        file(&mut fs, d, &format!("entry-with-a-long-name-{i:04}"));
        i += 1;
    }
    fs.sync().unwrap();
    // Entries of the first block and of the last two go; the compacted
    // tail lands in the second block, which cannot be read.
    let gone: Vec<String> = fs.dirs[&d]
        .entries
        .iter()
        .filter(|(_, s)| s.off + s.len <= BLOCK_SIZE || s.off >= 2 * BLOCK_SIZE)
        .map(|(n, _)| n.clone())
        .collect();
    let (p, _) = fs.inode(d).unwrap().extents.lookup(1).unwrap();
    dev.fail_read.store(p, Ordering::Relaxed);
    let mut failed = false;
    for n in gone {
        if fs.unlink(d, &n) == Err(Error::Io) {
            failed = true;
            break;
        }
    }
    assert!(failed);
    assert!(!fs.dirs.contains_key(&d));
}

// ---------------------------------------------------------------------------
// fsck
// ---------------------------------------------------------------------------

/// A filesystem with a directory `d` holding a file `f` of two blocks.
fn sample() -> (EclipseFsV3, u64, u64) {
    let (_, mut fs) = mounted(8);
    let d = dir(&mut fs, ROOT_INO, "d");
    let f = file(&mut fs, d, "f");
    fs.write(f, 0, &[7; 2 * BLOCK_SIZE as usize]).unwrap();
    fs.sync().unwrap();
    (fs, d, f)
}

fn fsck(fs: &mut EclipseFsV3) -> &'static str {
    match fs.check() {
        Err(Error::Corrupt(w)) => w,
        r => panic!("{:?}", r),
    }
}

#[test]
fn fsck_of_blocks() {
    let (mut fs, _, f) = sample();
    let j = fs.journal.unwrap();
    fs.sb.total_blocks = j.start + 1;
    assert_eq!(fsck(&mut fs), "fsck: block past the end");

    let (mut fs, _, f2) = sample();
    fs.inodes.get_mut(&f2).unwrap().chain = vec![0];
    assert_eq!(fsck(&mut fs), "fsck: block used twice");
    let _ = f;

    let (mut fs, _, f) = sample();
    fs.inodes.get_mut(&f).unwrap().nbytes += BLOCK_SIZE;
    assert_eq!(fsck(&mut fs), "fsck: nbytes");

    let (mut fs, d, _) = sample();
    let g = fs.symlink(d, "s", b"tiny").unwrap();
    assert!(fs.inodes[&g].is_inline());
    fs.inodes.get_mut(&g).unwrap().size = 9;
    assert_eq!(fsck(&mut fs), "fsck: inline size");

    let (mut fs, _, f) = sample();
    let e = fs.inodes[&f].extents.extents()[0];
    fs.alloc.free(e.physical_block, 1).unwrap();
    fs.sb.free_blocks = fs.alloc.free_blocks();
    assert_eq!(fsck(&mut fs), "fsck: block in use marked free");

    let (mut fs, _, _) = sample();
    fs.journal = None;
    assert_eq!(fsck(&mut fs), "fsck: block marked used but unowned");

    let (mut fs, _, _) = sample();
    fs.read_only = true;
    fs.check().unwrap();
}

#[test]
fn fsck_of_inodes_and_names() {
    let (mut fs, _, _) = sample();
    fs.sb.inode_count += 1;
    assert_eq!(fsck(&mut fs), "fsck: inode count");

    let (mut fs, _, _) = sample();
    fs.inodes.get_mut(&ROOT_INO).unwrap().kind = FileKind::Regular;
    assert_eq!(fsck(&mut fs), "fsck: root");

    let (mut fs, d, _) = sample();
    fs.add_entry(d, "ghost", 1 << 20, FileKind::Regular)
        .unwrap();
    assert_eq!(fsck(&mut fs), "fsck: entry for a free inode");

    let (mut fs, d, f) = sample();
    fs.add_entry(d, "wrong", f, FileKind::Dir).unwrap();
    assert_eq!(fsck(&mut fs), "fsck: entry kind");

    let (mut fs, d, _) = sample();
    fs.inodes.get_mut(&d).unwrap().parent = d;
    assert_eq!(fsck(&mut fs), "fsck: directory parent");

    let (mut fs, _, f) = sample();
    fs.inodes.get_mut(&f).unwrap().nlink = 5;
    assert_eq!(fsck(&mut fs), "fsck: link count");

    // a holds b holds a, cut off from the root.
    let (mut fs, a, _) = sample();
    let b = dir(&mut fs, a, "b");
    fs.remove_entry(ROOT_INO, "d").unwrap();
    fs.add_entry(b, "a", a, FileKind::Dir).unwrap();
    fs.inodes.get_mut(&a).unwrap().parent = b;
    fs.inodes.get_mut(&b).unwrap().nlink = 3;
    fs.inodes.get_mut(&ROOT_INO).unwrap().nlink = 2;
    assert_eq!(fsck(&mut fs), "fsck: directory cycle");

    // An inode record that does not decode.
    let (mut fs, _, f) = sample();
    fs.inodes.remove(&f);
    fs.write_record(f, &[0xff; INODE_SIZE]).unwrap();
    assert!(fs.check().is_err());
}
