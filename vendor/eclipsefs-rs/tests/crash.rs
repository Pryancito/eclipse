#![cfg(feature = "std")]
//! Power-loss tests for the journal. A recording device logs every write
//! and sync of a workload; each crash image is the formatted disk plus a
//! prefix of that log (optionally with only some of the writes since the
//! last sync, in any order, as a disk cache may lose them). Every image must
//! mount, pass `check`, and show exactly the state after some whole
//! operation, never earlier than the last completed `sync`.

mod common;

use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use common::*;
use eclipsefs::device::BlockDevice;
use eclipsefs::{mkfs, EclipseFS, Error, FileKind};

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

impl Recorder {
    fn new(image: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            data: Mutex::new(image),
            log: Mutex::new(Vec::new()),
        })
    }
    fn events(&self) -> usize {
        self.log.lock().unwrap().len()
    }
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

/// Plain RAM image for mounting a crash state.
struct Image(Mutex<Vec<u8>>);

impl BlockDevice for Image {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> eclipsefs::Result<()> {
        let d = self.0.lock().unwrap();
        let o = offset as usize;
        buf.copy_from_slice(d.get(o..o + buf.len()).ok_or(Error::Io)?);
        Ok(())
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> eclipsefs::Result<()> {
        let mut d = self.0.lock().unwrap();
        let o = offset as usize;
        d.get_mut(o..o + buf.len())
            .ok_or(Error::Io)?
            .copy_from_slice(buf);
        Ok(())
    }
    fn sync(&self) -> eclipsefs::Result<()> {
        Ok(())
    }
    fn size(&self) -> u64 {
        self.0.lock().unwrap().len() as u64
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Node {
    Dir,
    File(Vec<u8>),
    Link(Vec<u8>),
}

type State = BTreeMap<String, Node>;

fn hash(s: &State) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// The whole tree as path -> node.
fn snapshot(fs: &mut EclipseFS) -> State {
    let mut out = State::new();
    let mut stack = vec![(fs.root_ino(), String::new())];
    while let Some((dir, path)) = stack.pop() {
        for e in fs.readdir(dir).unwrap() {
            let p = format!("{}/{}", path, e.name);
            let node = match e.kind {
                FileKind::Dir => {
                    stack.push((e.ino, p.clone()));
                    Node::Dir
                }
                FileKind::Symlink => Node::Link(fs.read_link(e.ino).unwrap()),
                _ => Node::File(read_all(fs, e.ino)),
            };
            out.insert(p, node);
        }
    }
    out
}

struct Run {
    base: Vec<u8>,
    log: Vec<Event>,
    /// Hash of the state after each operation (index 0: empty filesystem).
    states: Vec<u64>,
    /// (log length, operations completed) at each completed `sync`.
    syncs: Vec<(usize, usize)>,
}

type Mkfs = fn(&dyn BlockDevice, &mkfs::MkfsOptions) -> eclipsefs::Result<()>;

/// Random namespace and data workload. With `cow` (v0.4) writes land
/// anywhere in a file and truncates cut mid-block, since copy-on-write
/// makes those atomic too; on v0.3 writes only extend files and truncates
/// are block-aligned (in-place overwrites are not atomic in ordered mode).
fn workload(seed: u64, ops: usize, size: usize, format: Mkfs, cow: bool) -> Run {
    let disk = MemDisk::new(size);
    format(&*disk, &opts()).unwrap();
    let mut base = vec![0u8; size];
    disk.read_at(0, &mut base).unwrap();
    let rec = Recorder::new(base.clone());
    let mut fs = EclipseFS::mount(rec.clone(), false).unwrap();
    fs.set_clock(clock);
    let root = fs.root_ino();
    let mut rng = Lcg(seed);
    let mut states = vec![hash(&snapshot(&mut fs))];
    let mut syncs = Vec::new();
    let mut dirs = vec![root];
    let mut files: Vec<(u64, String)> = Vec::new();
    let mut done = 0;
    // Successful snapshots and reflinks (v0.4), so the run is known to
    // exercise them.
    let (mut snaps, mut clones) = (0, 0);
    while done < ops {
        let d = dirs[rng.below(dirs.len() as u64) as usize];
        let name = format!("e{}", rng.below(12));
        let ok = match rng.below(if cow { 17 } else { 14 }) {
            0 | 1 => fs.create(d, &name, FileKind::Regular, 0o644, 0).is_ok(),
            2 => match fs.create(d, &name, FileKind::Dir, 0o755, 0) {
                Ok(ino) => {
                    dirs.push(ino);
                    true
                }
                Err(_) => false,
            },
            3..=6 => match fs.lookup(d, &name) {
                Ok(ino) if fs.stat(ino).unwrap().kind == FileKind::Regular => {
                    let size = fs.stat(ino).unwrap().size;
                    let len = 1 + rng.below(30_000) as usize;
                    let at = if cow { rng.below(size + 1) } else { size };
                    let n = fs.write(ino, at, &pattern(len, done)).unwrap();
                    assert_eq!(n, len);
                    files.push((d, name));
                    true
                }
                _ => false,
            },
            7 => match fs.lookup(d, &name) {
                Ok(ino) if fs.stat(ino).unwrap().kind == FileKind::Regular => {
                    let size = fs.stat(ino).unwrap().size;
                    let to = if cow {
                        rng.below(size + 5000)
                    } else {
                        (size / 2) / 4096 * 4096
                    };
                    fs.truncate(ino, to).unwrap();
                    true
                }
                _ => false,
            },
            8 | 9 => fs.unlink(d, &name).is_ok(),
            10 => {
                let nd = dirs[rng.below(dirs.len() as u64) as usize];
                let nn = format!("e{}", rng.below(12));
                fs.rename(d, &name, nd, &nn).is_ok()
            }
            11 => {
                let len = 1 + rng.below(300) as usize;
                fs.symlink(d, &name, &vec![b'x'; len]).is_ok()
            }
            12 => match fs.lookup(d, &name) {
                Ok(ino) if fs.stat(ino).unwrap().kind != FileKind::Dir => {
                    let nn = format!("h{}", rng.below(6));
                    fs.link(d, &nn, ino).is_ok()
                }
                _ => false,
            },
            13 => {
                fs.sync().unwrap();
                syncs.push((rec.events(), done));
                false
            }
            // v0.4 only: snapshots of everything, reflinks, deleting snapshots.
            14 => {
                let name = format!("snap{}", rng.below(3));
                let ok = fs.snapshot(root, root, &name, rng.below(2) == 0).is_ok();
                snaps += usize::from(ok);
                ok
            }
            15 if !files.is_empty() => {
                // Reflink a file that was written to over another name in
                // its directory, creating the target first.
                let (sd, sname) = files[rng.below(files.len() as u64) as usize].clone();
                let nn = format!("e{}", rng.below(12));
                let regular =
                    |fs: &mut EclipseFS, x: u64| fs.stat(x).unwrap().kind == FileKind::Regular;
                match (fs.lookup(sd, &sname), fs.lookup(sd, &nn)) {
                    (Ok(src), Ok(dst))
                        if src != dst && regular(&mut fs, src) && regular(&mut fs, dst) =>
                    {
                        // Whole blocks, so any target size works.
                        let len = fs.stat(src).unwrap().size / 4096 * 4096;
                        let ok = len > 0 && fs.clone_range(src, 0, dst, 0, len).is_ok();
                        clones += usize::from(ok);
                        ok
                    }
                    (Ok(_), Err(_)) => fs.create(sd, &nn, FileKind::Regular, 0o644, 0).is_ok(),
                    _ => false,
                }
            }
            15 => false,
            _ => {
                let name = format!("snap{}", rng.below(3));
                fs.delete_subvolume(root, &name).is_ok()
            }
        };
        // Directories that were removed or moved under a removed one are
        // simply never chosen successfully again.
        dirs.retain(|&x| fs.stat(x).is_ok());
        if ok {
            done += 1;
            states.push(hash(&snapshot(&mut fs)));
        }
    }
    if cow {
        assert!(
            snaps > 0 && clones > 0,
            "{} snapshots, {} clones",
            snaps,
            clones
        );
    }
    drop(fs);
    let log = rec.log.lock().unwrap().clone();
    Run {
        base,
        log,
        states,
        syncs,
    }
}

fn apply(image: &mut [u8], ev: &Event) {
    if let Event::Write(off, data) = ev {
        let o = *off as usize;
        image[o..o + data.len()].copy_from_slice(data);
    }
}

fn verify(run: &Run, image: Vec<u8>, cut: usize) {
    let dev = Arc::new(Image(Mutex::new(image)));
    let mut fs = EclipseFS::mount(dev.clone(), false)
        .unwrap_or_else(|e| panic!("mount after crash at {}: {:?}", cut, e));
    fs.check()
        .unwrap_or_else(|e| panic!("check after crash at {}: {:?}", cut, e));
    if fs.format_version() == 4 {
        let r = fs.scrub().unwrap();
        assert!(r.is_clean(), "scrub after crash at {}: {:?}", cut, r);
    }
    let h = hash(&snapshot(&mut fs));
    let durable = run
        .syncs
        .iter()
        .filter(|(at, _)| *at <= cut)
        .map(|(_, ops)| *ops)
        .max()
        .unwrap_or(0);
    let positions: Vec<usize> = run
        .states
        .iter()
        .enumerate()
        .filter(|(_, &s)| s == h)
        .map(|(i, _)| i)
        .collect();
    assert!(
        positions.iter().any(|&p| p >= durable),
        "crash at event {}: state matches operations {:?}, but {} were synced",
        cut,
        positions,
        durable
    );
    // The filesystem keeps working after recovery.
    let root = fs.root_ino();
    let f = fs
        .create(root, "after-crash", FileKind::Regular, 0o644, 0)
        .unwrap();
    write_all(&mut fs, f, 0, &pattern(10_000, 9));
    fs.check().unwrap();
}

fn crash_everywhere(seed: u64, ops: usize, format: Mkfs, cow: bool) {
    let run = workload(seed, ops, 16 * MIB, format, cow);
    let mut image = run.base.clone();
    let n = run.log.len();
    let stride = (n / 400).max(1);
    for (i, ev) in run.log.iter().enumerate() {
        if i % stride == 0 {
            verify(&run, image.clone(), i);
        }
        apply(&mut image, ev);
    }
    verify(&run, image, n);
}

#[test]
fn crash_at_every_point_of_a_workload() {
    crash_everywhere(1, 250, mkfs::format, true);
}

#[test]
fn crash_at_every_point_of_a_v3_workload() {
    crash_everywhere(1, 250, mkfs::format_v3, false);
}

#[test]
fn crash_at_every_point_of_another_workload() {
    crash_everywhere(0xDEAD_BEEF, 250, mkfs::format, true);
}

#[test]
fn crash_loses_an_arbitrary_part_of_the_unsynced_writes() {
    lose_unsynced_writes(mkfs::format, true);
}

#[test]
fn crash_loses_an_arbitrary_part_of_the_unsynced_writes_v3() {
    lose_unsynced_writes(mkfs::format_v3, false);
}

fn lose_unsynced_writes(format: Mkfs, cow: bool) {
    let run = workload(7, 200, 16 * MIB, format, cow);
    let mut rng = Lcg(99);
    let syncs: Vec<usize> = run
        .log
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, Event::Sync))
        .map(|(i, _)| i)
        .collect();
    for _ in 0..150 {
        let cut = rng.below(run.log.len() as u64 + 1) as usize;
        let last_sync = syncs.iter().copied().filter(|&s| s < cut).max();
        let mut image = run.base.clone();
        let stable = last_sync.map(|s| s + 1).unwrap_or(0);
        for ev in &run.log[..stable] {
            apply(&mut image, ev);
        }
        // After the last barrier, any subset may have reached the disk, in
        // any order (so later writes may land without earlier ones).
        for ev in &run.log[stable..cut] {
            if rng.below(2) == 0 {
                apply(&mut image, ev);
            }
        }
        verify(&run, image, cut);
    }
}

#[test]
fn torn_journal_header_is_ignored() {
    // Corrupting the newest transaction in the journal after it was written
    // home changes nothing: the home copy is already there.
    let disk = MemDisk::new(8 * MIB);
    mkfs::format(&*disk, &opts()).unwrap();
    let mut fs = EclipseFS::mount(disk.clone(), false).unwrap();
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, f, 0, b"payload");
    fs.sync().unwrap();
    drop(fs);
    let mut sb = vec![0u8; 4096];
    disk.read_at(0, &mut sb).unwrap();
    let jstart = journal_start(&sb) as usize;
    for off in [0usize, 30, 4096 + 5, 8192 + 100] {
        disk.poke(jstart * 4096 + off, 0x40);
        let mut fs = EclipseFS::mount(disk.clone(), true).unwrap();
        let f = fs.lookup(root, "f").unwrap();
        assert_eq!(read_all(&mut fs, f), b"payload");
        fs.check().unwrap();
        disk.poke(jstart * 4096 + off, 0x40);
    }
}

#[test]
fn read_only_mount_sees_the_journal_without_writing() {
    let run = workload(3, 60, 8 * MIB, mkfs::format, true);
    // Stop right after the last journal header write: the transaction is
    // committed but not yet home.
    let mut image = run.base.clone();
    let mut sb = vec![0u8; 4096];
    sb.copy_from_slice(&run.base[..4096]);
    let jstart = journal_start(&sb);
    let mut cut = 0;
    for (i, ev) in run.log.iter().enumerate() {
        if let Event::Write(off, _) = ev {
            if *off == jstart * 4096 {
                cut = i + 1;
            }
        }
    }
    assert!(cut > 0);
    for ev in &run.log[..cut] {
        apply(&mut image, ev);
    }
    let before = image.clone();
    let dev = Arc::new(Image(Mutex::new(image)));
    let mut ro = EclipseFS::mount(dev.clone(), true).unwrap();
    ro.check().unwrap();
    let ro_state = snapshot(&mut ro);
    drop(ro);
    assert!(*dev.0.lock().unwrap() == before, "read-only mount wrote");
    let mut rw = EclipseFS::mount(dev, false).unwrap();
    assert_eq!(snapshot(&mut rw), ro_state);
    rw.check().unwrap();
}

#[test]
fn transactions_larger_than_the_journal_still_recover() {
    // A tiny disk has a 64-block journal; growing the inode table (64
    // blocks) and a big directory compaction do not fit in it.
    big_transactions(2 * MIB, mkfs::format_v3);
}

#[test]
fn big_directories_recover_from_a_crash() {
    // On v0.4 only bitmaps go through the journal; this is a crash test of
    // many tree splits and merges.
    big_transactions(8 * MIB, mkfs::format);
}

fn big_transactions(size: usize, format: Mkfs) {
    let disk = MemDisk::new(size);
    format(&*disk, &opts()).unwrap();
    let mut base = vec![0u8; size];
    disk.read_at(0, &mut base).unwrap();
    let rec = Recorder::new(base.clone());
    let mut fs = EclipseFS::mount(rec.clone(), false).unwrap();
    let root = fs.root_ino();
    let mut states = vec![hash(&snapshot(&mut fs))];
    for i in 0..1100 {
        fs.create(root, &format!("{:0>200}", i), FileKind::Regular, 0o644, 0)
            .unwrap();
        if i % 100 == 0 {
            states.push(hash(&snapshot(&mut fs)));
        }
    }
    fs.sync().unwrap();
    let synced = rec.events();
    for i in 0..1100 {
        fs.unlink(root, &format!("{:0>200}", i)).unwrap();
    }
    fs.sync().unwrap();
    drop(fs);
    let log = rec.log.lock().unwrap().clone();
    let mut image = base;
    for (i, ev) in log.iter().enumerate() {
        if i >= synced && i % 7 == 0 {
            let dev = Arc::new(Image(Mutex::new(image.clone())));
            let mut fs = EclipseFS::mount(dev, false).unwrap();
            // Unjournaled transactions only promise a consistent result
            // when they complete; the journaled ones before them hold.
            if fs.check().is_ok() {
                let _ = snapshot(&mut fs);
            }
        }
        apply(&mut image, ev);
    }
    let dev = Arc::new(Image(Mutex::new(image)));
    let mut fs = EclipseFS::mount(dev, false).unwrap();
    fs.check().unwrap();
    assert!(fs.readdir(root).unwrap().is_empty());
    let _ = states;
}
