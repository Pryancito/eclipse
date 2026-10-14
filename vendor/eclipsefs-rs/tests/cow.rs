#![cfg(feature = "std")]
//! Copy-on-write guarantees of the v0.4 format: nothing the last commit
//! points at is overwritten before the next one.

mod common;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use common::*;
use eclipsefs::device::BlockDevice;
use eclipsefs::{mkfs, EclipseFS, Error, FileKind};

/// RAM disk that remembers which blocks were written since `clear`.
struct Tracker {
    disk: Arc<MemDisk>,
    written: Mutex<BTreeSet<u64>>,
}

impl BlockDevice for Tracker {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> eclipsefs::Result<()> {
        self.disk.read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> eclipsefs::Result<()> {
        let mut w = self.written.lock().unwrap();
        let end = offset + buf.len() as u64;
        for b in offset / 4096..end.div_ceil(4096) {
            w.insert(b);
        }
        self.disk.write_at(offset, buf)
    }
    fn sync(&self) -> eclipsefs::Result<()> {
        Ok(())
    }
    fn size(&self) -> u64 {
        self.disk.size()
    }
}

/// Blocks in use according to the on-disk bitmap of a one-group image.
fn used_blocks(disk: &MemDisk) -> BTreeSet<u64> {
    let mut bm = vec![0u8; 4096];
    disk.read_at(4096, &mut bm).unwrap();
    let total = disk.size() / 4096;
    (0..total)
        .filter(|&b| bm[(b / 8) as usize] & (1 << (b % 8)) != 0)
        .collect()
}

/// Blocks a commit may rewrite in place: superblock, bitmap, journal.
fn allowed(disk: &MemDisk) -> BTreeSet<u64> {
    let mut sb = vec![0u8; 4096];
    disk.read_at(0, &mut sb).unwrap();
    let js = journal_start(&sb);
    let jb = u32::from_le_bytes(sb[176..180].try_into().unwrap()) as u64;
    let mut a: BTreeSet<u64> = (js..js + jb).collect();
    a.insert(0);
    a.insert(1);
    a
}

#[test]
fn committed_blocks_are_never_overwritten() {
    let disk = MemDisk::new(16 * MIB);
    mkfs::format(&*disk, &opts()).unwrap();
    let dev = Arc::new(Tracker {
        disk: disk.clone(),
        written: Mutex::new(BTreeSet::new()),
    });
    let mut fs = EclipseFS::mount(dev.clone(), false).unwrap();
    fs.set_clock(clock);
    let root = fs.root_ino();
    let mut rng = Lcg(42);
    let mut files = Vec::new();
    for i in 0..12 {
        let f = fs
            .create(root, &format!("f{}", i), FileKind::Regular, 0o644, 0)
            .unwrap();
        write_all(&mut fs, f, 0, &pattern(1 + rng.below(200_000) as usize, i));
        files.push(f);
    }
    for round in 0..40 {
        fs.sync().unwrap();
        let live = used_blocks(&disk);
        let ok = allowed(&disk);
        dev.written.lock().unwrap().clear();
        for _ in 0..5 {
            let f = files[rng.below(files.len() as u64) as usize];
            let size = fs.stat(f).unwrap().size;
            match rng.below(4) {
                0 | 1 => {
                    let at = rng.below(size + 1);
                    let len = 1 + rng.below(40_000) as usize;
                    write_all(&mut fs, f, at, &pattern(len, round));
                }
                2 => fs.truncate(f, rng.below(size + 10_000)).unwrap(),
                _ => {
                    let name = format!("tmp{}", rng.below(4));
                    match fs.create(root, &name, FileKind::Regular, 0o644, 0) {
                        Ok(t) => write_all(&mut fs, t, 0, &pattern(9000, round)),
                        Err(Error::Exists) => fs.unlink(root, &name).unwrap(),
                        Err(e) => panic!("{:?}", e),
                    }
                }
            }
        }
        fs.sync().unwrap();
        let w = dev.written.lock().unwrap().clone();
        assert!(!w.is_empty() && live.len() > ok.len());
        let bad: Vec<u64> = w
            .iter()
            .copied()
            .filter(|b| live.contains(b) && !ok.contains(b))
            .collect();
        assert!(
            bad.is_empty(),
            "round {}: committed blocks rewritten: {:?}",
            round,
            bad
        );
    }
    fs.check().unwrap();
}

#[test]
fn rewriting_a_file_keeps_its_size_on_disk() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    let data = pattern(MIB, 1);
    write_all(&mut fs, f, 0, &data);
    fs.sync().unwrap();
    let used = fs.fsinfo().bytes_used;
    for i in 0..20 {
        write_all(&mut fs, f, (i * 50_000) as u64, &pattern(30_000, i));
        fs.sync().unwrap();
    }
    // Old copies are released at each commit; only tree nodes may differ.
    let after = fs.fsinfo().bytes_used;
    assert!(after <= used + 16 * 4096, "{} -> {}", used, after);
    assert_eq!(fs.stat(f).unwrap().nbytes, MIB as u64);
    let mut fs = remount(&disk, fs);
    fs.check().unwrap();
}

#[test]
fn overwrites_of_every_shape_read_back() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    let mut model = pattern(300_000, 3);
    write_all(&mut fs, f, 0, &model);
    let mut rng = Lcg(7);
    for step in 0..300 {
        let at = rng.below(model.len() as u64 + 8192) as usize;
        let len = 1 + rng.below(12_000) as usize;
        let chunk = pattern(len, step);
        write_all(&mut fs, f, at as u64, &chunk);
        if model.len() < at + len {
            model.resize(at + len, 0);
        }
        model[at..at + len].copy_from_slice(&chunk);
        if step % 37 == 0 {
            let to = rng.below(model.len() as u64 + 4096) as usize;
            fs.truncate(f, to as u64).unwrap();
            model.resize(to, 0);
        }
        if step % 50 == 49 {
            fs = remount(&disk, fs);
            fs.check().unwrap();
        }
        assert_eq!(read_all(&mut fs, f), model, "step {}", step);
    }
}

#[test]
fn small_files_live_inside_the_tree() {
    let (disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    let before = fs.fsinfo().bytes_used;
    let mut inos = Vec::new();
    for i in 0..50 {
        let f = fs
            .create(root, &format!("s{}", i), FileKind::Regular, 0o644, 0)
            .unwrap();
        write_all(&mut fs, f, 0, &pattern(100 + i * 10, i));
        assert_eq!(fs.stat(f).unwrap().nbytes, (100 + i * 10) as u64);
        inos.push(f);
    }
    fs.sync().unwrap();
    // 50 small files take a few tree nodes, not 50 data blocks.
    assert!(fs.fsinfo().bytes_used - before < 20 * 4096);
    // Growing one past the inline limit moves it to a block.
    write_all(&mut fs, inos[0], 1000, &[9; 100]);
    assert_eq!(fs.stat(inos[0]).unwrap().nbytes, 4096);
    let mut fs = remount(&disk, fs);
    for (i, &f) in inos.iter().enumerate().skip(1) {
        assert_eq!(read_all(&mut fs, f), pattern(100 + i * 10, i));
    }
    let mut want = pattern(100, 0);
    want.resize(1000, 0);
    want.extend_from_slice(&[9; 100]);
    assert_eq!(read_all(&mut fs, inos[0]), want);
    fs.check().unwrap();
}

#[test]
fn v3_images_keep_working() {
    let disk = MemDisk::new(8 * MIB);
    mkfs::format_v3(&*disk, &opts()).unwrap();
    let mut fs = EclipseFS::mount(disk.clone(), false).unwrap();
    assert_eq!(fs.format_version(), 3);
    let root = fs.root_ino();
    let f = fs.create(root, "old", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, f, 0, &pattern(70_000, 2));
    let mut fs = remount(&disk, fs);
    assert_eq!(fs.format_version(), 3);
    assert_eq!(read_all(&mut fs, f), pattern(70_000, 2));
    fs.check().unwrap();
}

#[test]
fn unknown_versions_are_refused() {
    let disk = MemDisk::new(8 * MIB);
    mkfs::format(&*disk, &opts()).unwrap();
    disk.poke(14, 0x07); // major 4 -> 3 would be the v0.3 parser; 4^7 = 3
    assert!(EclipseFS::mount(disk.clone(), false).is_err());
    disk.poke(14, 0x07);
    disk.poke(14, 0x0C); // major 8
    assert_eq!(
        EclipseFS::mount(disk.clone(), false).err(),
        Some(Error::Unsupported("EclipseFS format version"))
    );
    disk.poke(14, 0x0C);
    assert!(EclipseFS::mount(disk, false).is_ok());
}
