#![cfg(feature = "std")]
//! The fsync log: `fsync` of a file writes one record instead of
//! committing, and a crash after it (a copy of the disk taken while the
//! filesystem is still mounted) comes back with the file as it was at the
//! `fsync`.

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use common::*;
use eclipsefs::cowfmt::{FALLOC_FL_KEEP_SIZE, FALLOC_FL_PUNCH_HOLE, FL_NOCOW};
use eclipsefs::device::BlockDevice;
use eclipsefs::{mkfs, Compression, EclipseFS, Error, FileKind};

fn fresh(size: usize) -> (Arc<MemDisk>, EclipseFS) {
    let disk = MemDisk::new(size);
    mkfs::format(&*disk, &opts()).unwrap();
    let mut fs = EclipseFS::mount(disk.clone(), false).unwrap();
    fs.set_clock(clock);
    (disk, fs)
}

/// A file that already exists in the committed state (new inodes always
/// commit on fsync), with the log area made by a first fsync.
fn committed_file(fs: &mut EclipseFS, name: &str, data: &[u8]) -> u64 {
    let root = fs.root_ino();
    let f = fs.create(root, name, FileKind::Regular, 0o644, 0).unwrap();
    if !data.is_empty() {
        fs.write(f, 0, data).unwrap();
    }
    fs.fsync(f).unwrap();
    f
}

fn read_all(fs: &mut EclipseFS, ino: u64) -> Vec<u8> {
    let size = fs.stat(ino).unwrap().size as usize;
    let mut buf = vec![0u8; size];
    let n = fs.read(ino, 0, &mut buf).unwrap();
    buf.truncate(n);
    buf
}

/// Mount what a power cut right now would leave.
fn crash(disk: &MemDisk) -> EclipseFS {
    let copy = disk.copy();
    let mut fs = EclipseFS::mount(copy, false).unwrap();
    fs.set_clock(clock);
    fs
}

fn pattern(seed: u8, n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

#[test]
fn fsync_of_a_written_file_writes_a_record_not_a_commit() {
    let (disk, mut fs) = fresh(64 * MIB);
    let f = committed_file(&mut fs, "f", &pattern(1, 10_000));
    let gen = fs.generation();
    fs.write(f, 4096, &pattern(2, 9000)).unwrap();
    fs.fsync(f).unwrap();
    assert_eq!(fs.generation(), gen, "fsync committed");
    assert_eq!(fs.fsync_log_records(), 1);
    // Nothing new: nothing written.
    fs.fsync(f).unwrap();
    assert_eq!(fs.fsync_log_records(), 1);
    let want = read_all(&mut fs, f);
    let mut after = crash(&disk);
    assert_eq!(read_all(&mut after, f), want);
    after.check().unwrap();
}

#[test]
fn unsynced_changes_after_the_fsync_are_lost_and_the_rest_survives() {
    let (disk, mut fs) = fresh(64 * MIB);
    let f = committed_file(&mut fs, "f", &pattern(1, 3 * 4096));
    let g = committed_file(&mut fs, "g", &pattern(9, 5000));
    fs.write(f, 100, &pattern(3, 5000)).unwrap();
    fs.fsync(f).unwrap();
    let want = read_all(&mut fs, f);
    let g_before = read_all(&mut fs, g);
    // Not fsynced: neither survives.
    fs.write(f, 0, &pattern(4, 20_000)).unwrap();
    fs.write(g, 0, &pattern(5, 100)).unwrap();
    let mut after = crash(&disk);
    assert_eq!(read_all(&mut after, f), want);
    assert_eq!(read_all(&mut after, g), g_before);
    after.check().unwrap();
}

#[test]
fn several_records_merge_in_order() {
    let (disk, mut fs) = fresh(64 * MIB);
    let f = committed_file(&mut fs, "f", &pattern(1, 64 * 1024));
    let mut states = Vec::new();
    for k in 0..12u8 {
        let off = (k as u64 * 7919) % 60_000;
        fs.write(f, off, &pattern(k + 10, 3000 + k as usize * 500))
            .unwrap();
        if k % 3 == 2 {
            fs.truncate(f, 30_000 + k as u64 * 1000).unwrap();
        }
        fs.fsync(f).unwrap();
        states.push(read_all(&mut fs, f));
    }
    assert!(fs.fsync_log_records() >= 12);
    let mut after = crash(&disk);
    assert_eq!(read_all(&mut after, f), *states.last().unwrap());
    after.check().unwrap();
}

#[test]
fn truncate_fallocate_and_punch_go_through_the_log() {
    let (disk, mut fs) = fresh(64 * MIB);
    let f = committed_file(&mut fs, "f", &pattern(1, 100_000));
    fs.truncate(f, 50_001).unwrap();
    fs.fallocate(f, FALLOC_FL_KEEP_SIZE, 50_001, 40_000)
        .unwrap();
    fs.fallocate(f, 0, 90_000, 10_000).unwrap();
    fs.fallocate(f, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 8192, 12_288)
        .unwrap();
    fs.write(f, 95_000, b"tail").unwrap();
    let records = fs.fsync_log_records();
    fs.fsync(f).unwrap();
    assert_eq!(fs.fsync_log_records(), records + 1);
    let want = read_all(&mut fs, f);
    let st = fs.stat(f).unwrap();
    let mut after = crash(&disk);
    assert_eq!(read_all(&mut after, f), want);
    let st2 = after.stat(f).unwrap();
    assert_eq!((st2.size, st2.nbytes), (st.size, st.nbytes));
    after.check().unwrap();
}

#[test]
fn attributes_alone_are_logged() {
    let (disk, mut fs) = fresh(32 * MIB);
    let f = committed_file(&mut fs, "f", b"0123456789");
    fs.write(f, 0, &pattern(1, 8192)).unwrap();
    fs.fsync(f).unwrap();
    fs.set_attr(f, Some(0o600), Some(42), Some(43), None, Some((7, 8)))
        .unwrap();
    let before = fs.fsync_log_records();
    fs.fsync(f).unwrap();
    assert_eq!(fs.fsync_log_records(), before + 1);
    let mut after = crash(&disk);
    let st = after.stat(f).unwrap();
    assert_eq!((st.mode & 0o7777, st.uid, st.gid), (0o600, 42, 43));
    assert_eq!(st.mtime, (7, 8));
    after.check().unwrap();
}

#[test]
fn new_files_names_and_directories_commit_instead() {
    let (disk, mut fs) = fresh(32 * MIB);
    let root = fs.root_ino();
    committed_file(&mut fs, "first", b"x");
    let gen = fs.generation();
    // A new inode is not in the committed state: commit.
    let f = fs.create(root, "new", FileKind::Regular, 0o644, 0).unwrap();
    fs.write(f, 0, &pattern(1, 9000)).unwrap();
    fs.fsync(f).unwrap();
    assert!(fs.generation() > gen);
    // A rename changes names: commit.
    let gen = fs.generation();
    fs.rename(root, "new", root, "renamed").unwrap();
    fs.write(f, 0, b"after").unwrap();
    fs.fsync(f).unwrap();
    assert!(fs.generation() > gen);
    // A directory: commit.
    let d = fs.create(root, "d", FileKind::Dir, 0o755, 0).unwrap();
    fs.fsync(d).unwrap();
    let gen = fs.generation();
    fs.create(d, "inside", FileKind::Regular, 0o644, 0).unwrap();
    fs.fsync(d).unwrap();
    assert!(fs.generation() > gen);
    let mut after = crash(&disk);
    assert!(after.lookup(root, "renamed").is_ok());
    assert!(after.lookup(d, "inside").is_ok());
    after.check().unwrap();
}

#[test]
fn small_inline_files_and_xattrs_commit_instead() {
    let (disk, mut fs) = fresh(32 * MIB);
    let f = committed_file(&mut fs, "small", b"tiny");
    let gen = fs.generation();
    fs.write(f, 0, b"still inline").unwrap();
    fs.fsync(f).unwrap();
    assert!(fs.generation() > gen);
    let g = committed_file(&mut fs, "big", &pattern(3, 20_000));
    let gen = fs.generation();
    fs.set_xattr(g, "user.k", b"v", 0).unwrap();
    fs.fsync(g).unwrap();
    assert!(fs.generation() > gen);
    let mut after = crash(&disk);
    assert_eq!(read_all(&mut after, f), b"still inline");
    assert_eq!(after.get_xattr(g, "user.k").unwrap(), b"v");
    after.check().unwrap();
}

#[test]
fn reflinked_and_compressed_files_commit_instead() {
    let (disk, mut fs) = fresh(64 * MIB);
    let a = committed_file(&mut fs, "a", &pattern(1, 64 * 1024));
    let b = committed_file(&mut fs, "b", &pattern(2, 64 * 1024));
    let gen = fs.generation();
    fs.clone_range(a, 0, b, 0, 32 * 1024).unwrap();
    fs.fsync(b).unwrap();
    assert!(fs.generation() > gen);
    let c = committed_file(&mut fs, "c", &pattern(3, 4096 * 4));
    fs.set_compression(c, Some(Compression::Zstd)).unwrap();
    fs.sync().unwrap();
    let gen = fs.generation();
    fs.write(c, 0, &vec![7u8; 256 * 1024]).unwrap();
    fs.fsync(c).unwrap();
    assert!(fs.generation() > gen);
    let want_b = read_all(&mut fs, b);
    let want_c = read_all(&mut fs, c);
    let mut after = crash(&disk);
    assert_eq!(read_all(&mut after, b), want_b);
    assert_eq!(read_all(&mut after, c), want_c);
    after.check().unwrap();
}

#[test]
fn nocow_files_log_without_checksums() {
    let (disk, mut fs) = fresh(32 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "db", FileKind::Regular, 0o644, 0).unwrap();
    fs.set_flags(f, FL_NOCOW).unwrap();
    fs.write(f, 0, &pattern(1, 40_000)).unwrap();
    fs.fsync(f).unwrap();
    let before = fs.fsync_log_records();
    // In place and appended.
    fs.write(f, 1000, &pattern(2, 3000)).unwrap();
    fs.write(f, 40_000, &pattern(3, 10_000)).unwrap();
    fs.fsync(f).unwrap();
    assert_eq!(fs.fsync_log_records(), before + 1);
    let want = read_all(&mut fs, f);
    let mut after = crash(&disk);
    assert_eq!(read_all(&mut after, f), want);
    after.check().unwrap();
}

#[test]
fn a_record_whose_data_did_not_reach_the_disk_is_dropped() {
    let (disk, mut fs) = fresh(32 * MIB);
    let f = committed_file(&mut fs, "f", &pattern(1, 20_000));
    fs.write(f, 0, &pattern(2, 20_000)).unwrap();
    fs.fsync(f).unwrap();
    let first = read_all(&mut fs, f);
    fs.write(f, 8192, &pattern(3, 4096)).unwrap();
    let blocks = fs.fiemap(f, 8192, 4096).unwrap();
    fs.fsync(f).unwrap();
    // The block the second record points at is torn.
    let copy = disk.copy();
    copy.poke(blocks[0].physical as usize + 17, 0x40);
    let mut after = EclipseFS::mount(copy, false).unwrap();
    assert_eq!(read_all(&mut after, f), first);
    after.check().unwrap();
}

#[test]
fn a_torn_record_ends_the_log() {
    let (disk, mut fs) = fresh(32 * MIB);
    let f = committed_file(&mut fs, "f", &pattern(1, 20_000));
    let committed = read_all(&mut fs, f);
    fs.write(f, 0, &pattern(2, 9000)).unwrap();
    fs.fsync(f).unwrap();
    let first = read_all(&mut fs, f);
    fs.write(f, 12_000, &pattern(3, 100)).unwrap();
    fs.fsync(f).unwrap();
    // Find the records: headers start with "EFSL".
    let copy = disk.copy();
    let mut headers = Vec::new();
    let mut blk = vec![0u8; BLOCK];
    for b in 0..copy.size() / BLOCK as u64 {
        copy.read_at(b * BLOCK as u64, &mut blk).unwrap();
        if &blk[..4] == b"EFSL" {
            headers.push(b);
        }
    }
    assert_eq!(headers.len(), 2);
    let second = disk.copy();
    second.poke(headers[1] as usize * BLOCK + 50, 1);
    let mut after = EclipseFS::mount(second, false).unwrap();
    assert_eq!(read_all(&mut after, f), first);
    after.check().unwrap();
    let both = disk.copy();
    both.poke(headers[0] as usize * BLOCK + 30, 1);
    let mut after = EclipseFS::mount(both, false).unwrap();
    assert_eq!(read_all(&mut after, f), committed);
    after.check().unwrap();
}

#[test]
fn read_only_mount_sees_the_log_without_writing() {
    let (disk, mut fs) = fresh(32 * MIB);
    let f = committed_file(&mut fs, "f", &pattern(1, 20_000));
    fs.write(f, 5000, &pattern(2, 30_000)).unwrap();
    fs.fsync(f).unwrap();
    let want = read_all(&mut fs, f);
    let copy = disk.copy();
    let mut before = vec![0u8; copy.size() as usize];
    copy.read_at(0, &mut before).unwrap();
    {
        let mut ro = EclipseFS::mount(copy.clone(), true).unwrap();
        assert_eq!(read_all(&mut ro, f), want);
    }
    let mut now = vec![0u8; copy.size() as usize];
    copy.read_at(0, &mut now).unwrap();
    assert!(before == now, "read-only mount wrote");
    // And a read-write mount after it replays for good.
    let mut rw = EclipseFS::mount(copy.clone(), false).unwrap();
    assert_eq!(read_all(&mut rw, f), want);
    rw.check().unwrap();
    drop(rw);
    let mut again = EclipseFS::mount(copy, false).unwrap();
    assert_eq!(read_all(&mut again, f), want);
}

#[test]
fn a_full_log_area_falls_back_to_a_commit() {
    let (disk, mut fs) = fresh(16 * MIB);
    let f = committed_file(&mut fs, "f", &pattern(1, 8192));
    let mut want = Vec::new();
    let mut commits = 0;
    for k in 0..300u32 {
        let gen = fs.generation();
        fs.write(f, (k as u64 % 50) * 4096, &pattern(k as u8, 20_000))
            .unwrap();
        fs.fsync(f).unwrap();
        if fs.generation() != gen {
            commits += 1;
        }
        want = read_all(&mut fs, f);
    }
    assert!(commits > 0, "the area never filled");
    assert!(fs.fsync_log_records() > 100);
    let mut after = crash(&disk);
    assert_eq!(read_all(&mut after, f), want);
    after.check().unwrap();
}

#[test]
fn a_commit_makes_the_log_obsolete() {
    let (disk, mut fs) = fresh(32 * MIB);
    let f = committed_file(&mut fs, "f", &pattern(1, 20_000));
    fs.write(f, 0, &pattern(2, 9000)).unwrap();
    fs.fsync(f).unwrap();
    fs.truncate(f, 100).unwrap();
    fs.sync().unwrap();
    // The old record is still in the area, but against an older state.
    let mut after = crash(&disk);
    assert_eq!(read_all(&mut after, f), read_all(&mut fs, f));
    after.check().unwrap();
}

#[test]
fn fsynced_file_that_is_then_deleted_comes_back() {
    let (disk, mut fs) = fresh(32 * MIB);
    let root = fs.root_ino();
    let f = committed_file(&mut fs, "f", &pattern(1, 20_000));
    fs.write(f, 0, &pattern(2, 30_000)).unwrap();
    fs.fsync(f).unwrap();
    let g = committed_file(&mut fs, "g", &pattern(9, 4096));
    fs.write(f, 0, &pattern(3, 30_000)).unwrap();
    fs.fsync(f).unwrap();
    let want = read_all(&mut fs, f);
    fs.unlink(root, "f").unwrap();
    // Its blocks are not reused before the next commit.
    fs.write(g, 0, &pattern(9, 200_000)).unwrap();
    let mut after = crash(&disk);
    let f2 = after.lookup(root, "f").unwrap();
    assert_eq!(read_all(&mut after, f2), want);
    after.check().unwrap();
}

#[test]
fn files_in_snapshots_and_subvolumes_are_logged_too() {
    let (disk, mut fs) = fresh(64 * MIB);
    let root = fs.root_ino();
    committed_file(&mut fs, "area", b"makes the log area");
    let sv = fs.create_subvolume(root, "sv").unwrap();
    let f = fs.create(sv, "f", FileKind::Regular, 0o644, 0).unwrap();
    fs.write(f, 0, &pattern(1, 50_000)).unwrap();
    fs.sync().unwrap();
    fs.fsync(f).unwrap();
    fs.snapshot(sv, root, "snap", false).unwrap();
    fs.sync().unwrap();
    fs.write(f, 4096, &pattern(2, 8192)).unwrap();
    let records = fs.fsync_log_records();
    fs.fsync(f).unwrap();
    assert_eq!(fs.fsync_log_records(), records + 1);
    let want = read_all(&mut fs, f);
    let mut after = crash(&disk);
    assert_eq!(read_all(&mut after, f), want);
    let snap = after.lookup(root, "snap").unwrap();
    let sf = after.lookup(snap, "f").unwrap();
    assert_eq!(read_all(&mut after, sf), pattern(1, 50_000));
    after.check().unwrap();
}

#[test]
fn resizing_drops_the_area_and_the_next_fsync_makes_one() {
    let (disk, mut fs) = fresh(64 * MIB);
    let f = committed_file(&mut fs, "f", &pattern(1, 20_000));
    fs.write(f, 0, &pattern(2, 9000)).unwrap();
    fs.fsync(f).unwrap();
    fs.resize(1, Some(32 * MIB as u64)).unwrap();
    fs.check().unwrap();
    let gen = fs.generation();
    fs.write(f, 0, &pattern(3, 9000)).unwrap();
    fs.fsync(f).unwrap();
    assert!(fs.generation() > gen, "a new area is committed first");
    fs.write(f, 0, &pattern(4, 9000)).unwrap();
    let records = fs.fsync_log_records();
    fs.fsync(f).unwrap();
    assert_eq!(fs.fsync_log_records(), records + 1);
    let want = read_all(&mut fs, f);
    let mut after = crash(&disk);
    assert_eq!(read_all(&mut after, f), want);
    after.check().unwrap();
}

#[test]
fn a_large_file_fsyncs_through_the_log() {
    let (disk, mut fs) = fresh(256 * MIB);
    let f = committed_file(&mut fs, "big", &pattern(1, 4096));
    let chunk = pattern(5, MIB);
    for i in 0..24u64 {
        fs.write(f, i * MIB as u64, &chunk).unwrap();
    }
    fs.fsync(f).unwrap();
    let want = read_all(&mut fs, f);
    let mut after = crash(&disk);
    assert_eq!(read_all(&mut after, f), want);
    after.check().unwrap();
}

// ---------------------------------------------------------------------------
// Power cuts anywhere
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Event {
    Write(u64, Vec<u8>),
    Sync,
}

/// Device that applies writes and remembers them.
struct Recorder {
    data: Mutex<Vec<u8>>,
    log: Mutex<Vec<Event>>,
}

impl BlockDevice for Recorder {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> eclipsefs::Result<()> {
        let d = self.data.lock().unwrap();
        let o = offset as usize;
        buf.copy_from_slice(d.get(o..o + buf.len()).ok_or(Error::Io)?);
        Ok(())
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> eclipsefs::Result<()> {
        let mut d = self.data.lock().unwrap();
        let o = offset as usize;
        d.get_mut(o..o + buf.len())
            .ok_or(Error::Io)?
            .copy_from_slice(buf);
        self.log
            .lock()
            .unwrap()
            .push(Event::Write(offset, buf.to_vec()));
        Ok(())
    }
    fn sync(&self) -> eclipsefs::Result<()> {
        self.log.lock().unwrap().push(Event::Sync);
        Ok(())
    }
    fn size(&self) -> u64 {
        self.data.lock().unwrap().len() as u64
    }
}

struct Run {
    base: Vec<u8>,
    log: Vec<Event>,
    /// Per file: its contents after each operation, with the log length
    /// at which that operation began.
    versions: BTreeMap<u64, Vec<(usize, Vec<u8>)>>,
    /// Log length once the files exist and are committed.
    start: usize,
    /// Per file: (log length when its fsync returned, version index).
    durable: BTreeMap<u64, Vec<(usize, usize)>>,
}

fn lcg(x: &mut u64) -> u64 {
    *x = x
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *x >> 33
}

/// Random writes, truncates and fsyncs over a few files.
fn workload(seed: u64, ops: usize, timed: bool) -> Run {
    let size = 16 * MIB;
    let disk = MemDisk::new(size);
    mkfs::format(&*disk, &opts()).unwrap();
    let mut base = vec![0u8; size];
    disk.read_at(0, &mut base).unwrap();
    let rec = Arc::new(Recorder {
        data: Mutex::new(base.clone()),
        log: Mutex::new(Vec::new()),
    });
    let mut fs = EclipseFS::mount(rec.clone(), false).unwrap();
    fs.set_clock(clock);
    if timed {
        // The clock stands still: only fsync, sync and the size limits
        // commit.
        fs.set_commit_interval(Some(5));
    }
    let root = fs.root_ino();
    let mut files = Vec::new();
    for k in 0..3 {
        let f = fs
            .create(root, &format!("f{}", k), FileKind::Regular, 0o644, 0)
            .unwrap();
        fs.write(f, 0, &pattern(k as u8, 9000)).unwrap();
        files.push(f);
    }
    fs.sync().unwrap();
    fs.fsync(files[0]).unwrap();
    let events = |r: &Recorder| r.log.lock().unwrap().len();
    let mut versions: BTreeMap<u64, Vec<(usize, Vec<u8>)>> = BTreeMap::new();
    let mut durable: BTreeMap<u64, Vec<(usize, usize)>> = BTreeMap::new();
    for &f in &files {
        let v = read_all(&mut fs, f);
        versions.entry(f).or_default().push((events(&rec), v));
        durable.entry(f).or_default().push((events(&rec), 0));
    }
    let start = events(&rec);
    let mut x = seed;
    for _ in 0..ops {
        // An operation may be durable from any point of its own events on
        // (a commit at its end is part of it).
        let began = events(&rec);
        let f = files[(lcg(&mut x) % 3) as usize];
        match lcg(&mut x) % 10 {
            0..=4 => {
                let off = lcg(&mut x) % 40_000;
                let len = 1 + lcg(&mut x) % 12_000;
                let d = pattern(lcg(&mut x) as u8, len as usize);
                fs.write(f, off, &d).unwrap();
            }
            5 => {
                fs.truncate(f, lcg(&mut x) % 50_000).unwrap();
            }
            6 => {
                let off = lcg(&mut x) % 40_000;
                let len = 1 + lcg(&mut x) % 20_000;
                let mode = match lcg(&mut x) % 3 {
                    0 => 0,
                    1 => FALLOC_FL_KEEP_SIZE,
                    _ => FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE,
                };
                fs.fallocate(f, mode, off, len).unwrap();
            }
            _ => {
                fs.fsync(f).unwrap();
                let i = versions[&f].len() - 1;
                durable.get_mut(&f).unwrap().push((events(&rec), i));
                continue;
            }
        }
        let v = read_all(&mut fs, f);
        versions.get_mut(&f).unwrap().push((began, v));
    }
    let log = rec.log.lock().unwrap().clone();
    std::mem::forget(fs);
    Run {
        base,
        log,
        start,
        versions,
        durable,
    }
}

fn verify(run: &Run, image: Vec<u8>, cut: usize) {
    let disk = MemDisk::new(0);
    disk.resize(image.len());
    disk.write_at(0, &image).unwrap();
    let mut fs = EclipseFS::mount(disk, false)
        .unwrap_or_else(|e| panic!("mount after {} events: {:?}", cut, e));
    fs.check()
        .unwrap_or_else(|e| panic!("check after {} events: {:?}", cut, e));
    let root = fs.root_ino();
    for (&f, versions) in &run.versions {
        let name = format!("f{}", run.versions.keys().position(|&k| k == f).unwrap());
        let ino = fs.lookup(root, &name).unwrap();
        let got = read_all(&mut fs, ino);
        // Not older than its last fsync that returned, not newer than
        // what was done by the cut.
        let floor = run.durable[&f]
            .iter()
            .filter(|(at, _)| *at <= cut)
            .map(|&(_, i)| i)
            .max()
            .unwrap_or(0);
        let ok = versions
            .iter()
            .enumerate()
            .skip(floor)
            .take_while(|(i, (at, _))| *i == floor || *at <= cut)
            .any(|(_, (_, v))| *v == got);
        if !ok {
            let desc: Vec<(usize, usize, bool)> = versions
                .iter()
                .map(|(at, v)| (*at, v.len(), *v == got))
                .collect();
            panic!(
                "file {} after {} events: not a valid version (got {} bytes, floor {}, versions {:?}, durable {:?}, start {})",
                name,
                cut,
                got.len(),
                floor,
                &desc[..desc.len().min(12)],
                run.durable[&f],
                run.start
            );
        }
    }
}

fn apply(image: &mut [u8], ev: &Event) {
    if let Event::Write(off, data) = ev {
        image[*off as usize..*off as usize + data.len()].copy_from_slice(data);
    }
}

fn crash_everywhere(seed: u64, ops: usize, timed: bool) {
    let run = workload(seed, ops, timed);
    let mut image = run.base.clone();
    let step = (run.log.len() / 400).max(1);
    for (i, ev) in run.log.iter().enumerate() {
        apply(&mut image, ev);
        if i + 1 >= run.start && (i % step == 0 || i + 1 == run.log.len()) {
            verify(&run, image.clone(), i + 1);
        }
    }
}

#[test]
fn crash_at_every_point_of_an_fsync_workload() {
    crash_everywhere(1, 150, false);
}

#[test]
fn crash_at_every_point_of_another_fsync_workload() {
    crash_everywhere(77, 150, false);
}

#[test]
fn crash_at_every_point_of_a_long_fsync_workload() {
    crash_everywhere(2024, 600, false);
}

#[test]
fn crash_loses_an_arbitrary_part_of_the_writes_since_the_last_flush() {
    let run = workload(5, 120, false);
    let mut x = 99u64;
    let mut image = run.base.clone();
    let mut pending: Vec<&Event> = Vec::new();
    for (i, ev) in run.log.iter().enumerate() {
        match ev {
            Event::Sync => {
                for p in pending.drain(..) {
                    apply(&mut image, p);
                }
            }
            w => pending.push(w),
        }
        if i + 1 >= run.start && i % 7 == 0 {
            // Some of the writes since the last flush made it, in any
            // order.
            let mut img = image.clone();
            for p in &pending {
                if lcg(&mut x) % 2 == 0 {
                    apply(&mut img, p);
                }
            }
            let last_sync = run.log[..=i]
                .iter()
                .rposition(|e| matches!(e, Event::Sync))
                .map_or(0, |p| p + 1);
            verify_with_floor(&run, img, last_sync, i + 1);
        }
    }
}

/// [`verify`] when only the events up to `synced` surely reached the disk
/// and those up to `cut` may have.
fn verify_with_floor(run: &Run, image: Vec<u8>, synced: usize, cut: usize) {
    let disk = MemDisk::new(0);
    disk.resize(image.len());
    disk.write_at(0, &image).unwrap();
    let mut fs = EclipseFS::mount(disk, false)
        .unwrap_or_else(|e| panic!("mount after {} events: {:?}", cut, e));
    fs.check()
        .unwrap_or_else(|e| panic!("check after {} events: {:?}", cut, e));
    let root = fs.root_ino();
    for (n, (&f, versions)) in run.versions.iter().enumerate() {
        let ino = fs.lookup(root, &format!("f{}", n)).unwrap();
        let got = read_all(&mut fs, ino);
        let floor = run.durable[&f]
            .iter()
            .filter(|(at, _)| *at <= synced)
            .map(|&(_, i)| i)
            .max()
            .unwrap_or(0);
        let ok = versions
            .iter()
            .enumerate()
            .skip(floor)
            .take_while(|(i, (at, _))| *i == floor || *at <= cut)
            .any(|(_, (_, v))| *v == got);
        assert!(ok, "file f{} after {} events: not a valid version", n, cut);
    }
}

#[test]
fn crash_at_every_point_of_a_workload_with_timed_commits() {
    crash_everywhere(3, 300, true);
}

#[test]
fn crash_at_every_point_of_another_workload_with_timed_commits() {
    crash_everywhere(41, 300, true);
}
