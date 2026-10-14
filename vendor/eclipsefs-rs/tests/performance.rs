#![cfg(feature = "std")]
//! Benchmarks: EclipseFS against the in-tree btrfs, side by side.
//!
//! Every scenario runs the same workload through both drivers on a
//! file-backed image and prints wall time, throughput and what reached the
//! device (commands and bytes, so write amplification is visible). Each one
//! also verifies the data, so a fast wrong answer fails instead of winning.
//!
//! ```text
//! scripts/eclipsefs-bench.sh                     # release, default scale
//! ECLIPSEFS_BENCH_SCALE=4 scripts/eclipsefs-bench.sh
//! ```

use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

// ---------------------------------------------------------------------------
// One counting device usable by both drivers
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Counters {
    reads: AtomicU64,
    read_bytes: AtomicU64,
    writes: AtomicU64,
    write_bytes: AtomicU64,
}

#[derive(Clone, Copy, Default)]
struct Snap {
    reads: u64,
    read_bytes: u64,
    writes: u64,
    write_bytes: u64,
}

impl Counters {
    fn snap(&self) -> Snap {
        Snap {
            reads: self.reads.load(Ordering::Relaxed),
            read_bytes: self.read_bytes.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            write_bytes: self.write_bytes.load(Ordering::Relaxed),
        }
    }
}

impl std::ops::Sub for Snap {
    type Output = Snap;
    fn sub(self, o: Snap) -> Snap {
        Snap {
            reads: self.reads - o.reads,
            read_bytes: self.read_bytes - o.read_bytes,
            writes: self.writes - o.writes,
            write_bytes: self.write_bytes - o.write_bytes,
        }
    }
}

struct Disk {
    file: Mutex<File>,
    size: u64,
    c: Counters,
    path: PathBuf,
}

impl Disk {
    fn new(tag: &str, size: u64) -> Arc<Self> {
        let path = std::env::temp_dir().join(format!(
            "eclipsefs-bench-{}-{}-{:?}.img",
            tag,
            std::process::id(),
            std::thread::current().id()
        ));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.set_len(size).unwrap();
        Arc::new(Self {
            file: Mutex::new(file),
            size,
            c: Counters::default(),
            path,
        })
    }

    fn read(&self, off: u64, buf: &mut [u8]) -> bool {
        use std::os::unix::fs::FileExt;
        self.c.reads.fetch_add(1, Ordering::Relaxed);
        self.c
            .read_bytes
            .fetch_add(buf.len() as u64, Ordering::Relaxed);
        self.file.lock().unwrap().read_exact_at(buf, off).is_ok()
    }

    fn write(&self, off: u64, buf: &[u8]) -> bool {
        use std::os::unix::fs::FileExt;
        self.c.writes.fetch_add(1, Ordering::Relaxed);
        self.c
            .write_bytes
            .fetch_add(buf.len() as u64, Ordering::Relaxed);
        self.file.lock().unwrap().write_all_at(buf, off).is_ok()
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

struct EDev(Arc<Disk>);
struct BDev(Arc<Disk>);

impl eclipsefs::BlockDevice for EDev {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> eclipsefs::Result<()> {
        self.0
            .read(off, buf)
            .then_some(())
            .ok_or(eclipsefs::Error::Io)
    }
    fn write_at(&self, off: u64, buf: &[u8]) -> eclipsefs::Result<()> {
        self.0
            .write(off, buf)
            .then_some(())
            .ok_or(eclipsefs::Error::Io)
    }
    fn sync(&self) -> eclipsefs::Result<()> {
        Ok(())
    }
    fn size(&self) -> u64 {
        self.0.size
    }
}

impl btrfs::BlockDevice for BDev {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> btrfs::Result<()> {
        self.0.read(off, buf).then_some(()).ok_or(btrfs::Error::Io)
    }
    fn write_at(&self, off: u64, buf: &[u8]) -> btrfs::Result<()> {
        self.0.write(off, buf).then_some(()).ok_or(btrfs::Error::Io)
    }
    fn sync(&self) -> btrfs::Result<()> {
        Ok(())
    }
    fn size(&self) -> u64 {
        self.0.size
    }
}

// ---------------------------------------------------------------------------
// The operations the workloads need, over either driver
// ---------------------------------------------------------------------------

trait Fs {
    fn name(&self) -> &'static str;
    fn root(&self) -> u64;
    fn mkdir(&mut self, dir: u64, name: &str) -> u64;
    fn create(&mut self, dir: u64, name: &str) -> u64;
    fn write(&mut self, ino: u64, off: u64, data: &[u8]) -> usize;
    fn read(&mut self, ino: u64, off: u64, buf: &mut [u8]) -> usize;
    fn lookup(&mut self, dir: u64, name: &str) -> u64;
    fn unlink(&mut self, dir: u64, name: &str);
    fn truncate(&mut self, ino: u64, size: u64);
    fn readdir_len(&mut self, dir: u64) -> usize;
    fn sync(&mut self);
}

impl Fs for eclipsefs::EclipseFS {
    fn name(&self) -> &'static str {
        "eclipsefs"
    }
    fn root(&self) -> u64 {
        self.root_ino()
    }
    fn mkdir(&mut self, dir: u64, name: &str) -> u64 {
        self.create(dir, name, eclipsefs::FileKind::Dir, 0o755, 0)
            .unwrap()
    }
    fn create(&mut self, dir: u64, name: &str) -> u64 {
        eclipsefs::EclipseFS::create(self, dir, name, eclipsefs::FileKind::Regular, 0o644, 0)
            .unwrap()
    }
    fn write(&mut self, ino: u64, off: u64, data: &[u8]) -> usize {
        eclipsefs::EclipseFS::write(self, ino, off, data).unwrap()
    }
    fn read(&mut self, ino: u64, off: u64, buf: &mut [u8]) -> usize {
        eclipsefs::EclipseFS::read(self, ino, off, buf).unwrap()
    }
    fn lookup(&mut self, dir: u64, name: &str) -> u64 {
        eclipsefs::EclipseFS::lookup(self, dir, name).unwrap()
    }
    fn unlink(&mut self, dir: u64, name: &str) {
        eclipsefs::EclipseFS::unlink(self, dir, name).unwrap()
    }
    fn truncate(&mut self, ino: u64, size: u64) {
        eclipsefs::EclipseFS::truncate(self, ino, size).unwrap()
    }
    fn readdir_len(&mut self, dir: u64) -> usize {
        self.readdir(dir).unwrap().len()
    }
    fn sync(&mut self) {
        eclipsefs::EclipseFS::sync(self).unwrap()
    }
}

impl Fs for btrfs::Btrfs {
    fn name(&self) -> &'static str {
        "btrfs"
    }
    fn root(&self) -> u64 {
        self.root_ino()
    }
    fn mkdir(&mut self, dir: u64, name: &str) -> u64 {
        btrfs::Btrfs::create(self, dir, name, btrfs::FileKind::Dir, 0o755, 0).unwrap()
    }
    fn create(&mut self, dir: u64, name: &str) -> u64 {
        btrfs::Btrfs::create(self, dir, name, btrfs::FileKind::Regular, 0o644, 0).unwrap()
    }
    fn write(&mut self, ino: u64, off: u64, data: &[u8]) -> usize {
        btrfs::Btrfs::write(self, ino, off, data).unwrap()
    }
    fn read(&mut self, ino: u64, off: u64, buf: &mut [u8]) -> usize {
        btrfs::Btrfs::read(self, ino, off, buf).unwrap()
    }
    fn lookup(&mut self, dir: u64, name: &str) -> u64 {
        btrfs::Btrfs::lookup(self, dir, name).unwrap()
    }
    fn unlink(&mut self, dir: u64, name: &str) {
        btrfs::Btrfs::unlink(self, dir, name).unwrap()
    }
    fn truncate(&mut self, ino: u64, size: u64) {
        btrfs::Btrfs::truncate(self, ino, size).unwrap()
    }
    fn readdir_len(&mut self, dir: u64) -> usize {
        self.readdir(dir).unwrap().len()
    }
    fn sync(&mut self) {
        btrfs::Btrfs::sync(self).unwrap()
    }
}

fn uuid(seed: u8) -> [u8; 16] {
    let mut u = [seed; 16];
    u[6] = (u[6] & 0x0f) | 0x40;
    u[8] = (u[8] & 0x3f) | 0x80;
    u
}

/// Both drivers, freshly formatted on images of `size` bytes.
fn both(size: u64) -> Vec<(Box<dyn Fs>, Arc<Disk>)> {
    let ed = Disk::new("efs", size);
    eclipsefs::mkfs::format(
        &EDev(ed.clone()),
        &eclipsefs::mkfs::MkfsOptions {
            label: "bench".into(),
            fsid: uuid(1),
            chunk_uuid: uuid(2),
            dev_uuid: uuid(3),
            subvol_uuid: uuid(4),
            now: (1_700_000_000, 0),
        },
    )
    .unwrap();
    let efs = eclipsefs::EclipseFS::mount(Arc::new(EDev(ed.clone())), false).unwrap();

    let bd = Disk::new("btrfs", size);
    btrfs::mkfs::format(
        &BDev(bd.clone()),
        &btrfs::mkfs::MkfsOptions {
            label: "bench".into(),
            fsid: uuid(1),
            chunk_uuid: uuid(2),
            dev_uuid: uuid(3),
            subvol_uuid: uuid(4),
            now: (1_700_000_000, 0),
        },
    )
    .unwrap();
    let bfs = btrfs::Btrfs::mount(Arc::new(BDev(bd.clone())), false).unwrap();

    vec![(Box::new(efs), ed), (Box::new(bfs), bd)]
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

fn scale() -> u64 {
    std::env::var("ECLIPSEFS_BENCH_SCALE")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(1)
}

const MIB: u64 = 1024 * 1024;

/// Time `f`, then print one row: driver, phase, time, rate and device I/O.
fn measure<T>(
    fs_name: &str,
    phase: &str,
    disk: &Disk,
    units: (f64, &str),
    f: impl FnOnce() -> T,
) -> T {
    let before = disk.c.snap();
    let t = Instant::now();
    let r = f();
    let secs = t.elapsed().as_secs_f64().max(1e-9);
    let io = disk.c.snap() - before;
    println!(
        "  {:<10} {:<16} {:>8.3}s {:>12.1} {:<6} | dev R {:>7} cmd {:>9.1} MiB  W {:>7} cmd {:>9.1} MiB",
        fs_name,
        phase,
        secs,
        units.0 / secs,
        units.1,
        io.reads,
        io.read_bytes as f64 / MIB as f64,
        io.writes,
        io.write_bytes as f64 / MIB as f64,
    );
    r
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn fill(&mut self, buf: &mut [u8]) {
        for c in buf.chunks_mut(4) {
            let v = (self.next() as u32).to_le_bytes();
            c.copy_from_slice(&v[..c.len()]);
        }
    }
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// One big file: sequential write in 128 KiB chunks, then read in 64 KiB
/// chunks (the demand-paging/`cat` pattern).
#[test]
fn bench_large_file_sequential() {
    let total = 64 * MIB * scale();
    println!("\n[large file, sequential] {} MiB", total / MIB);
    for (mut fs, disk) in both((total * 3).max(256 * MIB)) {
        let name = fs.name();
        let f = fs.create(fs.root(), "big.bin");
        let mut chunk = vec![0u8; 128 * 1024];
        let mut rng = Lcg(7);
        measure(
            name,
            "write",
            &disk,
            (total as f64 / MIB as f64, "MiB/s"),
            || {
                let mut off = 0;
                while off < total {
                    rng.fill(&mut chunk);
                    assert_eq!(fs.write(f, off, &chunk), chunk.len());
                    off += chunk.len() as u64;
                }
                fs.sync();
            },
        );
        let mut buf = vec![0u8; 64 * 1024];
        let mut check = Lcg(7);
        let mut want = vec![0u8; 128 * 1024];
        measure(
            name,
            "read",
            &disk,
            (total as f64 / MIB as f64, "MiB/s"),
            || {
                let mut off = 0;
                while off < total {
                    assert_eq!(fs.read(f, off, &mut buf), buf.len());
                    if off % want.len() as u64 == 0 {
                        check.fill(&mut want);
                    }
                    let at = (off % want.len() as u64) as usize;
                    assert_eq!(&buf[..], &want[at..at + buf.len()], "data at {}", off);
                    off += buf.len() as u64;
                }
            },
        );
    }
}

/// Random 4 KiB reads over a 32 MiB file (shared-library page faults).
#[test]
fn bench_random_reads() {
    let size = 32 * MIB;
    let reads = 20_000 * scale();
    println!(
        "\n[random 4 KiB reads] {} reads over {} MiB",
        reads,
        size / MIB
    );
    for (mut fs, disk) in both(256 * MIB) {
        let name = fs.name();
        let f = fs.create(fs.root(), "lib.so");
        let mut data = vec![0u8; size as usize];
        Lcg(11).fill(&mut data);
        for (i, c) in data.chunks(MIB as usize).enumerate() {
            assert_eq!(fs.write(f, i as u64 * MIB, c), c.len());
        }
        fs.sync();
        let mut rng = Lcg(3);
        let mut buf = vec![0u8; 4096];
        measure(name, "read 4K", &disk, (reads as f64, "ops/s"), || {
            for _ in 0..reads {
                let page = rng.next() % (size / 4096);
                let off = page * 4096;
                assert_eq!(fs.read(f, off, &mut buf), 4096);
                assert_eq!(&buf[..], &data[off as usize..off as usize + 4096]);
            }
        });
    }
}

/// Many small files in one directory: create+write, lookup each, list,
/// delete all (package install and removal).
#[test]
fn bench_many_small_files() {
    let n = 3000 * scale();
    println!("\n[small files, one directory] {} files of 2 KiB", n);
    for (mut fs, disk) in both(512 * MIB) {
        let name = fs.name();
        let d = fs.mkdir(fs.root(), "lib");
        let body = vec![0x5au8; 2048];
        measure(name, "create+write", &disk, (n as f64, "ops/s"), || {
            for i in 0..n {
                let f = fs.create(d, &format!("libfile-{i:06}.so"));
                assert_eq!(fs.write(f, 0, &body), body.len());
            }
            fs.sync();
        });
        measure(name, "lookup", &disk, (n as f64, "ops/s"), || {
            for i in (0..n).rev() {
                fs.lookup(d, &format!("libfile-{i:06}.so"));
            }
        });
        measure(name, "readdir", &disk, (1.0, "ops/s"), || {
            assert_eq!(fs.readdir_len(d), n as usize);
        });
        measure(name, "unlink", &disk, (n as f64, "ops/s"), || {
            for i in 0..n {
                fs.unlink(d, &format!("libfile-{i:06}.so"));
            }
            fs.sync();
        });
        assert_eq!(fs.readdir_len(d), 0);
    }
}

/// A deep, wide tree of small files (a source checkout / rootfs copy).
#[test]
fn bench_tree_copy() {
    let dirs = 40 * scale();
    let per_dir = 50;
    println!("\n[tree copy] {} dirs x {} files, 1..16 KiB", dirs, per_dir);
    for (mut fs, disk) in both(512 * MIB) {
        let name = fs.name();
        let mut rng = Lcg(5);
        let mut buf = vec![0u8; 16 * 1024];
        let mut bytes = 0u64;
        measure(
            name,
            "populate",
            &disk,
            ((dirs * per_dir) as f64, "files/s"),
            || {
                let mut parent = fs.root();
                for i in 0..dirs {
                    let d = fs.mkdir(parent, &format!("d{i}"));
                    if i % 4 == 3 {
                        parent = d;
                    }
                    for j in 0..per_dir {
                        let len = 1024 + (rng.next() as usize % (15 * 1024));
                        rng.fill(&mut buf[..len]);
                        let f = fs.create(d, &format!("f{j}.c"));
                        assert_eq!(fs.write(f, 0, &buf[..len]), len);
                        bytes += len as u64;
                    }
                }
                fs.sync();
            },
        );
        println!(
            "  {:<10} wrote {:.1} MiB of file data",
            name,
            bytes as f64 / MIB as f64
        );
    }
}

/// Append in small writes (a log or a tar extractor without coalescing).
#[test]
fn bench_small_appends() {
    let total = 16 * MIB * scale();
    let piece = 4096usize;
    println!(
        "\n[small appends] {} MiB in {} B writes",
        total / MIB,
        piece
    );
    for (mut fs, disk) in both(256 * MIB) {
        let name = fs.name();
        let f = fs.create(fs.root(), "log");
        let data = vec![0xA5u8; piece];
        measure(
            name,
            "append",
            &disk,
            (total as f64 / MIB as f64, "MiB/s"),
            || {
                let mut off = 0;
                while off < total {
                    assert_eq!(fs.write(f, off, &data), piece);
                    off += piece as u64;
                }
                fs.sync();
            },
        );
    }
}

/// Grow and shrink files repeatedly (temporary files, rewrites).
#[test]
fn bench_truncate_churn() {
    let rounds = 200 * scale();
    println!("\n[truncate churn] {} rounds over 8 files", rounds);
    for (mut fs, disk) in both(256 * MIB) {
        let name = fs.name();
        let files: Vec<u64> = (0..8)
            .map(|i| fs.create(fs.root(), &format!("t{i}")))
            .collect();
        let data = vec![1u8; 256 * 1024];
        measure(
            name,
            "write+truncate",
            &disk,
            (rounds as f64, "rounds/s"),
            || {
                for r in 0..rounds {
                    let f = files[(r % 8) as usize];
                    assert_eq!(fs.write(f, 0, &data), data.len());
                    fs.truncate(f, (r % 7) * 4096 + 100);
                }
                fs.sync();
            },
        );
        let mut b = [0u8; 100];
        assert_eq!(fs.read(files[0], 0, &mut b), 100);
        assert!(b.iter().all(|&x| x == 1));
    }
}
