#![cfg(feature = "std")]
//! Online resize (`btrfs filesystem resize`): growing to a size or to the
//! device, and shrinking, which moves data and metadata out of the part
//! cut off while keeping what snapshots and clones share.

mod common;

use std::sync::Arc;

use common::*;
use eclipsefs::device::BlockDevice;
use eclipsefs::fs::FIEMAP_EXTENT_SHARED;
use eclipsefs::{mkfs, Compression, EclipseFS, Error, FileKind, Profile};

const B: u64 = BLOCK as u64;
const M: u64 = MIB as u64;

fn file(fs: &mut EclipseFS, dir: u64, name: &str, data: &[u8]) -> u64 {
    let f = fs.create(dir, name, FileKind::Regular, 0o644, 0).unwrap();
    write_all(fs, f, 0, data);
    f
}

/// Highest disk byte a file's data reaches.
fn top(fs: &mut EclipseFS, f: u64) -> u64 {
    fs.fiemap(f, 0, u64::MAX)
        .unwrap()
        .iter()
        .map(|m| m.physical + m.length)
        .max()
        .unwrap_or(0)
}

/// `n` files of `size` bytes whose data lies past `below` bytes: a filler
/// takes the space before them and is deleted afterwards.
fn files_past(fs: &mut EclipseFS, below: u64, n: usize, size: usize) -> Vec<(u64, Vec<u8>)> {
    let root = fs.root_ino();
    let filler = file(fs, root, "filler", &pattern(below as usize, 99));
    fs.sync().unwrap();
    let out: Vec<_> = (0..n)
        .map(|i| {
            let data = pattern(size + i * 777, i);
            (file(fs, root, &format!("t{i}"), &data), data)
        })
        .collect();
    let _ = filler;
    fs.unlink(root, "filler").unwrap();
    fs.sync().unwrap();
    assert!(out.iter().any(|&(f, _)| top(fs, f) > below));
    out
}

fn same(fs: &mut EclipseFS, files: &[(u64, Vec<u8>)]) {
    for (f, data) in files {
        assert_eq!(&read_all(fs, *f), data);
    }
}

fn total(fs: &EclipseFS) -> u64 {
    fs.fsinfo().total_bytes
}

#[test]
fn grow_to_a_size_then_to_the_device() {
    let (disk, mut fs) = fresh(32 * MIB);
    let root = fs.root_ino();
    let f = file(&mut fs, root, "f", &pattern(3 * BLOCK, 1));
    disk.resize(96 * MIB);
    assert_eq!(fs.resize(1, Some(64 * M + 123)).unwrap(), 64 * M);
    assert_eq!(total(&fs), 64 * M);
    assert_eq!(fs.devices()[0].size_bytes, 64 * M);
    // A size given by hand stays over a remount.
    let mut fs = remount(&disk, fs);
    assert!(!fs.grow_to_device().unwrap());
    assert_eq!(total(&fs), 64 * M);
    // `max` takes the device, and mounting grows with it again.
    assert_eq!(fs.resize(1, None).unwrap(), 96 * M);
    disk.resize(128 * MIB);
    let mut fs = remount(&disk, fs);
    assert!(fs.grow_to_device().unwrap());
    assert_eq!(total(&fs), 128 * M);
    assert_eq!(read_all(&mut fs, f), pattern(3 * BLOCK, 1));
    fs.check().unwrap();
}

#[test]
fn a_size_past_the_device_or_another_device_is_refused() {
    let (_disk, mut fs) = fresh(16 * MIB);
    assert_eq!(fs.resize(1, Some(17 * M)), Err(Error::Invalid));
    assert_eq!(fs.resize(2, Some(8 * M)), Err(Error::NotFound));
    // Smaller than a group may be, or than the journal needs.
    assert_eq!(fs.resize(1, Some(4 * B)), Err(Error::Invalid));
    assert_eq!(total(&fs), 16 * M);
    fs.check().unwrap();
}

#[test]
fn shrinking_an_empty_tail_just_cuts_it() {
    let (disk, mut fs) = fresh(32 * MIB);
    let root = fs.root_ino();
    let f = file(&mut fs, root, "f", &pattern(40 * BLOCK, 2));
    assert_eq!(fs.resize(1, Some(20 * M)).unwrap(), 20 * M);
    assert_eq!(total(&fs), 20 * M);
    assert_eq!(read_all(&mut fs, f), pattern(40 * BLOCK, 2));
    fs.check().unwrap();
    let mut fs = remount(&disk, fs);
    assert_eq!(total(&fs), 20 * M);
    assert!(!fs.grow_to_device().unwrap());
    fs.check().unwrap();
    // The space below is all there is: writes past it do not fit.
    let big = fs.create(root, "big", FileKind::Regular, 0o644, 0).unwrap();
    let r = fs.write(big, 0, &vec![1u8; 24 * MIB]);
    assert!(r.is_err() || r.unwrap() < 24 * MIB);
}

#[test]
fn shrinking_moves_data_and_metadata_out_of_the_tail() {
    let (disk, mut fs) = fresh(32 * MIB);
    let files = files_past(&mut fs, 18 * M, 6, 300 * 1024);
    let root = fs.root_ino();
    // Many small files too, so tree nodes land in the tail.
    let names: Vec<_> = (0..400)
        .map(|i| {
            let n = format!("small-{i:04}");
            file(&mut fs, root, &n, n.as_bytes());
            n
        })
        .collect();
    fs.sync().unwrap();
    assert_eq!(fs.resize(1, Some(14 * M)).unwrap(), 14 * M);
    for &(f, _) in &files {
        assert!(top(&mut fs, f) <= 14 * M);
    }
    same(&mut fs, &files);
    fs.check().unwrap();
    assert!(fs.scrub().unwrap().is_clean());
    let mut fs = remount(&disk, fs);
    same(&mut fs, &files);
    for n in &names {
        let f = fs.lookup(root, n).unwrap();
        assert_eq!(read_all(&mut fs, f), n.as_bytes());
    }
    fs.check().unwrap();
}

#[test]
fn shrinking_into_too_little_space_fails_and_changes_nothing_visible() {
    let (disk, mut fs) = fresh(32 * MIB);
    let root = fs.root_ino();
    let f = file(&mut fs, root, "f", &pattern(20 * MIB, 3));
    fs.sync().unwrap();
    assert_eq!(fs.resize(1, Some(12 * M)), Err(Error::NoSpace));
    assert_eq!(total(&fs), 32 * M);
    assert_eq!(read_all(&mut fs, f), pattern(20 * MIB, 3));
    fs.check().unwrap();
    // Nothing stays fenced off: the space is usable again.
    let g = file(&mut fs, root, "g", &pattern(8 * MIB, 4));
    let mut fs = remount(&disk, fs);
    assert_eq!(read_all(&mut fs, g), pattern(8 * MIB, 4));
    fs.check().unwrap();
}

#[test]
fn sharing_survives_a_shrink() {
    let (disk, mut fs) = fresh(48 * MIB);
    // fsck checks the quota counters too.
    fs.quota_enable().unwrap();
    let root = fs.root_ino();
    let filler = file(&mut fs, root, "filler", &pattern(30 * MIB, 9));
    let _ = filler;
    fs.sync().unwrap();
    let sv = fs.create_subvolume(root, "sv").unwrap();
    let a_data = pattern(2 * MIB, 5);
    let a = file(&mut fs, sv, "a", &a_data);
    let b = fs.create(sv, "b", FileKind::Regular, 0o644, 0).unwrap();
    fs.clone_range(a, 0, b, 0, 2 * M).unwrap();
    let z = fs.create(sv, "z", FileKind::Regular, 0o644, 0).unwrap();
    fs.set_compression(z, Some(Compression::Zstd)).unwrap();
    let z_data: Vec<u8> = (0..MIB).map(|i| (i / 300) as u8).collect();
    write_all(&mut fs, z, 0, &z_data);
    let p = fs.create(sv, "p", FileKind::Regular, 0o644, 0).unwrap();
    fs.fallocate(p, 0, 0, 64 * B).unwrap();
    let snap = fs.snapshot(sv, root, "snap", true).unwrap();
    fs.unlink(root, "filler").unwrap();
    fs.sync().unwrap();
    assert!(top(&mut fs, a) > 30 * M);
    let used = fs.fsinfo().bytes_used;
    assert_eq!(fs.resize(1, Some(24 * M)).unwrap(), 24 * M);
    // The data was copied once, not once per owner.
    assert!(
        fs.fsinfo().bytes_used <= used + 256 * 1024,
        "{} -> {}",
        used,
        fs.fsinfo().bytes_used
    );
    let sa = fs.lookup(snap, "a").unwrap();
    let sb = fs.lookup(snap, "b").unwrap();
    for f in [a, b, sa, sb] {
        assert_eq!(read_all(&mut fs, f), a_data);
        let m = fs.fiemap(f, 0, u64::MAX).unwrap();
        assert!(m.iter().all(|m| m.flags & FIEMAP_EXTENT_SHARED != 0));
        assert!(top(&mut fs, f) <= 24 * M);
    }
    let sz = fs.lookup(snap, "z").unwrap();
    assert_eq!(read_all(&mut fs, sz), z_data);
    assert!(fs.compsize(z).unwrap().compressed_extents > 0);
    let sp = fs.lookup(snap, "p").unwrap();
    assert_eq!(fs.fiemap(sp, 0, u64::MAX).unwrap().len(), 1);
    fs.check().unwrap();
    assert!(fs.scrub().unwrap().is_clean());
    let mut fs = remount(&disk, fs);
    assert_eq!(read_all(&mut fs, sb), a_data);
    // The snapshot still is one: writing to the copy leaves it alone.
    write_all(&mut fs, a, 0, b"changed");
    assert_eq!(read_all(&mut fs, sa), a_data);
    fs.check().unwrap();
}

#[test]
fn shrinking_a_two_group_filesystem_drops_the_second_group() {
    let (disk, mut fs) = fresh(160 * MIB);
    let files = files_past(&mut fs, 130 * M, 3, 2 * MIB);
    assert_eq!(fs.resize(1, Some(100 * M)).unwrap(), 100 * M);
    assert_eq!(fs.chunks().len(), 1);
    same(&mut fs, &files);
    fs.check().unwrap();
    let mut fs = remount(&disk, fs);
    same(&mut fs, &files);
    // A cut that would leave a sliver of a group drops the whole group.
    assert_eq!(fs.resize(1, Some(160 * M)).unwrap(), 160 * M);
    assert_eq!(fs.resize(1, Some(128 * M + 8 * B)).unwrap(), 128 * M);
    assert_eq!(fs.chunks().len(), 1);
    fs.check().unwrap();
}

fn devs(d: &[Arc<MemDisk>]) -> Vec<Arc<dyn BlockDevice>> {
    d.iter()
        .map(|d| d.clone() as Arc<dyn BlockDevice>)
        .collect()
}

fn volume(d: &[Arc<MemDisk>], data: Profile, meta: Profile) -> EclipseFS {
    mkfs::format_devices(&devs(d), &opts(), data, meta).unwrap();
    let mut fs = EclipseFS::mount_devices(devs(d), false).unwrap();
    fs.set_clock(clock);
    fs
}

fn reopen(d: &[Arc<MemDisk>], fs: EclipseFS) -> EclipseFS {
    let mut fs = fs;
    fs.sync().unwrap();
    drop(fs);
    let mut fs = EclipseFS::mount_devices(devs(d), false).unwrap();
    fs.set_clock(clock);
    fs
}

fn size_of(fs: &EclipseFS, devid: u32) -> u64 {
    fs.devices()
        .iter()
        .find(|d| d.devid == devid)
        .unwrap()
        .size_bytes
}

#[test]
fn a_volume_device_shrinks_by_moving_its_chunks() {
    let d: Vec<_> = (0..2).map(|_| MemDisk::new(64 * MIB)).collect();
    let mut fs = volume(&d, Profile::Raid1, Profile::Raid1);
    let root = fs.root_ino();
    let files: Vec<_> = (0..8)
        .map(|i| {
            let data = pattern(2 * MIB + i * 1000, i);
            (file(&mut fs, root, &format!("f{i}"), &data), data)
        })
        .collect();
    fs.sync().unwrap();
    assert_eq!(fs.resize(2, Some(40 * M)).unwrap(), 40 * M);
    assert_eq!(size_of(&fs, 2), 40 * M);
    assert_eq!(size_of(&fs, 1), 64 * M);
    for c in fs.chunks() {
        for &(dev, phys) in &c.stripes {
            if dev == 2 {
                let len = c.size_bytes;
                assert!(phys + len <= 40 * M, "{:?}", c);
            }
        }
    }
    same(&mut fs, &files);
    fs.check().unwrap();
    assert!(fs.scrub().unwrap().is_clean());
    let mut fs = reopen(&d, fs);
    assert!(!fs.grow_to_device().unwrap());
    assert_eq!(size_of(&fs, 2), 40 * M);
    same(&mut fs, &files);
    assert_eq!(fs.resize(2, None).unwrap(), 64 * M);
    assert_eq!(fs.resize(3, Some(M)), Err(Error::NotFound));
    fs.check().unwrap();
}

#[test]
fn a_volume_chunk_with_nowhere_to_go_is_emptied() {
    // One device made a volume: its chunks sit where its groups were, so
    // the second one has no room to move whole and is emptied instead.
    let (disk, mut fs) = fresh(160 * MIB);
    fs.balance(None, None).unwrap();
    let files = files_past(&mut fs, 126 * M, 3, 1536 * 1024);
    assert!(fs.chunks().len() >= 2);
    assert_eq!(fs.resize(1, Some(140 * M)).unwrap(), 140 * M);
    for c in fs.chunks() {
        for &(_, phys) in &c.stripes {
            assert!(phys + c.size_bytes <= 140 * M, "{:?}", c);
        }
    }
    same(&mut fs, &files);
    fs.check().unwrap();
    let mut fs = remount(&disk, fs);
    same(&mut fs, &files);
    // The room left past the first chunk takes new data.
    let root = fs.root_ino();
    let g = file(&mut fs, root, "more", &pattern(4 * MIB, 7));
    assert_eq!(read_all(&mut fs, g), pattern(4 * MIB, 7));
    fs.check().unwrap();
}

#[test]
fn resize_on_v3_is_unsupported() {
    let (_disk, mut fs) = fresh_v3(8 * MIB);
    assert!(matches!(
        fs.resize(1, Some(4 * M)),
        Err(Error::Unsupported(_))
    ));
}

/// Random files, snapshots, clones and deletes between random resizes:
/// contents always match and fsck stays clean.
#[test]
fn random_work_between_resizes() {
    let (disk, mut fs) = fresh(48 * MIB);
    let root = fs.root_ino();
    let mut rng = Lcg(31);
    let mut model: Vec<(String, Vec<u8>)> = Vec::new();
    let mut snaps = 0;
    for step in 0..120 {
        match rng.below(8) {
            0..=2 => {
                let name = format!("f{}", rng.below(40));
                let data = pattern(1 + rng.below(600_000) as usize, step);
                match fs.lookup(root, &name) {
                    Ok(f) => {
                        fs.truncate(f, 0).unwrap();
                        write_all(&mut fs, f, 0, &data);
                    }
                    Err(_) => {
                        file(&mut fs, root, &name, &data);
                    }
                }
                model.retain(|(n, _)| *n != name);
                model.push((name, data));
            }
            3 if !model.is_empty() => {
                let i = rng.below(model.len() as u64) as usize;
                let (name, _) = model.remove(i);
                fs.unlink(root, &name).unwrap();
            }
            4 if !model.is_empty() => {
                let (src, data) = model[rng.below(model.len() as u64) as usize].clone();
                let name = format!("c{step}");
                let s = fs.lookup(root, &src).unwrap();
                let d = fs.create(root, &name, FileKind::Regular, 0o644, 0).unwrap();
                let len = data.len() as u64 / B * B;
                if len > 0 {
                    fs.clone_range(s, 0, d, 0, len).unwrap();
                }
                model.push((name, data[..len as usize].to_vec()));
            }
            5 if snaps < 4 => {
                fs.snapshot(root, root, &format!("s{snaps}"), true).unwrap();
                snaps += 1;
            }
            _ => {
                let want = (16 + rng.below(33)) * M;
                match fs.resize(1, Some(want)) {
                    Ok(n) => assert!(n <= want && n + 64 * B > want, "{n} {want}"),
                    Err(Error::NoSpace) => {}
                    Err(e) => panic!("resize to {want}: {e:?}"),
                }
                fs.check().unwrap();
            }
        }
        if step % 30 == 29 {
            fs = remount(&disk, fs);
        }
    }
    for (name, data) in &model {
        let f = fs.lookup(root, name).unwrap();
        assert_eq!(&read_all(&mut fs, f), data, "{name}");
    }
    fs.check().unwrap();
    assert!(fs.scrub().unwrap().is_clean());
}
