#![cfg(feature = "std")]
//! RAID5 and RAID6 volumes: parity rebuilds lost and rotten blocks, scrub
//! puts bad data and bad parity right, balance converts to and from them,
//! and a power cut never leaves a committed block without its parity (no
//! write hole).

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use common::*;
use eclipsefs::device::BlockDevice;
use eclipsefs::{mkfs, ChunkKind, EclipseFS, Error, FileKind, Profile};

const MIB: usize = 1 << 20;

fn disks(n: usize, size: usize) -> Vec<Arc<MemDisk>> {
    (0..n).map(|_| MemDisk::new(size)).collect()
}

fn devs(d: &[Arc<MemDisk>]) -> Vec<Arc<dyn BlockDevice>> {
    d.iter()
        .map(|d| d.clone() as Arc<dyn BlockDevice>)
        .collect()
}

fn make(d: &[Arc<MemDisk>], data: Profile, meta: Profile) -> EclipseFS {
    mkfs::format_devices(&devs(d), &opts(), data, meta).unwrap();
    open(d)
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

fn fill(fs: &mut EclipseFS, n: usize, size: usize, seed: usize) -> Vec<(String, Vec<u8>)> {
    let root = fs.root_ino();
    (0..n)
        .map(|i| {
            let name = format!("f{seed}-{i}");
            let data = pattern(size + i * 1000, seed * 100 + i);
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

/// Flip a byte in every block of `len` bytes at `at` on `disk`.
fn rot(disk: &MemDisk, at: u64, len: u64) {
    for b in (at..at + len).step_by(BLOCK) {
        disk.poke(b as usize + 100, 0x5A);
    }
}

fn data_chunks(fs: &EclipseFS) -> Vec<eclipsefs::ChunkInfo> {
    fs.chunks()
        .into_iter()
        .filter(|c| c.kind == ChunkKind::Data)
        .collect()
}

/// Rows of a parity chunk on one device: its share of `size_bytes`.
fn stripe_bytes(c: &eclipsefs::ChunkInfo) -> u64 {
    let k = c.stripes.len() as u64 - c.profile.parity() as u64;
    (c.size_bytes / BLOCK as u64).div_ceil(k) * BLOCK as u64
}

#[test]
fn parity_volumes_read_back_after_a_remount() {
    for (n, p) in [
        (2, Profile::Raid5),
        (3, Profile::Raid5),
        (5, Profile::Raid5),
        (3, Profile::Raid6),
        (4, Profile::Raid6),
        (6, Profile::Raid6),
    ] {
        let d = disks(n, 32 * MIB);
        let mut fs = make(&d, p, p);
        assert_eq!(fs.profiles(), (p, p));
        let files = fill(&mut fs, 10, 180_000, n);
        let mut fs = reopen(&d, fs);
        same(&mut fs, &files);
        clean(&mut fs);
        for c in fs.chunks().iter().filter(|c| c.logical > 0) {
            assert_eq!(c.profile, p, "{:?}", c);
            assert_eq!(c.stripes.len(), n, "{:?}", c);
        }
        // Parity costs a device's worth, not a copy of everything.
        let info = fs.fsinfo();
        assert!(info.total_bytes > (n as u64 - p.parity() as u64) * 20 * MIB as u64);
    }
}

#[test]
fn raid5_survives_any_one_lost_device() {
    let d = disks(4, 32 * MIB);
    let mut fs = make(&d, Profile::Raid5, Profile::Raid5);
    let files = fill(&mut fs, 10, 200_000, 2);
    fs.sync().unwrap();
    drop(fs);
    for lost in 0..4 {
        let left: Vec<_> = (0..4)
            .filter(|&i| i != lost)
            .map(|i| d[i].clone())
            .collect();
        assert!(matches!(
            EclipseFS::mount_devices(devs(&left), false).err(),
            Some(Error::Unsupported(_))
        ));
        let mut fs = EclipseFS::mount_degraded(devs(&left), true).unwrap();
        same(&mut fs, &files);
        fs.check().unwrap();
        assert_eq!(fs.devices().iter().filter(|x| !x.present).count(), 1);
    }
    // Two lost devices are one too many.
    assert!(EclipseFS::mount_degraded(devs(&d[..2]), true).is_err());
}

#[test]
fn raid6_survives_any_two_lost_devices() {
    let d = disks(5, 32 * MIB);
    let mut fs = make(&d, Profile::Raid6, Profile::Raid6);
    let files = fill(&mut fs, 8, 220_000, 3);
    fs.sync().unwrap();
    drop(fs);
    for a in 0..5 {
        for b in a + 1..5 {
            let left: Vec<_> = (0..5)
                .filter(|&i| i != a && i != b)
                .map(|i| d[i].clone())
                .collect();
            let mut fs = EclipseFS::mount_degraded(devs(&left), true).unwrap();
            same(&mut fs, &files);
            fs.check().unwrap();
        }
    }
    assert!(EclipseFS::mount_degraded(devs(&d[..2]), true).is_err());
}

#[test]
fn degraded_writes_and_the_device_coming_back() {
    for p in [Profile::Raid5, Profile::Raid6] {
        let d = disks(4, 32 * MIB);
        let mut fs = make(&d, p, p);
        let files = fill(&mut fs, 6, 150_000, 4);
        fs.sync().unwrap();
        drop(fs);
        // Device 4 is away while things change...
        let mut fs = EclipseFS::mount_degraded(devs(&d[..3]), false).unwrap();
        fs.set_clock(clock);
        let root = fs.root_ino();
        fs.unlink(root, &files[0].0).unwrap();
        let more = fill(&mut fs, 5, 120_000, 5);
        same(&mut fs, &files[1..]);
        same(&mut fs, &more);
        fs.sync().unwrap();
        drop(fs);
        // ...and comes back: its share is rebuilt at mount.
        let mut fs = open(&d);
        same(&mut fs, &files[1..]);
        same(&mut fs, &more);
        clean(&mut fs);
        drop(fs);
        // Now another device can go, and the one that was away serves.
        let left = [d[0].clone(), d[2].clone(), d[3].clone()];
        let mut fs = EclipseFS::mount_degraded(devs(&left), true).unwrap();
        same(&mut fs, &files[1..]);
        same(&mut fs, &more);
        assert!(fs.lookup(root, &files[0].0).is_err());
    }
}

#[test]
fn rotten_blocks_are_rebuilt_on_read_and_scrub_fixes_parity() {
    for (n, p) in [
        (3, Profile::Raid5),
        (4, Profile::Raid5),
        (4, Profile::Raid6),
    ] {
        let d = disks(n, 64 * MIB);
        let mut fs = make(&d, p, Profile::Raid1);
        let files = fill(&mut fs, 8, 250_000, 6);
        fs.sync().unwrap();
        let chunks = data_chunks(&fs);
        assert!(!chunks.is_empty());
        // Rot everything device 2 holds of the data chunks (past the row
        // with the group's header): data and parity alike.
        for c in &chunks {
            let (_, at) = *c.stripes.iter().find(|s| s.0 == 2).unwrap();
            rot(&d[1], at + BLOCK as u64, stripe_bytes(c) - BLOCK as u64);
        }
        let mut fs = reopen(&d, fs);
        same(&mut fs, &files);
        // Reads put the data back; the rotten parity is scrub's to find.
        let r = fs.scrub().unwrap();
        assert!(r.is_clean(), "{p:?}: {r:?}");
        assert!(r.repaired > 0, "{p:?}: parity rot went unseen");
        let r = fs.scrub().unwrap();
        assert!(r.is_clean() && r.repaired == 0, "{p:?}: {r:?}");
        let mut fs = reopen(&d, fs);
        same(&mut fs, &files);
        clean(&mut fs);
    }
}

#[test]
fn raid6_rebuilds_rot_while_a_device_is_missing() {
    let d = disks(5, 64 * MIB);
    let mut fs = make(&d, Profile::Raid6, Profile::Raid1c3);
    let files = fill(&mut fs, 6, 300_000, 7);
    fs.sync().unwrap();
    let chunks = data_chunks(&fs);
    drop(fs);
    for c in &chunks {
        let (_, at) = *c.stripes.iter().find(|s| s.0 == 3).unwrap();
        rot(&d[2], at + BLOCK as u64, stripe_bytes(c) - BLOCK as u64);
    }
    // Device 1 gone and device 3 rotten: still every byte.
    let left: Vec<_> = d[1..].to_vec();
    let mut fs = EclipseFS::mount_degraded(devs(&left), true).unwrap();
    same(&mut fs, &files);
}

#[test]
fn rot_beyond_the_parity_is_reported_not_hidden() {
    let d = disks(3, 64 * MIB);
    let mut fs = make(&d, Profile::Raid5, Profile::Raid1);
    fill(&mut fs, 4, 300_000, 8);
    fs.sync().unwrap();
    let chunks = data_chunks(&fs);
    for c in &chunks {
        for dev in [1u32, 2] {
            let (_, at) = *c.stripes.iter().find(|s| s.0 == dev).unwrap();
            rot(
                &d[dev as usize - 1],
                at + BLOCK as u64,
                stripe_bytes(c) - BLOCK as u64,
            );
        }
    }
    let mut fs = reopen(&d, fs);
    let r = fs.scrub().unwrap();
    assert!(!r.is_clean(), "{r:?}");
}

#[test]
fn balance_converts_between_mirrors_and_parity() {
    let d = disks(4, 64 * MIB);
    let mut fs = make(&d, Profile::Raid1, Profile::Raid1);
    let files = fill(&mut fs, 12, 300_000, 9);
    for (data, meta) in [
        (Profile::Raid5, Profile::Raid1),
        (Profile::Raid6, Profile::Raid6),
        (Profile::Raid5, Profile::Raid5),
        (Profile::Raid10, Profile::Raid1c3),
        (Profile::Raid6, Profile::Raid1),
        (Profile::Raid1, Profile::Raid1),
    ] {
        fs.balance(Some(data), Some(meta)).unwrap();
        for c in fs.chunks() {
            let want = match (c.kind, c.logical) {
                (ChunkKind::Data, _) => data,
                (_, 0) if meta == Profile::Raid5 => Profile::Raid1,
                (_, 0) if meta == Profile::Raid6 => Profile::Raid1c3,
                _ => meta,
            };
            assert_eq!(c.profile, want, "{data:?}/{meta:?}: {c:?}");
        }
        same(&mut fs, &files);
        fs = reopen(&d, fs);
        same(&mut fs, &files);
        clean(&mut fs);
    }
}

#[test]
fn parity_needs_its_devices() {
    let d = disks(2, 32 * MIB);
    assert!(matches!(
        mkfs::format_devices(&devs(&d), &opts(), Profile::Raid6, Profile::Raid1),
        Err(Error::Unsupported(_))
    ));
    let mut fs = make(&d, Profile::Raid5, Profile::Raid1);
    assert!(matches!(
        fs.balance(Some(Profile::Raid6), None).unwrap_err(),
        Error::Unsupported(_)
    ));
    // The last-but-one device of a RAID5 cannot be removed.
    assert!(fs.remove_device(2).is_err());
}

/// Small files written and deleted over many commits: rows that a commit
/// left half used are never written again until wholly free, and the
/// space comes back once everything is gone.
#[test]
fn churn_keeps_rows_whole_and_space_comes_back() {
    let d = disks(4, 32 * MIB);
    let mut fs = make(&d, Profile::Raid5, Profile::Raid1);
    let root = fs.root_ino();
    fs.sync().unwrap();
    let base = fs.fsinfo().bytes_used;
    let mut rng = Lcg(5);
    let mut live: Vec<(String, Vec<u8>)> = Vec::new();
    for step in 0..400 {
        if live.len() > 40 || (rng.below(3) == 0 && !live.is_empty()) {
            let (name, _) = live.swap_remove(rng.below(live.len() as u64) as usize);
            fs.unlink(root, &name).unwrap();
        } else {
            let name = format!("s{step}");
            let data = pattern(2000 + rng.below(30_000) as usize, step);
            let f = fs.create(root, &name, FileKind::Regular, 0o644, 0).unwrap();
            write_all(&mut fs, f, 0, &data);
            live.push((name, data));
        }
        if step % 9 == 0 {
            fs.sync().unwrap();
        }
    }
    let mut fs = reopen(&d, fs);
    same(&mut fs, &live);
    clean(&mut fs);
    for (name, _) in live.drain(..) {
        fs.unlink(root, &name).unwrap();
    }
    fs.sync().unwrap();
    fs.sync().unwrap();
    let used = fs.fsinfo().bytes_used;
    assert!(
        used <= base + 64 * BLOCK as u64,
        "{} bytes still used after deleting everything ({} at start)",
        used,
        base
    );
    clean(&mut fs);
}

#[derive(Clone)]
enum Ev {
    Write(usize, u64, Vec<u8>),
    Sync,
}

/// A RAM disk that logs what is written to it.
struct Rec {
    idx: usize,
    data: Mutex<Vec<u8>>,
    log: Arc<Mutex<Vec<Ev>>>,
}

impl BlockDevice for Rec {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> eclipsefs::Result<()> {
        let d = self.data.lock().unwrap();
        let o = offset as usize;
        if o + buf.len() > d.len() {
            return Err(Error::Io);
        }
        buf.copy_from_slice(&d[o..o + buf.len()]);
        Ok(())
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> eclipsefs::Result<()> {
        let mut d = self.data.lock().unwrap();
        let o = offset as usize;
        if o + buf.len() > d.len() {
            return Err(Error::Io);
        }
        d[o..o + buf.len()].copy_from_slice(buf);
        self.log
            .lock()
            .unwrap()
            .push(Ev::Write(self.idx, offset, buf.to_vec()));
        Ok(())
    }
    fn sync(&self) -> eclipsefs::Result<()> {
        self.log.lock().unwrap().push(Ev::Sync);
        Ok(())
    }
    fn size(&self) -> u64 {
        self.data.lock().unwrap().len() as u64
    }
}

fn state(fs: &mut EclipseFS) -> BTreeMap<String, Vec<u8>> {
    let root = fs.root_ino();
    let mut all = BTreeMap::new();
    for e in fs.readdir(root).unwrap() {
        if e.kind == FileKind::Regular {
            all.insert(e.name.clone(), read_all(fs, e.ino));
        }
    }
    all
}

/// The write hole: cut the power at every point of a stretch of work on a
/// RAID5 volume, then lose a device. Every committed file must still read
/// back from the others, which only holds if no committed block's parity
/// was ever left half written.
#[test]
fn power_cuts_leave_no_write_hole() {
    const SIZE: usize = 24 * MIB;
    let log = Arc::new(Mutex::new(Vec::new()));
    let recs: Vec<Arc<Rec>> = (0..3)
        .map(|idx| {
            Arc::new(Rec {
                idx,
                data: Mutex::new(vec![0u8; SIZE]),
                log: log.clone(),
            })
        })
        .collect();
    let all: Vec<Arc<dyn BlockDevice>> = recs.iter().map(|r| r.clone() as _).collect();
    mkfs::format_devices(&all, &opts(), Profile::Raid5, Profile::Raid5).unwrap();
    let base: Vec<Vec<u8>> = recs
        .iter()
        .map(|r| r.data.lock().unwrap().clone())
        .collect();
    log.lock().unwrap().clear();
    let mut fs = EclipseFS::mount_devices(all.clone(), false).unwrap();
    fs.set_clock(clock);
    let root = fs.root_ino();
    let mut states = vec![state(&mut fs)];
    let mut syncs = Vec::new();
    let mut rng = Lcg(91);
    let mut names: Vec<String> = Vec::new();
    for step in 0..50 {
        match rng.below(4) {
            0 | 1 => {
                let name = format!("f{step}");
                let f = fs.create(root, &name, FileKind::Regular, 0o644, 0).unwrap();
                write_all(&mut fs, f, 0, &pattern(rng.below(60_000) as usize, step));
                names.push(name);
            }
            2 if !names.is_empty() => {
                let name = names[rng.below(names.len() as u64) as usize].clone();
                let f = fs.lookup(root, &name).unwrap();
                let off = rng.below(40_000);
                write_all(&mut fs, f, off, &pattern(rng.below(20_000) as usize, step));
            }
            _ if !names.is_empty() => {
                let name = names.swap_remove(rng.below(names.len() as u64) as usize);
                fs.unlink(root, &name).unwrap();
            }
            _ => {}
        }
        states.push(state(&mut fs));
        if step % 5 == 4 {
            fs.sync().unwrap();
            syncs.push((log.lock().unwrap().len(), states.len() - 1));
        }
    }
    fs.sync().unwrap();
    syncs.push((log.lock().unwrap().len(), states.len() - 1));
    drop(fs);
    let log = log.lock().unwrap().clone();
    let mut images = base;
    let stride = (log.len() / 120).max(1);
    let mut checked = 0;
    for (i, ev) in log.iter().enumerate() {
        if i % stride == 0 || i + 1 == log.len() {
            // Lose each device in turn after the cut.
            for lost in 0..3 {
                let left: Vec<Arc<dyn BlockDevice>> = (0..3)
                    .filter(|&j| j != lost)
                    .map(|j| {
                        let d = MemDisk::new(SIZE);
                        d.write_at(0, &images[j]).unwrap();
                        d as Arc<dyn BlockDevice>
                    })
                    .collect();
                let mut fs = EclipseFS::mount_degraded(left, true)
                    .unwrap_or_else(|e| panic!("mount after a cut at {i} without {lost}: {e:?}"));
                fs.check()
                    .unwrap_or_else(|e| panic!("check after a cut at {i} without {lost}: {e:?}"));
                let h = state(&mut fs);
                let durable = syncs
                    .iter()
                    .filter(|(at, _)| *at <= i)
                    .map(|(_, ops)| *ops)
                    .max()
                    .unwrap_or(0);
                assert!(
                    states
                        .iter()
                        .enumerate()
                        .any(|(p, s)| *s == h && p >= durable),
                    "a cut at {i} without device {lost} shows no state from {durable} on"
                );
            }
            checked += 1;
        }
        if let Ev::Write(d, off, data) = ev {
            images[*d][*off as usize..*off as usize + data.len()].copy_from_slice(data);
        }
    }
    assert!(checked > 100);
}

/// An fsync on a parity volume goes through the fsync log, and what a power
/// cut right after it leaves holds the synced data even with a device gone.
#[test]
fn fsync_log_records_survive_on_parity_volumes() {
    for (n, profile) in [(3, Profile::Raid5), (4, Profile::Raid6)] {
        let d = disks(n, 32 * MIB);
        let mut fs = make(&d, profile, profile);
        let root = fs.root_ino();
        let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
        write_all(&mut fs, f, 0, &pattern(50_000, 1));
        fs.fsync(f).unwrap();
        let gen = fs.generation();
        let records = fs.fsync_log_records();
        write_all(&mut fs, f, 8192, &pattern(30_000, 2));
        fs.fsync(f).unwrap();
        assert_eq!(fs.generation(), gen, "{profile:?}: fsync committed");
        assert_eq!(fs.fsync_log_records(), records + 1, "{profile:?}");
        let want = read_all(&mut fs, f);
        let copies: Vec<Arc<MemDisk>> = d.iter().map(|d| d.copy()).collect();
        drop(fs);
        {
            let mut fs = open(&copies);
            assert_eq!(read_all(&mut fs, f), want, "{profile:?}");
            clean(&mut fs);
        }
        for gone in 0..n {
            let left: Vec<Arc<MemDisk>> = copies
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != gone)
                .map(|(_, d)| d.copy())
                .collect();
            let mut fs = EclipseFS::mount_degraded(devs(&left), true).unwrap();
            assert_eq!(read_all(&mut fs, f), want, "{profile:?} without {gone}");
        }
    }
}
