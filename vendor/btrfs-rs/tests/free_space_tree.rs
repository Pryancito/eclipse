#![cfg(feature = "std")]
//! The driver allocates extents in the extent tree but does not maintain the
//! free space tree. `mkfs.btrfs` has enabled that tree by default since
//! btrfs-progs 5.15, so a volume formatted on Linux normally carries it.
//!
//! Writing to such a volume while leaving `FREE_SPACE_TREE_VALID` set tells the
//! Linux kernel that the (now stale) tree still describes free space, and it
//! hands out extents we already allocated — silent corruption of live data.
//! Clearing `VALID` is the state the kernel treats as "rebuild at mount".
//!
//! These tests pin that the bit is cleared on the first mutation, that it is
//! cleared *on disk* (not merely in memory), and that a volume without a free
//! space tree keeps its `compat_ro` flags untouched.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use btrfs::device::{BlockDevice, FileDevice};
use btrfs::structs::{
    get_u64, put_u64, sb, COMPAT_RO_FREE_SPACE_TREE, COMPAT_RO_FREE_SPACE_TREE_VALID, CSUM_SIZE,
    SUPERBLOCK_OFFSETS, SUPERBLOCK_SIZE,
};
use btrfs::{mkfs, Btrfs, FileKind};

const IMAGE_SIZE: u64 = 512 * 1024 * 1024;

fn tmpfile(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("btrfs-fst-{}-{}", std::process::id(), name));
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    f.set_len(IMAGE_SIZE).unwrap();
    path
}

fn open_dev(path: &Path) -> Arc<dyn BlockDevice> {
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    Arc::new(FileDevice::open(f).unwrap())
}

fn opts() -> mkfs::MkfsOptions {
    let mut seed = 0x0fed_cba9_8765_4321u64;
    let mut uuid = || {
        let mut u = [0u8; 16];
        for b in u.iter_mut() {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (seed >> 33) as u8;
        }
        u[6] = (u[6] & 0x0f) | 0x40;
        u[8] = (u[8] & 0x3f) | 0x80;
        u
    };
    mkfs::MkfsOptions {
        label: "eclipse".into(),
        fsid: uuid(),
        chunk_uuid: uuid(),
        dev_uuid: uuid(),
        subvol_uuid: uuid(),
        now: (1_700_000_000, 0),
    }
}

/// Read `compat_ro_flags` straight out of the primary superblock on disk.
fn on_disk_compat_ro(dev: &Arc<dyn BlockDevice>) -> u64 {
    let mut raw = vec![0u8; SUPERBLOCK_SIZE];
    dev.read_at(SUPERBLOCK_OFFSETS[0], &mut raw).unwrap();
    get_u64(&raw, sb::OFF_COMPAT_RO_FLAGS)
}

/// Rewrite `compat_ro_flags` in every superblock mirror, standing in for a
/// `mkfs.btrfs` that enabled the free space tree.
fn force_compat_ro(dev: &Arc<dyn BlockDevice>, flags: u64) {
    for &off in SUPERBLOCK_OFFSETS.iter() {
        if off + SUPERBLOCK_SIZE as u64 > dev.size() {
            continue;
        }
        let mut raw = vec![0u8; SUPERBLOCK_SIZE];
        dev.read_at(off, &mut raw).unwrap();
        put_u64(&mut raw, sb::OFF_COMPAT_RO_FLAGS, flags);
        let sum = btrfs::crc::checksum(&raw[CSUM_SIZE..]);
        raw[..CSUM_SIZE].fill(0);
        raw[..4].copy_from_slice(&sum.to_le_bytes());
        dev.write_at(off, &raw).unwrap();
    }
}

fn format(name: &str) -> (PathBuf, Arc<dyn BlockDevice>) {
    let path = tmpfile(name);
    let dev = open_dev(&path);
    mkfs::format(&*dev, &opts()).unwrap();
    (path, dev)
}

#[test]
fn first_mutation_clears_free_space_tree_valid() {
    let (path, dev) = format("clears-valid");
    force_compat_ro(
        &dev,
        COMPAT_RO_FREE_SPACE_TREE | COMPAT_RO_FREE_SPACE_TREE_VALID,
    );

    let mut fs = Btrfs::mount(dev.clone(), false).unwrap();
    let root = fs.root_ino();
    // Mounting read-write on its own must not touch the flags.
    assert_eq!(
        on_disk_compat_ro(&dev),
        COMPAT_RO_FREE_SPACE_TREE | COMPAT_RO_FREE_SPACE_TREE_VALID,
        "mount alone changed compat_ro flags",
    );

    fs.create(root, "hello", FileKind::Regular, 0o644, 0)
        .unwrap();

    // The clear must hit the disk immediately, not wait for the next deferred
    // superblock commit: a crash in between would leave a volume whose extent
    // tree moved while `VALID` still claimed the free space tree described it.
    assert_eq!(
        on_disk_compat_ro(&dev),
        COMPAT_RO_FREE_SPACE_TREE,
        "FREE_SPACE_TREE_VALID survived the first mutation",
    );

    fs.sync().unwrap();
    assert_eq!(on_disk_compat_ro(&dev), COMPAT_RO_FREE_SPACE_TREE);

    // Every mirror has to agree, or Linux may pick a stale one.
    for &off in SUPERBLOCK_OFFSETS.iter() {
        if off + SUPERBLOCK_SIZE as u64 > dev.size() {
            continue;
        }
        let mut raw = vec![0u8; SUPERBLOCK_SIZE];
        dev.read_at(off, &mut raw).unwrap();
        assert_eq!(
            get_u64(&raw, sb::OFF_COMPAT_RO_FLAGS),
            COMPAT_RO_FREE_SPACE_TREE,
            "superblock mirror at {:#x} still claims a valid free space tree",
            off,
        );
    }

    drop(fs);
    let _ = fs::remove_file(path);
}

#[test]
fn read_only_mount_leaves_the_flag_alone() {
    let (path, dev) = format("read-only");
    let want = COMPAT_RO_FREE_SPACE_TREE | COMPAT_RO_FREE_SPACE_TREE_VALID;
    force_compat_ro(&dev, want);

    let mut fs = Btrfs::mount(dev.clone(), true).unwrap();
    let root = fs.root_ino();
    fs.readdir(root).unwrap();
    assert!(fs
        .create(root, "nope", FileKind::Regular, 0o644, 0)
        .is_err());
    assert_eq!(
        on_disk_compat_ro(&dev),
        want,
        "a read-only mount must not rewrite the superblock",
    );

    drop(fs);
    let _ = fs::remove_file(path);
}

#[test]
fn volume_without_free_space_tree_is_untouched() {
    let (path, dev) = format("no-fst");
    let before = on_disk_compat_ro(&dev);
    assert_eq!(
        before & COMPAT_RO_FREE_SPACE_TREE,
        0,
        "our mkfs is not expected to build a free space tree",
    );

    let mut fs = Btrfs::mount(dev.clone(), false).unwrap();
    let root = fs.root_ino();
    fs.create(root, "hello", FileKind::Regular, 0o644, 0)
        .unwrap();
    fs.sync().unwrap();

    assert_eq!(
        on_disk_compat_ro(&dev),
        before,
        "compat_ro flags changed on a volume with no free space tree",
    );

    drop(fs);
    let _ = fs::remove_file(path);
}
