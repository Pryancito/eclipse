#![cfg(feature = "std")]
//! A disk that dies: every workload is run again and again with the device
//! failing for good at its 1st, 2nd, ... N-th operation (read, write or
//! flush), so every place the driver touches the device sees an error once.
//!
//! What must hold each time, wherever the error lands:
//! - the call that hit it returns an error instead of panicking or
//!   carrying on with half-updated state;
//! - unmounting (which tries to commit, and fails too) does not panic;
//! - the image as the failure left it (everything written before the error
//!   is on disk, nothing after) mounts and passes `check`, with the log
//!   replayed: a disk that dies is a power cut, and must be survived as one.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use common::*;
use eclipsefs::cowfmt::{
    FALLOC_FL_KEEP_SIZE, FALLOC_FL_PUNCH_HOLE, FALLOC_FL_ZERO_RANGE, FL_NOCOW,
};
use eclipsefs::device::BlockDevice;
use eclipsefs::{
    mkfs, Compression, EclipseFS, EncryptionPolicy, Error, FileKind, Profile, Result, Tier,
};

/// Operations of all the devices of one run, and where they start failing.
struct Clock {
    ops: AtomicU64,
    fail_at: u64,
}

impl Clock {
    fn new(fail_at: u64) -> Arc<Self> {
        Arc::new(Self {
            ops: AtomicU64::new(0),
            fail_at,
        })
    }
    fn tick(&self) -> Result<()> {
        if self.ops.fetch_add(1, Ordering::Relaxed) >= self.fail_at {
            return Err(Error::Io);
        }
        Ok(())
    }
    fn ops(&self) -> u64 {
        self.ops.load(Ordering::Relaxed)
    }
}

/// A RAM disk whose operations count against a [`Clock`].
struct Faulty {
    disk: Arc<MemDisk>,
    clock: Arc<Clock>,
}

impl BlockDevice for Faulty {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.clock.tick()?;
        self.disk.read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        self.clock.tick()?;
        self.disk.write_at(offset, buf)
    }
    fn sync(&self) -> Result<()> {
        self.clock.tick()?;
        self.disk.sync()
    }
    fn size(&self) -> u64 {
        self.disk.size()
    }
}

type Work = fn(&mut EclipseFS, &[Arc<dyn BlockDevice>]) -> Result<()>;

/// How the devices of a run are formatted.
#[derive(Clone, Copy)]
enum Layout {
    Single,
    Volume(Profile, Profile, &'static [Tier]),
}

/// One run: format and mount (outside the fault clock), then the workload
/// with the device failing from operation `fail_at` on. Returns the
/// operations the workload made.
fn run(layout: Layout, disks: usize, mib: usize, spare: usize, work: Work, fail_at: u64) -> u64 {
    let raw: Vec<Arc<MemDisk>> = (0..disks + spare)
        .map(|_| MemDisk::new(mib * MIB))
        .collect();
    let plain: Vec<Arc<dyn BlockDevice>> = raw[..disks]
        .iter()
        .map(|d| d.clone() as Arc<dyn BlockDevice>)
        .collect();
    match layout {
        Layout::Single => mkfs::format(&*raw[0], &opts()).unwrap(),
        Layout::Volume(d, m, tiers) => {
            mkfs::format_devices_tiered(&plain, &opts(), d, m, tiers).unwrap()
        }
    }
    let clock = Clock::new(fail_at);
    let faulty: Vec<Arc<dyn BlockDevice>> = raw
        .iter()
        .map(|d| {
            Arc::new(Faulty {
                disk: d.clone(),
                clock: clock.clone(),
            }) as Arc<dyn BlockDevice>
        })
        .collect();
    let mut fs = match EclipseFS::mount_devices(faulty[..disks].to_vec(), false) {
        Ok(fs) => fs,
        Err(e) => {
            assert!(fail_at != u64::MAX, "mount failed: {:?}", e);
            return clock.ops();
        }
    };
    fs.set_clock(clock_fn);
    let r = work(&mut fs, &faulty[disks..]).and_then(|_| fs.sync());
    if fail_at == u64::MAX {
        r.unwrap_or_else(|e| panic!("workload fails without faults: {:?}", e));
    }
    drop(fs);
    let ops = clock.ops();
    // The image as the failure left it. Which devices are in the volume
    // depends on where the failure stopped an add, remove or replace: the
    // ones holding the newest superblock are.
    let newest = raw
        .iter()
        .filter_map(|d| eclipsefs::probe(&**d).ok())
        .map(|p| p.generation)
        .max()
        .unwrap();
    let members: Vec<Arc<dyn BlockDevice>> = raw
        .iter()
        .filter(|d| eclipsefs::probe(&***d).is_ok_and(|p| p.generation + 1 >= newest))
        .map(|d| d.clone() as Arc<dyn BlockDevice>)
        .collect();
    let mut after = EclipseFS::mount_devices(members.clone(), false)
        .or_else(|_| EclipseFS::mount_degraded(members, false))
        .unwrap_or_else(|e| panic!("failure at op {}: no mount: {:?}", fail_at, e));
    after
        .check()
        .unwrap_or_else(|e| panic!("failure at op {}: check: {:?}", fail_at, e));
    ops
}

fn clock_fn() -> (u64, u32) {
    clock()
}

/// Every failure point of `work`, or `samples` of them spread evenly when
/// there are more.
fn torture(layout: Layout, disks: usize, mib: usize, spare: usize, work: Work, samples: u64) {
    let total = run(layout, disks, mib, spare, work, u64::MAX);
    assert!(total > 0);
    eprintln!("{} operations", total);
    let step = (total / samples).max(1);
    let points: Vec<u64> = (0..total).step_by(step as usize).collect();
    std::thread::scope(|s| {
        for chunk in points.chunks(points.len().div_ceil(8)) {
            s.spawn(move || {
                for &n in chunk {
                    run(layout, disks, mib, spare, work, n);
                }
            });
        }
    });
}

fn file(fs: &mut EclipseFS, dir: u64, name: &str) -> Result<u64> {
    fs.create(dir, name, FileKind::Regular, 0o644, 0)
}

fn put(fs: &mut EclipseFS, ino: u64, off: u64, data: &[u8]) -> Result<()> {
    let mut done = 0;
    while done < data.len() {
        let n = fs.write(ino, off + done as u64, &data[done..])?;
        if n == 0 {
            return Err(Error::NoSpace);
        }
        done += n;
    }
    Ok(())
}

/// Commit and forget the cached nodes, so what comes next reads the trees
/// from the (dying) device again.
fn commit(fs: &mut EclipseFS) -> Result<()> {
    fs.sync()?;
    fs.drop_caches();
    Ok(())
}

fn get(fs: &mut EclipseFS, ino: u64) -> Result<Vec<u8>> {
    let size = fs.stat(ino)?.size as usize;
    let mut buf = vec![0u8; size];
    let n = fs.read(ino, 0, &mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Workloads
// ---------------------------------------------------------------------------

/// Names, data of every shape, attributes and the fsync log.
fn files(fs: &mut EclipseFS, _: &[Arc<dyn BlockDevice>]) -> Result<()> {
    let root = fs.root_ino();
    let d = fs.create(root, "d", FileKind::Dir, 0o755, 0)?;
    let e = fs.create(d, "e", FileKind::Dir, 0o755, 0)?;
    let tiny = file(fs, d, "tiny")?;
    put(fs, tiny, 0, b"inline")?;
    let mid = file(fs, e, "mid")?;
    put(fs, mid, 0, &pattern(9000, 1))?;
    let big = file(fs, root, "big")?;
    put(fs, big, 0, &pattern(300_000, 2))?;
    commit(fs)?;
    put(fs, big, 4096 * 10 + 7, &pattern(20_000, 3))?;
    put(fs, tiny, 6, &pattern(3000, 4))?;
    fs.fsync(big)?;
    fs.fsync(tiny)?;
    fs.truncate(big, 150_001)?;
    fs.truncate(mid, 50_000)?;
    fs.fsync(big)?;
    fs.symlink(d, "ln", b"../big")?;
    fs.link(e, "big2", big)?;
    fs.rename(d, "tiny", e, "moved")?;
    let other = file(fs, e, "other")?;
    fs.rename(e, "moved", e, "other")?;
    let _ = other;
    fs.unlink(root, "big")?;
    fs.set_xattr(mid, "user.a", b"1", 0)?;
    fs.set_xattr(mid, "user.b", &pattern(1500, 5), 0)?;
    fs.remove_xattr(mid, "user.a")?;
    fs.set_attr(mid, Some(0o600), Some(1), Some(2), None, None)?;
    let p = file(fs, root, "prealloc")?;
    fs.fallocate(p, 0, 0, 200_000)?;
    put(fs, p, 8192, &pattern(5000, 6))?;
    fs.fallocate(p, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 4096, 8192)?;
    fs.fallocate(p, FALLOC_FL_ZERO_RANGE, 100_000, 20_000)?;
    fs.fsync(p)?;
    let n = file(fs, root, "nocow")?;
    fs.set_flags(n, FL_NOCOW)?;
    put(fs, n, 0, &pattern(40_000, 7))?;
    commit(fs)?;
    put(fs, n, 4096, &pattern(8192, 8))?;
    fs.fsync(n)?;
    if fs.atomic_write_supported(p)? {
        fs.write_atomic(p, 0, &pattern(16384, 9))?;
    }
    fs.seek_data(p, 0)?;
    fs.seek_hole(p, 0)?;
    fs.fiemap(p, 0, u64::MAX)?;
    for ino in [mid, p, n, big] {
        get(fs, ino)?;
    }
    fs.readdir(e)?;
    let ln = fs.lookup(d, "ln")?;
    fs.read_link(ln)?;
    fs.unlink(e, "big2")?;
    fs.fsync(p)?;
    Ok(())
}

/// Compressed writes, per file and per mount, and defrag.
fn compressed(fs: &mut EclipseFS, _: &[Arc<dyn BlockDevice>]) -> Result<()> {
    let root = fs.root_ino();
    let text: Vec<u8> = (0..400_000)
        .map(|i| b"compressible line\n"[i % 18])
        .collect();
    fs.set_mount_compression(Some(Compression::Zstd))?;
    let a = file(fs, root, "a")?;
    put(fs, a, 0, &text)?;
    let b = file(fs, root, "b")?;
    fs.set_compression(b, Some(Compression::Lz4))?;
    put(fs, b, 0, &text[..200_000])?;
    commit(fs)?;
    put(fs, a, 70_000, &pattern(30_000, 1))?;
    fs.set_mount_compression(None)?;
    let c = file(fs, root, "c")?;
    for i in 0..8 {
        put(fs, c, i * 40_960, &text[..4096])?;
        commit(fs)?;
    }
    fs.defrag(c, 0, u64::MAX, 0, Some(Compression::Zlib))?;
    fs.compsize(a)?;
    fs.truncate(a, 100_000)?;
    for ino in [a, b, c] {
        get(fs, ino)?;
    }
    Ok(())
}

/// Subvolumes, snapshots, reflinks, dedupe and quotas.
fn snapshots(fs: &mut EclipseFS, _: &[Arc<dyn BlockDevice>]) -> Result<()> {
    let root = fs.root_ino();
    fs.quota_enable()?;
    let sv = fs.create_subvolume(root, "sv")?;
    let f = file(fs, sv, "f")?;
    put(fs, f, 0, &pattern(100_000, 1))?;
    let g = file(fs, sv, "g")?;
    fs.clone_range(f, 0, g, 0, 65536)?;
    put(fs, g, 70_000, &pattern(100_000, 1)[70_000..])?;
    fs.dedupe_range(f, 0, g, 0, 98_304)?;
    let snap = fs.snapshot(sv, root, "snap", false)?;
    let f2 = fs.lookup(snap, "f")?;
    put(fs, f2, 5000, b"changed in the snapshot")?;
    put(fs, f, 9000, b"changed in the original")?;
    let id = fs.subvolume_of(snap);
    fs.set_qgroup_limit(id, Some(10 << 20))?;
    fs.qgroup_show()?;
    fs.set_subvolume_read_only(id, true)?;
    fs.set_subvolume_read_only(id, false)?;
    fs.set_default_subvolume(fs.subvolume_of(sv))?;
    fs.list_subvolumes()?;
    fs.quota_rescan()?;
    commit(fs)?;
    fs.delete_subvolume(root, "snap")?;
    fs.quota_disable()?;
    get(fs, f)?;
    get(fs, g)?;
    Ok(())
}

/// A full and an incremental stream, received on the same filesystem.
fn send_receive(fs: &mut EclipseFS, _: &[Arc<dyn BlockDevice>]) -> Result<()> {
    let root = fs.root_ino();
    let sv = fs.create_subvolume(root, "sv")?;
    let a = file(fs, sv, "a")?;
    put(fs, a, 0, &pattern(150_000, 1))?;
    let b = file(fs, sv, "b")?;
    fs.clone_range(a, 0, b, 0, 65536)?;
    fs.set_xattr(a, "user.x", b"y", 0)?;
    fs.symlink(sv, "l", b"a")?;
    let s1 = fs.snapshot(sv, root, "s1", true)?;
    put(fs, a, 4096, &pattern(10_000, 2))?;
    fs.unlink(sv, "b")?;
    fs.create(sv, "dir", FileKind::Dir, 0o755, 0)?;
    fs.truncate(a, 90_000)?;
    let s2 = fs.snapshot(sv, root, "s2", true)?;
    let mut full = Vec::new();
    fs.send(s1, None, &mut |b| {
        full.extend_from_slice(b);
        Ok(())
    })?;
    let mut inc = Vec::new();
    fs.send(s2, Some(s1), &mut |b| {
        inc.extend_from_slice(b);
        Ok(())
    })?;
    let rx = fs.create(root, "rx", FileKind::Dir, 0o755, 0)?;
    for stream in [full, inc] {
        let mut at = 0;
        fs.receive(rx, &mut |buf| {
            let end = at + buf.len();
            if end > stream.len() {
                return Err(Error::Invalid);
            }
            buf.copy_from_slice(&stream[at..end]);
            at = end;
            Ok(())
        })?;
    }
    Ok(())
}

fn random(buf: &mut [u8]) {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i * 37 + 11) as u8;
    }
}

/// Encrypted directories: names, contents, symlinks, with and without key.
fn encrypted(fs: &mut EclipseFS, _: &[Arc<dyn BlockDevice>]) -> Result<()> {
    let root = fs.root_ino();
    fs.set_random(random);
    let key = [0x42u8; 64];
    let id = fs.add_encryption_key(&key)?;
    let d = fs.create(root, "secret", FileKind::Dir, 0o700, 0)?;
    fs.set_encryption_policy(d, &EncryptionPolicy::new(id))?;
    let f = file(fs, d, "a file with a long enough name")?;
    put(fs, f, 0, &pattern(50_000, 1))?;
    fs.symlink(d, "ln", b"a file with a long enough name")?;
    let sub = fs.create(d, "sub", FileKind::Dir, 0o700, 0)?;
    let g = file(fs, sub, "g")?;
    put(fs, g, 0, b"small")?;
    commit(fs)?;
    put(fs, f, 4096, &pattern(9000, 2))?;
    fs.fsync(f)?;
    fs.rename(d, "ln", sub, "ln2")?;
    fs.remove_encryption_key(&id)?;
    fs.readdir(d)?;
    fs.add_encryption_key(&key)?;
    get(fs, f)?;
    let ln = fs.lookup(sub, "ln2")?;
    fs.read_link(ln)?;
    fs.unlink(sub, "g")?;
    Ok(())
}

/// Shrink (moving data and nodes out of the cut) and grow back.
fn resize(fs: &mut EclipseFS, _: &[Arc<dyn BlockDevice>]) -> Result<()> {
    let root = fs.root_ino();
    for i in 0..12 {
        let f = file(fs, root, &format!("f{}", i))?;
        put(fs, f, 0, &pattern(400_000, i))?;
    }
    let s = fs.snapshot(root, root, "snap", true)?;
    let _ = s;
    commit(fs)?;
    for i in 0..12 {
        if i % 2 == 0 {
            fs.unlink(root, &format!("f{}", i))?;
        }
    }
    fs.resize(1, Some(24 << 20))?;
    fs.resize(1, None)?;
    fs.grow_to_device()?;
    Ok(())
}

/// Scrub and repair on a healthy filesystem: every read they make.
fn maintenance(fs: &mut EclipseFS, _: &[Arc<dyn BlockDevice>]) -> Result<()> {
    let root = fs.root_ino();
    for i in 0..4 {
        let f = file(fs, root, &format!("f{}", i))?;
        put(fs, f, 0, &pattern(60_000, i))?;
    }
    commit(fs)?;
    fs.scrub()?;
    fs.repair()?;
    fs.check()?;
    fs.drop_caches();
    for i in 0..4 {
        let f = fs.lookup(root, &format!("f{}", i))?;
        get(fs, f)?;
    }
    Ok(())
}

/// A RAID1 volume that gains, loses and swaps devices and changes profile.
fn volume(fs: &mut EclipseFS, spare: &[Arc<dyn BlockDevice>]) -> Result<()> {
    let root = fs.root_ino();
    for i in 0..6 {
        let f = file(fs, root, &format!("f{}", i))?;
        put(fs, f, 0, &pattern(200_000, i))?;
    }
    commit(fs)?;
    let id = fs.add_device(spare[0].clone())?;
    fs.balance(Some(Profile::Raid1), Some(Profile::Raid1))?;
    fs.scrub()?;
    let new = fs.replace_device(id, spare[1].clone())?;
    fs.remove_device(new)?;
    fs.balance(Some(Profile::Single), Some(Profile::Dup))?;
    Ok(())
}

/// Parity RAID: full and partial rows, reads that reconstruct nothing.
fn parity(fs: &mut EclipseFS, _: &[Arc<dyn BlockDevice>]) -> Result<()> {
    let root = fs.root_ino();
    for i in 0..5 {
        let f = file(fs, root, &format!("f{}", i))?;
        put(fs, f, 0, &pattern(150_000 + i * 4096, i))?;
        fs.fsync(f)?;
    }
    commit(fs)?;
    let f = fs.lookup(root, "f1")?;
    put(fs, f, 1000, &pattern(5000, 9))?;
    fs.scrub()?;
    get(fs, f)?;
    Ok(())
}

/// Tiers: metadata and new data on the fast device, migration to the slow.
fn tiers(fs: &mut EclipseFS, _: &[Arc<dyn BlockDevice>]) -> Result<()> {
    let root = fs.root_ino();
    for i in 0..6 {
        let f = file(fs, root, &format!("f{}", i))?;
        fs.set_compression(f, Some(Compression::Lz4))?;
        put(fs, f, 0, &vec![i as u8; 300_000])?;
    }
    commit(fs)?;
    fs.tier_migrate(true)?;
    fs.set_device_tier(1, Tier::Fast)?;
    Ok(())
}

// ---------------------------------------------------------------------------

const SAMPLES: u64 = 1500;

#[test]
fn a_dying_disk_under_files() {
    torture(Layout::Single, 1, 32, 0, files, SAMPLES);
}

#[test]
fn a_dying_disk_under_compression() {
    torture(Layout::Single, 1, 32, 0, compressed, SAMPLES);
}

#[test]
fn a_dying_disk_under_snapshots() {
    torture(Layout::Single, 1, 32, 0, snapshots, SAMPLES);
}

#[test]
fn a_dying_disk_under_send_receive() {
    torture(Layout::Single, 1, 32, 0, send_receive, SAMPLES);
}

#[test]
fn a_dying_disk_under_encryption() {
    torture(Layout::Single, 1, 32, 0, encrypted, SAMPLES);
}

#[test]
fn a_dying_disk_under_resize() {
    torture(Layout::Single, 1, 64, 0, resize, SAMPLES);
}

#[test]
fn a_dying_disk_under_scrub_and_repair() {
    torture(Layout::Single, 1, 32, 0, maintenance, SAMPLES);
}

#[test]
fn a_dying_volume_under_device_changes() {
    torture(
        Layout::Volume(Profile::Single, Profile::Raid1, &[]),
        2,
        64,
        2,
        volume,
        SAMPLES,
    );
}

#[test]
fn a_dying_raid6() {
    torture(
        Layout::Volume(Profile::Raid6, Profile::Raid1, &[]),
        4,
        48,
        0,
        parity,
        SAMPLES,
    );
}

#[test]
fn a_dying_raid5() {
    torture(
        Layout::Volume(Profile::Raid5, Profile::Raid1, &[]),
        3,
        48,
        0,
        parity,
        SAMPLES,
    );
}

#[test]
fn a_dying_tiered_volume() {
    torture(
        Layout::Volume(Profile::Single, Profile::Single, &[Tier::Fast, Tier::Slow]),
        2,
        64,
        0,
        tiers,
        SAMPLES,
    );
}
