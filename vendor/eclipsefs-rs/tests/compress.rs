#![cfg(feature = "std")]
//! Transparent compression (format v0.4): zlib, LZ4 and zstd extents.

mod common;

use common::*;
use eclipsefs::compress::{pack, ZCHUNK};
use eclipsefs::cowfmt::{FALLOC_FL_KEEP_SIZE, FALLOC_FL_PUNCH_HOLE, FL_COMPR, FL_NOCOMP, FL_NOCOW};
use eclipsefs::device::BlockDevice;
use eclipsefs::{Compression, EclipseFS, Error, FileKind};

const ALL: [Compression; 3] = Compression::ALL;

/// Compressible bytes that differ from block to block.
fn text(len: usize, seed: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len + 64);
    let mut i = 0;
    while v.len() < len {
        v.extend_from_slice(format!("{} record {} of the log, status ok\n", seed, i).as_bytes());
        i += 1;
    }
    v.truncate(len);
    v
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = Lcg(seed);
    (0..len).map(|_| rng.next() as u8).collect()
}

fn file(fs: &mut EclipseFS, dir: u64, name: &str, data: &[u8]) -> u64 {
    let f = fs.create(dir, name, FileKind::Regular, 0o644, 0).unwrap();
    write_all(fs, f, 0, data);
    f
}

fn clean(fs: &mut EclipseFS) {
    fs.check().unwrap();
    let r = fs.scrub().unwrap();
    assert!(r.is_clean(), "{:?}", r);
}

fn used(fs: &mut EclipseFS) -> u64 {
    fs.sync().unwrap();
    fs.fsinfo().bytes_used
}

/// A fresh filesystem mounted with `compress=algo`.
fn compressed(size: usize, algo: Compression) -> (std::sync::Arc<MemDisk>, EclipseFS, u64) {
    let (disk, mut fs) = fresh(size);
    fs.set_mount_compression(Some(algo)).unwrap();
    let root = fs.root_ino();
    (disk, fs, root)
}

#[test]
fn every_algorithm_saves_space_and_reads_back() {
    for algo in ALL {
        let (disk, mut fs) = fresh(32 * MIB);
        let root = fs.root_ino();
        let before = used(&mut fs);
        fs.set_mount_compression(Some(algo)).unwrap();
        let data = text(2 * MIB + 1234, 1);
        let f = file(&mut fs, root, "log", &data);
        let after = used(&mut fs);
        assert!(
            after - before < data.len() as u64 / 3,
            "{:?}: {} bytes used for {}",
            algo,
            after - before,
            data.len()
        );
        let c = fs.compsize(f).unwrap();
        assert!(
            c.disk_bytes * 3 < c.uncompressed_bytes,
            "{:?} {:?}",
            algo,
            c
        );
        assert_eq!(
            c.uncompressed_bytes,
            (data.len() as u64).div_ceil(4096) * 4096
        );
        assert_eq!(fs.stat(f).unwrap().nbytes, c.uncompressed_bytes);
        assert_eq!(read_all(&mut fs, f), data);
        clean(&mut fs);
        // Reading needs no mount option.
        let mut fs = remount(&disk, fs);
        assert_eq!(read_all(&mut fs, f), data);
        let mut buf = vec![0u8; 10_000];
        fs.read(f, 300_001, &mut buf).unwrap();
        assert_eq!(buf, data[300_001..310_001]);
        clean(&mut fs);
    }
}

#[test]
fn incompressible_data_is_stored_as_it_is() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    fs.set_mount_compression(Some(Compression::Zstd)).unwrap();
    let data = noise(500_000, 3);
    let f = file(&mut fs, root, "random", &data);
    let c = fs.compsize(f).unwrap();
    assert_eq!(c.compressed_extents, 0);
    assert_eq!(c.disk_bytes, c.uncompressed_bytes);
    assert_eq!(read_all(&mut fs, f), data);
    // Half and half: only the compressible half is.
    let mut mixed = text(256 * 1024, 4);
    mixed.extend_from_slice(&noise(256 * 1024, 5));
    let g = file(&mut fs, root, "mixed", &mixed);
    let c = fs.compsize(g).unwrap();
    assert_eq!(c.compressed_extents, 2);
    assert!(c.disk_bytes < 256 * 1024 + 64 * 1024, "{:?}", c);
    assert_eq!(read_all(&mut fs, g), mixed);
    clean(&mut fs);
}

#[test]
fn the_compression_property_is_per_file_and_inherited() {
    let (disk, mut fs) = fresh(32 * MIB);
    let root = fs.root_ino();
    assert_eq!(fs.compression(root).unwrap(), None);
    let d = fs.create(root, "logs", FileKind::Dir, 0o755, 0).unwrap();
    fs.set_compression(d, Some(Compression::Lz4)).unwrap();
    assert_eq!(fs.compression(d).unwrap(), Some(Compression::Lz4));
    assert_ne!(fs.get_flags(d).unwrap() & FL_COMPR, 0);
    let sub = fs.create(d, "sub", FileKind::Dir, 0o755, 0).unwrap();
    let data = text(400_000, 6);
    let f = file(&mut fs, sub, "a.log", &data);
    assert_eq!(fs.compression(f).unwrap(), Some(Compression::Lz4));
    assert!(fs.compsize(f).unwrap().compressed_extents > 0);
    // Elsewhere nothing is compressed.
    let g = file(&mut fs, root, "plain", &data);
    assert_eq!(fs.compsize(g).unwrap().compressed_extents, 0);
    // The property survives a remount; clearing it stops compression of
    // new data only.
    let mut fs = remount(&disk, fs);
    assert_eq!(fs.compression(f).unwrap(), Some(Compression::Lz4));
    fs.set_compression(f, None).unwrap();
    assert_eq!(fs.compression(f).unwrap(), None);
    assert_eq!(fs.get_flags(f).unwrap() & FL_COMPR, 0);
    let before = fs.compsize(f).unwrap().compressed_extents;
    write_all(&mut fs, f, data.len() as u64, &data);
    assert_eq!(fs.compsize(f).unwrap().compressed_extents, before);
    let mut want = data.clone();
    want.extend_from_slice(&data);
    assert_eq!(read_all(&mut fs, f), want);
    clean(&mut fs);
}

#[test]
fn chattr_c_compresses_with_zlib_and_nocompress_wins_over_the_mount_option() {
    let (_disk, mut fs) = fresh(32 * MIB);
    let root = fs.root_ino();
    let data = text(300_000, 7);
    let f = fs.create(root, "c", FileKind::Regular, 0o644, 0).unwrap();
    fs.set_flags(f, FL_COMPR).unwrap();
    write_all(&mut fs, f, 0, &data);
    assert!(fs.compsize(f).unwrap().compressed_extents > 0);
    fs.set_mount_compression(Some(Compression::Zstd)).unwrap();
    let g = fs.create(root, "nc", FileKind::Regular, 0o644, 0).unwrap();
    fs.set_flags(g, FL_NOCOMP).unwrap();
    write_all(&mut fs, g, 0, &data);
    assert_eq!(fs.compsize(g).unwrap().compressed_extents, 0);
    let h = fs
        .create(root, "nocow", FileKind::Regular, 0o644, 0)
        .unwrap();
    fs.set_flags(h, FL_NOCOW).unwrap();
    write_all(&mut fs, h, 0, &data);
    assert_eq!(fs.compsize(h).unwrap().compressed_extents, 0);
    assert_eq!(read_all(&mut fs, h), data);
    clean(&mut fs);
}

#[test]
fn nocow_and_compression_exclude_each_other() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "a", FileKind::Regular, 0o644, 0).unwrap();
    assert_eq!(fs.set_flags(f, FL_NOCOW | FL_COMPR), Err(Error::Invalid));
    fs.set_compression(f, Some(Compression::Zstd)).unwrap();
    assert_eq!(fs.set_flags(f, FL_NOCOW), Err(Error::Invalid));
    let g = fs.create(root, "b", FileKind::Regular, 0o644, 0).unwrap();
    fs.set_flags(g, FL_NOCOW).unwrap();
    assert_eq!(
        fs.set_compression(g, Some(Compression::Zstd)),
        Err(Error::Invalid)
    );
    fs.set_compression(g, None).unwrap();
}

#[test]
fn small_appends_still_compress() {
    for algo in ALL {
        let (_disk, mut fs, root) = compressed(16 * MIB, algo);
        let data = text(1 << 20, 8);
        let f = fs.create(root, "log", FileKind::Regular, 0o644, 0).unwrap();
        for (i, c) in data.chunks(4096).enumerate() {
            write_all(&mut fs, f, (i * 4096) as u64, c);
        }
        let g = fs.create(root, "odd", FileKind::Regular, 0o644, 0).unwrap();
        for (i, c) in data.chunks(1000).enumerate() {
            write_all(&mut fs, g, (i * 1000) as u64, c);
        }
        for x in [f, g] {
            let c = fs.compsize(x).unwrap();
            assert!(
                c.disk_bytes * 3 < c.uncompressed_bytes,
                "{:?} {:?} {}",
                algo,
                c,
                x
            );
            assert_eq!(read_all(&mut fs, x), data);
        }
        clean(&mut fs);
    }
}

#[test]
fn appends_across_commits_and_snapshots_end_up_compressed() {
    let (disk, mut fs, root) = compressed(32 * MIB, Compression::Zstd);
    let sv = fs.create_subvolume(root, "sv").unwrap();
    let data = text(3 << 20, 17);
    let f = fs.create(sv, "log", FileKind::Regular, 0o644, 0).unwrap();
    let mut off = 0;
    let mut rng = Lcg(17);
    let mut snaps = 0;
    while off < data.len() {
        let n = (1 + rng.below(9000) as usize).min(data.len() - off);
        write_all(&mut fs, f, off as u64, &data[off..off + n]);
        off += n;
        match rng.below(40) {
            0 => fs.sync().unwrap(),
            1 if snaps < 3 => {
                fs.snapshot(sv, root, &format!("s{}", snaps), true).unwrap();
                snaps += 1;
            }
            _ => {}
        }
    }
    fs.sync().unwrap();
    let c = fs.compsize(f).unwrap();
    assert!(c.disk_bytes * 4 < c.uncompressed_bytes, "{:?}", c);
    assert_eq!(read_all(&mut fs, f), data);
    clean(&mut fs);
    let mut fs = remount(&disk, fs);
    assert_eq!(read_all(&mut fs, f), data);
    for s in 0..snaps {
        let d = fs.lookup(root, &format!("s{}", s)).unwrap();
        let g = fs.lookup(d, "log").unwrap();
        let got = read_all(&mut fs, g);
        assert_eq!(got, data[..got.len()]);
    }
    clean(&mut fs);
}

#[test]
fn overwriting_part_of_a_compressed_extent() {
    let (disk, mut fs, root) = compressed(16 * MIB, Compression::Zstd);
    let mut data = text(ZCHUNK as usize * 4096 * 3, 9);
    let f = file(&mut fs, root, "a", &data);
    fs.sync().unwrap();
    let base = used(&mut fs);
    // Inside one extent, across two, unaligned.
    for (at, len) in [
        (5000usize, 3000usize),
        (130_000, 9000),
        (4096 * 40, 4096),
        (1, 1),
    ] {
        let new = noise(len, at as u64);
        write_all(&mut fs, f, at as u64, &new);
        data[at..at + len].copy_from_slice(&new);
        assert_eq!(read_all(&mut fs, f), data);
    }
    clean(&mut fs);
    let mut fs = remount(&disk, fs);
    assert_eq!(read_all(&mut fs, f), data);
    // The mount option does not outlive the mount.
    fs.set_mount_compression(Some(Compression::Zstd)).unwrap();
    // Rewriting it all leaves nothing of the first extents behind.
    let new = text(data.len(), 10);
    write_all(&mut fs, f, 0, &new);
    let now = used(&mut fs);
    assert!(now <= base + 16 * 4096, "{} vs {}", now, base);
    assert_eq!(read_all(&mut fs, f), new);
    clean(&mut fs);
}

#[test]
fn truncating_and_punching_compressed_files() {
    let (_disk, mut fs, root) = compressed(16 * MIB, Compression::Lz4);
    let data = text(400_000, 11);
    let f = file(&mut fs, root, "a", &data);
    fs.truncate(f, 70_001).unwrap();
    fs.truncate(f, 200_000).unwrap();
    let mut want = data[..70_001].to_vec();
    want.resize(200_000, 0);
    assert_eq!(read_all(&mut fs, f), want);
    let g = file(&mut fs, root, "b", &data);
    let mode = FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE;
    fs.fallocate(g, mode, 10_000, 150_000).unwrap();
    let mut want = data.clone();
    want[10_000..160_000].fill(0);
    assert_eq!(read_all(&mut fs, g), want);
    clean(&mut fs);
    fs.truncate(f, 0).unwrap();
    fs.unlink(root, "a").unwrap();
    fs.unlink(root, "b").unwrap();
    clean(&mut fs);
}

#[test]
fn snapshots_and_reflinks_share_compressed_extents() {
    let (disk, mut fs, root) = compressed(32 * MIB, Compression::Zlib);
    let sv = fs.create_subvolume(root, "sv").unwrap();
    let data = text(1 << 20, 12);
    let f = file(&mut fs, sv, "a", &data);
    let base = used(&mut fs);
    let snap = fs.snapshot(sv, root, "snap", true).unwrap();
    let g = fs
        .create(root, "clone", FileKind::Regular, 0o644, 0)
        .unwrap();
    fs.clone_range(f, 0, g, 0, 0).unwrap();
    // A piece from the middle of an extent, at another offset.
    let h = fs
        .create(root, "piece", FileKind::Regular, 0o644, 0)
        .unwrap();
    fs.clone_range(f, 4096 * 5, h, 4096 * 2, 4096 * 40).unwrap();
    assert!(
        used(&mut fs) < base + 64 * 1024,
        "sharing costs only metadata"
    );
    assert_eq!(fs.compsize(h).unwrap().compressed_extents, 2);
    write_all(&mut fs, f, 12_345, b"changed");
    let mut piece = vec![0u8; 4096 * 2];
    piece.extend_from_slice(&data[4096 * 5..4096 * 45]);
    let s = fs.lookup(snap, "a").unwrap();
    assert_eq!(read_all(&mut fs, s), data);
    assert_eq!(read_all(&mut fs, g), data);
    assert_eq!(read_all(&mut fs, h), piece);
    clean(&mut fs);
    let mut fs = remount(&disk, fs);
    fs.unlink(sv, "a").unwrap();
    fs.delete_subvolume(root, "snap").unwrap();
    assert_eq!(read_all(&mut fs, g), data);
    fs.unlink(root, "clone").unwrap();
    assert_eq!(read_all(&mut fs, h), piece);
    clean(&mut fs);
    fs.unlink(root, "piece").unwrap();
    clean(&mut fs);
}

#[test]
fn damaged_compressed_extents_are_detected() {
    let (disk, mut fs, root) = compressed(16 * MIB, Compression::Zstd);
    let data = text(ZCHUNK as usize * 4096 * 2, 13);
    let f = file(&mut fs, root, "a", &data);
    let g = file(&mut fs, root, "b", &text(300_000, 14));
    let fs = remount(&disk, fs);
    // The first extent is exactly what `pack` makes of the first chunk.
    let on_disk = pack(Compression::Zstd, &data[..ZCHUNK as usize * 4096]).unwrap();
    let mut image = vec![0u8; disk.size() as usize];
    disk.read_at(0, &mut image).unwrap();
    let at = image
        .chunks(4096)
        .position(|b| b == &on_disk[..4096])
        .expect("extent on disk");
    disk.poke(at * 4096 + 100, 0x40);
    let mut fs = remount(&disk, fs);
    let mut buf = vec![0u8; 4096];
    assert!(matches!(fs.read(f, 0, &mut buf), Err(Error::Corrupt(_))));
    assert!(matches!(
        fs.read(f, 50_000, &mut buf),
        Err(Error::Corrupt(_))
    ));
    // The second extent and the other file are fine.
    fs.read(f, ZCHUNK * 4096 + 10, &mut buf).unwrap();
    assert_eq!(
        buf,
        data[ZCHUNK as usize * 4096 + 10..ZCHUNK as usize * 4096 + 4106]
    );
    assert_eq!(read_all(&mut fs, g), text(300_000, 14));
    let r = fs.scrub().unwrap();
    assert_eq!(r.bad_data_blocks, vec![at as u64]);
}

#[test]
fn compressed_and_plain_extents_mix_in_one_file() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let data = text(300_000, 15);
    let f = file(&mut fs, root, "a", &data);
    fs.set_compression(f, Some(Compression::Zstd)).unwrap();
    write_all(&mut fs, f, 300_000, &data);
    write_all(&mut fs, f, 100_000, &data[..50_000]);
    let mut want = data.clone();
    want.extend_from_slice(&data);
    want[100_000..150_000].copy_from_slice(&data[..50_000]);
    let c = fs.compsize(f).unwrap();
    assert!(c.compressed_extents > 0 && c.disk_bytes < c.uncompressed_bytes);
    let mut fs = remount(&disk, fs);
    assert_eq!(read_all(&mut fs, f), want);
    clean(&mut fs);
}

#[test]
fn compression_is_v4_only() {
    let (_disk, mut fs) = fresh_v3(16 * MIB);
    let root = fs.root_ino();
    assert!(matches!(
        fs.set_mount_compression(Some(Compression::Zlib)),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        fs.set_compression(root, Some(Compression::Zlib)),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn a_full_disk_falls_back_cleanly() {
    let (disk, mut fs, root) = compressed(4 * MIB, Compression::Lz4);
    // Random data does not compress, so it fills the disk.
    let f = fs.create(root, "big", FileKind::Regular, 0o644, 0).unwrap();
    let mut off = 0u64;
    loop {
        match fs.write(f, off, &noise(64 * 1024, off)) {
            Ok(n) => off += n as u64,
            Err(Error::NoSpace) => break,
            Err(e) => panic!("{:?}", e),
        }
    }
    clean(&mut fs);
    let mut fs = remount(&disk, fs);
    fs.set_mount_compression(Some(Compression::Lz4)).unwrap();
    clean(&mut fs);
    fs.unlink(root, "big").unwrap();
    // Its blocks come back with the commit.
    fs.sync().unwrap();
    // Compressible data takes a fraction of the space.
    let g = file(&mut fs, root, "text", &text(off as usize * 2, 16));
    assert_eq!(read_all(&mut fs, g), text(off as usize * 2, 16));
    clean(&mut fs);
}
