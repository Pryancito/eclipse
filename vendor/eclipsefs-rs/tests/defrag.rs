#![cfg(feature = "std")]
//! `btrfs filesystem defragment`: fragmented files become contiguous,
//! optionally compressed, and nothing else about them changes.

mod common;

use common::*;
use eclipsefs::cowfmt::{FALLOC_FL_KEEP_SIZE, FL_IMMUTABLE, FL_NOCOW};
use eclipsefs::fs::{FIEMAP_EXTENT_ENCODED, FIEMAP_EXTENT_SHARED, FIEMAP_EXTENT_UNWRITTEN};
use eclipsefs::{Compression, EclipseFS, Error, FileKind};

const B: u64 = BLOCK as u64;

fn file(fs: &mut EclipseFS, name: &str) -> u64 {
    let root = fs.root_ino();
    fs.create(root, name, FileKind::Regular, 0o644, 0).unwrap()
}

/// Pieces of `f` on disk, merging those that follow each other there too.
fn fragments(fs: &mut EclipseFS, f: u64) -> usize {
    let m = fs.fiemap(f, 0, u64::MAX).unwrap();
    let mut n = 0;
    let mut next = None;
    for p in m {
        if next != Some((p.logical, p.physical)) || p.flags & FIEMAP_EXTENT_ENCODED != 0 {
            n += 1;
        }
        next = Some((p.logical + p.length, p.physical + p.length));
    }
    n
}

/// Two files written a block at a time, in turns, so that their blocks
/// alternate on disk.
fn interleaved(fs: &mut EclipseFS, blocks: usize) -> (u64, u64, Vec<u8>, Vec<u8>) {
    let a = file(fs, "a");
    let b = file(fs, "b");
    let (da, db) = (pattern(blocks * BLOCK, 1), pattern(blocks * BLOCK, 2));
    for i in 0..blocks {
        let r = i * BLOCK..(i + 1) * BLOCK;
        write_all(fs, a, r.start as u64, &da[r.clone()]);
        write_all(fs, b, r.start as u64, &db[r]);
        // Each write its own transaction, so no block is rewritten in place.
        fs.sync().unwrap();
    }
    (a, b, da, db)
}

#[test]
fn interleaved_files_become_one_extent_each() {
    let (disk, mut fs) = fresh(32 * MIB);
    let (a, b, da, db) = interleaved(&mut fs, 24);
    assert!(fragments(&mut fs, a) > 1, "the setup did not fragment");
    assert!(fragments(&mut fs, b) > 1);
    let before = fs.fsinfo();
    assert_eq!(fs.defrag(a, 0, u64::MAX, 0, None).unwrap(), 24 * B);
    assert_eq!(fs.defrag(b, 0, u64::MAX, 0, None).unwrap(), 24 * B);
    assert_eq!(fragments(&mut fs, a), 1);
    assert_eq!(fragments(&mut fs, b), 1);
    assert_eq!(read_all(&mut fs, a), da);
    assert_eq!(read_all(&mut fs, b), db);
    fs.check().unwrap();
    let mut fs = remount(&disk, fs);
    assert_eq!(read_all(&mut fs, a), da);
    assert_eq!(fragments(&mut fs, b), 1);
    // The old blocks went back.
    assert_eq!(fs.fsinfo().bytes_used, before.bytes_used);
    fs.check().unwrap();
}

#[test]
fn a_contiguous_file_is_left_alone() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let f = file(&mut fs, "f");
    write_all(&mut fs, f, 0, &pattern(40 * BLOCK, 3));
    fs.sync().unwrap();
    let before = fs.fiemap(f, 0, u64::MAX).unwrap();
    assert_eq!(fs.defrag(f, 0, u64::MAX, 0, None).unwrap(), 0);
    assert_eq!(fs.fiemap(f, 0, u64::MAX).unwrap(), before);
}

#[test]
fn extents_over_the_threshold_stay_where_they_are() {
    let (_disk, mut fs) = fresh(32 * MIB);
    let (a, _, da, _) = interleaved(&mut fs, 8);
    let before = fs.fiemap(a, 0, u64::MAX).unwrap();
    // Every piece is one block: a one-block threshold moves none of them.
    assert_eq!(fs.defrag(a, 0, u64::MAX, B, None).unwrap(), 0);
    assert_eq!(fs.fiemap(a, 0, u64::MAX).unwrap(), before);
    assert_eq!(fs.defrag(a, 0, u64::MAX, 2 * B, None).unwrap(), 8 * B);
    assert_eq!(fragments(&mut fs, a), 1);
    assert_eq!(read_all(&mut fs, a), da);
}

#[test]
fn only_the_range_asked_for_moves() {
    let (_disk, mut fs) = fresh(32 * MIB);
    let (a, _, da, _) = interleaved(&mut fs, 16);
    let before = fs.fiemap(a, 0, u64::MAX).unwrap();
    // Unaligned edges round out to whole blocks.
    assert_eq!(fs.defrag(a, 4 * B + 100, 6 * B, 0, None).unwrap(), 7 * B);
    let after = fs.fiemap(a, 0, u64::MAX).unwrap();
    assert_eq!(after[..4], before[..4]);
    assert_eq!(after[after.len() - 5..], before[before.len() - 5..]);
    let mid: Vec<_> = after.iter().filter(|m| m.logical == 4 * B).collect();
    assert_eq!(mid[0].length, 7 * B);
    assert_eq!(read_all(&mut fs, a), da);
    assert_eq!(fs.defrag(a, 0, 0, 0, None).unwrap(), 0);
    assert_eq!(fs.defrag(a, 100 * B, B, 0, None).unwrap(), 0);
    fs.check().unwrap();
}

#[test]
fn holes_and_preallocated_space_survive() {
    let (_disk, mut fs) = fresh(32 * MIB);
    let f = file(&mut fs, "f");
    let other = file(&mut fs, "other");
    // data 0..4 (fragmented) | hole 4..6 | prealloc 6..8 | data 8..12 (fragmented)
    for i in [0u64, 1, 2, 3, 8, 9, 10, 11] {
        write_all(&mut fs, f, i * B, &pattern(BLOCK, i as usize));
        write_all(&mut fs, other, i * B, &pattern(BLOCK, 99));
        fs.sync().unwrap();
    }
    fs.fallocate(f, FALLOC_FL_KEEP_SIZE, 6 * B, 2 * B).unwrap();
    fs.truncate(f, 14 * B).unwrap();
    let data = read_all(&mut fs, f);
    let unwritten = |fs: &mut EclipseFS| -> Vec<_> {
        let m = fs.fiemap(f, 0, u64::MAX).unwrap();
        m.into_iter()
            .filter(|m| m.flags & FIEMAP_EXTENT_UNWRITTEN != 0)
            .collect()
    };
    let prealloc = unwritten(&mut fs);
    assert_eq!(fs.defrag(f, 0, u64::MAX, 0, None).unwrap(), 8 * B);
    let m = fs.fiemap(f, 0, u64::MAX).unwrap();
    let written: Vec<_> = m
        .iter()
        .filter(|m| m.flags & FIEMAP_EXTENT_UNWRITTEN == 0)
        .map(|m| (m.logical / B, m.length / B))
        .collect();
    assert_eq!(written, [(0, 4), (8, 4)]);
    assert_eq!(unwritten(&mut fs), prealloc);
    assert_eq!(read_all(&mut fs, f), data);
    assert_eq!(fs.stat(f).unwrap().size, 14 * B);
    assert_eq!(fs.seek_hole(f, 0).unwrap(), Some(4 * B));
    fs.check().unwrap();
}

#[test]
fn defrag_with_compression_rewrites_compressed() {
    let (disk, mut fs) = fresh(32 * MIB);
    let f = file(&mut fs, "f");
    let data: Vec<u8> = (0..64 * BLOCK).map(|i| (i / 100 % 7) as u8).collect();
    write_all(&mut fs, f, 0, &data);
    fs.sync().unwrap();
    assert_eq!(fs.compsize(f).unwrap().compressed_extents, 0);
    let rewritten = fs
        .defrag(f, 0, u64::MAX, 0, Some(Compression::Zstd))
        .unwrap();
    assert_eq!(rewritten, 64 * B);
    let z = fs.compsize(f).unwrap();
    assert!(z.compressed_extents > 0);
    assert!(z.disk_bytes < z.uncompressed_bytes / 4, "{:?}", z);
    assert!(fs
        .fiemap(f, 0, u64::MAX)
        .unwrap()
        .iter()
        .all(|m| m.flags & FIEMAP_EXTENT_ENCODED != 0));
    // Already compressed that way: nothing to do; another way: all of it.
    assert_eq!(
        fs.defrag(f, 0, u64::MAX, 0, Some(Compression::Zstd))
            .unwrap(),
        0
    );
    assert_eq!(
        fs.defrag(f, 0, u64::MAX, 0, Some(Compression::Lz4))
            .unwrap(),
        64 * B
    );
    // The file's own compression setting did not change.
    assert_eq!(fs.compression(f).unwrap(), None);
    assert_eq!(fs.stat(f).unwrap().size, 64 * B);
    assert_eq!(read_all(&mut fs, f), data);
    fs.check().unwrap();
    let mut fs = remount(&disk, fs);
    assert_eq!(read_all(&mut fs, f), data);
    fs.check().unwrap();
}

#[test]
fn incompressible_data_and_a_short_tail_are_written_plain() {
    let (_disk, mut fs) = fresh(32 * MIB);
    let f = file(&mut fs, "f");
    // Random bytes do not compress; the tail is not a whole chunk.
    let mut rng = Lcg(5);
    let data: Vec<u8> = (0..40 * BLOCK + 123).map(|_| rng.next() as u8).collect();
    write_all(&mut fs, f, 0, &data);
    fs.sync().unwrap();
    fs.defrag(f, 0, u64::MAX, 0, Some(Compression::Zlib))
        .unwrap();
    assert_eq!(read_all(&mut fs, f), data);
    assert_eq!(fs.stat(f).unwrap().size, data.len() as u64);
    fs.check().unwrap();
}

#[test]
fn defrag_unshares_but_the_snapshot_keeps_its_data() {
    let (_disk, mut fs) = fresh(32 * MIB);
    let root = fs.root_ino();
    let sv = fs.create_subvolume(root, "sv").unwrap();
    let a = fs.create(sv, "a", FileKind::Regular, 0o644, 0).unwrap();
    let b = fs.create(sv, "b", FileKind::Regular, 0o644, 0).unwrap();
    let da = pattern(12 * BLOCK, 4);
    for i in 0..12 {
        let r = i * BLOCK..(i + 1) * BLOCK;
        write_all(&mut fs, a, r.start as u64, &da[r.clone()]);
        write_all(&mut fs, b, r.start as u64, &da[r]);
        fs.sync().unwrap();
    }
    let snap = fs.snapshot(sv, root, "snap", true).unwrap();
    let sa = fs.lookup(snap, "a").unwrap();
    assert_eq!(fs.defrag(a, 0, u64::MAX, 0, None).unwrap(), 12 * B);
    assert!(fs
        .fiemap(a, 0, u64::MAX)
        .unwrap()
        .iter()
        .all(|m| m.flags & FIEMAP_EXTENT_SHARED == 0));
    assert_eq!(read_all(&mut fs, a), da);
    assert_eq!(read_all(&mut fs, sa), da);
    assert!(fragments(&mut fs, sa) > 1);
    // A read-only snapshot is not rewritten.
    assert!(fs.defrag(sa, 0, u64::MAX, 0, None).is_err());
    fs.check().unwrap();
}

#[test]
fn what_defrag_does_with_odd_inodes() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    assert_eq!(fs.defrag(root, 0, u64::MAX, 0, None).unwrap(), 0);
    let empty = file(&mut fs, "empty");
    assert_eq!(fs.defrag(empty, 0, u64::MAX, 0, None).unwrap(), 0);
    let small = file(&mut fs, "small");
    write_all(&mut fs, small, 0, b"inline");
    assert_eq!(
        fs.defrag(small, 0, u64::MAX, 0, Some(Compression::Zstd))
            .unwrap(),
        0
    );
    assert_eq!(read_all(&mut fs, small), b"inline");
    let link = fs.symlink(root, "l", b"small").unwrap();
    assert_eq!(fs.defrag(link, 0, u64::MAX, 0, None), Err(Error::Invalid));
    let frozen = file(&mut fs, "frozen");
    write_all(&mut fs, frozen, 0, &pattern(3 * BLOCK, 1));
    fs.set_flags(frozen, FL_IMMUTABLE).unwrap();
    assert!(fs.defrag(frozen, 0, u64::MAX, 0, None).is_err());
}

#[test]
fn nocow_files_move_but_are_never_compressed() {
    let (_disk, mut fs) = fresh(32 * MIB);
    let a = file(&mut fs, "a");
    fs.set_flags(a, FL_NOCOW).unwrap();
    let b = file(&mut fs, "b");
    let da = pattern(10 * BLOCK, 8);
    for i in 0..10 {
        let r = i * BLOCK..(i + 1) * BLOCK;
        write_all(&mut fs, a, r.start as u64, &da[r.clone()]);
        write_all(&mut fs, b, r.start as u64, &da[r]);
        fs.sync().unwrap();
    }
    assert!(fragments(&mut fs, a) > 1);
    fs.defrag(a, 0, u64::MAX, 0, Some(Compression::Zstd))
        .unwrap();
    assert_eq!(fragments(&mut fs, a), 1);
    assert_eq!(fs.compsize(a).unwrap().compressed_extents, 0);
    assert_eq!(read_all(&mut fs, a), da);
    fs.check().unwrap();
}

#[test]
fn defrag_on_v3_is_unsupported() {
    let (_disk, mut fs) = fresh_v3(8 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    assert!(matches!(
        fs.defrag(f, 0, u64::MAX, 0, None),
        Err(Error::Unsupported(_))
    ));
}

/// Random writes to a few files, then random defragments: the data always
/// matches a plain copy and fsck stays clean.
#[test]
fn random_writes_and_defrags_keep_the_data() {
    let (disk, mut fs) = fresh(64 * MIB);
    let mut rng = Lcg(21);
    let files: Vec<u64> = (0..3).map(|i| file(&mut fs, &format!("f{}", i))).collect();
    let mut model = vec![Vec::<u8>::new(); files.len()];
    for step in 0..300 {
        let k = rng.below(files.len() as u64) as usize;
        let f = files[k];
        match rng.below(6) {
            0..=3 => {
                let off = rng.below(200 * B);
                let len = 1 + rng.below(3 * B) as usize;
                let data = pattern(len, step);
                write_all(&mut fs, f, off, &data);
                let m = &mut model[k];
                if m.len() < off as usize + len {
                    m.resize(off as usize + len, 0);
                }
                m[off as usize..off as usize + len].copy_from_slice(&data);
            }
            4 => {
                let start = rng.below(200 * B);
                let len = rng.below(100 * B);
                let algo = match rng.below(4) {
                    0 => Some(Compression::ALL[rng.below(3) as usize]),
                    _ => None,
                };
                fs.defrag(f, start, len, rng.below(3) * 4 * B, algo)
                    .unwrap();
            }
            _ => fs.sync().unwrap(),
        }
    }
    for (k, &f) in files.iter().enumerate() {
        assert_eq!(read_all(&mut fs, f), model[k], "file {}", k);
    }
    fs.check().unwrap();
    let mut fs = remount(&disk, fs);
    for (k, &f) in files.iter().enumerate() {
        fs.defrag(f, 0, u64::MAX, 0, None).unwrap();
        assert_eq!(read_all(&mut fs, f), model[k], "file {}", k);
    }
    fs.check().unwrap();
}
