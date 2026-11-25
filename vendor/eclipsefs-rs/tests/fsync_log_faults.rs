#![cfg(feature = "std")]
//! The fsync log when the disk fails while it is being used: during the
//! `fsync` that writes a record, and during the mount that replays the
//! records after a power cut.
//!
//! `tests/fsync_log.rs` cuts the power at every point of fsync workloads on
//! a disk that never fails, and `tests/faults.rs` kills the disk at sampled
//! points of whole workloads. Neither reached the error paths of the log
//! itself (a coverage run on 2026-11-25 left almost every `?` of
//! `cow/fsync_log.rs` unexecuted): the record writer running out of a
//! device mid-record, the reader failing on a header, a payload block or a
//! data block it checks, and the replay failing half way through its
//! remapping or its commit. The replaying mount is the boot after a crash,
//! on a root filesystem that is EclipseFS on real hardware, so it is the
//! one place where an error must cost nothing that was promised.
//!
//! Here every device operation of those two calls is made to fail in turn,
//! once (a transient error) and for good (a disk that dies), and each time:
//! - the call returns an error or succeeds, and never panics;
//! - what a later mount on a healthy disk finds is the state the last
//!   successful `fsync` promised, or for a failed `fsync` either that state
//!   or the one before it, never a mix, and `check` passes;
//! - a replay that failed is done again, in full, by the next mount.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use common::*;
use eclipsefs::cowfmt::{FALLOC_FL_KEEP_SIZE, FALLOC_FL_PUNCH_HOLE};
use eclipsefs::device::BlockDevice;
use eclipsefs::{mkfs, EclipseFS, Error, FileKind, Result};

/// Counts device operations; operation `fail_at` fails, and with `dies`
/// every one after it too.
struct Clock {
    ops: AtomicU64,
    fail_at: AtomicU64,
    dies: bool,
}

impl Clock {
    fn new(fail_at: u64, dies: bool) -> Arc<Self> {
        Arc::new(Self {
            ops: AtomicU64::new(0),
            fail_at: AtomicU64::new(fail_at),
            dies,
        })
    }
    /// Fail the `k`-th operation from now (0 = the next one).
    fn arm(&self, k: u64) {
        self.fail_at
            .store(self.ops().saturating_add(k), Ordering::Relaxed);
    }
    fn tick(&self) -> Result<()> {
        let n = self.ops.fetch_add(1, Ordering::Relaxed);
        let at = self.fail_at.load(Ordering::Relaxed);
        if n == at || (self.dies && n > at) {
            return Err(Error::Io);
        }
        Ok(())
    }
    fn ops(&self) -> u64 {
        self.ops.load(Ordering::Relaxed)
    }
    fn failed(&self) -> bool {
        self.ops() > self.fail_at.load(Ordering::Relaxed)
    }
}

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

fn faulty(disk: &Arc<MemDisk>, clock: &Arc<Clock>) -> Arc<dyn BlockDevice> {
    Arc::new(Faulty {
        disk: disk.clone(),
        clock: clock.clone(),
    })
}

fn bytes(seed: u8, n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(37).wrapping_add(seed))
        .collect()
}

/// What the files hold, by name, in name order.
type State = Vec<(String, Vec<u8>)>;

fn state(fs: &mut EclipseFS) -> State {
    let root = fs.root_ino();
    let mut ents = fs.readdir(root).unwrap();
    ents.sort_by(|a, b| a.name.cmp(&b.name));
    ents.into_iter()
        .filter(|e| e.kind == FileKind::Regular)
        .map(|e| (e.name.clone(), read_all(fs, e.ino)))
        .collect()
}

fn try_state(fs: &mut EclipseFS) -> Result<State> {
    let root = fs.root_ino();
    let mut ents = fs.readdir(root)?;
    ents.sort_by(|a, b| a.name.cmp(&b.name));
    let mut out = Vec::new();
    for e in ents.into_iter().filter(|e| e.kind == FileKind::Regular) {
        let size = fs.stat(e.ino)?.size as usize;
        let mut buf = vec![0u8; size];
        let mut done = 0;
        while done < size {
            let n = fs.read(e.ino, done as u64, &mut buf[done..])?;
            if n == 0 {
                break;
            }
            done += n;
        }
        buf.truncate(done);
        out.push((e.name, buf));
    }
    Ok(out)
}

/// Mount `disk` healthy, check it and say what it holds.
fn healthy(disk: &Arc<MemDisk>) -> State {
    let mut fs = EclipseFS::mount(disk.copy(), false)
        .unwrap_or_else(|e| panic!("the image does not mount: {e:?}"));
    fs.set_clock(clock);
    fs.check()
        .unwrap_or_else(|e| panic!("the image fails check: {e:?}"));
    state(&mut fs)
}

/// A crash image with three files whose last changes live only in fsync
/// records (an overwrite, an append, a truncate, a punched hole, several
/// records for one file), and the state those records promise.
fn image_with_records() -> (Vec<u8>, State) {
    let (disk, mut fs) = fresh(64 * MIB);
    let root = fs.root_ino();
    let mut files = Vec::new();
    for (i, name) in ["a", "b", "c"].iter().enumerate() {
        let f = fs.create(root, name, FileKind::Regular, 0o644, 0).unwrap();
        write_all(&mut fs, f, 0, &bytes(i as u8, 40_000 + 7_000 * i));
        files.push(f);
    }
    fs.sync().unwrap();
    // The first fsync of a mount makes the log area, which commits.
    write_all(&mut fs, files[0], 0, &bytes(9, 100));
    fs.fsync(files[0]).unwrap();
    fs.sync().unwrap();
    // Each file gets its own fsync records from here on; none commits.
    let gen = fs.generation();
    write_all(&mut fs, files[0], 4096, &bytes(10, 12_000));
    fs.fsync(files[0]).unwrap();
    write_all(&mut fs, files[1], 47_000, &bytes(11, 30_000));
    fs.fsync(files[1]).unwrap();
    fs.truncate(files[2], 20_000).unwrap();
    fs.fsync(files[2]).unwrap();
    fs.fallocate(
        files[0],
        FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE,
        8192,
        8192,
    )
    .unwrap();
    write_all(&mut fs, files[0], 30_000, &bytes(12, 5_000));
    fs.fsync(files[0]).unwrap();
    assert_eq!(fs.generation(), gen, "an fsync of the setup committed");
    assert!(
        fs.fsync_log_records() >= 4,
        "the setup wrote too few records"
    );
    let want = state(&mut fs);
    let image = disk.bytes();
    std::mem::forget(fs); // the power goes: no unmount commit
    (image, want)
}

/// Mount the crash image with operation `k` of the mount failing, then
/// prove the image as it was left still holds what the records promised.
/// Returns how many operations the mount made.
fn replay_failing_at(image: &[u8], want: &State, k: u64, dies: bool) -> u64 {
    let disk = MemDisk::from_bytes(image.to_vec());
    let clock = Clock::new(k, dies);
    let ok = match EclipseFS::mount(faulty(&disk, &clock), false) {
        Ok(mut fs) => {
            // Whatever the mount let through, a failure it swallowed must
            // not have changed what the files hold, as far as a read that
            // the device answers can tell.
            if !clock.failed() {
                fs.set_clock(clock_fn);
                // The reads may hit the failure themselves; what they do
                // read must be what the records promised.
                if let Ok(got) = try_state(&mut fs) {
                    if !clock.failed() {
                        assert_eq!(&got, want, "op {k}: the replayed state differs");
                    }
                }
            }
            drop(fs); // the unmount commit, which may fail too
            true
        }
        Err(_) => false,
    };
    assert!(
        ok || clock.failed(),
        "op {k}: the mount failed without a device error"
    );
    let ops = clock.ops();
    assert_eq!(
        &healthy(&disk),
        want,
        "op {k} ({}): the next mount lost what the records promised",
        if dies { "dies" } else { "once" }
    );
    ops
}

#[test]
fn a_disk_error_anywhere_in_the_replaying_mount_loses_nothing() {
    let (image, want) = image_with_records();
    // The mount without faults: how many operations there are to fail.
    let total = replay_failing_at(&image, &want, u64::MAX, false);
    assert!(
        total > 10,
        "the replaying mount made only {total} operations"
    );
    for k in 0..total {
        replay_failing_at(&image, &want, k, false);
        replay_failing_at(&image, &want, k, true);
    }
}

/// How the change is made durable, and what follows the failure.
#[derive(Clone, Copy)]
struct Case {
    /// `sync` (a whole commit) instead of `fsync`.
    commit: bool,
    /// Unmount after it (the unmount commits whatever is left, on a device
    /// that may work again) instead of cutting the power.
    unmount: bool,
}

/// A committed file with the log area already made, then a change and an
/// `fsync` (or `sync`) with operation `k` of it failing. The image left
/// must come back as before the change, or, if the call said it
/// succeeded, as after it.
fn fsync_failing_at(k: u64, dies: bool, case: Case, change: fn(&mut EclipseFS, u64)) -> u64 {
    let disk = MemDisk::new(64 * MIB);
    mkfs::format(&*disk, &opts()).unwrap();
    // The same disk, but behind a clock that only starts failing at the
    // fsync: everything before it goes through.
    let clock = Clock::new(u64::MAX, dies);
    let mut fs = EclipseFS::mount(faulty(&disk, &clock), false).unwrap();
    fs.set_clock(clock_fn);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, f, 0, &bytes(1, 50_000));
    fs.fsync(f).unwrap(); // a new inode: commits, and makes the log area
    let other = fs
        .create(root, "other", FileKind::Regular, 0o644, 0)
        .unwrap();
    write_all(&mut fs, other, 0, &bytes(2, 9_000));
    fs.sync().unwrap();
    write_all(&mut fs, other, 0, &bytes(3, 9_000));
    fs.fsync(other).unwrap(); // a record already in the log

    change(&mut fs, f);
    let after = state(&mut fs);
    // What a power cut right before the fsync leaves: the change may have
    // committed in part on its own (the dirty limit), so this, and not the
    // state before the change, is what a failed fsync may fall back to.
    let before = healthy(&disk);

    let start = clock.ops();
    clock.arm(k);
    let r = if case.commit { fs.sync() } else { fs.fsync(f) };
    let ops = clock.ops() - start;
    let failed = clock.failed();
    if case.unmount {
        drop(fs);
    } else {
        std::mem::forget(fs); // the power goes right after the call
    }
    let image = disk.copy();

    let got = healthy(&image);
    match r {
        Ok(()) => assert_eq!(got, after, "op {k}: a successful fsync lost data"),
        Err(_) => {
            assert!(failed, "op {k}: fsync failed without a device error");
            assert!(
                got == before || got == after,
                "op {k}: a failed fsync left neither the old nor the new state"
            );
        }
    }
    ops
}

fn torture_fsync(change: fn(&mut EclipseFS, u64)) {
    for commit in [false, true] {
        for unmount in [false, true] {
            let case = Case { commit, unmount };
            let total = fsync_failing_at(u64::MAX, false, case, change);
            assert!(total > 2, "the call made only {total} operations");
            for k in 0..total {
                fsync_failing_at(k, false, case, change);
                fsync_failing_at(k, true, case, change);
            }
        }
    }
}

#[test]
fn a_disk_error_anywhere_in_an_overwrite_fsync_keeps_one_whole_state() {
    torture_fsync(|fs, f| write_all(fs, f, 8192, &bytes(20, 20_000)));
}

#[test]
fn a_disk_error_anywhere_in_an_append_fsync_keeps_one_whole_state() {
    torture_fsync(|fs, f| write_all(fs, f, 50_000, &bytes(21, 70_000)));
}

#[test]
fn a_disk_error_anywhere_in_a_truncate_and_punch_fsync_keeps_one_whole_state() {
    torture_fsync(|fs, f| {
        fs.truncate(f, 30_000).unwrap();
        fs.fallocate(f, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 4096, 8192)
            .unwrap();
    });
}

/// A file with more extents than one record block holds: the writer
/// spills into further payload blocks, and the reader reads them back.
#[test]
fn a_disk_error_anywhere_in_a_many_extent_fsync_keeps_one_whole_state() {
    torture_fsync(|fs, f| {
        for i in 0..200u64 {
            write_all(fs, f, i * 8192, &bytes(i as u8, 100));
        }
    });
}

fn clock_fn() -> (u64, u32) {
    clock()
}
