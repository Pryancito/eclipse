#![cfg(feature = "std")]
//! Checksums of the allocator bitmaps: a bitmap that does not match its
//! checksum is taken from another copy or rebuilt from the trees at mount,
//! so a damaged bitmap can never hand out a block that is in use.

mod common;

use std::sync::Arc;

use common::*;
use eclipsefs::cowfmt::{SuperV4, FEAT_BITMAP_CSUM};
use eclipsefs::device::BlockDevice;
use eclipsefs::{mkfs, EclipseFS, FileKind, Profile};

const B: u64 = BLOCK as u64;
const GROUP: u64 = 32768;

fn sb(disk: &MemDisk) -> SuperV4 {
    let mut b = vec![0u8; BLOCK];
    disk.read_at(0, &mut b).unwrap();
    SuperV4::decode(&b).unwrap()
}

fn block(disk: &MemDisk, b: u64) -> Vec<u8> {
    let mut buf = vec![0u8; BLOCK];
    disk.read_at(b * B, &mut buf).unwrap();
    buf
}

fn fill(fs: &mut EclipseFS, n: usize, size: usize) -> Vec<(u64, Vec<u8>)> {
    let root = fs.root_ino();
    (0..n)
        .map(|i| {
            let f = fs
                .create(root, &format!("f{size}-{i}"), FileKind::Regular, 0o644, 0)
                .unwrap();
            let data = pattern(size + i * 1000, i);
            write_all(fs, f, 0, &data);
            (f, data)
        })
        .collect()
}

fn same(fs: &mut EclipseFS, files: &[(u64, Vec<u8>)]) {
    for (f, data) in files {
        assert_eq!(&read_all(fs, *f), data);
    }
}

/// Flip every bit of the bitmap bytes for blocks `from..to` of group 0
/// (in use become free and the other way round).
fn flip_bits(disk: &MemDisk, group: u64, from: u64, to: u64) {
    let at = (group * GROUP + 1) * B;
    for byte in from / 8..to.div_ceil(8) {
        disk.poke((at + byte) as usize, 0xFF);
    }
}

/// Drop the last transaction from the journal (already in place after a
/// clean unmount), so that mounting reads the blocks where they live.
fn forget_journal(disk: &MemDisk) {
    let js = journal_start(&block(disk, 0));
    disk.write_at(js * B, &[0u8; BLOCK]).unwrap();
}

fn reopen(disk: &Arc<MemDisk>, read_only: bool) -> EclipseFS {
    let mut fs = EclipseFS::mount(disk.clone(), read_only).unwrap();
    fs.set_clock(clock);
    fs
}

#[test]
fn a_new_filesystem_has_the_table() {
    let (disk, mut fs) = fresh(64 * MIB);
    let s = sb(&disk);
    assert_ne!(s.features & FEAT_BITMAP_CSUM, 0);
    let (at, n) = s.bitmap_sums.unwrap();
    assert_eq!(n, 1);
    assert!((2..GROUP).contains(&at));
    fs.check().unwrap();
}

#[test]
fn a_filesystem_without_it_gets_it_at_its_next_commit() {
    let (disk, fs) = fresh(64 * MIB);
    drop(fs);
    forget_journal(&disk);
    // Turn the feature off (as an older filesystem would have it): no
    // table, and its block free.
    let mut s = sb(&disk);
    let (at, _) = s.bitmap_sums.take().unwrap();
    disk.write_at(0, &s.encode()).unwrap();
    disk.poke((B + at / 8) as usize, 1 << (at % 8));
    let mut fs = reopen(&disk, true);
    fs.check().unwrap();
    drop(fs);
    assert!(sb(&disk).bitmap_sums.is_none(), "a read-only mount writes");
    let mut fs = reopen(&disk, false);
    let files = fill(&mut fs, 2, 10_000);
    fs.sync().unwrap();
    assert!(sb(&disk).bitmap_sums.is_some());
    let mut fs = remount(&disk, fs);
    same(&mut fs, &files);
    fs.check().unwrap();
}

#[test]
fn a_damaged_bitmap_is_rebuilt_from_the_trees() {
    let (disk, mut fs) = fresh(64 * MIB);
    let files = fill(&mut fs, 8, 200_000);
    fs.sync().unwrap();
    let used = fs.fsinfo().bytes_used;
    drop(fs);
    forget_journal(&disk);
    // Blocks in use marked free: reusing them would destroy the files.
    flip_bits(&disk, 0, 16, 256);
    let mut fs = reopen(&disk, false);
    assert_eq!(fs.fsinfo().bytes_used, used);
    fs.check().unwrap();
    // New data goes to free space only.
    let more = fill(&mut fs, 4, 300_000);
    let mut fs = remount(&disk, fs);
    same(&mut fs, &files[4..]);
    same(&mut fs, &more);
    fs.check().unwrap();
}

#[test]
fn a_bitmap_marking_its_own_header_free_still_mounts() {
    let (disk, mut fs) = fresh(160 * MIB);
    let files = fill(&mut fs, 3, 50_000);
    drop(fs);
    forget_journal(&disk);
    flip_bits(&disk, 0, 0, 8);
    flip_bits(&disk, 1, 0, 8);
    let mut fs = reopen(&disk, false);
    same(&mut fs, &files);
    fs.check().unwrap();
    let mut fs = remount(&disk, fs);
    fs.check().unwrap();
}

#[test]
fn a_damaged_table_block_is_rewritten() {
    let (disk, mut fs) = fresh(64 * MIB);
    let files = fill(&mut fs, 3, 80_000);
    drop(fs);
    forget_journal(&disk);
    let (at, _) = sb(&disk).bitmap_sums.unwrap();
    disk.poke((at * B) as usize + 17, 0x40);
    // fsck on a read-only mount sees it.
    let mut fs = reopen(&disk, true);
    same(&mut fs, &files);
    assert!(fs.check().is_err());
    drop(fs);
    let fs = reopen(&disk, false);
    let mut fs = remount(&disk, fs);
    same(&mut fs, &files);
    fs.check().unwrap();
}

#[test]
fn a_read_only_mount_repairs_in_memory_only() {
    let (disk, mut fs) = fresh(64 * MIB);
    let files = fill(&mut fs, 4, 100_000);
    drop(fs);
    forget_journal(&disk);
    flip_bits(&disk, 0, 16, 64);
    let before = disk.copy();
    let mut fs = reopen(&disk, true);
    same(&mut fs, &files);
    drop(fs);
    for b in 0..(64 * MIB as u64) / B {
        assert_eq!(block(&disk, b), block(&before, b), "block {b} written");
    }
}

#[test]
fn every_commit_keeps_the_table_in_step() {
    let (disk, mut fs) = fresh(64 * MIB);
    let root = fs.root_ino();
    let mut rng = Lcg(5);
    for round in 0..30 {
        let name = format!("r{}", rng.below(8));
        match fs.lookup(root, &name) {
            Ok(_) if round % 3 == 0 => fs.unlink(root, &name).unwrap(),
            Ok(f) => write_all(&mut fs, f, rng.below(100_000), &pattern(50_000, round)),
            Err(_) => {
                let f = fs.create(root, &name, FileKind::Regular, 0o644, 0).unwrap();
                write_all(&mut fs, f, 0, &pattern(120_000, round));
            }
        }
        fs.sync().unwrap();
        fs.check().unwrap();
    }
    let mut fs = remount(&disk, fs);
    fs.check().unwrap();
}

#[test]
fn growing_and_shrinking_keep_the_table_in_step() {
    let (disk, mut fs) = fresh(64 * MIB);
    let files = fill(&mut fs, 3, 100_000);
    disk.resize(400 * MIB);
    fs.resize(1, None).unwrap();
    fs.check().unwrap();
    let mut fs = remount(&disk, fs);
    fs.check().unwrap();
    fs.resize(1, Some(100 * MIB as u64)).unwrap();
    let mut fs = remount(&disk, fs);
    same(&mut fs, &files);
    fs.check().unwrap();
}

fn devs(d: &[Arc<MemDisk>]) -> Vec<Arc<dyn BlockDevice>> {
    d.iter()
        .map(|d| d.clone() as Arc<dyn BlockDevice>)
        .collect()
}

#[test]
fn a_mirrored_bitmap_is_taken_from_its_other_copy() {
    let d: Vec<_> = (0..2).map(|_| MemDisk::new(300 * MIB)).collect();
    mkfs::format_devices(&devs(&d), &opts(), Profile::Raid1, Profile::Raid1).unwrap();
    let mut fs = EclipseFS::mount_devices(devs(&d), false).unwrap();
    fs.set_clock(clock);
    let root = fs.root_ino();
    let mut files = Vec::new();
    for i in 0..6 {
        let f = fs
            .create(root, &format!("f{i}"), FileKind::Regular, 0o644, 0)
            .unwrap();
        let data = pattern(3 * MIB, i);
        write_all(&mut fs, f, 0, &data);
        files.push((f, data));
    }
    fs.sync().unwrap();
    // Damage one copy of every chunk's bitmap.
    let chunks = fs.chunks();
    drop(fs);
    // The journal goes too, so the bitmaps are read where they live.
    let js = journal_start(&block(&d[0], 0));
    let sys = chunks.iter().find(|c| c.logical == 0).unwrap();
    for &(dev, phys) in &sys.stripes {
        d[dev as usize - 1]
            .write_at(phys + js * B, &[0u8; BLOCK])
            .unwrap();
    }
    let mut copies = Vec::new();
    for c in &chunks {
        let (dev, phys) = c.stripes[0];
        let at = phys + B;
        d[dev as usize - 1].poke(at as usize + 100, 0xFF);
        copies.push(c.stripes.clone());
    }
    let mut fs = EclipseFS::mount_devices(devs(&d), false).unwrap();
    same(&mut fs, &files);
    fs.check().unwrap();
    fs.sync().unwrap();
    drop(fs);
    // Written back to the damaged copies.
    for s in copies {
        let read = |(dev, phys): (u32, u64)| {
            let mut b = vec![0u8; BLOCK];
            d[dev as usize - 1].read_at(phys + B, &mut b).unwrap();
            b
        };
        assert_eq!(read(s[0]), read(s[1]));
    }
}
