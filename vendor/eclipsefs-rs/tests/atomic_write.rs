#![cfg(feature = "std")]
//! `RWF_ATOMIC` writes: all of the data or nothing, and never torn by a
//! power cut.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use eclipsefs::cow::ATOMIC_WRITE_MAX;
use eclipsefs::cowfmt::FL_NOCOW;
use eclipsefs::device::BlockDevice;
use eclipsefs::{mkfs, EclipseFS, Error, FileKind};

fn fresh(size: usize) -> (Arc<MemDisk>, EclipseFS) {
    let disk = MemDisk::new(size);
    mkfs::format(&*disk, &opts()).unwrap();
    let mut fs = EclipseFS::mount(disk.clone(), false).unwrap();
    fs.set_clock(clock);
    (disk, fs)
}

fn read_all(fs: &mut EclipseFS, ino: u64) -> Vec<u8> {
    let size = fs.stat(ino).unwrap().size as usize;
    let mut buf = vec![0u8; size];
    let n = fs.read(ino, 0, &mut buf).unwrap();
    buf.truncate(n);
    buf
}

#[test]
fn an_atomic_write_writes_everything() {
    let (_disk, mut fs) = fresh(32 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    fs.write(f, 0, &vec![1u8; 300_000]).unwrap();
    assert!(fs.atomic_write_supported(f).unwrap());
    let data = vec![9u8; 65_536];
    assert_eq!(fs.write_atomic(f, 65_536, &data).unwrap(), data.len());
    let all = read_all(&mut fs, f);
    assert!(all[..65_536].iter().all(|&b| b == 1));
    assert!(all[65_536..131_072].iter().all(|&b| b == 9));
    assert!(all[131_072..].iter().all(|&b| b == 1));
    // Past the end grows the file.
    fs.write_atomic(f, 1 << 20, &data).unwrap();
    assert_eq!(fs.stat(f).unwrap().size, (1 << 20) + 65_536);
    fs.check().unwrap();
}

#[test]
fn what_atomic_writes_refuse() {
    let (_disk, mut fs) = fresh(32 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    let big = vec![0u8; ATOMIC_WRITE_MAX as usize + 4096];
    assert_eq!(fs.write_atomic(f, 0, &big), Err(Error::Invalid));
    assert_eq!(fs.write_atomic(f, 0, &[]), Err(Error::Invalid));
    let d = fs.create(root, "d", FileKind::Dir, 0o755, 0).unwrap();
    assert!(!fs.atomic_write_supported(d).unwrap());
    assert!(fs.write_atomic(d, 0, b"x").is_err());
    // No-COW files are rewritten in place: not atomic.
    let n = fs.create(root, "n", FileKind::Regular, 0o644, 0).unwrap();
    fs.set_flags(n, FL_NOCOW).unwrap();
    assert!(!fs.atomic_write_supported(n).unwrap());
    assert!(matches!(
        fs.write_atomic(n, 0, &[1u8; 4096]),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn without_room_an_atomic_write_writes_nothing() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    fs.write(f, 0, &vec![3u8; 64 * 1024]).unwrap();
    let filler = fs
        .create(root, "fill", FileKind::Regular, 0o644, 0)
        .unwrap();
    let chunk = vec![7u8; 256 * 1024];
    let mut off = 0;
    loop {
        match fs.write(filler, off, &chunk) {
            Ok(n) if n == chunk.len() => off += n as u64,
            _ => break,
        }
    }
    fs.sync().unwrap();
    let before = read_all(&mut fs, f);
    let size = fs.stat(f).unwrap().size;
    assert_eq!(
        fs.write_atomic(f, 0, &vec![5u8; ATOMIC_WRITE_MAX as usize]),
        Err(Error::NoSpace)
    );
    assert_eq!(fs.stat(f).unwrap().size, size);
    assert_eq!(read_all(&mut fs, f), before);
    fs.check().unwrap();
}

// ---------------------------------------------------------------------------
// Power cuts during atomic writes
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Event {
    Write(u64, Vec<u8>),
    Sync,
}

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

/// Every prefix of the device's writes during a run of atomic overwrites
/// (each followed by an fsync or not): the file always reads as one of the
/// versions, never part of one and part of another.
fn untorn(timed: bool) {
    let size = 16 * MIB;
    let disk = MemDisk::new(size);
    mkfs::format(&*disk, &opts()).unwrap();
    let mut fs = EclipseFS::mount(disk.clone(), false).unwrap();
    fs.set_clock(clock);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    fs.write(f, 0, &vec![0u8; 256 * 1024]).unwrap();
    fs.sync().unwrap();
    drop(fs);
    let mut base = vec![0u8; size];
    disk.read_at(0, &mut base).unwrap();
    let rec = Arc::new(Recorder {
        data: Mutex::new(base.clone()),
        log: Mutex::new(Vec::new()),
    });
    let mut fs = EclipseFS::mount(rec.clone(), false).unwrap();
    fs.set_clock(clock);
    if timed {
        fs.set_commit_interval(Some(5));
    }
    let mut versions = vec![vec![0u8; 256 * 1024]];
    for k in 1..=40u8 {
        let len = 4096usize << (k % 6);
        let off = ((k as usize * 3) % 8) * len % (256 * 1024 - len);
        let off = off / len * len;
        let data = vec![k; len];
        fs.write_atomic(f, off as u64, &data).unwrap();
        let mut v = versions.last().unwrap().clone();
        v[off..off + len].copy_from_slice(&data);
        versions.push(v);
        if k % 3 == 0 {
            fs.fsync(f).unwrap();
        }
    }
    let log = rec.log.lock().unwrap().clone();
    std::mem::forget(fs);
    let mut image = base;
    for (i, ev) in log.iter().enumerate() {
        if let Event::Write(off, d) = ev {
            image[*off as usize..*off as usize + d.len()].copy_from_slice(d);
        }
        let copy = MemDisk::new(0);
        copy.resize(image.len());
        copy.write_at(0, &image).unwrap();
        let mut after = EclipseFS::mount(copy, false).unwrap();
        let got = read_all(&mut after, f);
        assert!(
            versions.contains(&got),
            "torn file after {} of {} events",
            i + 1,
            log.len()
        );
    }
}

#[test]
fn atomic_writes_are_never_torn() {
    untorn(false);
}

#[test]
fn atomic_writes_are_never_torn_with_timed_commits() {
    untorn(true);
}
