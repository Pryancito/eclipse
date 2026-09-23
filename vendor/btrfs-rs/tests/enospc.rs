#![cfg(feature = "std")]
//! Regressions for "no space left on device" on a filesystem that is not
//! full.
//!
//! The shape of the bug these cover: free space is tracked as a map of
//! coalesced ranges, and block groups sit back to back on the logical
//! address space, so the free space at the *head* of one block group is
//! routinely described by a range keyed inside its predecessor. An
//! allocator that only looks at ranges keyed within the block group it is
//! allocating from cannot see that space at all -- and a chunk created on
//! demand, which is by construction both entirely free and adjacent to its
//! predecessor, is the worst case: every byte of it is invisible.
//!
//! These tests therefore work in terms of *what the user sees*: how much
//! fits before the filesystem starts refusing writes.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use btrfs::device::{BlockDevice, FileDevice};
use btrfs::{mkfs, Btrfs, Error, FileKind};

/// Big enough for mkfs (1 MiB reserved + 4 MiB system + 8 MiB metadata +
/// a data chunk), small enough to fill in a few seconds.
const MKFS_SIZE: u64 = 24 * 1024 * 1024;

fn opts() -> mkfs::MkfsOptions {
    mkfs::MkfsOptions {
        label: "enospc".into(),
        fsid: [0x11; 16],
        chunk_uuid: [0x22; 16],
        dev_uuid: [0x33; 16],
        subvol_uuid: [0x44; 16],
        now: (1_700_000_000, 0),
    }
}

fn tmpfile(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("btrfs-enospc-{}-{}", std::process::id(), name))
}

fn open_dev(path: &PathBuf, size: u64) -> Arc<dyn BlockDevice> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
        .unwrap();
    f.set_len(size).unwrap();
    Arc::new(FileDevice::open(f).unwrap())
}

/// Format `size` bytes, then present the same image on a `device_size`-byte
/// device and grow into it (what the installer does after copying a small
/// image onto a real partition).
fn fresh(name: &str, size: u64, device_size: u64) -> (Btrfs, PathBuf) {
    let path = tmpfile(name);
    let _ = std::fs::remove_file(&path);
    let dev = open_dev(&path, size);
    mkfs::format(&*dev, &opts()).unwrap();
    drop(dev);
    let dev = open_dev(&path, device_size);
    let mut fs = Btrfs::mount(dev, false).unwrap();
    if device_size > size {
        assert!(fs.grow_to_device().unwrap(), "grow_to_device did nothing");
    }
    (fs, path)
}

fn have_progs() -> bool {
    std::process::Command::new("btrfs")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn btrfs_check(path: &PathBuf) {
    let out = std::process::Command::new("btrfs")
        .args(["check", "--force"])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "btrfs check failed for {:?}\nstdout:\n{}\nstderr:\n{}",
        path,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Long names, so each entry costs a few hundred bytes of metadata and the
/// metadata block group fills in a reasonable number of iterations.
fn filename(i: u64) -> String {
    format!("{}{:06}", "n".repeat(200), i)
}

/// Create files until the filesystem refuses. Returns (how many fit, the
/// name of the create that was refused).
fn fill_with_files(fs: &mut Btrfs) -> (u64, String) {
    let root = fs.root_ino();
    let mut created = 0u64;
    loop {
        let name = filename(created);
        match fs.create(root, &name, FileKind::Regular, 0o644, 0) {
            Ok(_) => created += 1,
            Err(Error::NoSpace) => return (created, name),
            Err(e) => panic!("create #{} failed with {:?}, not NoSpace", created, e),
        }
    }
}

/// Growing onto a bigger device has to buy usable space. The extra room
/// reaches the filesystem as a chunk created on demand, laid down at
/// `logical_end()` -- right after an existing block group whose tail is
/// free, so its free space coalesces into a range keyed *before* it. When
/// the allocator could not see such a range, that chunk was created,
/// accounted for as free by `df`, and never handed out a single byte: the
/// grown filesystem refused writes at exactly the same point as the
/// ungrown one.
#[test]
fn growing_the_device_buys_space_that_can_actually_be_used() {
    let (mut small, _) = fresh("ungrown", MKFS_SIZE, MKFS_SIZE);
    let (ungrown, _) = fill_with_files(&mut small);
    assert!(ungrown > 0, "nothing fit on a freshly formatted filesystem");

    let (mut big, _) = fresh("grown", MKFS_SIZE, MKFS_SIZE + 8 * 1024 * 1024);
    let (grown, _) = fill_with_files(&mut big);

    assert!(
        grown > ungrown,
        "8 MiB of extra device space fit no extra files: {} before growing, {} after",
        ungrown,
        grown
    );
}

/// What "disk full" looked like from userspace: a 512 MiB filesystem with
/// 8 MiB used, refusing to create another empty file.
#[test]
fn a_nearly_empty_filesystem_does_not_report_enospc() {
    let (mut fs, _) = fresh("roomy", MKFS_SIZE, 512 * 1024 * 1024);
    let root = fs.root_ino();
    // Comfortably past the point where the metadata chunk mkfs laid down is
    // exhausted and the driver has to create its own (~6k files here).
    for i in 0..20_000u64 {
        if let Err(e) = fs.create(root, &filename(i), FileKind::Regular, 0o644, 0) {
            let st = fs.fsinfo();
            panic!(
                "create #{} failed with {:?} on a filesystem with {} of {} bytes used \
                 ({} MiB free)",
                i,
                e,
                st.bytes_used,
                st.total_bytes,
                (st.total_bytes - st.bytes_used) / 1024 / 1024
            );
        }
    }
    let st = fs.fsinfo();
    assert!(
        st.bytes_used < st.total_bytes,
        "accounting says the filesystem is over-full: {} of {}",
        st.bytes_used,
        st.total_bytes
    );
    fs.sync().unwrap();
}

/// Handing out space that was previously never handed out is only a fix if
/// it is handed out *once*. Fill a grown filesystem until it genuinely
/// refuses, then let btrfs-progs audit the result: `btrfs check` verifies the
/// extent tree, the per-block-group `used` counters and that nothing is
/// allocated twice.
#[test]
fn a_filesystem_filled_across_two_metadata_chunks_passes_btrfs_check() {
    if !have_progs() {
        eprintln!("btrfs-progs not available; skipping");
        return;
    }
    let (mut fs, path) = fresh("checked", MKFS_SIZE, MKFS_SIZE + 8 * 1024 * 1024);
    let (created, _) = fill_with_files(&mut fs);
    fs.sync().unwrap();
    drop(fs);
    btrfs_check(&path);

    // And our own driver still reads back every name it accepted.
    let dev = open_dev(&path, MKFS_SIZE + 8 * 1024 * 1024);
    let mut fs = Btrfs::mount(dev, true).unwrap();
    let root = fs.root_ino();
    assert_eq!(fs.readdir(root).unwrap().len() as u64, created);
}

/// Every inode must be reachable by name. An INODE_ITEM with no directory
/// entry pointing at it is an orphan: `btrfs check` reports it, nothing can
/// open or delete it, and it holds metadata for good.
fn assert_no_orphan_inodes(fs: &mut Btrfs) {
    let root = fs.root_ino();
    let mut named: BTreeSet<u64> = fs.readdir(root).unwrap().iter().map(|e| e.ino).collect();
    named.insert(root);
    let highest = named.iter().copied().max().unwrap_or(root);
    // A little past the highest named inode, to catch the ones a refused
    // create would have left at the end.
    for ino in root..=highest + 64 {
        if named.contains(&ino) {
            continue;
        }
        assert!(
            fs.stat(ino).is_err(),
            "inode {} exists but no directory entry points at it",
            ino
        );
    }
}

/// A create that runs out of space must leave nothing behind. The driver has
/// no transactions, so a new file is four separate items -- INODE_ITEM, then
/// DIR_ITEM, DIR_INDEX and INODE_REF -- written one at a time, and any of
/// them can be the one that finds no room for a leaf split. Whichever fails,
/// the ones already written have to come back out.
///
/// The probe: fill the filesystem with 200-byte names, then free exactly one
/// of them per round and ask for a 248-byte one. The inode item is a fixed
/// size and fits in the room just freed, while the three name-sized items
/// need more than was freed in the leaves they hash to -- so the failure
/// walks through all four positions across the rounds instead of always
/// landing on the first.
#[test]
fn a_create_that_runs_out_of_space_leaves_no_trace() {
    let (mut fs, _) = fresh("partial", MKFS_SIZE, MKFS_SIZE + 4 * 1024 * 1024);
    let (created, _) = fill_with_files(&mut fs);
    assert!(created > 0);
    let root = fs.root_ino();

    let mut live: Vec<u64> = (0..created).collect();
    let mut refused = 0;
    for round in 0..200u64 {
        if let Some(i) = live.pop() {
            fs.unlink(root, &filename(i)).unwrap();
        }
        let probe = format!("p{:03}{}", round, "q".repeat(248));
        let before = fs.readdir(root).unwrap().len();
        match fs.create(root, &probe, FileKind::Regular, 0o644, 0) {
            Ok(ino) => {
                assert_eq!(fs.readdir(root).unwrap().len(), before + 1);
                assert_eq!(fs.lookup(root, &probe).unwrap(), ino);
            }
            Err(Error::NoSpace) => {
                refused += 1;
                assert_eq!(
                    fs.readdir(root).unwrap().len(),
                    before,
                    "round {}: a refused create changed the directory listing",
                    round
                );
                assert_eq!(
                    fs.lookup(root, &probe).err(),
                    Some(Error::NotFound),
                    "round {}: the name of a refused create is still resolvable",
                    round
                );
                assert_no_orphan_inodes(&mut fs);
            }
            Err(e) => panic!("round {}: create failed with {:?}, not NoSpace", round, e),
        }
    }
    assert!(
        refused > 0,
        "the filesystem never ran out of space; the test proved nothing"
    );
    // Everything that is listed still resolves and reads back.
    for entry in fs.readdir(root).unwrap() {
        assert_eq!(fs.lookup(root, &entry.name).unwrap(), entry.ino);
        fs.stat(entry.ino).unwrap();
    }
    fs.sync().unwrap();
}

/// A `rename` the filesystem cannot fit must leave the file under the name
/// it already had. Writing the new name meant removing the old one first,
/// and on a filesystem with no metadata left the old name could not be put
/// back: `mv` returned an error and the file was gone -- no name in any
/// directory, nothing left to remove or recover it with.
#[test]
fn a_rename_that_runs_out_of_space_keeps_the_old_name() {
    let (mut fs, _) = fresh("rename", MKFS_SIZE, MKFS_SIZE + 4 * 1024 * 1024);
    let (created, _) = fill_with_files(&mut fs);
    assert!(created > 0);
    let root = fs.root_ino();

    let mut names: Vec<String> = (0..created).map(filename).collect();
    let mut refused = 0;
    for round in 0..200u64 {
        let at = round as usize % names.len();
        let old = names[at].clone();
        let ino = fs.lookup(root, &old).unwrap();
        // Longer than the name it replaces, so it needs more room than
        // dropping the old name gives back.
        let new = format!("r{:03}{}", round, "s".repeat(248));
        match fs.rename(root, &old, root, &new) {
            Ok(()) => {
                assert_eq!(fs.lookup(root, &new).unwrap(), ino);
                assert_eq!(fs.lookup(root, &old).err(), Some(Error::NotFound));
                names[at] = new;
            }
            Err(Error::NoSpace) => {
                refused += 1;
                assert_eq!(
                    fs.lookup(root, &old).ok(),
                    Some(ino),
                    "round {}: a refused rename lost the old name",
                    round
                );
                assert_eq!(
                    fs.lookup(root, &new).err(),
                    Some(Error::NotFound),
                    "round {}: a refused rename left the new name behind",
                    round
                );
                assert_no_orphan_inodes(&mut fs);
            }
            Err(e) => panic!("round {}: rename failed with {:?}, not NoSpace", round, e),
        }
    }
    assert!(
        refused > 0,
        "the filesystem never ran out of space; the test proved nothing"
    );
    // Not one entry was gained or lost along the way.
    let listed = fs.readdir(root).unwrap();
    assert_eq!(listed.len() as u64, created);
    for name in &names {
        fs.lookup(root, name).unwrap();
    }
    fs.sync().unwrap();
}
