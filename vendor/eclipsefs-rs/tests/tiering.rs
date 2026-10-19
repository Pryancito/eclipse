#![cfg(feature = "std")]
//! Tiered volumes: fast (SSD) and slow (HDD) devices. Metadata and new
//! data go to the fast ones while they have room, full data chunks move to
//! the slow ones with `tier_migrate`, and tiers survive a remount.

mod common;

use std::sync::Arc;

use common::*;
use eclipsefs::device::BlockDevice;
use eclipsefs::{mkfs, ChunkKind, EclipseFS, FileKind, Profile, Tier};

const MIB: usize = 1 << 20;

fn devs(d: &[Arc<MemDisk>]) -> Vec<Arc<dyn BlockDevice>> {
    d.iter()
        .map(|d| d.clone() as Arc<dyn BlockDevice>)
        .collect()
}

fn open(d: &[Arc<MemDisk>]) -> EclipseFS {
    let mut fs = EclipseFS::mount_devices(devs(d), false).unwrap();
    fs.set_clock(clock);
    fs
}

fn reopen(d: &[Arc<MemDisk>], fs: EclipseFS) -> EclipseFS {
    let mut fs = fs;
    fs.sync().unwrap();
    drop(fs);
    open(d)
}

/// Devices of the given sizes (MiB) and tiers, formatted.
fn make(spec: &[(usize, Tier)], data: Profile, meta: Profile) -> (Vec<Arc<MemDisk>>, EclipseFS) {
    let d: Vec<Arc<MemDisk>> = spec.iter().map(|&(m, _)| MemDisk::new(m * MIB)).collect();
    let tiers: Vec<Tier> = spec.iter().map(|&(_, t)| t).collect();
    mkfs::format_devices_tiered(&devs(&d), &opts(), data, meta, &tiers).unwrap();
    let fs = open(&d);
    (d, fs)
}

fn fill(fs: &mut EclipseFS, n: usize, size: usize, seed: usize) -> Vec<(String, Vec<u8>)> {
    let root = fs.root_ino();
    (0..n)
        .map(|i| {
            let name = format!("f{seed}-{i}");
            let data = pattern(size + i * 777, seed * 100 + i);
            let f = fs.create(root, &name, FileKind::Regular, 0o644, 0).unwrap();
            write_all(fs, f, 0, &data);
            (name, data)
        })
        .collect()
}

fn same(fs: &mut EclipseFS, files: &[(String, Vec<u8>)]) {
    let root = fs.root_ino();
    for (name, data) in files {
        let f = fs.lookup(root, name).unwrap();
        assert_eq!(&read_all(fs, f), data, "{name}");
    }
}

fn clean(fs: &mut EclipseFS) {
    fs.check().unwrap();
    let r = fs.scrub().unwrap();
    assert!(r.is_clean(), "{:?}", r);
}

/// Every stripe of every chunk of `kind` on devices of `tier`.
fn all_on(fs: &EclipseFS, kind: ChunkKind, tier: Tier) -> bool {
    fs.chunks()
        .iter()
        .filter(|c| c.kind == kind)
        .all(|c| c.stripes.iter().all(|s| fs.device_tier(s.0) == tier))
}

fn bytes_on(fs: &EclipseFS, kind: ChunkKind, tier: Tier) -> u64 {
    fs.chunks()
        .iter()
        .filter(|c| c.kind == kind && c.stripes.iter().all(|s| fs.device_tier(s.0) == tier))
        .map(|c| c.used_bytes)
        .sum()
}

#[test]
fn metadata_and_new_data_go_to_the_fast_device() {
    let (d, mut fs) = make(
        &[(48, Tier::Fast), (48, Tier::Slow), (48, Tier::Slow)],
        Profile::Single,
        Profile::Single,
    );
    assert_eq!(fs.device_tier(1), Tier::Fast);
    assert_eq!(fs.device_tier(2), Tier::Slow);
    let files = fill(&mut fs, 10, 120_000, 1);
    fs.sync().unwrap();
    assert!(all_on(&fs, ChunkKind::Metadata, Tier::Fast));
    assert!(all_on(&fs, ChunkKind::Data, Tier::Fast));
    assert!(bytes_on(&fs, ChunkKind::Data, Tier::Fast) > 0);
    let mut fs = reopen(&d, fs);
    // Tiers are kept.
    let tiers: Vec<Tier> = fs.devices().iter().map(|x| x.tier).collect();
    assert_eq!(tiers, [Tier::Fast, Tier::Slow, Tier::Slow]);
    same(&mut fs, &files);
    clean(&mut fs);
}

#[test]
fn migration_moves_data_to_the_slow_devices() {
    let (d, mut fs) = make(
        &[(64, Tier::Fast), (64, Tier::Slow)],
        Profile::Single,
        Profile::Single,
    );
    let files = fill(&mut fs, 16, 400_000, 2);
    fs.sync().unwrap();
    let before = bytes_on(&fs, ChunkKind::Data, Tier::Fast);
    assert!(before > 0);
    let r = fs.tier_migrate(true).unwrap();
    assert!(r.relocated > 0, "{r:?}");
    assert!(all_on(&fs, ChunkKind::Data, Tier::Slow));
    assert!(all_on(&fs, ChunkKind::Metadata, Tier::Fast));
    same(&mut fs, &files);
    let mut fs = reopen(&d, fs);
    same(&mut fs, &files);
    clean(&mut fs);
    // New data lands on the fast device again.
    let more = fill(&mut fs, 3, 100_000, 3);
    fs.sync().unwrap();
    assert!(bytes_on(&fs, ChunkKind::Data, Tier::Fast) > 0);
    let mut fs = reopen(&d, fs);
    same(&mut fs, &files);
    same(&mut fs, &more);
    // Nothing left to move twice.
    let r = fs.tier_migrate(true).unwrap();
    assert!(all_on(&fs, ChunkKind::Data, Tier::Slow), "{r:?}");
    assert_eq!(fs.tier_migrate(true).unwrap().relocated, 0);
}

#[test]
fn only_full_chunks_move_unless_asked() {
    let (_d, mut fs) = make(
        &[(64, Tier::Fast), (64, Tier::Slow)],
        Profile::Single,
        Profile::Single,
    );
    // A little data: its chunk is far from full, so it stays.
    let files = fill(&mut fs, 2, 50_000, 4);
    fs.sync().unwrap();
    assert_eq!(fs.tier_migrate(false).unwrap().relocated, 0);
    assert!(all_on(&fs, ChunkKind::Data, Tier::Fast));
    // Enough to fill chunks: those move, the one being written stays.
    let more = fill(&mut fs, 20, 500_000, 5);
    fs.sync().unwrap();
    let r = fs.tier_migrate(false).unwrap();
    assert!(r.relocated > 0, "{r:?}");
    assert!(bytes_on(&fs, ChunkKind::Data, Tier::Slow) > 0);
    same(&mut fs, &files);
    same(&mut fs, &more);
    clean(&mut fs);
}

#[test]
fn a_full_fast_device_spills_to_the_slow_ones() {
    let (d, mut fs) = make(
        &[(12, Tier::Fast), (96, Tier::Slow)],
        Profile::Single,
        Profile::Single,
    );
    // Far more than the fast device holds.
    let files = fill(&mut fs, 12, 3 * MIB, 6);
    fs.sync().unwrap();
    assert!(bytes_on(&fs, ChunkKind::Data, Tier::Slow) > 0);
    let mut fs = reopen(&d, fs);
    same(&mut fs, &files);
    clean(&mut fs);
}

#[test]
fn mirrored_metadata_lives_on_the_fast_pair() {
    let (d, mut fs) = make(
        &[
            (48, Tier::Slow),
            (48, Tier::Fast),
            (48, Tier::Slow),
            (48, Tier::Fast),
        ],
        Profile::Raid1,
        Profile::Raid1,
    );
    let files = fill(&mut fs, 8, 200_000, 7);
    fs.sync().unwrap();
    // Even group 0, made on (slow) device 1, moved to the fast pair.
    assert!(
        all_on(&fs, ChunkKind::Metadata, Tier::Fast),
        "{:?}",
        fs.chunks()
    );
    fs.tier_migrate(true).unwrap();
    assert!(all_on(&fs, ChunkKind::Data, Tier::Slow));
    let mut fs = reopen(&d, fs);
    same(&mut fs, &files);
    clean(&mut fs);
    // Lose a fast and a slow device: RAID1 still has every block.
    drop(fs);
    let left = [d[0].clone(), d[1].clone()];
    let mut fs = EclipseFS::mount_degraded(devs(&left), true).unwrap();
    same(&mut fs, &files);
}

#[test]
fn tiers_set_later_and_on_added_devices() {
    let a = MemDisk::new(48 * MIB);
    mkfs::format_devices(
        &devs(std::slice::from_ref(&a)),
        &opts(),
        Profile::Single,
        Profile::Single,
    )
    .unwrap();
    let mut fs = open(std::slice::from_ref(&a));
    let files = fill(&mut fs, 6, 150_000, 8);
    // No tiers: nothing changes.
    assert_eq!(fs.device_tier(1), Tier::Any);
    assert_eq!(fs.tier_migrate(true).unwrap().relocated, 0);
    let b = MemDisk::new(48 * MIB);
    let id = fs.add_device_tier(b.clone(), Tier::Slow).unwrap();
    assert_eq!(fs.device_tier(id), Tier::Slow);
    fs.set_device_tier(1, Tier::Fast).unwrap();
    assert!(fs.set_device_tier(9, Tier::Fast).is_err());
    let r = fs.tier_migrate(true).unwrap();
    assert!(r.relocated > 0);
    assert!(all_on(&fs, ChunkKind::Data, Tier::Slow));
    let d = vec![a, b];
    let mut fs = reopen(&d, fs);
    same(&mut fs, &files);
    clean(&mut fs);
    // Untiering a device turns placement back to plain.
    fs.set_device_tier(id, Tier::Any).unwrap();
    let mut fs = reopen(&d, fs);
    assert_eq!(fs.device_tier(id), Tier::Any);
    let more = fill(&mut fs, 3, 100_000, 9);
    let mut fs = reopen(&d, fs);
    same(&mut fs, &files);
    same(&mut fs, &more);
    clean(&mut fs);
}

#[test]
fn tiers_and_parity_together() {
    let (d, mut fs) = make(
        &[
            (48, Tier::Fast),
            (48, Tier::Fast),
            (48, Tier::Fast),
            (48, Tier::Slow),
            (48, Tier::Slow),
            (48, Tier::Slow),
        ],
        Profile::Raid5,
        Profile::Raid1,
    );
    let files = fill(&mut fs, 10, 300_000, 10);
    fs.sync().unwrap();
    assert!(all_on(&fs, ChunkKind::Data, Tier::Fast));
    fs.tier_migrate(true).unwrap();
    assert!(all_on(&fs, ChunkKind::Data, Tier::Slow));
    for c in fs.chunks().iter().filter(|c| c.kind == ChunkKind::Data) {
        assert_eq!((c.profile, c.stripes.len()), (Profile::Raid5, 3));
    }
    let mut fs = reopen(&d, fs);
    same(&mut fs, &files);
    clean(&mut fs);
}
