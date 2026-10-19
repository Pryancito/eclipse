#![cfg(feature = "std")]
//! Shared reads (`read_shared`, `stat_shared`): through `&self`, so that
//! readers holding the filesystem shared run side by side. Each answers
//! exactly what the exclusive call would, or `None` to send the caller to
//! it.

mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use common::*;
use eclipsefs::cowfmt::{FALLOC_FL_KEEP_SIZE, FALLOC_FL_PUNCH_HOLE, FL_NOCOW};
use eclipsefs::device::BlockDevice;
use eclipsefs::{Compression, EclipseFS, EncryptionPolicy, Error, FileKind};

/// What the exclusive read gives for `len` bytes at `off`.
fn exclusive(fs: &mut EclipseFS, f: u64, off: u64, len: usize) -> Result<Vec<u8>, Error> {
    let mut buf = vec![0u8; len];
    let n = fs.read(f, off, &mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

/// What the shared read gives, `None` when it hands over.
fn shared(fs: &EclipseFS, f: u64, off: u64, len: usize) -> Result<Option<Vec<u8>>, Error> {
    let mut buf = vec![0xEEu8; len];
    Ok(fs.read_shared(f, off, &mut buf)?.map(|n| {
        buf.truncate(n);
        buf
    }))
}

/// The shared read of every probe matches the exclusive one.
fn same_everywhere(fs: &mut EclipseFS, f: u64, seed: u64) {
    let size = fs.stat(f).unwrap().size;
    let mut rng = Lcg(seed);
    let mut probes = vec![(0, size as usize + 10), (size, 10), (size + 4096, 1)];
    for _ in 0..40 {
        let off = rng.below(size + 1);
        let len = 1 + rng.below(3 * BLOCK as u64) as usize;
        probes.push((off, len));
    }
    for (off, len) in probes {
        let want = exclusive(fs, f, off, len).unwrap();
        let got = shared(fs, f, off, len).unwrap();
        assert_eq!(got.as_deref(), Some(&want[..]), "read of {len} at {off}");
    }
    let st = fs.stat(f).unwrap();
    assert_eq!(
        format!("{:?}", fs.stat_shared(f).unwrap()),
        format!("{:?}", Some(st))
    );
}

#[test]
fn shared_reads_match_exclusive_reads() {
    let (disk, mut fs) = fresh(64 * MIB);
    let root = fs.root_ino();
    let mut files = Vec::new();
    for (i, len) in [0, 1, 100, 3000, 4096, 5000, 70_000, 1_500_000]
        .into_iter()
        .enumerate()
    {
        let f = fs
            .create(root, &format!("f{i}"), FileKind::Regular, 0o644, 0)
            .unwrap();
        write_all(&mut fs, f, 0, &pattern(len, i));
        files.push(f);
    }
    // Holes, a preallocated range and a punched one.
    let h = fs
        .create(root, "holes", FileKind::Regular, 0o644, 0)
        .unwrap();
    write_all(&mut fs, h, 3 * MIB as u64, &pattern(9000, 9));
    write_all(&mut fs, h, 10_000, &pattern(20_000, 10));
    fs.fallocate(h, FALLOC_FL_KEEP_SIZE, 5 * MIB as u64, MIB as u64)
        .unwrap();
    fs.fallocate(h, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 12_288, 8192)
        .unwrap();
    files.push(h);
    // No checksums.
    let n = fs
        .create(root, "nocow", FileKind::Regular, 0o644, 0)
        .unwrap();
    fs.set_flags(n, FL_NOCOW).unwrap();
    write_all(&mut fs, n, 0, &pattern(40_000, 11));
    files.push(n);

    // Uncommitted: the open transaction's nodes and pending checksums.
    for (i, &f) in files.iter().enumerate() {
        same_everywhere(&mut fs, f, i as u64);
    }
    // Committed, cache warm.
    fs.sync().unwrap();
    for (i, &f) in files.iter().enumerate() {
        same_everywhere(&mut fs, f, 100 + i as u64);
    }
    // Cold: every node from the device, through the side cache.
    let mut fs = remount(&disk, fs);
    for (i, &f) in files.iter().enumerate() {
        assert!(fs.stat_shared(f).unwrap().is_some());
        same_everywhere(&mut fs, f, 200 + i as u64);
    }
    fs.drop_caches();
    for (i, &f) in files.iter().enumerate() {
        same_everywhere(&mut fs, f, 300 + i as u64);
    }
    fs.check().unwrap();
}

#[test]
fn errors_match_too() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let mut buf = [0u8; 10];
    assert_eq!(fs.read_shared(root, 0, &mut buf), Err(Error::IsDir));
    assert_eq!(fs.read(root, 0, &mut buf), Err(Error::IsDir));
    assert_eq!(fs.read_shared(999_999, 0, &mut buf), Err(Error::NotFound));
    assert!(matches!(fs.stat_shared(999_999), Err(Error::NotFound)));
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    assert_eq!(fs.read_shared(f, 0, &mut []), Ok(Some(0)));
    let l = fs.symlink(root, "l", b"target").unwrap();
    assert_eq!(
        exclusive(&mut fs, l, 0, 10).map(Some),
        shared(&fs, l, 0, 10)
    );
}

#[test]
fn compressed_files_go_to_the_exclusive_path() {
    let (disk, mut fs) = fresh(32 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "z", FileKind::Regular, 0o644, 0).unwrap();
    fs.set_compression(f, Some(Compression::Zlib)).unwrap();
    let text: Vec<u8> = (0..300_000).map(|i| b"abcabcabd"[i % 9]).collect();
    write_all(&mut fs, f, 0, &text);
    let mut fs = remount(&disk, fs);
    assert_eq!(shared(&fs, f, 0, 1000).unwrap(), None);
    assert_eq!(exclusive(&mut fs, f, 0, 1000).unwrap(), &text[..1000]);
    // Past the end needs no extent.
    assert_eq!(shared(&fs, f, 400_000, 10).unwrap(), Some(Vec::new()));
}

#[test]
fn encrypted_files_read_shared_once_their_keys_are_derived() {
    fn random(buf: &mut [u8]) {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        for b in buf {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            *b = (n.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as u8;
        }
    }
    let (disk, mut fs) = fresh(32 * MIB);
    fs.set_random(random);
    let id = fs.add_encryption_key(&[0x42; 64]).unwrap();
    let root = fs.root_ino();
    let dir = fs.create(root, "vault", FileKind::Dir, 0o700, 0).unwrap();
    fs.set_encryption_policy(dir, &EncryptionPolicy::new(id))
        .unwrap();
    let f = fs
        .create(dir, "secret", FileKind::Regular, 0o600, 0)
        .unwrap();
    let data = pattern(50_000, 3);
    write_all(&mut fs, f, 0, &data);
    let mut fs = remount(&disk, fs);
    fs.set_random(random);
    fs.add_encryption_key(&[0x42; 64]).unwrap();
    assert_eq!(
        shared(&fs, f, 0, 100).unwrap(),
        None,
        "keys not derived yet"
    );
    assert_eq!(read_all(&mut fs, f), data);
    assert_eq!(shared(&fs, f, 0, 50_000).unwrap(), Some(data.clone()));
    assert_eq!(
        shared(&fs, f, 4000, 300).unwrap(),
        Some(data[4000..4300].to_vec())
    );
}

#[test]
fn a_rotten_block_goes_to_the_exclusive_path() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    let mut rng = Lcg(5);
    let data: Vec<u8> = (0..20_000).map(|_| rng.next() as u8).collect();
    write_all(&mut fs, f, 0, &data);
    let fs = remount(&disk, fs);
    let mut image = vec![0u8; disk.size() as usize];
    disk.read_at(0, &mut image).unwrap();
    let at = image
        .chunks(BLOCK)
        .position(|b| b == &data[4096..8192])
        .unwrap();
    disk.poke(at * BLOCK + 7, data[4096 + 7] ^ 0xFF);
    let mut fs = fs;
    // The block's neighbours still read shared.
    assert_eq!(
        shared(&fs, f, 0, 4096).unwrap(),
        Some(data[..4096].to_vec())
    );
    assert_eq!(shared(&fs, f, 5000, 100).unwrap(), None);
    assert!(matches!(
        fs.read(f, 5000, &mut [0u8; 100]),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn subvolumes_read_shared_once_loaded() {
    let (_disk, mut fs) = fresh(32 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, f, 0, &pattern(30_000, 1));
    let snap = fs.snapshot(root, root, "snap", false).unwrap();
    let sf = fs.lookup(snap, "f").unwrap();
    write_all(&mut fs, f, 0, &pattern(30_000, 2));
    // Both trees are loaded now; each reads its own version.
    assert_eq!(shared(&fs, f, 0, 30_000).unwrap(), Some(pattern(30_000, 2)));
    assert_eq!(
        shared(&fs, sf, 0, 30_000).unwrap(),
        Some(pattern(30_000, 1))
    );
    same_everywhere(&mut fs, sf, 1);
    same_everywhere(&mut fs, f, 2);
}

/// Blocks freed and reused while the side cache holds what they had: the
/// generation check sends the read to the device.
#[test]
fn the_side_cache_never_serves_a_reused_block() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let mut files = Vec::new();
    for i in 0..200 {
        let f = fs
            .create(root, &format!("f{i}"), FileKind::Regular, 0o644, 0)
            .unwrap();
        write_all(&mut fs, f, 0, &pattern(5000 + i * 37, i));
        files.push(f);
    }
    let mut fs = remount(&disk, fs);
    let mut rng = Lcg(77);
    for round in 0..30 {
        // Fill the side cache from the device.
        fs.drop_caches();
        for &f in &files {
            let len = fs.stat_shared(f).unwrap().unwrap().size as usize;
            assert!(shared(&fs, f, 0, len).unwrap().is_some());
        }
        // Rewrite a few, commit (the old nodes are freed and reused).
        for _ in 0..10 {
            let i = rng.below(files.len() as u64) as usize;
            let f = files[i];
            let len = 3000 + rng.below(9000) as usize;
            fs.truncate(f, 0).unwrap();
            write_all(&mut fs, f, 0, &pattern(len, round * 1000 + i));
        }
        fs.sync().unwrap();
        fs.drop_caches();
        for (i, &f) in files.iter().enumerate() {
            let want = read_all(&mut fs, f);
            fs.drop_caches();
            assert_eq!(
                shared(&fs, f, 0, want.len() + 1).unwrap(),
                Some(want),
                "round {round} file {i}"
            );
        }
    }
    fs.check().unwrap();
}

/// Readers under a shared lock while a writer rewrites the same files under
/// the exclusive one: every read sees one whole version, and they ran side
/// by side.
#[test]
fn readers_run_side_by_side_with_a_writer() {
    const FILES: usize = 8;
    const LEN: usize = 60_000;
    let (_disk, mut fs) = fresh(64 * MIB);
    let root = fs.root_ino();
    let mut files = Vec::new();
    for i in 0..FILES {
        let f = fs
            .create(root, &format!("f{i}"), FileKind::Regular, 0o644, 0)
            .unwrap();
        write_all(&mut fs, f, 0, &version(i, 0));
        files.push(f);
    }
    fn version(file: usize, v: usize) -> Vec<u8> {
        let mut d = pattern(LEN, file * 1000 + v);
        d[..8].copy_from_slice(&(v as u64).to_le_bytes());
        d
    }
    let fs = Arc::new(RwLock::new(fs));
    let stop = Arc::new(AtomicBool::new(false));
    let together = Arc::new(AtomicU64::new(0));
    let inside = Arc::new(AtomicU64::new(0));
    let readers: Vec<_> = (0..4)
        .map(|t| {
            let (fs, stop, files) = (fs.clone(), stop.clone(), files.clone());
            let (together, inside) = (together.clone(), inside.clone());
            std::thread::spawn(move || {
                let mut rng = Lcg(t);
                let mut reads = 0u64;
                while !stop.load(Ordering::Relaxed) || reads < 50 {
                    let i = rng.below(FILES as u64) as usize;
                    let g = fs.read().unwrap();
                    if inside.fetch_add(1, Ordering::SeqCst) > 0 {
                        together.fetch_add(1, Ordering::Relaxed);
                    }
                    let mut buf = vec![0u8; LEN];
                    let got = g.read_shared(files[i], 0, &mut buf).unwrap();
                    inside.fetch_sub(1, Ordering::SeqCst);
                    drop(g);
                    assert_eq!(got, Some(LEN));
                    let v = u64::from_le_bytes(buf[..8].try_into().unwrap()) as usize;
                    assert!(buf == version(i, v), "file {i} torn at version {v}");
                    reads += 1;
                }
                reads
            })
        })
        .collect();
    for v in 1..=60 {
        let i = v % FILES;
        let mut g = fs.write().unwrap();
        write_all(&mut g, files[i], 0, &version(i, v));
        if v % 7 == 0 {
            g.sync().unwrap();
        }
        if v % 11 == 0 {
            g.drop_caches();
        }
        drop(g);
        std::thread::yield_now();
    }
    stop.store(true, Ordering::Relaxed);
    let reads: u64 = readers.into_iter().map(|r| r.join().unwrap()).sum();
    assert!(reads >= 200);
    assert!(
        together.load(Ordering::Relaxed) > 0,
        "no two reads ever overlapped"
    );
    fs.write().unwrap().check().unwrap();
}
