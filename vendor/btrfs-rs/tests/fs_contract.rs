#![cfg(feature = "std")]
//! What the filesystem promises about the numbers nobody reads back.
//!
//! The tests here were written against surviving mutants. Each of them
//! changes a number the driver writes down rather than the data it stores:
//! the size a directory reports, the total a grown device reports, the
//! longest target a symlink may hold. The whole suite stayed green through
//! all three, because every other test asks what a file contains and never
//! what the accounting says.
//!
//! A reader does ask. `ls -ld` prints a directory's size, `df` prints the
//! device total, and a growth check decides from that total whether to
//! rewrite the superblock at all. So the numbers are stated here, at their
//! exact boundaries, where an off-by-one has somewhere to show.

use std::path::PathBuf;
use std::sync::Arc;

use btrfs::device::{BlockDevice, FileDevice};
use btrfs::structs::MAX_NAME_LEN;
use btrfs::{mkfs, Btrfs, Error, FileKind};

const MIB: u64 = 1024 * 1024;

/// Big enough for mkfs (1 MiB reserved, 4 MiB system, 8 MiB metadata and a
/// data chunk), small enough to format in no time.
const MKFS_SIZE: u64 = 24 * MIB;

fn opts() -> mkfs::MkfsOptions {
    mkfs::MkfsOptions {
        label: "contract".into(),
        fsid: [0x51; 16],
        chunk_uuid: [0x52; 16],
        dev_uuid: [0x53; 16],
        subvol_uuid: [0x54; 16],
        now: (1_700_000_000, 0),
    }
}

fn tmpfile(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("btrfs-contract-{}-{}", std::process::id(), name))
}

/// Present `path` as a device of exactly `size` bytes. `truncate` is spelled
/// out because the point of several tests below is that a device keeps the
/// size they gave it.
fn open_dev(path: &PathBuf, size: u64) -> Arc<dyn BlockDevice> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap();
    f.set_len(size).unwrap();
    Arc::new(FileDevice::open(f).unwrap())
}

/// Format `size` bytes and mount the result read-write on a device of
/// exactly that size.
fn fresh(name: &str, size: u64) -> (Btrfs, PathBuf) {
    let path = tmpfile(name);
    let _ = std::fs::remove_file(&path);
    let dev = open_dev(&path, size);
    mkfs::format(&*dev, &opts()).unwrap();
    drop(dev);
    let fs = Btrfs::mount(open_dev(&path, size), false).unwrap();
    (fs, path)
}

#[test]
fn a_filesystem_that_already_fills_its_device_reports_no_growth() {
    let (mut fs, path) = fresh("nogrow", MKFS_SIZE);
    let before = fs.fsinfo().total_bytes;

    // The device is exactly the size the format described, so there is
    // nothing to grow into. The installer calls this on every boot; a bound
    // that treats "same size" as "bigger" rewrites the device item and both
    // superblock totals for a zero-byte growth and reports success, and the
    // only way to see it is to ask for the answer.
    assert!(
        !fs.grow_to_device().unwrap(),
        "a filesystem that already describes its whole device reported a growth"
    );
    assert_eq!(
        fs.fsinfo().total_bytes,
        before,
        "the total moved on a growth that did not happen"
    );
    drop(fs);

    // The complement, so this is a boundary and not just a refusal: the same
    // image on a bigger device grows, and by the whole difference.
    const ADDED: u64 = 40 * MIB;
    let mut fs = Btrfs::mount(open_dev(&path, MKFS_SIZE + ADDED), false).unwrap();
    assert!(
        fs.grow_to_device().unwrap(),
        "did not grow into a device that got bigger"
    );
    assert_eq!(
        fs.fsinfo().total_bytes,
        before + ADDED,
        "grew by something other than the space that was added"
    );
    // Having grown, it fills this device too, so a second call is a no-op.
    assert!(
        !fs.grow_to_device().unwrap(),
        "grew a second time into a device it already fills"
    );

    drop(fs);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_directory_charges_itself_for_every_name_it_holds() {
    let (mut fs, path) = fresh("dirsize", MKFS_SIZE);
    let root = fs.root_ino();
    let empty = fs.stat(root).unwrap().size;

    // btrfs charges a directory twice the length of each name it holds, once
    // for the DIR_ITEM and once for the DIR_INDEX. Nothing else here reads
    // that number back, so an unlink that adds the length instead of taking
    // it away leaves a directory whose reported size only ever grows, and
    // every test that lists it still passes.
    fs.create(root, "alpha", FileKind::Regular, 0o644, 0)
        .unwrap();
    assert_eq!(
        fs.stat(root).unwrap().size,
        empty + 10,
        "a five-character name cost something other than ten"
    );
    fs.create(root, "be", FileKind::Regular, 0o644, 0).unwrap();
    assert_eq!(
        fs.stat(root).unwrap().size,
        empty + 14,
        "a second, two-character name cost something other than four"
    );

    fs.unlink(root, "alpha").unwrap();
    assert_eq!(
        fs.stat(root).unwrap().size,
        empty + 4,
        "removing the five-character name did not give its ten bytes back"
    );
    fs.unlink(root, "be").unwrap();
    assert_eq!(
        fs.stat(root).unwrap().size,
        empty,
        "an emptied directory did not come back to the size it started at"
    );

    drop(fs);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_symlink_target_may_be_exactly_as_long_as_the_ceiling_allows() {
    let (mut fs, path) = fresh("symlink", MKFS_SIZE);
    let root = fs.root_ino();
    const CEILING: usize = MAX_NAME_LEN * 16;

    // The VFS creates a symlink empty and fills its target through write_at,
    // so the ceiling is enforced on that write. It is the longest target
    // this driver takes, not the first length it refuses: one off here turns
    // a legal symlink into EINVAL, and nothing else comes near the bound.
    let ino = fs.symlink(root, "longest", b"x").unwrap();
    let target = vec![b'y'; CEILING];
    assert_eq!(
        fs.write_symlink(ino, 0, &target).unwrap(),
        CEILING,
        "the longest legal target was not written whole"
    );
    assert_eq!(
        fs.read_link(ino).unwrap(),
        target,
        "the longest legal target did not read back"
    );

    let over = fs.symlink(root, "toolong", b"x").unwrap();
    assert_eq!(
        fs.write_symlink(over, 0, &vec![b'z'; CEILING + 1]),
        Err(Error::Invalid),
        "one byte past the ceiling was accepted"
    );

    drop(fs);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn an_inline_extent_reads_from_an_offset_inside_it() {
    let (mut fs, path) = fresh("inlineoff", MKFS_SIZE);
    let root = fs.root_ino();

    // A symlink target is the one thing this driver stores as an
    // uncompressed inline extent, and read_link only ever asks for it from
    // zero, so no read of an inline extent has ever started anywhere else.
    // An inline extent begins at file offset 0 by definition: the window has
    // to start where the caller asked, and starting it at the extent instead
    // underflows the index into the caller's buffer.
    let target: Vec<u8> = (0..200u32).map(|i| b'a' + (i % 26) as u8).collect();
    let ino = fs.symlink(root, "link", &target).unwrap();
    assert_eq!(
        fs.read_link(ino).unwrap(),
        target,
        "the target did not survive"
    );

    for &(off, len) in &[(1usize, 199usize), (64, 100), (199, 1), (120, 80)] {
        let mut got = vec![0u8; len];
        assert_eq!(
            fs.read(ino, off as u64, &mut got).unwrap(),
            len,
            "short read of an inline extent at offset {}",
            off
        );
        assert_eq!(
            got,
            target[off..off + len],
            "wrong bytes reading an inline extent at offset {}",
            off
        );
    }

    drop(fs);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_truncate_gives_back_the_blocks_it_drops() {
    let (mut fs, path) = fresh("nbytes", MKFS_SIZE);
    let root = fs.root_ino();
    let ino = fs
        .create(root, "shrink", FileKind::Regular, 0o644, 0)
        .unwrap();

    let block = vec![0x5au8; 64 * 1024];
    assert_eq!(fs.write(ino, 0, &block).unwrap(), block.len());
    assert_eq!(fs.write(ino, MIB, &block).unwrap(), block.len());
    let two = fs.stat(ino).unwrap().nbytes;
    assert!(
        two > 0,
        "a file with two extents accounts for no blocks at all"
    );

    // nbytes is where st_blocks comes from, so it is what du reports. A
    // truncate has to give back the bytes it stops pointing at. Adding them
    // instead leaves a shrinking file whose block count only grows, and
    // every test that reads the data back still passes.
    //
    // The extent past the new end is dropped whole and freed.
    fs.truncate(ino, 64 * 1024).unwrap();
    let one = fs.stat(ino).unwrap().nbytes;
    assert!(
        one < two,
        "dropping the far extent did not reduce the block count ({} -> {})",
        two,
        one
    );

    // The one straddling the new end keeps its disk extent and shrinks its
    // mapping, so only the part that went away comes off the count.
    fs.truncate(ino, 40 * 1024).unwrap();
    let cut = fs.stat(ino).unwrap().nbytes;
    assert!(
        cut < one,
        "cutting inside the extent did not reduce the block count ({} -> {})",
        one,
        cut
    );
    assert_eq!(
        fs.stat(ino).unwrap().size,
        40 * 1024,
        "the size did not follow"
    );

    drop(fs);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_full_filesystem_writes_again_in_the_space_a_grow_added() {
    // A small tail, which is the interesting size: the space a grow adds is
    // useless until it is big enough to become a chunk of its own.
    const TAIL: u64 = 4 * MIB;
    let (mut fs, path) = fresh("mintail", MKFS_SIZE);
    let root = fs.root_ino();

    // Fill it, so the only device space left to make a chunk out of is
    // whatever the grow below adds.
    let block = vec![0xa5u8; 64 * 1024];
    let mut filled = 0u64;
    for i in 0.. {
        let ino = match fs.create(root, &format!("f{}", i), FileKind::Regular, 0o644, 0) {
            Ok(ino) => ino,
            Err(Error::NoSpace) => break,
            Err(e) => panic!("create failed with {:?}", e),
        };
        match fs.write(ino, 0, &block) {
            Ok(n) => filled += n as u64,
            Err(Error::NoSpace) => break,
            Err(e) => panic!("write failed with {:?}", e),
        }
        assert!(i < 4096, "this image never filled up");
    }
    assert!(
        filled > 0,
        "nothing was written before the filesystem was full"
    );
    drop(fs);

    // This is the installer's path, end to end: an image copied onto a bigger
    // partition, filled, and then grown. Nothing else here fills a
    // filesystem and then gives it more room, and the space a grow adds is
    // not usable until something turns it into a chunk -- until then the
    // filesystem answers ENOSPC with free space sitting on the disk.
    let mut fs = Btrfs::mount(open_dev(&path, MKFS_SIZE + TAIL), false).unwrap();
    assert!(fs.grow_to_device().unwrap(), "did not grow into the tail");
    let ino = fs
        .create(root, "after-the-grow", FileKind::Regular, 0o644, 0)
        .unwrap();
    let n = fs
        .write(ino, 0, &block)
        .expect("a four MiB tail could not be turned into a chunk");
    assert_eq!(n, block.len(), "the write into the new chunk was short");
    let mut got = vec![0u8; block.len()];
    assert_eq!(fs.read(ino, 0, &mut got).unwrap(), block.len());
    assert_eq!(
        got, block,
        "what came back out of the new chunk is not what went in"
    );

    drop(fs);
    let _ = std::fs::remove_file(&path);
}
