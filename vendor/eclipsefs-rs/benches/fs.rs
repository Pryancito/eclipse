//! Reference benchmarks for the EclipseFS driver itself: the calls the
//! kernel adapter (`linux-object/src/fs/eclipsefs_mount.rs`) makes, on a RAM
//! disk, with no VFS, no FUSE and no real device.
//!
//! `scripts/eclipsefs-linux-bench.sh` puts EclipseFS next to ext4 and xfs,
//! but through FUSE and a loop device, and `tests/performance.rs` next to the
//! in-tree btrfs, with the cost of a file-backed image in every row. These
//! rows take both out: what is left is the driver's own CPU per call. When
//! a Linux row says EclipseFS is 4x slower than ext4 at creating files, the
//! `create_unlink_*` rows say whether the driver is the reason.
//!
//! The rows are families over one size, so the SLOPE can be read:
//!
//! - `lookup_in_dir_of_{1,100,10000}`, `create_unlink_in_dir_of_*`:
//!   directory entries are hashed into the tree, so these should stay flat
//!   (a log factor). A family that grows linearly means a scan.
//! - `readdir_{10,1000}`: linear by nature; the per-entry cost is the slope.
//! - `read_4k_in_file_of_{1m,64m}`: the extent lookup behind a random read.
//! - `write_4k_*`: an overwrite is copy-on-write (new block, tree update,
//!   checksum item); the `_nocow` row is the same write in place, which is
//!   the control: their difference is what copy-on-write costs per write.
//! - `fsync_4k` (the fsync log) against `sync_4k` (a full commit): what the
//!   log saves a database that fsyncs every write.
//! - `read_shared_*` against `read_*`: the `&self` path parallel readers
//!   take, against the exclusive one.
//!
//! The harness is the native `#[bench]` one (`test::Bencher`), for the
//! reasons given in `zircon-object/benches/vm.rs`. Rows that move file data
//! set `b.bytes`. Inputs and results go through `test::black_box`.
//!
//! Run:
//!
//! ```sh
//! cargo bench -p eclipsefs --features std --bench fs
//! cargo bench -p eclipsefs --features std --bench fs -- lookup   # one family
//! ```

#![feature(test)]

extern crate test;

use std::sync::{Arc, Mutex};

use eclipsefs::cowfmt::FL_NOCOW;
use eclipsefs::device::BlockDevice;
use eclipsefs::{mkfs, Compression, EclipseFS, Error, FileKind};
use test::{black_box, Bencher};

const MIB: usize = 1024 * 1024;
const BLOCK: usize = 4096;

/// A disk in RAM: what is measured is the driver, not a device.
struct Ram(Mutex<Vec<u8>>);

impl BlockDevice for Ram {
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

fn clock() -> (u64, u32) {
    (1_800_000_000, 0)
}

/// A mounted, empty filesystem of `mib` MiB.
fn fresh(mib: usize) -> (Arc<Ram>, EclipseFS) {
    let dev = Arc::new(Ram(Mutex::new(vec![0; mib * MIB])));
    let opts = mkfs::MkfsOptions {
        label: "bench".into(),
        fsid: [1; 16],
        chunk_uuid: [0; 16],
        dev_uuid: [0; 16],
        subvol_uuid: [0; 16],
        now: (1_700_000_000, 0),
    };
    mkfs::format(&*dev, &opts).unwrap();
    let mut fs = EclipseFS::mount(dev.clone(), false).unwrap();
    fs.set_clock(clock);
    (dev, fs)
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31) ^ seed)
        .collect()
}

/// A directory holding `n` empty files `f0..f{n-1}`, committed.
fn dir_of(fs: &mut EclipseFS, n: usize) -> u64 {
    let root = fs.root_ino();
    let dir = fs.create(root, "d", FileKind::Dir, 0o755, 0).unwrap();
    for i in 0..n {
        fs.create(dir, &format!("f{}", i), FileKind::Regular, 0o644, 0)
            .unwrap();
    }
    fs.sync().unwrap();
    dir
}

/// A committed file of `len` bytes.
fn file_of(fs: &mut EclipseFS, name: &str, len: usize) -> u64 {
    let f = fs
        .create(fs.root_ino(), name, FileKind::Regular, 0o644, 0)
        .unwrap();
    let chunk = pattern(MIB, 3);
    let mut at = 0;
    while at < len {
        let n = (len - at).min(MIB);
        fs.write(f, at as u64, &chunk[..n]).unwrap();
        at += n;
    }
    fs.sync().unwrap();
    f
}

/// Offsets of 4 KiB blocks spread over a file, in an order no readahead or
/// cache line could predict.
fn scattered(blocks: u64, n: usize) -> Vec<u64> {
    let mut x = 0x2545_F491u64;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % blocks) * BLOCK as u64
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Names: lookup, create, readdir, stat.
// ---------------------------------------------------------------------------

fn bench_lookup(b: &mut Bencher, n: usize) {
    let (_d, mut fs) = fresh(64);
    let dir = dir_of(&mut fs, n);
    let name = format!("f{}", n / 2);
    b.iter(|| black_box(fs.lookup(black_box(dir), black_box(&name)).unwrap()));
}

#[bench]
fn lookup_in_dir_of_1(b: &mut Bencher) {
    bench_lookup(b, 1);
}

#[bench]
fn lookup_in_dir_of_100(b: &mut Bencher) {
    bench_lookup(b, 100);
}

#[bench]
fn lookup_in_dir_of_10000(b: &mut Bencher) {
    bench_lookup(b, 10_000);
}

/// A failed lookup: every `open(O_CREAT)` of a new name and every `PATH`
/// entry an `exec` tries pays one.
#[bench]
fn lookup_miss_in_dir_of_10000(b: &mut Bencher) {
    let (_d, mut fs) = fresh(64);
    let dir = dir_of(&mut fs, 10_000);
    b.iter(|| black_box(fs.lookup(black_box(dir), black_box("absent")).err()));
}

/// Create and unlink one name, so the directory stays the size the row
/// says however many iterations run. The commits the driver makes on its
/// own every `COMMIT_OPS` changes are part of the figure, as they are of a
/// real `untar`.
fn bench_create_unlink(b: &mut Bencher, n: usize) {
    let (_d, mut fs) = fresh(128);
    let dir = dir_of(&mut fs, n);
    b.iter(|| {
        let f = fs.create(dir, "new", FileKind::Regular, 0o644, 0).unwrap();
        fs.unlink(dir, "new").unwrap();
        black_box(f)
    });
}

#[bench]
fn create_unlink_in_dir_of_1(b: &mut Bencher) {
    bench_create_unlink(b, 1);
}

#[bench]
fn create_unlink_in_dir_of_10000(b: &mut Bencher) {
    bench_create_unlink(b, 10_000);
}

/// `mkdir` + `rmdir`: a directory adds its own `..` bookkeeping.
#[bench]
fn mkdir_rmdir(b: &mut Bencher) {
    let (_d, mut fs) = fresh(64);
    let root = fs.root_ino();
    b.iter(|| {
        let d = fs.create(root, "sub", FileKind::Dir, 0o755, 0).unwrap();
        fs.unlink(root, "sub").unwrap();
        black_box(d)
    });
}

#[bench]
fn rename_back_and_forth(b: &mut Bencher) {
    let (_d, mut fs) = fresh(64);
    let dir = dir_of(&mut fs, 100);
    let mut flip = false;
    b.iter(|| {
        let (from, to) = if flip { ("g", "f0") } else { ("f0", "g") };
        flip = !flip;
        fs.rename(dir, from, dir, to).unwrap();
    });
}

fn bench_readdir(b: &mut Bencher, n: usize) {
    let (_d, mut fs) = fresh(64);
    let dir = dir_of(&mut fs, n);
    b.iter(|| black_box(fs.readdir(black_box(dir)).unwrap()));
}

#[bench]
fn readdir_10(b: &mut Bencher) {
    bench_readdir(b, 10);
}

#[bench]
fn readdir_1000(b: &mut Bencher) {
    bench_readdir(b, 1000);
}

#[bench]
fn stat(b: &mut Bencher) {
    let (_d, mut fs) = fresh(64);
    let dir = dir_of(&mut fs, 100);
    let f = fs.lookup(dir, "f50").unwrap();
    b.iter(|| black_box(fs.stat(black_box(f)).unwrap()));
}

/// `stat` through `&self`, as parallel `stat`s under the read lock do. The
/// first call warms the side cache; then this is the steady state.
#[bench]
fn stat_shared(b: &mut Bencher) {
    let (_d, mut fs) = fresh(64);
    let dir = dir_of(&mut fs, 100);
    let f = fs.lookup(dir, "f50").unwrap();
    assert!(fs.stat_shared(f).unwrap().is_some());
    b.iter(|| black_box(fs.stat_shared(black_box(f)).unwrap()));
}

#[bench]
fn set_and_get_xattr(b: &mut Bencher) {
    let (_d, mut fs) = fresh(64);
    let f = file_of(&mut fs, "x", 0);
    b.iter(|| {
        fs.set_xattr(f, "user.tag", black_box(b"value"), 0).unwrap();
        black_box(fs.get_xattr(f, "user.tag").unwrap())
    });
}

// ---------------------------------------------------------------------------
// Data: reads.
// ---------------------------------------------------------------------------

fn bench_read_4k(b: &mut Bencher, len: usize, shared: bool) {
    let (_d, mut fs) = fresh(len / MIB * 2 + 64);
    let f = file_of(&mut fs, "big", len);
    let offs = scattered((len / BLOCK) as u64, 4096);
    let mut buf = vec![0u8; BLOCK];
    let mut i = 0;
    b.bytes = BLOCK as u64;
    b.iter(|| {
        let off = offs[i % offs.len()];
        i += 1;
        let n = if shared {
            fs.read_shared(f, off, &mut buf)
                .unwrap()
                .expect("committed data")
        } else {
            fs.read(f, off, &mut buf).unwrap()
        };
        black_box((n, &buf));
    });
}

#[bench]
fn read_4k_in_file_of_1m(b: &mut Bencher) {
    bench_read_4k(b, MIB, false);
}

#[bench]
fn read_4k_in_file_of_64m(b: &mut Bencher) {
    bench_read_4k(b, 64 * MIB, false);
}

#[bench]
fn read_shared_4k_in_file_of_64m(b: &mut Bencher) {
    bench_read_4k(b, 64 * MIB, true);
}

/// Sequential reads of 1 MiB: checksums of 256 blocks and the copy out.
#[bench]
fn read_1m_sequential(b: &mut Bencher) {
    let len = 32 * MIB;
    let (_d, mut fs) = fresh(128);
    let f = file_of(&mut fs, "big", len);
    let mut buf = vec![0u8; MIB];
    let mut at = 0;
    b.bytes = MIB as u64;
    b.iter(|| {
        let n = fs.read(f, at as u64, &mut buf).unwrap();
        at = (at + MIB) % len;
        black_box((n, &buf));
    });
}

/// A file written with zstd: every read decompresses its extent (one-entry
/// cache aside). Against `read_1m_sequential`, the cost of compression on
/// the read side. The text repeats every 32 bytes, the best case for the
/// decoder; `unpack_zstd` in `benches/primitives.rs` is a harder one.
#[bench]
fn read_1m_zstd(b: &mut Bencher) {
    let len = 16 * MIB;
    let (_d, mut fs) = fresh(128);
    let f = fs
        .create(fs.root_ino(), "z", FileKind::Regular, 0o644, 0)
        .unwrap();
    fs.set_compression(f, Some(Compression::Zstd)).unwrap();
    let text: Vec<u8> = (0..MIB)
        .map(|i| b"compressible text, line by line\n"[i % 32])
        .collect();
    for i in 0..len / MIB {
        fs.write(f, (i * MIB) as u64, &text).unwrap();
    }
    fs.sync().unwrap();
    let c = fs.compsize(f).unwrap();
    assert!(
        c.compressed_extents > 0 && c.disk_bytes < len as u64 / 4,
        "the file is compressed: {:?}",
        c
    );
    let mut buf = vec![0u8; MIB];
    let mut at = 0;
    b.bytes = MIB as u64;
    b.iter(|| {
        let n = fs.read(f, at as u64, &mut buf).unwrap();
        at = (at + MIB) % len;
        black_box((n, &buf));
    });
}

// ---------------------------------------------------------------------------
// Data: writes and durability.
// ---------------------------------------------------------------------------

/// Overwrite one 4 KiB block somewhere in a 16 MiB file. Every write goes to
/// a new block; the old ones come back at each commit.
fn bench_write_4k(b: &mut Bencher, nocow: bool) {
    let len = 16 * MIB;
    let (_d, mut fs) = fresh(128);
    let f = fs
        .create(fs.root_ino(), "w", FileKind::Regular, 0o644, 0)
        .unwrap();
    if nocow {
        fs.set_flags(f, FL_NOCOW).unwrap();
    }
    let chunk = pattern(MIB, 1);
    for i in 0..len / MIB {
        fs.write(f, (i * MIB) as u64, &chunk).unwrap();
    }
    fs.sync().unwrap();
    let offs = scattered((len / BLOCK) as u64, 4096);
    let data = pattern(BLOCK, 9);
    let mut i = 0;
    b.bytes = BLOCK as u64;
    b.iter(|| {
        let off = offs[i % offs.len()];
        i += 1;
        black_box(fs.write(f, off, black_box(&data)).unwrap())
    });
}

#[bench]
fn write_4k_overwrite(b: &mut Bencher) {
    bench_write_4k(b, false);
}

#[bench]
fn write_4k_overwrite_nocow(b: &mut Bencher) {
    bench_write_4k(b, true);
}

/// Appending 1 MiB at a time, truncating back when the file reaches 32 MiB
/// so the disk never fills.
#[bench]
fn write_1m_append(b: &mut Bencher) {
    let (_d, mut fs) = fresh(128);
    let f = fs
        .create(fs.root_ino(), "a", FileKind::Regular, 0o644, 0)
        .unwrap();
    let data = pattern(MIB, 5);
    let mut at = 0u64;
    b.bytes = MIB as u64;
    b.iter(|| {
        if at == 32 * MIB as u64 {
            fs.truncate(f, 0).unwrap();
            fs.sync().unwrap();
            at = 0;
        }
        fs.write(f, at, black_box(&data)).unwrap();
        at += MIB as u64;
    });
}

/// One 4 KiB write followed by `fsync`: the fsync log writes one record and
/// flushes once instead of committing the transaction.
#[bench]
fn fsync_4k(b: &mut Bencher) {
    let (_d, mut fs) = fresh(128);
    let f = file_of(&mut fs, "db", 4 * MIB);
    let offs = scattered(1024, 4096);
    let data = pattern(BLOCK, 2);
    // The first `fsync` after mount commits to set up the log area; the
    // row measures the ones after it.
    fs.write(f, 0, &data).unwrap();
    fs.fsync(f).unwrap();
    let mut i = 0;
    b.iter(|| {
        let off = offs[i % offs.len()];
        i += 1;
        fs.write(f, off, &data).unwrap();
        fs.fsync(black_box(f)).unwrap();
    });
    assert!(fs.fsync_log_records() > 0, "fsync went to the log");
}

/// The control for `fsync_4k`: the same write followed by a full commit.
#[bench]
fn sync_4k(b: &mut Bencher) {
    let (_d, mut fs) = fresh(128);
    let f = file_of(&mut fs, "db", 4 * MIB);
    let offs = scattered(1024, 4096);
    let data = pattern(BLOCK, 2);
    let mut i = 0;
    b.iter(|| {
        let off = offs[i % offs.len()];
        i += 1;
        fs.write(f, off, &data).unwrap();
        fs.sync().unwrap();
    });
}

/// `RWF_ATOMIC` of one block: all of it or none of it after a crash.
#[bench]
fn write_atomic_4k(b: &mut Bencher) {
    let (_d, mut fs) = fresh(128);
    let f = file_of(&mut fs, "atomic", 4 * MIB);
    let offs = scattered(1024, 4096);
    let data = pattern(BLOCK, 4);
    let mut i = 0;
    b.bytes = BLOCK as u64;
    b.iter(|| {
        let off = offs[i % offs.len()];
        i += 1;
        black_box(fs.write_atomic(f, off, &data).unwrap())
    });
}

// ---------------------------------------------------------------------------
// Whole-filesystem operations.
// ---------------------------------------------------------------------------

/// A snapshot shares the tree root, so it should cost a few nodes whatever
/// the subvolume holds. Taken and deleted, so the row stays steady.
#[bench]
fn snapshot_and_delete_of_1000_files(b: &mut Bencher) {
    let (_d, mut fs) = fresh(128);
    dir_of(&mut fs, 1000);
    let root = fs.root_ino();
    b.iter(|| {
        let s = fs.snapshot(root, root, "snap", true).unwrap();
        fs.delete_subvolume(root, "snap").unwrap();
        black_box(s)
    });
}

/// Mounting: superblock, journal replay check, the trees' roots. What every
/// boot pays before `/` exists.
#[bench]
fn mount_with_1000_files(b: &mut Bencher) {
    let (dev, mut fs) = fresh(64);
    dir_of(&mut fs, 1000);
    drop(fs);
    b.iter(|| black_box(EclipseFS::mount(dev.clone(), true).unwrap()));
}

/// `fsck` of a small tree: read every node, account for every block.
#[bench]
fn check_with_1000_files(b: &mut Bencher) {
    let (_d, mut fs) = fresh(64);
    dir_of(&mut fs, 1000);
    b.iter(|| black_box(fs.check().unwrap()));
}

/// The commit itself, after one small change: nodes out, journal, flushes.
#[bench]
fn commit_after_one_change(b: &mut Bencher) {
    let (_d, mut fs) = fresh(64);
    let f = file_of(&mut fs, "c", 0);
    let mut mode = 0o600;
    b.iter(|| {
        mode ^= 0o044;
        fs.set_attr(f, Some(mode), None, None, None, None).unwrap();
        fs.sync().unwrap();
    });
}
