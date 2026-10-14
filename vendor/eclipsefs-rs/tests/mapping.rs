#![cfg(feature = "std")]
//! FIEMAP (`filefrag`) and SEEK_DATA / SEEK_HOLE.

mod common;

use common::*;
use eclipsefs::cowfmt::FALLOC_FL_KEEP_SIZE;
use eclipsefs::fs::{
    FIEMAP_EXTENT_DATA_INLINE, FIEMAP_EXTENT_ENCODED, FIEMAP_EXTENT_LAST, FIEMAP_EXTENT_SHARED,
    FIEMAP_EXTENT_UNWRITTEN,
};
use eclipsefs::{Compression, EclipseFS, Error, FileKind};

const B: u64 = BLOCK as u64;

fn file(fs: &mut EclipseFS, name: &str) -> u64 {
    let root = fs.root_ino();
    fs.create(root, name, FileKind::Regular, 0o644, 0).unwrap()
}

/// data [0,3) | hole [3,6) | prealloc [6,8) | data [9,11) | hole to 14.
fn sparse(fs: &mut EclipseFS) -> u64 {
    let f = file(fs, "sparse");
    write_all(fs, f, 0, &pattern(3 * BLOCK, 1));
    fs.fallocate(f, FALLOC_FL_KEEP_SIZE, 6 * B, 2 * B).unwrap();
    write_all(fs, f, 9 * B, &pattern(2 * BLOCK, 2));
    fs.truncate(f, 14 * B).unwrap();
    f
}

#[test]
fn fiemap_lists_data_and_unwritten_pieces_in_order() {
    let (disk, mut fs) = fresh(16 * MIB);
    let f = sparse(&mut fs);
    let mut fs = remount(&disk, fs);
    let m = fs.fiemap(f, 0, u64::MAX).unwrap();
    let got: Vec<_> = m.iter().map(|m| (m.logical / B, m.length / B)).collect();
    assert_eq!(got, [(0, 3), (6, 2), (9, 2)]);
    assert_eq!(m[0].flags, 0);
    assert_eq!(m[1].flags, FIEMAP_EXTENT_UNWRITTEN);
    assert_eq!(m[2].flags, FIEMAP_EXTENT_LAST);
    assert!(m.iter().all(|m| m.physical != 0 && m.physical % B == 0));
    // A window only returns what overlaps it, and LAST stays where the
    // file's data ends.
    let w = fs.fiemap(f, 4 * B, 3 * B).unwrap();
    assert_eq!(w.len(), 1);
    assert_eq!((w[0].logical / B, w[0].flags & FIEMAP_EXTENT_LAST), (6, 0));
    assert!(fs.fiemap(f, 14 * B, B).unwrap().is_empty());
    assert!(fs.fiemap(f, 0, 0).unwrap().is_empty());
}

#[test]
fn fiemap_of_inline_compressed_and_shared_files() {
    let (_disk, mut fs) = fresh(32 * MIB);
    let small = file(&mut fs, "small");
    write_all(&mut fs, small, 0, b"tiny");
    let m = fs.fiemap(small, 0, u64::MAX).unwrap();
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].length, 4);
    assert_ne!(m[0].flags & FIEMAP_EXTENT_DATA_INLINE, 0);
    assert_ne!(m[0].flags & FIEMAP_EXTENT_LAST, 0);

    let z = file(&mut fs, "z");
    fs.set_compression(z, Some(Compression::Zstd)).unwrap();
    write_all(&mut fs, z, 0, &vec![7u8; 256 * 1024]);
    fs.sync().unwrap();
    let m = fs.fiemap(z, 0, u64::MAX).unwrap();
    assert!(!m.is_empty());
    assert!(m.iter().all(|m| m.flags & FIEMAP_EXTENT_ENCODED != 0));
    assert_eq!(m.iter().map(|m| m.length).sum::<u64>(), 256 * 1024);

    let a = file(&mut fs, "a");
    write_all(&mut fs, a, 0, &pattern(4 * BLOCK, 3));
    let b = file(&mut fs, "b");
    fs.clone_range(a, 0, b, 0, 4 * B).unwrap();
    fs.sync().unwrap();
    for f in [a, b] {
        let m = fs.fiemap(f, 0, u64::MAX).unwrap();
        assert!(
            m.iter().all(|m| m.flags & FIEMAP_EXTENT_SHARED != 0),
            "{m:?}"
        );
    }
    // Rewriting b's blocks unshares both.
    write_all(&mut fs, b, 0, &pattern(4 * BLOCK, 4));
    fs.sync().unwrap();
    for f in [a, b] {
        let m = fs.fiemap(f, 0, u64::MAX).unwrap();
        assert!(
            m.iter().all(|m| m.flags & FIEMAP_EXTENT_SHARED == 0),
            "{m:?}"
        );
    }
}

#[test]
fn files_in_a_snapshot_report_shared_extents() {
    let (_disk, mut fs) = fresh(32 * MIB);
    let root = fs.root_ino();
    let sv = fs.create_subvolume(root, "sv").unwrap();
    let f = fs.create(sv, "f", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, f, 0, &pattern(8 * BLOCK, 5));
    fs.sync().unwrap();
    assert!(fs
        .fiemap(f, 0, u64::MAX)
        .unwrap()
        .iter()
        .all(|m| m.flags & FIEMAP_EXTENT_SHARED == 0));
    let snap = fs.snapshot(sv, root, "snap", true).unwrap();
    let g = fs.lookup(snap, "f").unwrap();
    for h in [f, g] {
        let m = fs.fiemap(h, 0, u64::MAX).unwrap();
        assert!(
            m.iter().all(|m| m.flags & FIEMAP_EXTENT_SHARED != 0),
            "{m:?}"
        );
    }
}

#[test]
fn seek_data_and_seek_hole_walk_a_sparse_file() {
    let (disk, mut fs) = fresh(16 * MIB);
    let f = sparse(&mut fs);
    let mut fs = remount(&disk, fs);
    assert_eq!(fs.seek_data(f, 0).unwrap(), Some(0));
    assert_eq!(fs.seek_data(f, 100).unwrap(), Some(100));
    assert_eq!(fs.seek_hole(f, 0).unwrap(), Some(3 * B));
    assert_eq!(fs.seek_hole(f, 2 * B + 5).unwrap(), Some(3 * B));
    // Preallocated space is a hole, as on btrfs.
    assert_eq!(fs.seek_data(f, 3 * B).unwrap(), Some(9 * B));
    assert_eq!(fs.seek_data(f, 7 * B).unwrap(), Some(9 * B));
    assert_eq!(fs.seek_hole(f, 4 * B).unwrap(), Some(4 * B));
    assert_eq!(fs.seek_hole(f, 9 * B).unwrap(), Some(11 * B));
    // Past the last data only holes follow; the end of file is a hole.
    assert_eq!(fs.seek_data(f, 11 * B).unwrap(), None);
    assert_eq!(fs.seek_hole(f, 12 * B).unwrap(), Some(12 * B));
    assert_eq!(fs.seek_hole(f, 14 * B).unwrap(), None);
    assert_eq!(fs.seek_data(f, 14 * B).unwrap(), None);
}

#[test]
fn seek_on_small_empty_and_directory_inodes() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let small = file(&mut fs, "small");
    write_all(&mut fs, small, 0, b"0123456789");
    assert_eq!(fs.seek_data(small, 3).unwrap(), Some(3));
    assert_eq!(fs.seek_hole(small, 3).unwrap(), Some(10));
    assert_eq!(fs.seek_hole(small, 10).unwrap(), None);
    let empty = file(&mut fs, "empty");
    assert_eq!(fs.seek_data(empty, 0).unwrap(), None);
    assert_eq!(fs.seek_hole(empty, 0).unwrap(), None);
    let root = fs.root_ino();
    assert_eq!(fs.seek_data(root, 0), Err(Error::IsDir));
    assert_eq!(fs.fiemap(root, 0, 1).map(|_| ()), Err(Error::IsDir));
}

/// Random block writes, punched holes and truncates against a model of
/// which blocks hold data: every SEEK_DATA / SEEK_HOLE answer and the
/// FIEMAP map must agree with it.
#[test]
fn seek_and_fiemap_agree_with_a_model_of_written_blocks() {
    use eclipsefs::cowfmt::FALLOC_FL_PUNCH_HOLE;
    let (disk, mut fs) = fresh(32 * MIB);
    let f = file(&mut fs, "m");
    let n = 64u64;
    let mut model = vec![false; n as usize];
    let mut size = 0u64;
    let mut rng = Lcg(77);
    for round in 0..120 {
        match rng.below(4) {
            0 | 1 => {
                let b = rng.below(n);
                let len = 1 + rng.below(4).min(n - b - 1);
                write_all(&mut fs, f, b * B, &pattern((len * B) as usize, round));
                for i in b..b + len {
                    model[i as usize] = true;
                }
                size = size.max((b + len) * B);
            }
            2 if size > 0 => {
                let b = rng.below(size / B);
                fs.fallocate(f, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, b * B, B)
                    .unwrap();
                model[b as usize] = false;
            }
            _ => {
                let new = rng.below(n + 1) * B;
                fs.truncate(f, new).unwrap();
                for i in new / B..n {
                    model[i as usize] = false;
                }
                size = new;
            }
        }
        if round % 10 == 9 {
            fs = remount(&disk, fs);
        }
        let blocks = size / B;
        for off in (0..size).step_by(BLOCK / 2) {
            let b = (off / B) as usize;
            let next_data = (b..blocks as usize).find(|&i| model[i]);
            let want = next_data.map(|i| (i as u64 * B).max(off));
            assert_eq!(
                fs.seek_data(f, off).unwrap(),
                want,
                "round {round} off {off}"
            );
            let hole = (b..blocks as usize)
                .find(|&i| !model[i])
                .map(|i| i as u64 * B);
            let want = Some(hole.unwrap_or(size).max(off));
            assert_eq!(
                fs.seek_hole(f, off).unwrap(),
                want,
                "round {round} off {off}"
            );
        }
        let mapped: u64 = fs
            .fiemap(f, 0, u64::MAX)
            .unwrap()
            .iter()
            .filter(|m| m.flags & FIEMAP_EXTENT_UNWRITTEN == 0)
            .map(|m| m.length / B)
            .sum();
        assert_eq!(
            mapped,
            model[..blocks as usize].iter().filter(|&&d| d).count() as u64
        );
    }
    fs.check().unwrap();
}
