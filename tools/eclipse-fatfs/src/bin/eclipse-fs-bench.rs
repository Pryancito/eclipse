//! `eclipse-fs-bench DIR [BIG]`: time what a program does with the files
//! under `DIR`, the first time and again, on the clock of the machine it
//! runs in. What a filesystem is measured with: the same command on the
//! same volume mounted two ways says what one way costs over the other.
//!
//! The tree under `DIR` is walked (`readdir` and `stat` of everything),
//! every file under 1 MiB is read whole, and `BIG` (a path under `DIR`,
//! `big.bin` when not given) is read in order through 64 KiB and 4 KiB
//! buffers, at 500 scattered offsets, and through a mapping. Each line is
//! one of those: how many, the bytes, the time, and the time for each.
//!
//! `eclipse-fs-bench -w DIR` times writing instead, in a directory it
//! makes under `DIR` and removes: 200 files of 4 KiB made, a 2 MiB file
//! written in order through 64 KiB and 4 KiB buffers, written over, written
//! at 500 scattered offsets, synced and read back, then the files renamed
//! and removed.
//!
//! `eclipse-fs-bench -d DEVICE` times the disk under a filesystem: 8 MiB
//! of the device, 1 MiB in, read through 1 MiB, 64 KiB and 4 KiB buffers
//! and written back as read -- the device is left as it was -- in order and
//! at 500 scattered places, with an `fsync` after each pass of writes.
//! `eclipse-fs-bench -d DEVICE 1M` (or `64k`, `4k`) keeps to the passes of
//! that size, for a profile of one of them.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn die(why: impl std::fmt::Display) -> ! {
    eprintln!("eclipse-fs-bench: {why}");
    std::process::exit(1)
}

/// One line: `what`, the `n` things done, the `bytes` moved, in `us`.
fn line(what: &str, n: u64, bytes: u64, us: u128) {
    let each = us as f64 / n.max(1) as f64;
    let rate = match (bytes, us) {
        (0, _) | (_, 0) => String::new(),
        (bytes, us) => format!("  {:.1} MB/s", bytes as f64 / us as f64),
    };
    println!("{what:<22} n={n:<6} bytes={bytes:<10} us={us:<9} each={each:.1}us{rate}");
}

/// Every file under `dir` with its size, after a `stat` of each entry.
fn walk(dir: &Path, files: &mut Vec<(PathBuf, u64)>, seen: &mut u64) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| die(format_args!("{}: {e}", dir.display())));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| die(format_args!("{}: {e}", dir.display())));
        let path = entry.path();
        let meta =
            fs::metadata(&path).unwrap_or_else(|e| die(format_args!("{}: {e}", path.display())));
        *seen += 1;
        if meta.is_dir() {
            walk(&path, files, seen);
        } else {
            files.push((path, meta.len()));
        }
    }
}

/// Read `file` from its start to its end through a buffer of `buf` bytes:
/// the bytes, and their sum, which is what says two runs read the same.
fn read_in_order(path: &Path, buf: usize) -> (u64, u64) {
    let mut file =
        File::open(path).unwrap_or_else(|e| die(format_args!("{}: {e}", path.display())));
    let mut buf = vec![0u8; buf];
    let (mut bytes, mut sum) = (0u64, 0u64);
    loop {
        match file.read(&mut buf) {
            Ok(0) => return (bytes, sum),
            Ok(n) => {
                bytes += n as u64;
                sum = buf[..n].iter().fold(sum, |s, b| s.wrapping_add(*b as u64));
            }
            Err(e) => die(format_args!("{}: {e}", path.display())),
        }
    }
}

fn timed<T>(f: impl FnOnce() -> T) -> (T, u128) {
    let start = Instant::now();
    let out = f();
    (out, start.elapsed().as_micros())
}

/// The byte at `i` of what is written: so that what is read back says
/// whether it is what was written.
fn pattern(from: u64, len: usize) -> Vec<u8> {
    (from..from + len as u64)
        .map(|i| (i ^ (i >> 9) ^ (i >> 17)) as u8)
        .collect()
}

/// `-w`: time writing under `dir`.
fn writing(dir: &Path) {
    const SMALL: u64 = 200;
    const BIG: usize = 2 << 20;
    let top = dir.join("fs-bench.tmp");
    let _ = fs::remove_dir_all(&top);
    let ok =
        |what: &str, r: std::io::Result<()>| r.unwrap_or_else(|e| die(format_args!("{what}: {e}")));
    ok("mkdir", fs::create_dir(&top));
    let small = |i: u64, to: &str| top.join(format!("{to}-{i:03}.dat"));

    let piece = pattern(0, 4096);
    let ((), us) = timed(|| {
        for i in 0..SMALL {
            let mut file = File::create(small(i, "small")).unwrap_or_else(|e| die(e));
            ok("write", file.write_all(&piece));
        }
    });
    line("make small files", SMALL, SMALL * 4096, us);

    let whole = pattern(0, BIG);
    for (pass, name, buf) in [
        ("write 64k new", "big-64k.bin", 64 * 1024),
        ("write 4k new", "big-4k.bin", 4096),
        ("write 64k over", "big-64k.bin", 64 * 1024),
    ] {
        let path = top.join(name);
        let ((), us) = timed(|| {
            // Over what is there, without cutting it first: `over` is a
            // write that needs no new room.
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .unwrap_or_else(|e| die(e));
            for chunk in whole.chunks(buf) {
                ok("write", file.write_all(chunk));
            }
        });
        line(pass, (BIG / buf) as u64, BIG as u64, us);
    }

    let big = top.join("big-64k.bin");
    let mut expect = whole.clone();
    {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&big)
            .unwrap_or_else(|e| die(e));
        let mut at = 12345u64;
        let ((), us) = timed(|| {
            for i in 0..500u64 {
                at = at
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let offset = (at >> 20) % (BIG as u64 - 4096);
                let bytes = pattern(i * 7919, 4096);
                ok("pwrite", file.write_all_at(&bytes, offset));
                expect[offset as usize..offset as usize + 4096].copy_from_slice(&bytes);
            }
        });
        line("pwrite 4k", 500, 500 * 4096, us);
        let ((), us) = timed(|| ok("fsync", file.sync_all()));
        line("fsync", 1, 0, us);
    }

    let (back, us) = timed(|| fs::read(&big).unwrap_or_else(|e| die(e)));
    line("read back", 1, back.len() as u64, us);
    let small_ok = (0..SMALL).all(|i| fs::read(small(i, "small")).is_ok_and(|b| b == piece));
    let four_k = fs::read(top.join("big-4k.bin")).is_ok_and(|b| b == whole);
    println!(
        "read back: big {} 4k {} small {}",
        if back == expect { "same" } else { "DIFFERENT" },
        if four_k { "same" } else { "DIFFERENT" },
        if small_ok { "same" } else { "DIFFERENT" },
    );

    let ((), us) = timed(|| {
        for i in 0..SMALL {
            ok("rename", fs::rename(small(i, "small"), small(i, "moved")));
        }
    });
    line("rename", SMALL, 0, us);
    let ((), us) = timed(|| {
        for i in 0..SMALL {
            ok("unlink", fs::remove_file(small(i, "moved")));
        }
    });
    line("unlink", SMALL, 0, us);
    ok("remove", fs::remove_dir_all(&top));
}

/// `-d`: time the device at `path`, writing back only what was read.
fn device(path: &Path, only: Option<&str>) {
    const AT: u64 = 1 << 20;
    const SPAN: usize = 8 << 20;
    let ok =
        |what: &str, r: std::io::Result<()>| r.unwrap_or_else(|e| die(format_args!("{what}: {e}")));
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap_or_else(|e| die(format_args!("{}: {e}", path.display())));
    let mut held = vec![0u8; SPAN];
    let sizes = [("1M", 1 << 20), ("64k", 64 << 10), ("4k", 4096)];
    let sizes: Vec<(&str, usize)> = sizes
        .into_iter()
        .filter(|(name, _)| only.is_none_or(|only| only == *name))
        .collect();
    if sizes.is_empty() {
        die("the size is one of 1M, 64k, 4k");
    }
    for &(name, buf) in &sizes {
        let ((), us) = timed(|| {
            for (i, chunk) in held.chunks_mut(buf).enumerate() {
                ok("pread", file.read_exact_at(chunk, AT + (i * buf) as u64));
            }
        });
        line(
            &format!("dev read {name}"),
            (SPAN / buf) as u64,
            SPAN as u64,
            us,
        );
    }
    let sum = held.iter().fold(0u64, |s, &b| s.wrapping_add(b as u64));
    for &(name, buf) in &sizes {
        let ((), us) = timed(|| {
            for (i, chunk) in held.chunks(buf).enumerate() {
                ok("pwrite", file.write_all_at(chunk, AT + (i * buf) as u64));
            }
        });
        line(
            &format!("dev write {name}"),
            (SPAN / buf) as u64,
            SPAN as u64,
            us,
        );
        let ((), us) = timed(|| ok("fsync", file.sync_all()));
        line(&format!("dev fsync after {name}"), 1, 0, us);
    }
    let mut at = 12345u64;
    let scattered = if only.is_none_or(|only| only == "4k") {
        500
    } else {
        0
    };
    let ((), us) = timed(|| {
        for _ in 0..scattered {
            at = at
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let off = ((at >> 20) % (SPAN as u64 - 4096)) / 512 * 512;
            let piece = &held[off as usize..off as usize + 4096];
            ok("pwrite", file.write_all_at(piece, AT + off));
        }
    });
    if scattered != 0 {
        line("dev pwrite 4k", scattered, scattered * 4096, us);
        let ((), us) = timed(|| ok("fsync", file.sync_all()));
        line("dev fsync", 1, 0, us);
    }
    let mut back = vec![0u8; SPAN];
    ok("pread", file.read_exact_at(&mut back, AT));
    println!(
        "device: sum {sum:x}, read back {}",
        if back == held { "same" } else { "DIFFERENT" }
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [flag, dir] = args.as_slice() {
        if flag == "-w" {
            return writing(Path::new(dir));
        }
        if flag == "-d" {
            return device(Path::new(dir), None);
        }
    }
    if let [flag, dir, only] = args.as_slice() {
        if flag == "-d" {
            return device(Path::new(dir), Some(only));
        }
    }
    let Some(dir) = args.first() else {
        eprintln!(
            "usage: eclipse-fs-bench DIR [BIG]\n       eclipse-fs-bench -w DIR\n       eclipse-fs-bench -d DEVICE [1M|64k|4k]"
        );
        std::process::exit(2)
    };
    let dir = Path::new(dir);
    let big = dir.join(args.get(1).map_or("big.bin", |s| s.as_str()));

    // The tree: every name and its `stat`, the first time and again.
    let mut files = Vec::new();
    for pass in ["walk first", "walk again"] {
        files.clear();
        let mut seen = 0;
        let ((), us) = timed(|| walk(dir, &mut files, &mut seen));
        line(pass, seen, 0, us);
    }

    // The small files, whole.
    let small: Vec<&(PathBuf, u64)> = files.iter().filter(|(_, len)| *len < 1 << 20).collect();
    let mut sums = Vec::new();
    for pass in ["small files first", "small files again"] {
        let ((bytes, sum), us) = timed(|| {
            small.iter().fold((0, 0u64), |(bytes, sum), (path, _)| {
                let (b, s) = read_in_order(path, 16 * 1024);
                (bytes + b, sum.wrapping_add(s))
            })
        });
        line(pass, small.len() as u64, bytes, us);
        sums.push(sum);
    }

    // Opening and closing one file, which is the lookup of its path.
    if let Some((path, _)) = small.first() {
        let ((), us) = timed(|| {
            for _ in 0..500 {
                drop(File::open(path).unwrap_or_else(|e| die(e)));
            }
        });
        line("open+close", 500, 0, us);
        let ((), us) = timed(|| {
            for _ in 0..500 {
                fs::metadata(path).unwrap_or_else(|e| die(e));
            }
        });
        line("stat", 500, 0, us);
    }

    // The big file.
    let len = fs::metadata(&big)
        .unwrap_or_else(|e| die(format_args!("{}: {e}", big.display())))
        .len();
    for (pass, buf) in [
        ("big 64k first", 64 * 1024),
        ("big 64k again", 64 * 1024),
        ("big 4k again", 4096),
    ] {
        let ((bytes, sum), us) = timed(|| read_in_order(&big, buf));
        line(pass, bytes.div_ceil(buf as u64), bytes, us);
        sums.push(sum);
    }
    {
        let file = File::open(&big).unwrap_or_else(|e| die(e));
        let mut buf = [0u8; 4096];
        let mut at = 12345u64;
        let ((), us) = timed(|| {
            for _ in 0..500 {
                // A walk over the file that no read-ahead follows.
                at = at
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let offset = (at >> 20) % len.saturating_sub(4096).max(1);
                file.read_exact_at(&mut buf, offset)
                    .unwrap_or_else(|e| die(e));
            }
        });
        line("big pread 4k", 500, 500 * 4096, us);
    }
    {
        let file = File::open(&big).unwrap_or_else(|e| die(e));
        let ((sum, bytes), us) = timed(|| {
            // SAFETY: a private, read-only mapping of the whole file, read
            // below within its length and unmapped at the end.
            unsafe {
                let p = libc::mmap(
                    std::ptr::null_mut(),
                    len as usize,
                    libc::PROT_READ,
                    libc::MAP_PRIVATE,
                    file.as_raw_fd(),
                    0,
                );
                if p == libc::MAP_FAILED {
                    die(format_args!("mmap: {}", std::io::Error::last_os_error()));
                }
                let bytes = std::slice::from_raw_parts(p as *const u8, len as usize);
                let sum = bytes.iter().fold(0u64, |s, b| s.wrapping_add(*b as u64));
                libc::munmap(p, len as usize);
                (sum, len)
            }
        });
        line("big mmap", bytes.div_ceil(4096), bytes, us);
        sums.push(sum);
    }
    println!("sums {sums:x?}");
}
