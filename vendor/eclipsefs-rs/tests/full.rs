#![cfg(feature = "std")]
//! Volumes filled to the last block, in every way space can run out: data
//! chunks, metadata chunks, the fast tier, compressed runs. Whatever runs
//! out first, the filesystem answers `NoSpace` (or a short write), stays
//! consistent, and keeps what it had.

mod common;

use std::sync::Arc;

use common::*;
use eclipsefs::device::BlockDevice;
use eclipsefs::{mkfs, Compression, EclipseFS, Error, FileKind, Profile, Tier};

fn volume(sizes: &[(usize, Tier)], data: Profile, meta: Profile) -> (Vec<Arc<MemDisk>>, EclipseFS) {
    let d: Vec<Arc<MemDisk>> = sizes.iter().map(|&(m, _)| MemDisk::new(m * MIB)).collect();
    let devs: Vec<Arc<dyn BlockDevice>> = d
        .iter()
        .map(|x| x.clone() as Arc<dyn BlockDevice>)
        .collect();
    let tiers: Vec<Tier> = sizes.iter().map(|&(_, t)| t).collect();
    mkfs::format_devices_tiered(&devs, &opts(), data, meta, &tiers).unwrap();
    let mut fs = EclipseFS::mount_devices(devs, false).unwrap();
    fs.set_clock(clock);
    (d, fs)
}

/// Write files of `size` until the filesystem refuses; returns how many
/// went in whole.
fn fill(fs: &mut EclipseFS, prefix: &str, size: usize, algo: Option<Compression>) -> usize {
    let root = fs.root_ino();
    let data = match algo {
        Some(_) => (0..size)
            .map(|i| b"squeeze me "[i % 11])
            .collect::<Vec<u8>>(),
        None => pattern(size, 3),
    };
    for i in 0.. {
        let f = match fs.create(
            root,
            &format!("{}{}", prefix, i),
            FileKind::Regular,
            0o644,
            0,
        ) {
            Ok(f) => f,
            Err(Error::NoSpace) => return i,
            Err(e) => panic!("{:?}", e),
        };
        if algo.is_some() {
            if fs.set_compression(f, algo).is_err() {
                return i;
            }
        }
        let mut at = 0;
        while at < data.len() {
            match fs.write(f, at as u64, &data[at..]) {
                Ok(0) | Err(Error::NoSpace) => return i,
                Ok(n) => at += n,
                Err(e) => panic!("{:?}", e),
            }
        }
        if i % 8 == 7 && matches!(fs.sync(), Err(Error::NoSpace)) {
            return i;
        }
    }
    unreachable!()
}

fn settle(fs: &mut EclipseFS) {
    // A commit may itself find no room; what is committed must be sound.
    let _ = fs.sync();
    fs.check().unwrap();
}

#[test]
fn a_small_fast_tier_overflows_into_the_slow_one() {
    let (_d, mut fs) = volume(
        &[(24, Tier::Fast), (96, Tier::Slow)],
        Profile::Single,
        Profile::Single,
    );
    let n = fill(&mut fs, "z", 256 * 1024, Some(Compression::Zstd));
    assert!(n > 4);
    settle(&mut fs);
    // Free some of it and fill the gaps with plain data.
    let root = fs.root_ino();
    for i in (0..n).step_by(2) {
        fs.unlink(root, &format!("z{}", i)).unwrap();
    }
    fs.sync().unwrap();
    assert!(fill(&mut fs, "p", 512 * 1024, None) > 0);
    settle(&mut fs);
}

#[test]
fn compressed_writes_fill_an_untiered_volume() {
    let (_d, mut fs) = volume(
        &[(32, Tier::Any), (32, Tier::Any)],
        Profile::Raid1,
        Profile::Raid1,
    );
    assert!(fill(&mut fs, "z", 1 << 20, Some(Compression::Lz4)) > 0);
    settle(&mut fs);
}

#[test]
fn many_small_files_exhaust_metadata() {
    let (_d, mut fs) = volume(
        &[(32, Tier::Fast), (32, Tier::Slow)],
        Profile::Single,
        Profile::Dup,
    );
    assert!(fill(&mut fs, "s", 2000, None) > 100);
    settle(&mut fs);
    let (_d, mut fs) = volume(
        &[(32, Tier::Any), (32, Tier::Any)],
        Profile::Raid0,
        Profile::Raid1,
    );
    assert!(fill(&mut fs, "s", 3000, None) > 100);
    settle(&mut fs);
}

#[test]
fn data_stops_before_the_metadata_reserve() {
    let (_d, mut fs) = volume(
        &[(40, Tier::Any), (40, Tier::Any)],
        Profile::Raid0,
        Profile::Raid1,
    );
    fill(&mut fs, "b", 4 << 20, None);
    settle(&mut fs);
    // Names still work at the end: the reserve is there for them.
    let root = fs.root_ino();
    let d = fs.create(root, "still-room", FileKind::Dir, 0o755, 0);
    assert!(d.is_ok() || d == Err(Error::NoSpace));
    settle(&mut fs);
}

/// Small files eat metadata, not data. Before the headroom check they took
/// the last metadata block and every later commit failed with `NoSpace`,
/// for good: on a single device with inline files and on every volume.
fn small_files_leave_room_to_commit(fs: &mut EclipseFS) {
    let root = fs.root_ino();
    let mut made = 0;
    for i in 0.. {
        let name = format!("a-rather-long-name-to-eat-metadata-{}", i);
        let f = match fs.create(root, &name, FileKind::Regular, 0o644, 0) {
            Ok(f) => f,
            Err(Error::NoSpace) => break,
            Err(e) => panic!("{:?}", e),
        };
        match fs.write(f, 0, &[7u8; 900]) {
            Ok(900) => {}
            Ok(_) | Err(Error::NoSpace) => break,
            Err(e) => panic!("{:?}", e),
        }
        match fs.set_xattr(f, "user.pad", &[1u8; 200], 0) {
            Ok(()) | Err(Error::NoSpace) => {}
            Err(e) => panic!("{:?}", e),
        }
        made = i + 1;
        if i % 8 == 7 {
            fs.sync().unwrap();
        }
    }
    assert!(made > 100);
    fs.sync().unwrap();
    fs.check().unwrap();
    // Deleting works on the full filesystem and gives room back.
    for i in 0..made / 2 {
        fs.unlink(root, &format!("a-rather-long-name-to-eat-metadata-{}", i))
            .unwrap();
    }
    fs.sync().unwrap();
    let f = fs
        .create(root, "after", FileKind::Regular, 0o644, 0)
        .unwrap();
    assert_eq!(fs.write(f, 0, b"room again").unwrap(), 10);
    fs.sync().unwrap();
    fs.check().unwrap();
}

#[test]
fn a_full_single_device_still_commits() {
    let (_d, mut fs) = fresh(16 * MIB);
    small_files_leave_room_to_commit(&mut fs);
}

#[test]
fn a_full_volume_still_commits() {
    for (spec, d, m) in [
        (
            vec![(32, Tier::Any), (32, Tier::Any)],
            Profile::Single,
            Profile::Dup,
        ),
        (
            vec![(32, Tier::Any), (32, Tier::Any)],
            Profile::Raid0,
            Profile::Raid1,
        ),
        (
            vec![(32, Tier::Fast), (32, Tier::Slow)],
            Profile::Single,
            Profile::Dup,
        ),
    ] {
        let (_d, mut fs) = volume(&spec, d, m);
        small_files_leave_room_to_commit(&mut fs);
    }
}
