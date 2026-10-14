#![cfg(feature = "std")]
//! Volumes (format v0.4): several devices, DUP and RAID profiles, copies
//! repaired from each other, devices added, removed and replaced, balance.

mod common;

use std::sync::Arc;

use common::*;
use eclipsefs::device::BlockDevice;
use eclipsefs::{mkfs, ChunkKind, EclipseFS, Error, FileKind, Profile};

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

fn degraded(d: &[Arc<MemDisk>]) -> EclipseFS {
    let mut fs = EclipseFS::mount_degraded(devs(d), false).unwrap();
    fs.set_clock(clock);
    fs
}

/// Files with known contents in the root directory.
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

/// Chunks of `kind` beyond group 0 (whose first blocks hold no checksums).
fn data_chunks(fs: &EclipseFS, kind: ChunkKind) -> Vec<eclipsefs::ChunkInfo> {
    fs.chunks()
        .into_iter()
        .filter(|c| c.kind == kind && c.logical > 0)
        .collect()
}

#[test]
fn every_profile_reads_back_after_a_remount() {
    for p in Profile::ALL {
        let d = disks(4, 32 * MIB);
        let mut fs = make(&d, p, p);
        assert_eq!(fs.profiles(), (p, p));
        let files = fill(&mut fs, 12, 150_000, 1);
        let mut fs = reopen(&d, fs);
        same(&mut fs, &files);
        clean(&mut fs);
        for c in fs.chunks() {
            if c.logical == 0 && p.striped() {
                // Group 0 is mirrored instead.
                assert_eq!(c.profile, Profile::Raid1);
                continue;
            }
            assert_eq!(c.profile, p, "{:?}", c);
            let want = match p {
                Profile::Single => 1,
                Profile::Dup | Profile::Raid1 => 2,
                Profile::Raid1c3 => 3,
                Profile::Raid1c4 | Profile::Raid0 | Profile::Raid10 => 4,
            };
            assert_eq!(c.stripes.len(), want, "{:?}", c);
        }
        // Space shows the cost of the copies.
        let info = fs.fsinfo();
        assert!(
            info.bytes_used >= 12 * 150_000 * p.copies() as u64 / 2,
            "{p:?}"
        );
    }
}

#[test]
fn raid1_survives_a_lost_device() {
    let d = disks(2, 32 * MIB);
    let mut fs = make(&d, Profile::Raid1, Profile::Raid1);
    let files = fill(&mut fs, 10, 200_000, 2);
    fs.sync().unwrap();
    drop(fs);
    for lost in 0..2 {
        let left = [d[1 - lost].clone()];
        assert!(matches!(
            EclipseFS::mount_devices(devs(&left), false).err(),
            Some(Error::Unsupported(_))
        ));
        let mut fs = EclipseFS::mount_degraded(devs(&left), true).unwrap();
        same(&mut fs, &files);
        fs.check().unwrap();
        assert_eq!(fs.devices().iter().filter(|x| !x.present).count(), 1);
    }
}

#[test]
fn single_and_raid0_need_every_device() {
    for p in [Profile::Single, Profile::Raid0] {
        let d = disks(2, 32 * MIB);
        let mut fs = make(&d, p, Profile::Raid1);
        fill(&mut fs, 6, 300_000, 3);
        fs.sync().unwrap();
        drop(fs);
        let used: Vec<bool> = d
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let fs = open(&d);
                fs.chunks().iter().any(|c| {
                    c.kind == ChunkKind::Data && c.stripes.iter().any(|s| s.0 == i as u32 + 1)
                })
            })
            .collect();
        for (i, u) in used.iter().enumerate() {
            if *u {
                let left = [d[1 - i].clone()];
                assert!(
                    EclipseFS::mount_degraded(devs(&left), true).is_err(),
                    "{p:?}"
                );
            }
        }
    }
}

#[test]
fn dup_repairs_rotten_data_on_read_and_in_scrub() {
    let d = disks(1, 64 * MIB);
    let mut fs = make(&d, Profile::Dup, Profile::Dup);
    let files = fill(&mut fs, 8, 250_000, 4);
    fs.sync().unwrap();
    let chunks = data_chunks(&fs, ChunkKind::Data);
    assert!(!chunks.is_empty());
    // Rot every block of the first copy (past the group header).
    for c in &chunks {
        let (_, at) = c.stripes[0];
        rot(
            &d[0],
            at + 2 * BLOCK as u64,
            c.size_bytes - 2 * BLOCK as u64,
        );
    }
    let mut fs = reopen(&d, fs);
    same(&mut fs, &files);
    // Reads put the good copies back: nothing left for scrub.
    let r = fs.scrub().unwrap();
    assert!(r.is_clean(), "{:?}", r);
    assert_eq!(r.repaired, 0);
    // Rot the second copy now, and let scrub find it.
    for c in &chunks {
        let (_, at) = c.stripes[1];
        rot(
            &d[0],
            at + 2 * BLOCK as u64,
            c.size_bytes - 2 * BLOCK as u64,
        );
    }
    let r = fs.scrub().unwrap();
    assert!(r.is_clean(), "{:?}", r);
    assert!(r.repaired as usize >= files.iter().map(|f| f.1.len() / BLOCK).sum::<usize>());
    assert_eq!(fs.scrub().unwrap().repaired, 0);
    let mut fs = reopen(&d, fs);
    same(&mut fs, &files);
    clean(&mut fs);
}

#[test]
fn rot_in_both_copies_is_reported_not_hidden() {
    let d = disks(2, 32 * MIB);
    let mut fs = make(&d, Profile::Raid1, Profile::Raid1);
    let files = fill(&mut fs, 2, 100_000, 5);
    fs.sync().unwrap();
    for c in data_chunks(&fs, ChunkKind::Data) {
        for (dev, at) in &c.stripes {
            rot(
                &d[*dev as usize - 1],
                at + 2 * BLOCK as u64,
                c.size_bytes - 2 * BLOCK as u64,
            );
        }
    }
    let mut fs = reopen(&d, fs);
    let root = fs.root_ino();
    let f = fs.lookup(root, &files[0].0).unwrap();
    let mut buf = vec![0u8; 4096];
    assert_eq!(
        fs.read(f, 0, &mut buf).unwrap_err(),
        Error::Corrupt("data checksum")
    );
    let r = fs.scrub().unwrap();
    assert!(!r.bad_data_blocks.is_empty());
}

#[test]
fn metadata_copies_repair_each_other() {
    let d = disks(2, 64 * MIB);
    let mut fs = make(&d, Profile::Single, Profile::Raid1);
    let root = fs.root_ino();
    // Enough metadata for chunks beyond group 0.
    let mut i = 0;
    while data_chunks(&fs, ChunkKind::Metadata).is_empty() {
        let dir = fs
            .create(root, &format!("d{i}"), FileKind::Dir, 0o755, 0)
            .unwrap();
        for j in 0..50 {
            let f = fs
                .create(
                    dir,
                    &format!("file-with-a-long-name-{j}"),
                    FileKind::Regular,
                    0o644,
                    0,
                )
                .unwrap();
            fs.set_xattr(f, "user.pad", &[j as u8; 600], 0).unwrap();
        }
        i += 1;
        assert!(i < 400, "metadata never left group 0");
    }
    for k in 0..20 {
        let dir = fs.lookup(root, &format!("d{k}")).unwrap();
        fs.unlink(dir, "file-with-a-long-name-0").unwrap();
    }
    fs.sync().unwrap();
    let expect = fs.readdir(root).unwrap().len();
    for c in data_chunks(&fs, ChunkKind::Metadata) {
        let (dev, at) = c.stripes[0];
        rot(
            &d[dev as usize - 1],
            at + 2 * BLOCK as u64,
            c.size_bytes - 2 * BLOCK as u64,
        );
    }
    let mut fs = reopen(&d, fs);
    assert_eq!(fs.readdir(root).unwrap().len(), expect);
    fs.check().unwrap();
    let r = fs.scrub().unwrap();
    assert!(r.is_clean(), "{:?}", r);
    let mut fs = reopen(&d, fs);
    clean(&mut fs);
    assert_eq!(fs.scrub().unwrap().repaired, 0);
}

#[test]
fn a_single_device_filesystem_grows_into_raid1() {
    let a = MemDisk::new(32 * MIB);
    mkfs::format(&*a, &opts()).unwrap();
    let mut fs = EclipseFS::mount(a.clone(), false).unwrap();
    fs.set_clock(clock);
    let files = fill(&mut fs, 6, 200_000, 6);
    assert_eq!(fs.devices().len(), 1);
    let b = MemDisk::new(48 * MIB);
    assert_eq!(fs.add_device(b.clone()).unwrap(), 2);
    assert_eq!(fs.add_device(b.clone()).unwrap_err(), Error::Exists);
    let r = fs
        .balance(Some(Profile::Raid1), Some(Profile::Raid1))
        .unwrap();
    assert!(r.relocated > 0);
    for c in fs.chunks() {
        assert_eq!(c.profile, Profile::Raid1, "{:?}", c);
        assert_ne!(c.stripes[0].0, c.stripes[1].0);
    }
    let more = fill(&mut fs, 3, 100_000, 7);
    let d = vec![a.clone(), b.clone()];
    let mut fs = reopen(&d, fs);
    same(&mut fs, &files);
    same(&mut fs, &more);
    clean(&mut fs);
    drop(fs);
    // Either device alone still has everything.
    for one in [a, b] {
        let mut fs = EclipseFS::mount_degraded(devs(&[one]), true).unwrap();
        same(&mut fs, &files);
        same(&mut fs, &more);
    }
}

#[test]
fn removing_a_device_moves_its_chunks() {
    let d = disks(3, 32 * MIB);
    let mut fs = make(&d, Profile::Raid1, Profile::Raid1);
    let files = fill(&mut fs, 10, 150_000, 8);
    fs.remove_device(2).unwrap();
    assert!(fs
        .chunks()
        .iter()
        .all(|c| c.stripes.iter().all(|s| s.0 != 2)));
    assert_eq!(fs.devices().len(), 2);
    // RAID1 needs two devices.
    assert!(matches!(
        fs.remove_device(1).unwrap_err(),
        Error::Unsupported(_)
    ));
    assert_eq!(fs.remove_device(2).unwrap_err(), Error::NotFound);
    let left = vec![d[0].clone(), d[2].clone()];
    let mut fs = reopen(&left, fs);
    same(&mut fs, &files);
    clean(&mut fs);
    // The removed device is no longer an EclipseFS member.
    assert!(EclipseFS::mount(d[1].clone(), true).is_err());
    assert!(EclipseFS::mount_devices(devs(&d), true).is_err());
}

#[test]
fn a_lost_device_is_replaced() {
    let d = disks(2, 32 * MIB);
    let mut fs = make(&d, Profile::Raid1, Profile::Raid1);
    let files = fill(&mut fs, 8, 150_000, 9);
    fs.sync().unwrap();
    drop(fs);
    // Device 2 dies; keep working without it.
    let mut fs = degraded(&d[..1]);
    let more = fill(&mut fs, 4, 100_000, 10);
    let c = MemDisk::new(40 * MIB);
    let id = fs.replace_device(2, c.clone()).unwrap();
    assert_eq!(id, 3);
    assert!(fs.devices().iter().all(|x| x.present));
    let d2 = vec![d[0].clone(), c.clone()];
    let mut fs = reopen(&d2, fs);
    same(&mut fs, &files);
    same(&mut fs, &more);
    clean(&mut fs);
    drop(fs);
    // And the new device holds a full copy.
    let mut fs = EclipseFS::mount_degraded(devs(&[c]), true).unwrap();
    same(&mut fs, &files);
    same(&mut fs, &more);
}

#[test]
fn a_device_that_missed_commits_is_resynced() {
    let d = disks(2, 32 * MIB);
    let mut fs = make(&d, Profile::Raid1, Profile::Raid1);
    let files = fill(&mut fs, 6, 150_000, 11);
    fs.sync().unwrap();
    drop(fs);
    // Device 2 is away while things change...
    let mut fs = degraded(&d[..1]);
    let root = fs.root_ino();
    fs.unlink(root, &files[0].0).unwrap();
    let more = fill(&mut fs, 5, 120_000, 12);
    fs.sync().unwrap();
    drop(fs);
    // ...and comes back: it is brought up to date at mount.
    let mut fs = open(&d);
    same(&mut fs, &files[1..]);
    same(&mut fs, &more);
    clean(&mut fs);
    drop(fs);
    // Now device 1 can go and device 2 alone is current.
    let mut fs = EclipseFS::mount_degraded(devs(&d[1..]), true).unwrap();
    same(&mut fs, &files[1..]);
    same(&mut fs, &more);
    assert!(fs.lookup(root, &files[0].0).is_err());
}

#[test]
fn balance_spreads_chunks_and_frees_empty_ones() {
    let d = disks(2, 64 * MIB);
    let mut fs = make(&d, Profile::Single, Profile::Dup);
    let root = fs.root_ino();
    let files = fill(&mut fs, 20, 1_000_000, 13);
    // Empty a lot of data chunks.
    for (name, _) in &files[..15] {
        fs.unlink(root, name).unwrap();
    }
    fs.sync().unwrap();
    let before = fs.chunks().len();
    let r = fs.balance(None, None).unwrap();
    assert!(r.freed > 0, "{:?}", r);
    assert!(fs.chunks().len() < before);
    let mut fs = reopen(&d, fs);
    same(&mut fs, &files[15..]);
    clean(&mut fs);
    // Converting data to RAID1 and metadata down to single.
    fs.balance(Some(Profile::Raid1), Some(Profile::Single))
        .unwrap();
    for c in fs.chunks() {
        let want = if c.kind == ChunkKind::Data {
            Profile::Raid1
        } else {
            Profile::Single
        };
        assert_eq!(c.profile, want);
    }
    let mut fs = reopen(&d, fs);
    same(&mut fs, &files[15..]);
    clean(&mut fs);
}

#[test]
fn profiles_need_their_devices() {
    let d = disks(2, 32 * MIB);
    assert!(matches!(
        mkfs::format_devices(&devs(&d), &opts(), Profile::Raid1c3, Profile::Raid1),
        Err(Error::Unsupported(_))
    ));
    let mut fs = make(&d, Profile::Single, Profile::Single);
    assert!(matches!(
        fs.balance(Some(Profile::Raid10), None).unwrap_err(),
        Error::Unsupported(_)
    ));
    // A device of another filesystem is refused at mount.
    let other = disks(1, 32 * MIB);
    let _ = make(&other, Profile::Single, Profile::Dup);
    let mixed = vec![d[0].clone(), other[0].clone()];
    drop(fs);
    assert!(EclipseFS::mount_devices(devs(&mixed), true).is_err());
    // The same device twice too.
    let twice = vec![d[0].clone(), d[0].clone()];
    assert!(EclipseFS::mount_devices(devs(&twice), true).is_err());
}

#[test]
fn full_volumes_say_so_and_stay_consistent() {
    let d = disks(2, 16 * MIB);
    let mut fs = make(&d, Profile::Raid1, Profile::Raid1);
    let root = fs.root_ino();
    let f = fs.create(root, "big", FileKind::Regular, 0o644, 0).unwrap();
    let chunk = pattern(1 << 20, 14);
    let mut off = 0;
    loop {
        match fs.write(f, off, &chunk) {
            Ok(n) if n == chunk.len() => off += n as u64,
            Ok(n) => {
                off += n as u64;
                break;
            }
            Err(Error::NoSpace) => break,
            Err(e) => panic!("{:?}", e),
        }
    }
    assert!(off > 8 * MIB as u64, "only {off} bytes fit");
    assert!(off < 16 * MIB as u64);
    fs.check().unwrap();
    // Metadata still has room to free things.
    fs.unlink(root, "big").unwrap();
    let mut fs = reopen(&d, fs);
    clean(&mut fs);
    fill(&mut fs, 3, 1_000_000, 15);
    clean(&mut fs);
}

// ---------------------------------------------------------------------------
// Power cuts across several devices
// ---------------------------------------------------------------------------

use std::collections::BTreeMap;
use std::sync::Mutex;

/// Every write and sync to any device of a volume, in order.
#[derive(Clone)]
enum Ev {
    Write(usize, u64, Vec<u8>),
    Sync,
}

/// A RAM disk that also logs what is written to it.
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

/// What a crash may show: names and contents in two directories.
fn state(fs: &mut EclipseFS) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let root = fs.root_ino();
    let mut stack = vec![(String::new(), root)];
    let mut all = BTreeMap::new();
    while let Some((path, d)) = stack.pop() {
        for e in fs.readdir(d).unwrap() {
            let p = format!("{path}/{}", e.name);
            match e.kind {
                FileKind::Dir => stack.push((p, e.ino)),
                _ => {
                    all.insert(p, read_all(fs, e.ino));
                }
            }
        }
    }
    all.hash(&mut h);
    h.finish()
}

/// Mount the volume on whichever images look like members.
fn mount_images(images: &[Vec<u8>]) -> EclipseFS {
    let devs: Vec<Arc<dyn BlockDevice>> = images
        .iter()
        .filter(|i| &i[0..9] == b"ECLIPSEFS")
        .map(|i| {
            let d = MemDisk::new(i.len());
            d.write_at(0, i).unwrap();
            d as Arc<dyn BlockDevice>
        })
        .collect();
    let mut fs = EclipseFS::mount_devices(devs, false).unwrap();
    fs.set_clock(clock);
    fs
}

#[test]
fn power_cuts_while_devices_come_and_go() {
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
    mkfs::format_devices(&all[..2], &opts(), Profile::Raid1, Profile::Raid1).unwrap();
    let base: Vec<Vec<u8>> = recs
        .iter()
        .map(|r| r.data.lock().unwrap().clone())
        .collect();
    log.lock().unwrap().clear();
    let mut fs = EclipseFS::mount_devices(all[..2].to_vec(), false).unwrap();
    fs.set_clock(clock);
    let root = fs.root_ino();
    let dir = fs.create(root, "dir", FileKind::Dir, 0o755, 0).unwrap();
    let mut states = vec![state(&mut fs)];
    // (log position, operations done) at each sync.
    let mut syncs = Vec::new();
    let mut rng = Lcg(77);
    let mut names: Vec<(u64, String)> = Vec::new();
    for step in 0..60 {
        match step {
            15 => {
                fs.add_device(all[2].clone()).unwrap();
            }
            25 => {
                fs.balance(None, None).unwrap();
            }
            32 => {
                fs.balance(Some(Profile::Raid0), None).unwrap();
            }
            40 => {
                fs.remove_device(1).unwrap();
            }
            50 => {
                fs.balance(Some(Profile::Single), Some(Profile::Dup))
                    .unwrap();
            }
            _ => match rng.below(4) {
                0 | 1 => {
                    let d = if rng.below(2) == 0 { root } else { dir };
                    let name = format!("f{step}");
                    let f = fs.create(d, &name, FileKind::Regular, 0o644, 0).unwrap();
                    let n = rng.below(200_000) as usize;
                    write_all(&mut fs, f, 0, &pattern(n, step));
                    names.push((d, name));
                }
                2 if !names.is_empty() => {
                    let (d, name) = names[rng.below(names.len() as u64) as usize].clone();
                    let f = fs.lookup(d, &name).unwrap();
                    let off = rng.below(100_000);
                    write_all(&mut fs, f, off, &pattern(rng.below(50_000) as usize, step));
                }
                _ if !names.is_empty() => {
                    let (d, name) = names.swap_remove(rng.below(names.len() as u64) as usize);
                    fs.unlink(d, &name).unwrap();
                }
                _ => {}
            },
        }
        states.push(state(&mut fs));
        if step % 7 == 6 {
            fs.sync().unwrap();
            syncs.push((log.lock().unwrap().len(), states.len() - 1));
        }
    }
    fs.sync().unwrap();
    syncs.push((log.lock().unwrap().len(), states.len() - 1));
    assert_eq!(fs.devices().len(), 2);
    assert_eq!(fs.profiles(), (Profile::Single, Profile::Dup));
    drop(fs);
    let log = log.lock().unwrap().clone();
    let mut images = base;
    let stride = (log.len() / 300).max(1);
    let mut checked = 0;
    for (i, ev) in log.iter().enumerate() {
        if i % stride == 0 || i + 1 == log.len() {
            let mut fs = mount_images(&images);
            fs.check()
                .unwrap_or_else(|e| panic!("check after a cut at {i}: {e:?}"));
            let r = fs.scrub().unwrap();
            assert!(r.is_clean(), "scrub after a cut at {i}: {r:?}");
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
                "a cut at {i} shows no state from operation {durable} on"
            );
            checked += 1;
        }
        if let Ev::Write(d, off, data) = ev {
            images[*d][*off as usize..*off as usize + data.len()].copy_from_slice(data);
        }
    }
    assert!(checked > 250);
}

#[test]
fn probe_tells_which_device_of_which_volume_without_mounting() {
    let d = disks(3, 32 * MIB);
    let mut fs = make(&d, Profile::Raid1, Profile::Raid1);
    let uuid = fs.uuid();
    fill(&mut fs, 2, 50_000, 3);
    let gen = {
        fs.sync().unwrap();
        fs.generation()
    };
    drop(fs);
    let mut ids: Vec<u32> = d
        .iter()
        .map(|disk| {
            let p = eclipsefs::probe(&**disk).unwrap();
            assert_eq!(p.uuid, uuid);
            assert_eq!(p.num_devices, 3);
            assert_eq!(p.generation, gen);
            assert_eq!(p.label, "eclipse");
            assert_eq!(p.version, eclipsefs::ECLIPSEFS_VERSION);
            // A member holds its own superblock; the chunk map places the rest.
            assert!(p.min_device_bytes <= 4096);
            p.devid
        })
        .collect();
    ids.sort();
    assert_eq!(ids, [1, 2, 3]);

    // One device: all of the filesystem has to fit on it.
    let one = MemDisk::new(16 * MIB);
    mkfs::format(&*one, &opts()).unwrap();
    let p = eclipsefs::probe(&*one).unwrap();
    assert_eq!((p.devid, p.num_devices), (1, 1));
    assert_eq!(p.min_device_bytes, 16 * MIB as u64);

    // The older layout, a blank disk and a damaged superblock.
    let old = MemDisk::new(16 * MIB);
    mkfs::format_v3(&*old, &opts()).unwrap();
    assert_eq!(eclipsefs::probe(&*old).unwrap().version >> 16, 3);
    assert_eq!(
        eclipsefs::probe(&*MemDisk::new(MIB)).unwrap_err(),
        Error::BadSuperblock
    );
    let mut sb = vec![0u8; 4096];
    one.read_at(0, &mut sb).unwrap();
    sb[100] ^= 0xff;
    one.write_at(0, &sb).unwrap();
    assert!(matches!(eclipsefs::probe(&*one), Err(Error::Corrupt(_))));
}
