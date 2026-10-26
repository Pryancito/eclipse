#![cfg(feature = "std")]
//! Format 1.0 is frozen (`FORMAT.md`): images written by the driver of
//! the day the format was frozen, kept in `tests/format/`, must mount,
//! read the same and take writes with every later driver.
//!
//! - every image mounts read-only, passes `check` and `scrub`, and lists
//!   (names, attributes, flags, xattrs, contents) exactly as its `.txt`;
//! - every image mounts writable, takes new files, overwrites, renames
//!   and deletes, and still passes `check` after a remount;
//! - the numbers that are the format (magics, item types, feature bits,
//!   flags, profiles...) keep their values;
//! - feature bits a driver does not know: an incompatible one refuses the
//!   mount, a read-only compatible one allows a read-only mount, a
//!   compatible one is ignored and kept.
//!
//! A new on-disk feature gets a new image (`bless_missing_images` only
//! makes the ones that are not there yet); the frozen ones are never
//! made again.

mod common;

use std::sync::Arc;

use common::frozen::{self, IMAGES, V3_IMAGE};
use common::*;
use eclipsefs::cowfmt::*;
use eclipsefs::device::BlockDevice;
use eclipsefs::{mkfs, Compression, EclipseFS, EncryptionPolicy, Error, FileKind, Profile, Tier};

const KIB: usize = 1024;

fn file(fs: &mut EclipseFS, dir: u64, name: &str, data: &[u8]) -> u64 {
    let f = fs.create(dir, name, FileKind::Regular, 0o644, 0).unwrap();
    write_all(fs, f, 0, data);
    f
}

fn mkdir(fs: &mut EclipseFS, dir: u64, name: &str) -> u64 {
    fs.create(dir, name, FileKind::Dir, 0o755, 0).unwrap()
}

/// Data that compresses (text-like) and data that does not.
fn text(len: usize, seed: usize) -> Vec<u8> {
    let words = [
        "eclipse ",
        "kernel ",
        "bloque ",
        "extent ",
        "árbol ",
        "nodo ",
        "fn main() {} ",
        "\n",
        "copy-on-write ",
        "superbloque ",
    ];
    let mut out = Vec::with_capacity(len);
    let mut i = seed;
    while out.len() < len {
        out.extend_from_slice(words[(i * 7 + i / 3) % words.len()].as_bytes());
        i += 1;
    }
    out.truncate(len);
    out
}

/// File data: different for every seed and offset, yet it compresses
/// well, so the stored images stay small.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut r = Lcg(seed);
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let b = r.next() as u8;
        let run = 64 + (r.next() % 192) as usize;
        out.extend(std::iter::repeat_n(b, run));
    }
    out.truncate(len);
    out
}

/// Data that does not compress at all.
fn random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut r = Lcg(seed);
    (0..len).map(|_| r.next() as u8).collect()
}

// ---------------------------------------------------------------------------
// What each image holds
// ---------------------------------------------------------------------------

/// Files of every size and kind, hard links, symlinks, special files,
/// xattrs and ACLs, chattr flags, fallocate in its modes, sparse files,
/// a directory spread over many leaves, long and non-ASCII names.
fn fill_basic(fs: &mut EclipseFS) {
    let root = fs.root_ino();
    let docs = mkdir(fs, root, "docs");
    let deep = (0..6).fold(docs, |d, i| mkdir(fs, d, &format!("nivel-{i}")));
    file(fs, deep, "fondo.txt", b"lo mas hondo\n");
    file(fs, root, "vacio", b"");
    file(fs, root, "en-linea", &text(100, 1));
    file(fs, root, "en-linea-max", &text(1024, 2));
    file(fs, root, "un-bloque-mas", &text(1025, 3));
    file(fs, root, "tres-bloques", &noise(3 * BLOCK, 4));
    let big = file(fs, root, "grande", &noise(MIB + 123, 5));
    let sparse = file(fs, root, "disperso", b"inicio");
    write_all(fs, sparse, 5 * MIB as u64, b"final tras un hueco");
    // Rewritten in the middle: more than one extent.
    write_all(fs, big, 300_000, &noise(70_000, 6));
    fs.link(docs, "enlace-duro", big).unwrap();
    fs.symlink(root, "corto", b"docs/nivel-0").unwrap();
    let long: Vec<u8> = text(3000, 7).into_iter().filter(|&c| c != 0).collect();
    fs.symlink(root, "largo", &long).unwrap();
    let dev = mkdir(fs, root, "dev");
    fs.create(dev, "fifo", FileKind::Fifo, 0o600, 0).unwrap();
    fs.create(dev, "tty", FileKind::CharDevice, 0o620, (4 << 8) | 1)
        .unwrap();
    fs.create(dev, "sda", FileKind::BlockDevice, 0o660, 8 << 8)
        .unwrap();
    fs.create(dev, "socket", FileKind::Socket, 0o755, 0)
        .unwrap();
    // Extended attributes in every namespace, a POSIX ACL, a big value.
    let x = file(fs, root, "con-xattrs", b"atributos");
    fs.set_xattr(x, "user.comentario", "¡hola!".as_bytes(), 0)
        .unwrap();
    fs.set_xattr(x, "trusted.marca", b"1", 0).unwrap();
    fs.set_xattr(x, "security.selinux", b"system_u:object_r:etc_t:s0", 0)
        .unwrap();
    let acl = [
        2u8, 0, 0, 0, 1, 0, 6, 0, 0xff, 0xff, 0xff, 0xff, 4, 0, 4, 0, 0xff, 0xff, 0xff, 0xff, 0x20,
        0, 4, 0, 0xff, 0xff, 0xff, 0xff,
    ];
    fs.set_xattr(x, "system.posix_acl_access", &acl, 0).unwrap();
    fs.set_xattr(x, "user.grande", &noise(1500, 8), 0).unwrap();
    fs.set_xattr(docs, "user.en-directorio", b"si", 0).unwrap();
    // chattr flags.
    let f = file(fs, root, "inmutable", b"no cambia");
    fs.set_flags(f, FL_IMMUTABLE).unwrap();
    let f = file(fs, root, "solo-anadir", b"registro\n");
    fs.set_flags(f, FL_APPEND | FL_NODUMP | FL_NOATIME | FL_SYNC)
        .unwrap();
    let f = fs
        .create(root, "nocow", FileKind::Regular, 0o644, 0)
        .unwrap();
    fs.set_flags(f, FL_NOCOW).unwrap();
    write_all(fs, f, 0, &noise(5 * BLOCK, 9));
    write_all(fs, f, BLOCK as u64, &noise(BLOCK, 10));
    // fallocate: reserved (with and without KEEP_SIZE), punched, zeroed.
    let f = file(fs, root, "reservado", b"");
    fs.fallocate(f, 0, 0, 64 * KIB as u64).unwrap();
    write_all(fs, f, 8 * KIB as u64, b"escrito dentro de lo reservado");
    let f = file(fs, root, "reservado-keep", b"corto");
    fs.fallocate(f, FALLOC_FL_KEEP_SIZE, 0, 128 * KIB as u64)
        .unwrap();
    let f = file(fs, root, "agujereado", &noise(64 * KIB, 11));
    fs.fallocate(f, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 5000, 30_000)
        .unwrap();
    fs.fallocate(f, FALLOC_FL_ZERO_RANGE, 50_000, 3000).unwrap();
    // A directory big enough for several leaves of DIR_ITEM/DIR_INDEX.
    let many = mkdir(fs, root, "muchos");
    for i in 0..400 {
        file(fs, many, &format!("archivo-{i:04}.rs"), &text(i % 50, i));
    }
    for i in (0..400).step_by(7) {
        fs.unlink(many, &format!("archivo-{i:04}.rs")).unwrap();
    }
    for i in (1..400).step_by(11) {
        let _ = fs.rename(
            many,
            &format!("archivo-{i:04}.rs"),
            docs,
            &format!("movido-{i}"),
        );
    }
    file(fs, root, &"n".repeat(255), b"nombre de 255 bytes");
    file(fs, root, "ñandú-日本語-🦀", b"utf-8");
    // Attributes set by hand, a truncated file, a removed one.
    let f = file(fs, root, "permisos", b"x");
    fs.set_attr(
        f,
        Some(0o4751),
        Some(1000),
        Some(100),
        Some((1_500_000_000, 7)),
        Some((1_600_000_000, 9)),
    )
    .unwrap();
    let f = file(fs, root, "truncado", &noise(40 * KIB, 12));
    fs.truncate(f, 10_001).unwrap();
    file(fs, root, "borrado", b"se va");
    fs.unlink(root, "borrado").unwrap();
}

/// Subvolumes, snapshots (writable and read-only) with changes on both
/// sides, a nested subvolume, reflinks and dedupe, a received snapshot,
/// quotas with a limit, and a default subvolume other than 5.
fn fill_subvolumes(fs: &mut EclipseFS) {
    let root = fs.root_ino();
    fs.quota_enable().unwrap();
    let home = fs.create_subvolume(root, "home").unwrap();
    let user = mkdir(fs, home, "moebius");
    let a = file(fs, user, "a.bin", &noise(200 * KIB, 1));
    file(fs, user, "nota", b"antes del snapshot");
    let nested = fs.create_subvolume(home, "anidado").unwrap();
    file(fs, nested, "dentro", b"subvolumen anidado");
    let snaps = mkdir(fs, root, "snapshots");
    let ro = fs.snapshot(home, snaps, "home-ro", true).unwrap();
    let rw = fs.snapshot(home, snaps, "home-rw", false).unwrap();
    write_all(fs, a, 4096, &noise(8 * KIB, 2));
    let n = fs.lookup(user, "nota").unwrap();
    write_all(fs, n, 0, b"DESPUES");
    let rwu = fs.lookup(rw, "moebius").unwrap();
    file(fs, rwu, "solo-en-rw", b"nuevo");
    // Reflinks: whole file, a range, across subvolumes; dedupe.
    let b = file(fs, user, "b.bin", b"");
    fs.clone_range(a, 0, b, 0, 0).unwrap();
    let c = file(fs, user, "c.bin", &noise(64 * KIB, 3));
    fs.clone_range(a, 16 * KIB as u64, c, 8 * KIB as u64, 16 * KIB as u64)
        .unwrap();
    let d = file(fs, root, "copia-entre-subvolumenes", b"");
    fs.clone_range(a, 0, d, 0, 64 * KIB as u64).unwrap();
    let e = file(fs, user, "igual.bin", &noise(200 * KIB, 1));
    fs.dedupe_range(e, 0, b, 0, 128 * KIB as u64).unwrap();
    // send/receive into the same filesystem: a received subvolume.
    let mut stream = Vec::new();
    fs.send(ro, None, &mut |b| {
        stream.extend_from_slice(b);
        Ok(())
    })
    .unwrap();
    let recv = mkdir(fs, root, "recibidos");
    let mut at = 0;
    fs.receive(recv, &mut |buf| {
        if at + buf.len() > stream.len() {
            return Err(Error::Io);
        }
        buf.copy_from_slice(&stream[at..at + buf.len()]);
        at += buf.len();
        Ok(())
    })
    .unwrap();
    let id = fs.subvolume_of(home);
    fs.set_qgroup_limit(id, Some(8 * MIB as u64)).unwrap();
    fs.set_default_subvolume(id).unwrap();
    // A deleted subvolume leaves nothing behind.
    let tmp = fs.create_subvolume(root, "temporal").unwrap();
    file(fs, tmp, "x", b"x");
    fs.delete_subvolume(root, "temporal").unwrap();
}

/// zlib, LZ4 and zstd, from the mount option, the per-file property and
/// chattr +c; small writes compacted at commit; data that does not
/// compress; a compressed file rewritten in the middle.
fn fill_compressed(fs: &mut EclipseFS) {
    let root = fs.root_ino();
    fs.set_mount_compression(Some(Compression::Zlib)).unwrap();
    file(fs, root, "zlib-montaje", &text(300 * KIB, 1));
    file(fs, root, "aleatorio", &random_bytes(140 * KIB, 2));
    fs.set_mount_compression(None).unwrap();
    let lz = mkdir(fs, root, "lz4");
    fs.set_compression(lz, Some(Compression::Lz4)).unwrap();
    file(fs, lz, "heredado", &text(256 * KIB, 3));
    let z = file(fs, root, "zstd", b"");
    fs.set_compression(z, Some(Compression::Zstd)).unwrap();
    write_all(fs, z, 0, &text(512 * KIB + 77, 4));
    write_all(fs, z, 100_000, &random_bytes(10_000, 5));
    let c = fs
        .create(root, "chattr-c", FileKind::Regular, 0o644, 0)
        .unwrap();
    fs.set_flags(c, FL_COMPR).unwrap();
    write_all(fs, c, 0, &text(130 * KIB, 6));
    let s = fs
        .create(root, "poco-a-poco", FileKind::Regular, 0o644, 0)
        .unwrap();
    fs.set_compression(s, Some(Compression::Zstd)).unwrap();
    for i in 0..64 {
        write_all(fs, s, (i * 3000) as u64, &text(3000, i));
    }
    let n = fs
        .create(root, "sin-compresion", FileKind::Regular, 0o644, 0)
        .unwrap();
    fs.set_flags(n, FL_NOCOMP).unwrap();
    write_all(fs, n, 0, &text(64 * KIB, 7));
}

/// A directory with an encryption policy: files, a subdirectory, a
/// symlink, a long name (no-key names hash its tail), a hard link.
fn fill_encrypted(fs: &mut EclipseFS) {
    let root = fs.root_ino();
    let id = fs.add_encryption_key(&frozen::KEY).unwrap();
    let secret = mkdir(fs, root, "secreto");
    fs.set_encryption_policy(secret, &EncryptionPolicy::new(id))
        .unwrap();
    let f = file(fs, secret, "diario.txt", &text(20 * KIB + 5, 1));
    file(fs, secret, "corto", b"x");
    file(fs, secret, &"largo".repeat(40), &noise(9000, 2));
    let sub = mkdir(fs, secret, "subdir");
    file(fs, sub, "dentro", &noise(3 * BLOCK, 3));
    fs.symlink(secret, "enlace", b"diario.txt").unwrap();
    fs.link(sub, "otro-nombre", f).unwrap();
    file(fs, root, "en-claro", b"sin cifrar");
}

fn fresh_disk(size: usize) -> (Arc<MemDisk>, EclipseFS) {
    let (d, mut fs) = fresh(size);
    fs.set_random(frozen::random);
    (d, fs)
}

fn volume(sizes: &[usize], data: Profile, meta: Profile, tiers: &[Tier]) -> Vec<Arc<MemDisk>> {
    let d: Vec<Arc<MemDisk>> = sizes.iter().map(|&s| MemDisk::new(s)).collect();
    mkfs::format_devices_tiered(&frozen::devs(&d), &opts(), data, meta, tiers).unwrap();
    d
}

fn fill_volume(d: &[Arc<MemDisk>]) {
    let mut fs = EclipseFS::mount_devices(frozen::devs(d), false).unwrap();
    fs.set_clock(clock);
    let root = fs.root_ino();
    for i in 0..12 {
        file(
            &mut fs,
            root,
            &format!("archivo-{i}"),
            &noise(i * 9 * KIB + 100, i as u64),
        );
    }
    let dir = mkdir(&mut fs, root, "dir");
    let t = file(&mut fs, dir, "texto", &text(90 * KIB, 1));
    fs.sync().unwrap();
    write_all(&mut fs, t, 3000, b"reescrito");
    fs.sync().unwrap();
}

/// Build image `name`.
fn build(name: &str) -> Vec<Arc<MemDisk>> {
    match name {
        "basic" => {
            let (d, mut fs) = fresh_disk(32 * MIB);
            fill_basic(&mut fs);
            fs.sync().unwrap();
            vec![d]
        }
        "subvolumes" => {
            let (d, mut fs) = fresh_disk(32 * MIB);
            fill_subvolumes(&mut fs);
            fs.sync().unwrap();
            vec![d]
        }
        "compressed" => {
            let (d, mut fs) = fresh_disk(32 * MIB);
            fill_compressed(&mut fs);
            fs.sync().unwrap();
            vec![d]
        }
        "encrypted" => {
            let (d, mut fs) = fresh_disk(16 * MIB);
            fill_encrypted(&mut fs);
            fs.sync().unwrap();
            vec![d]
        }
        "fsync-log" => {
            // fsynced writes left in the log, no commit after them: the
            // records replay at mount.
            let (d, mut fs) = fresh_disk(16 * MIB);
            let root = fs.root_ino();
            let f = file(&mut fs, root, "base", &noise(64 * KIB, 1));
            let g = file(&mut fs, root, "otro", b"");
            fs.sync().unwrap();
            fs.set_commit_interval(Some(3600));
            write_all(&mut fs, f, 4096, &noise(12 * KIB, 2));
            fs.fsync(f).unwrap();
            write_all(&mut fs, g, 0, &noise(20 * KIB, 3));
            fs.truncate(f, 50 * KIB as u64).unwrap();
            fs.fsync(g).unwrap();
            fs.fsync(f).unwrap();
            eprintln!("fsync records {}", fs.fsync_log_records());
            assert!(fs.fsync_log_records() >= 2);
            let cut = d.copy();
            std::mem::forget(fs);
            vec![cut]
        }
        "journal-pending" => {
            // A transaction in the journal whose blocks were not written
            // home yet: a power cut after the commit's journal flush.
            let (d, mut fs) = fresh_disk(16 * MIB);
            let root = fs.root_ino();
            for n in ["a", "b", "c"] {
                file(&mut fs, root, n, &text(3000, n.len()));
            }
            fs.sync().unwrap();
            drop(fs);
            let cut = Arc::new(CutAfterFlushes::new(d.clone(), 2));
            let mut fs = EclipseFS::mount(cut.clone(), false).unwrap();
            fs.set_clock(clock);
            let root = fs.root_ino();
            file(&mut fs, root, "en-el-journal", &text(7000, 10));
            fs.unlink(root, "c").unwrap();
            fs.sync().unwrap();
            std::mem::forget(fs);
            vec![d.copy()]
        }
        "resized" => {
            let (d, mut fs) = fresh_disk(32 * MIB);
            let root = fs.root_ino();
            file(&mut fs, root, "a", &noise(3 * MIB, 1));
            fs.resize(1, Some(20 * MIB as u64)).unwrap();
            file(&mut fs, root, "b", &noise(MIB, 2));
            fs.sync().unwrap();
            vec![d]
        }
        "raid1" => {
            let d = volume(&[24 * MIB, 24 * MIB], Profile::Raid1, Profile::Raid1, &[]);
            fill_volume(&d);
            d
        }
        "raid10" => {
            let d = volume(&[24 * MIB; 4], Profile::Raid10, Profile::Raid1c4, &[]);
            fill_volume(&d);
            d
        }
        "raid5" => {
            let d = volume(&[24 * MIB; 3], Profile::Raid5, Profile::Raid1c3, &[]);
            fill_volume(&d);
            d
        }
        "raid6" => {
            let d = volume(&[24 * MIB; 3], Profile::Raid6, Profile::Raid1c3, &[]);
            fill_volume(&d);
            d
        }
        "tiers" => {
            let d = volume(
                &[16 * MIB, 32 * MIB],
                Profile::Single,
                Profile::Dup,
                &[Tier::Fast, Tier::Slow],
            );
            fill_volume(&d);
            let mut fs = EclipseFS::mount_devices(frozen::devs(&d), false).unwrap();
            fs.set_clock(clock);
            fs.tier_migrate(true).unwrap();
            fs.sync().unwrap();
            d
        }
        V3_IMAGE => {
            let (d, mut fs) = fresh_v3(16 * MIB);
            let root = fs.root_ino();
            let dir = mkdir(&mut fs, root, "dir");
            file(&mut fs, dir, "pequeño", b"v0.3");
            let big = file(&mut fs, root, "grande", &noise(700 * KIB, 1));
            write_all(&mut fs, big, 100_000, &noise(5000, 2));
            fs.link(dir, "enlace", big).unwrap();
            fs.symlink(root, "simbolico", "dir/pequeño".as_bytes())
                .unwrap();
            for i in 0..80 {
                file(&mut fs, dir, &format!("f{i}"), &text(i * 13, i));
            }
            fs.rename(dir, "f3", root, "renombrado").unwrap();
            fs.unlink(dir, "f4").unwrap();
            fs.sync().unwrap();
            vec![d]
        }
        _ => unreachable!("{name}"),
    }
}

/// A device that stops writing (silently, as a power cut) after `n`
/// flushes.
struct CutAfterFlushes {
    disk: Arc<MemDisk>,
    left: std::sync::atomic::AtomicUsize,
}

impl CutAfterFlushes {
    fn new(disk: Arc<MemDisk>, n: usize) -> Self {
        Self {
            disk,
            left: n.into(),
        }
    }
}

impl BlockDevice for CutAfterFlushes {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> eclipsefs::Result<()> {
        self.disk.read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> eclipsefs::Result<()> {
        if self.left.load(std::sync::atomic::Ordering::Relaxed) > 0 {
            self.disk.write_at(offset, buf)?;
        }
        Ok(())
    }
    fn sync(&self) -> eclipsefs::Result<()> {
        let n = self.left.load(std::sync::atomic::Ordering::Relaxed);
        self.left
            .store(n.saturating_sub(1), std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
    fn size(&self) -> u64 {
        self.disk.size()
    }
}

fn all_images() -> Vec<(&'static str, usize)> {
    let mut v = IMAGES.to_vec();
    v.push((V3_IMAGE, 1));
    v
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// Makes the images that are not in `tests/format/` yet, with their
/// listings. Frozen images are never rebuilt: delete one by hand only if
/// it was never part of a release of the format.
///
///   EFS_BLESS=1 cargo test --release -p eclipsefs --features std \
///       --test format -- --ignored bless_missing_images
#[test]
#[ignore]
fn bless_missing_images() {
    assert_eq!(
        std::env::var("EFS_BLESS").as_deref(),
        Ok("1"),
        "set EFS_BLESS=1"
    );
    for (name, devices) in all_images() {
        if frozen::image_path(name, 0, devices).exists() {
            continue;
        }
        let disks = build(name);
        assert_eq!(disks.len(), devices, "{name}");
        frozen::store(name, &disks);
        // The listing comes from the stored image, mounted the way the
        // test mounts it.
        let disks = frozen::load(name, devices);
        let mut fs = frozen::mount(&disks, true).unwrap();
        std::fs::write(frozen::listing_path(name), frozen::listing(&mut fs)).unwrap();
        eprintln!("made {name}");
    }
}

#[test]
fn frozen_images_read_as_they_were() {
    for (name, devices) in all_images() {
        let disks = frozen::load(name, devices);
        let before: Vec<Vec<u8>> = disks.iter().map(|d| d.bytes()).collect();
        let mut fs = frozen::mount(&disks, true).unwrap();
        let want = std::fs::read_to_string(frozen::listing_path(name)).unwrap();
        let got = frozen::listing(&mut fs);
        if got != want {
            let first = got
                .lines()
                .zip(want.lines())
                .find(|(g, w)| g != w)
                .map(|(g, w)| format!("\n  now:    {g}\n  frozen: {w}"))
                .unwrap_or_else(|| "\n  (one listing is longer)".into());
            panic!("{name}: reads differently from the day it was frozen:{first}");
        }
        fs.check()
            .unwrap_or_else(|e| panic!("{name}: check: {e:?}"));
        if fs.format_version() == 4 {
            let s = fs.scrub().unwrap();
            assert!(s.is_clean(), "{name}: scrub {s:?}");
        }
        drop(fs);
        let after: Vec<Vec<u8>> = disks.iter().map(|d| d.bytes()).collect();
        assert!(
            before == after,
            "{name}: a read-only mount wrote to the image"
        );
    }
}

#[test]
fn frozen_images_take_writes() {
    for (name, devices) in all_images() {
        let disks = frozen::load(name, devices);
        let mut fs = frozen::mount(&disks, false).unwrap();
        let root = fs.root_ino();
        let mut ents = fs.readdir(root).unwrap();
        ents.retain(|e| e.name != "." && e.name != "..");
        let data = noise(150 * KIB, 77);
        let dir = mkdir(&mut fs, root, "despues-de-1.0");
        let f = file(&mut fs, dir, "nuevo", &data);
        // Overwrite the start of the first regular file that allows it,
        // rename something, delete something.
        let mut changed = None;
        for e in &ents {
            if e.kind == FileKind::Regular
                && fs.stat(e.ino).unwrap().size > 0
                && fs.write(e.ino, 0, b"1.0").is_ok()
            {
                changed = Some(e.ino);
                break;
            }
        }
        let victim = ents
            .iter()
            .find(|e| {
                e.kind == FileKind::Regular
                    && Some(e.ino) != changed
                    && fs.unlink(root, &e.name).is_ok()
            })
            .map(|e| e.name.clone());
        fs.rename(dir, "nuevo", root, "renombrado-en-1.0").unwrap();
        fs.sync().unwrap();
        drop(fs);
        let mut fs = frozen::mount(&disks, false).unwrap();
        fs.check()
            .unwrap_or_else(|e| panic!("{name}: check after writes: {e:?}"));
        let root = fs.root_ino();
        assert_eq!(fs.lookup(root, "renombrado-en-1.0").unwrap(), f, "{name}");
        assert_eq!(read_all(&mut fs, f), data, "{name}");
        if let Some(ino) = changed {
            let mut b = [0u8; 3];
            fs.read(ino, 0, &mut b).unwrap();
            assert_eq!(&b, b"1.0", "{name}");
        }
        if let Some(v) = victim {
            assert_eq!(fs.lookup(root, &v), Err(Error::NotFound), "{name}");
        }
        // Everything else still lists (and so reads) without errors.
        frozen::listing(&mut fs);
    }
}

/// The encrypted image without its key: names show as no-key names,
/// contents refuse, and it can still be checked and scrubbed.
#[test]
fn the_encrypted_image_without_its_key() {
    let disks = frozen::load("encrypted", 1);
    let mut fs = EclipseFS::mount(disks[0].clone(), true).unwrap();
    let root = fs.root_ino();
    let secret = fs.lookup(root, "secreto").unwrap();
    let names = fs.readdir(secret).unwrap();
    assert_eq!(
        names
            .iter()
            .filter(|e| e.name != "." && e.name != "..")
            .count(),
        5
    );
    let e = names.iter().find(|e| e.kind == FileKind::Regular).unwrap();
    assert_eq!(fs.read(e.ino, 0, &mut [0u8; 16]), Err(Error::NoKey));
    fs.check().unwrap();
    assert!(fs.scrub().unwrap().is_clean());
}

/// The numbers that make up the format. Changing any of them breaks every
/// filesystem made before.
#[test]
fn the_numbers_of_the_format_stay() {
    assert_eq!(eclipsefs::format::MAGIC, b"ECLIPSEFS");
    assert_eq!(VERSION4, 0x0004_0000);
    assert_eq!(eclipsefs::format::VERSION, 0x0003_0000);
    assert_eq!(eclipsefs::format::BLOCK_SIZE, 4096);
    assert_eq!(eclipsefs::format::GROUP_BLOCKS, 32768);
    assert_eq!(CSUM_CRC32C, 1);
    assert_eq!(eclipsefs::btree::NODE_MAGIC, u32::from_le_bytes(*b"EFSN"));
    assert_eq!(eclipsefs::btree::NODE_HDR, 64);
    assert_eq!(
        eclipsefs::journal::JOURNAL_MAGIC,
        u32::from_le_bytes(*b"EFSJ")
    );
    assert_eq!(eclipsefs::journal::JOURNAL_VERSION, 1);
    assert_eq!(
        [
            ROOT_TREE,
            REFS_TREE,
            FS_TREE,
            CSUM_TREE,
            LOG_TREE,
            FIRST_INO,
            FIRST_SUBVOL
        ],
        [1, 2, 5, 7, 8, 256, 256]
    );
    assert_eq!(SUBVOL_SHIFT, 40);
    assert_eq!(
        [
            INODE_ITEM,
            INODE_REF,
            XATTR_ITEM,
            ENC_CONTEXT,
            DIR_ITEM,
            DIR_INDEX,
            EXTENT_DATA,
            CSUM_ITEM,
            ROOT_ITEM,
            ROOT_BACKREF,
            REF_ITEM
        ],
        [1, 12, 24, 26, 84, 96, 108, 128, 132, 144, 200]
    );
    assert_eq!(
        [EXT_INLINE, EXT_REG, EXT_PREALLOC, EXT_COMPRESSED],
        [0, 1, 2, 3]
    );
    assert_eq!(
        [INODE_ITEM_SIZE, ROOT_ITEM_SIZE, INLINE_MAX],
        [128, 168, 1024]
    );
    assert_eq!(CSUM_BUCKET, 256);
    assert_eq!(MAX_EXTENT_BLOCKS, 32768);
    assert_eq!(
        [
            FEAT_QUOTA,
            FEAT_VOLUME,
            FEAT_FIXED_SIZE,
            FEAT_BITMAP_CSUM,
            FEAT_ENCRYPT,
            FEAT_RAID56,
            FEAT_TIERS
        ],
        [1, 2, 4, 8, 16, 256, 512]
    );
    assert_eq!(FEAT_KNOWN, 0x31f);
    assert_eq!([ROOT_FLAG_RDONLY, ROOT_FLAG_RECEIVING], [1, 2]);
    assert_eq!(
        [
            FL_COMPR,
            FL_SYNC,
            FL_IMMUTABLE,
            FL_APPEND,
            FL_NODUMP,
            FL_NOATIME,
            FL_NOCOMP,
            FL_ENCRYPT,
            FL_NOCOW
        ],
        [0x4, 0x8, 0x10, 0x20, 0x40, 0x80, 0x400, 0x800, 0x80_0000]
    );
    for (c, id) in [
        (Compression::Zlib, 1),
        (Compression::Zstd, 3),
        (Compression::Lz4, 4),
    ] {
        assert_eq!(c.to_disk(), id);
    }
    let ids: Vec<u8> = Profile::ALL.iter().map(|p| p.id()).collect();
    assert_eq!(ids, (0..=8).collect::<Vec<u8>>());
    assert_eq!(
        [Tier::Any.id(), Tier::Fast.id(), Tier::Slow.id()],
        [0, 1, 2]
    );
    let kinds: Vec<u8> = [
        FileKind::Regular,
        FileKind::Dir,
        FileKind::Symlink,
        FileKind::CharDevice,
        FileKind::BlockDevice,
        FileKind::Fifo,
        FileKind::Socket,
    ]
    .iter()
    .map(|k| k.to_disk())
    .collect();
    assert_eq!(kinds, (1..=7).collect::<Vec<u8>>());
    assert_eq!(eclipsefs::volume::STRIPE_UNIT, 16);
    assert_eq!(name_hash(b"eclipse"), name_hash(b"eclipse"));
    assert_eq!(
        name_hash(b"eclipse"),
        (eclipsefs::crc::crc32c(b"eclipse") as u64)
            | ((eclipsefs::crc::crc32(b"eclipse") as u64) << 32)
    );
}

/// Byte 56, 304 or 312 of the superblock with `bits` set, resealed.
fn with_feature(disk: &MemDisk, at: usize, bits: u64) {
    let mut sb = vec![0u8; BLOCK];
    disk.read_at(0, &mut sb).unwrap();
    let v = u64::from_le_bytes(sb[at..at + 8].try_into().unwrap()) | bits;
    sb[at..at + 8].copy_from_slice(&v.to_le_bytes());
    let c = eclipsefs::crc::crc32c(&sb[..508]);
    sb[508..512].copy_from_slice(&c.to_le_bytes());
    disk.write_at(0, &sb).unwrap();
    // The journal's copy of the superblock would win at mount.
    let j = journal_start(&sb);
    disk.write_at(j * BLOCK as u64, &[0u8; BLOCK]).unwrap();
}

#[test]
fn an_unknown_incompatible_feature_is_not_mounted() {
    let (disk, fs) = fresh(16 * MIB);
    drop(remount(&disk, fs));
    with_feature(&disk, 56, 1 << 40);
    for ro in [false, true] {
        assert_eq!(
            EclipseFS::mount(disk.clone(), ro).err(),
            Some(Error::Unsupported("EclipseFS incompatible feature"))
        );
    }
    assert!(eclipsefs::probe(&*disk).is_err());
}

#[test]
fn an_unknown_read_only_feature_mounts_read_only() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    file(&mut fs, root, "a", b"hola");
    drop(remount(&disk, fs));
    with_feature(&disk, 312, 1 << 5);
    assert_eq!(
        EclipseFS::mount(disk.clone(), false).err(),
        Some(Error::Unsupported("EclipseFS read-only feature"))
    );
    let mut fs = EclipseFS::mount(disk.clone(), true).unwrap();
    let a = fs.lookup(fs.root_ino(), "a").unwrap();
    assert_eq!(read_all(&mut fs, a), b"hola");
    fs.check().unwrap();
}

#[test]
fn an_unknown_compatible_feature_is_ignored_and_kept() {
    let (disk, fs) = fresh(16 * MIB);
    drop(remount(&disk, fs));
    with_feature(&disk, 304, 1 << 7);
    let mut fs = EclipseFS::mount(disk.clone(), false).unwrap();
    fs.set_clock(clock);
    let root = fs.root_ino();
    file(
        &mut fs,
        root,
        "b",
        b"escrito por un driver que no la conoce",
    );
    fs.sync().unwrap();
    fs.check().unwrap();
    drop(fs);
    let mut sb = vec![0u8; BLOCK];
    disk.read_at(0, &mut sb).unwrap();
    assert_eq!(u64::from_le_bytes(sb[304..312].try_into().unwrap()), 1 << 7);
    EclipseFS::mount(disk.clone(), false).unwrap();
}
