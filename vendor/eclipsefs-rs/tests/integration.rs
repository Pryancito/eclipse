#![cfg(feature = "std")]
//! End-to-end behaviour of the driver through its public API: format, mount,
//! the namespace operations the kernel issues, persistence across remounts,
//! ENOSPC and growth.

use std::sync::{Arc, Mutex};

use eclipsefs::device::BlockDevice;
use eclipsefs::{mkfs, EclipseFS, Error, FileKind};

/// RAM disk that can be resized like a partition growing under the image.
struct MemDisk {
    data: Mutex<Vec<u8>>,
}

impl MemDisk {
    fn new(size: usize) -> Arc<Self> {
        Arc::new(Self {
            data: Mutex::new(vec![0xA5; size]),
        })
    }

    fn resize(&self, size: usize) {
        self.data.lock().unwrap().resize(size, 0xA5);
    }

    fn poke(&self, at: usize, val: u8) {
        self.data.lock().unwrap()[at] ^= val;
    }
}

impl BlockDevice for MemDisk {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> eclipsefs::Result<()> {
        let d = self.data.lock().unwrap();
        let o = offset as usize;
        if o + buf.len() > d.len() {
            return Err(Error::Io);
        }
        buf.copy_from_slice(&d[o..o + buf.len()]);
        Ok(())
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> eclipsefs::Result<()> {
        let mut d = self.data.lock().unwrap();
        let o = offset as usize;
        if o + buf.len() > d.len() {
            return Err(Error::Io);
        }
        d[o..o + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn sync(&self) -> eclipsefs::Result<()> {
        Ok(())
    }
    fn size(&self) -> u64 {
        self.data.lock().unwrap().len() as u64
    }
}

const MIB: usize = 1024 * 1024;

fn opts() -> mkfs::MkfsOptions {
    mkfs::MkfsOptions {
        label: "eclipse".into(),
        fsid: [1; 16],
        chunk_uuid: [2; 16],
        dev_uuid: [3; 16],
        subvol_uuid: [4; 16],
        now: (1_700_000_000, 0),
    }
}

fn clock() -> (u64, u32) {
    (1_800_000_000, 5)
}

fn fresh(size: usize) -> (Arc<MemDisk>, EclipseFS) {
    let disk = MemDisk::new(size);
    mkfs::format(&*disk, &opts()).unwrap();
    let mut fs = EclipseFS::mount(disk.clone(), false).unwrap();
    fs.set_clock(clock);
    (disk, fs)
}

fn remount(disk: &Arc<MemDisk>, fs: EclipseFS) -> EclipseFS {
    let mut fs = fs;
    fs.sync().unwrap();
    drop(fs);
    let mut fs = EclipseFS::mount(disk.clone(), false).unwrap();
    fs.set_clock(clock);
    fs
}

fn pattern(len: usize, seed: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 7 + seed * 13 + i / 4096) % 251) as u8)
        .collect()
}

fn read_all(fs: &mut EclipseFS, ino: u64) -> Vec<u8> {
    let size = fs.stat(ino).unwrap().size as usize;
    let mut buf = vec![0u8; size];
    let n = fs.read(ino, 0, &mut buf).unwrap();
    buf.truncate(n);
    buf
}

fn write_all(fs: &mut EclipseFS, ino: u64, off: u64, data: &[u8]) {
    let mut done = 0;
    while done < data.len() {
        let n = fs.write(ino, off + done as u64, &data[done..]).unwrap();
        assert!(n > 0);
        done += n;
    }
}

#[test]
fn empty_filesystem_mounts() {
    let (_disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    let st = fs.stat(root).unwrap();
    assert_eq!(st.kind, FileKind::Dir);
    assert_eq!(st.nlink, 2);
    assert_eq!(st.mtime, (1_700_000_000, 0));
    assert!(fs.readdir(root).unwrap().is_empty());
    assert_eq!(fs.parent(root).unwrap(), root);
    assert_eq!(fs.label(), "eclipse");
    let info = fs.fsinfo();
    assert_eq!(info.total_bytes, 8 * MIB as u64);
    // Journal (256 KiB on a small disk), group headers and three tree nodes.
    assert!(info.bytes_used < 300 * 1024, "{:?}", info);
    assert_eq!(fs.format_version(), 4);
}

#[test]
fn files_and_dirs_survive_remount() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let etc = fs.create(root, "etc", FileKind::Dir, 0o755, 0).unwrap();
    let f = fs
        .create(etc, "hostname", FileKind::Regular, 0o644, 0)
        .unwrap();
    write_all(&mut fs, f, 0, b"eclipse\n");
    let big = fs
        .create(root, "big.bin", FileKind::Regular, 0o600, 0)
        .unwrap();
    let data = pattern(3 * MIB + 123, 1);
    write_all(&mut fs, big, 0, &data);
    let null = fs
        .create(root, "null", FileKind::CharDevice, 0o666, 0x103)
        .unwrap();

    let mut fs = remount(&disk, fs);
    assert_eq!(fs.lookup(root, "etc").unwrap(), etc);
    assert_eq!(fs.lookup(etc, "hostname").unwrap(), f);
    assert_eq!(read_all(&mut fs, f), b"eclipse\n");
    assert_eq!(read_all(&mut fs, big), data);
    let st = fs.stat(big).unwrap();
    assert_eq!(st.mode, 0o100600);
    assert_eq!(st.mtime, clock());
    assert_eq!(st.nbytes, (data.len() as u64).div_ceil(4096) * 4096);
    let st = fs.stat(null).unwrap();
    assert_eq!((st.kind, st.rdev), (FileKind::CharDevice, 0x103));
    assert_eq!(fs.stat(root).unwrap().nlink, 3);
    assert_eq!(fs.parent(etc).unwrap(), root);
    let names: Vec<String> = fs
        .readdir(root)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["etc", "big.bin", "null"]);
    assert_eq!(fs.lookup(root, "nope"), Err(Error::NotFound));
    assert_eq!(fs.lookup(f, "x"), Err(Error::NotDir));
    assert_eq!(fs.read(etc, 0, &mut [0; 4]), Err(Error::IsDir));
}

#[test]
fn sequential_file_is_one_extent_and_reads_at_any_offset() {
    let (_disk, mut fs) = fresh(64 * MIB);
    let root = fs.root_ino();
    let f = fs
        .create(root, "lib.so", FileKind::Regular, 0o755, 0)
        .unwrap();
    let data = pattern(20 * MIB, 2);
    // Small appends, the way a tar extractor writes.
    for chunk in data.chunks(64 * 1024) {
        let off = fs.stat(f).unwrap().size;
        write_all(&mut fs, f, off, chunk);
    }
    for &(off, len) in &[
        (0usize, 1usize),
        (4095, 2),
        (12345, 70000),
        (20 * MIB - 10, 10),
    ] {
        let mut buf = vec![0u8; len];
        assert_eq!(fs.read(f, off as u64, &mut buf).unwrap(), len);
        assert_eq!(&buf[..], &data[off..off + len]);
    }
    let mut past = [0u8; 8];
    assert_eq!(fs.read(f, 20 * MIB as u64, &mut past).unwrap(), 0);
}

#[test]
fn fragmented_files_spill_into_extent_blocks() {
    let (disk, mut fs) = fresh(64 * MIB);
    let root = fs.root_ino();
    let a = fs.create(root, "a", FileKind::Regular, 0o644, 0).unwrap();
    let b = fs.create(root, "b", FileKind::Regular, 0o644, 0).unwrap();
    // Interleaved block-sized appends fragment both files: hundreds of extents.
    let da = pattern(400 * 4096, 3);
    let db = pattern(400 * 4096, 4);
    for i in 0..400 {
        let r = i * 4096..(i + 1) * 4096;
        // Write backwards within each pair so the allocator cannot merge.
        write_all(&mut fs, a, r.start as u64, &da[r.clone()]);
        write_all(&mut fs, b, r.start as u64, &db[r.clone()]);
    }
    let mut fs = remount(&disk, fs);
    assert_eq!(read_all(&mut fs, a), da);
    assert_eq!(read_all(&mut fs, b), db);
    // Shrinking drops most extents and their overflow blocks.
    fs.truncate(a, 10 * 4096 + 1).unwrap();
    let mut fs = remount(&disk, fs);
    assert_eq!(read_all(&mut fs, a), &da[..10 * 4096 + 1]);
    assert_eq!(read_all(&mut fs, b), db);
}

#[test]
fn sparse_files_and_truncate() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = fs
        .create(root, "sparse", FileKind::Regular, 0o644, 0)
        .unwrap();
    write_all(&mut fs, f, 5 * MIB as u64, b"tail");
    let st = fs.stat(f).unwrap();
    assert_eq!(st.size, 5 * MIB as u64 + 4);
    assert_eq!(st.nbytes, 4096);
    let mut buf = vec![0xFFu8; 8192];
    fs.read(f, 4 * MIB as u64, &mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0));

    let g = fs.create(root, "t", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, g, 0, &pattern(10000, 5));
    fs.truncate(g, 100).unwrap();
    fs.truncate(g, 9000).unwrap();
    let back = read_all(&mut fs, g);
    assert_eq!(&back[..100], &pattern(10000, 5)[..100]);
    assert!(
        back[100..].iter().all(|&b| b == 0),
        "stale bytes after truncate"
    );
    let mut fs = remount(&disk, fs);
    assert_eq!(fs.stat(g).unwrap().size, 9000);
    assert_eq!(fs.truncate(root, 0), Err(Error::IsDir));
}

#[test]
fn unlink_frees_space_and_checks_emptiness() {
    let (disk, mut fs) = fresh(32 * MIB);
    let root = fs.root_ino();
    fs.sync().unwrap();
    let base = fs.fsinfo().bytes_used;
    let d = fs.create(root, "d", FileKind::Dir, 0o755, 0).unwrap();
    let f = fs.create(d, "f", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, f, 0, &pattern(5 * MIB, 6));
    assert!(fs.fsinfo().bytes_used >= base + 5 * MIB as u64);
    assert_eq!(fs.unlink(root, "d"), Err(Error::NotEmpty));
    fs.unlink(d, "f").unwrap();
    assert_eq!(fs.stat(f).unwrap_err(), Error::NotFound);
    fs.unlink(root, "d").unwrap();
    assert_eq!(fs.stat(root).unwrap().nlink, 2);
    // The inode table may have grown; data must all be back.
    assert!(
        fs.fsinfo().bytes_used <= base + 4096 * 4,
        "{} vs {}",
        fs.fsinfo().bytes_used,
        base
    );
    let mut fs = remount(&disk, fs);
    assert!(fs.readdir(root).unwrap().is_empty());
    assert_eq!(fs.unlink(root, "d"), Err(Error::NotFound));
}

#[test]
fn hard_links() {
    let (disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "a", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, f, 0, b"shared");
    fs.link(root, "b", f).unwrap();
    assert_eq!(fs.link(root, "b", f), Err(Error::Exists));
    assert_eq!(fs.link(root, "r", root), Err(Error::IsDir));
    assert_eq!(fs.stat(f).unwrap().nlink, 2);
    fs.unlink(root, "a").unwrap();
    let mut fs = remount(&disk, fs);
    assert_eq!(fs.lookup(root, "b").unwrap(), f);
    assert_eq!(fs.stat(f).unwrap().nlink, 1);
    assert_eq!(read_all(&mut fs, f), b"shared");
}

#[test]
fn rename_semantics() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let a = fs.create(root, "a", FileKind::Dir, 0o755, 0).unwrap();
    let b = fs.create(root, "b", FileKind::Dir, 0o755, 0).unwrap();
    let sub = fs.create(a, "sub", FileKind::Dir, 0o755, 0).unwrap();
    let f = fs.create(a, "f", FileKind::Regular, 0o644, 0).unwrap();
    let g = fs.create(b, "g", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, f, 0, b"F");

    // A directory cannot go under itself.
    assert_eq!(fs.rename(root, "a", sub, "x"), Err(Error::Invalid));
    // File over file replaces it.
    fs.rename(a, "f", b, "g").unwrap();
    assert_eq!(fs.lookup(b, "g").unwrap(), f);
    assert_eq!(fs.stat(g).unwrap_err(), Error::NotFound);
    assert_eq!(fs.lookup(a, "f"), Err(Error::NotFound));
    // Kind mismatches.
    assert_eq!(fs.rename(b, "g", a, "sub"), Err(Error::IsDir));
    assert_eq!(fs.rename(a, "sub", b, "g"), Err(Error::NotDir));
    // Directory moves update `..` and both link counts.
    fs.rename(a, "sub", b, "moved").unwrap();
    assert_eq!(fs.parent(sub).unwrap(), b);
    assert_eq!(fs.stat(a).unwrap().nlink, 2);
    assert_eq!(fs.stat(b).unwrap().nlink, 3);
    // Non-empty target directory.
    fs.create(sub, "inner", FileKind::Regular, 0o644, 0)
        .unwrap();
    let empty = fs.create(root, "empty", FileKind::Dir, 0o755, 0).unwrap();
    assert_eq!(fs.rename(root, "empty", b, "moved"), Err(Error::NotEmpty));
    // Empty target directory is replaced.
    fs.rename(b, "moved", root, "empty").unwrap();
    assert_eq!(fs.stat(empty).unwrap_err(), Error::NotFound);
    assert_eq!(fs.parent(sub).unwrap(), root);
    // Same-dir rename.
    fs.rename(root, "empty", root, "renamed").unwrap();

    let mut fs = remount(&disk, fs);
    assert_eq!(fs.lookup(root, "renamed").unwrap(), sub);
    assert!(fs.lookup(sub, "inner").is_ok());
    assert_eq!(fs.stat(root).unwrap().nlink, 5);
    assert_eq!(fs.stat(b).unwrap().nlink, 2);
    assert_eq!(read_all(&mut fs, f), b"F");
}

#[test]
fn symlinks_inline_and_in_blocks() {
    let (disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    let s = fs.symlink(root, "lib64", b"usr/lib").unwrap();
    let long: Vec<u8> = (0..600).map(|i| b'a' + (i % 26) as u8).collect();
    let l = fs.symlink(root, "long", &long).unwrap();
    // The VFS way: create empty, then write the target.
    let v = fs.create(root, "vfs", FileKind::Symlink, 0o777, 0).unwrap();
    fs.write_symlink(v, 0, b"../run").unwrap();
    fs.write_symlink(v, 3, b"var/run").unwrap();
    assert_eq!(fs.write(v, 0, b"x"), Err(Error::Invalid));

    let mut fs = remount(&disk, fs);
    assert_eq!(fs.read_link(s).unwrap(), b"usr/lib");
    assert_eq!(fs.read_link(l).unwrap(), long);
    assert_eq!(fs.read_link(v).unwrap(), b"../var/run");
    assert_eq!(fs.stat(s).unwrap().kind, FileKind::Symlink);
    assert_eq!(fs.stat(s).unwrap().size, 7);
    // Long target shrinks back to inline and frees its block.
    fs.write_symlink(l, 0, b"short").unwrap();
    assert_eq!(&fs.read_link(l).unwrap()[..5], b"short");
    assert_eq!(fs.read_link(root), Err(Error::Invalid));
}

#[test]
fn full_device_gives_partial_writes_then_enospc() {
    let (disk, mut fs) = fresh(2 * MIB);
    let root = fs.root_ino();
    let f = fs
        .create(root, "fill", FileKind::Regular, 0o644, 0)
        .unwrap();
    let data = pattern(4 * MIB, 7);
    let mut n = fs.write(f, 0, &data).unwrap();
    assert!(n > 0 && n < data.len(), "n = {}", n);
    // Committing frees the blocks old tree nodes held, so a retry can
    // still place a little more (as on btrfs); it runs dry quickly.
    let mut tries = 0;
    loop {
        match fs.write(f, n as u64, &data[n..]) {
            Ok(m) => {
                assert!(m > 0 && n + m < data.len());
                n += m;
            }
            Err(e) => {
                assert_eq!(e, Error::NoSpace);
                break;
            }
        }
        tries += 1;
        assert!(tries < 8, "never ran out of space");
    }
    let mut fs = remount(&disk, fs);
    assert_eq!(read_all(&mut fs, f), &data[..n]);
    // Space comes back.
    fs.unlink(root, "fill").unwrap();
    let g = fs
        .create(root, "again", FileKind::Regular, 0o644, 0)
        .unwrap();
    write_all(&mut fs, g, 0, &data[..MIB]);
}

#[test]
fn grows_to_the_device_across_groups() {
    let disk = MemDisk::new(4 * MIB);
    mkfs::format(&*disk, &opts()).unwrap();
    {
        let mut fs = EclipseFS::mount(disk.clone(), false).unwrap();
        let root = fs.root_ino();
        let f = fs
            .create(root, "before", FileKind::Regular, 0o644, 0)
            .unwrap();
        write_all(&mut fs, f, 0, &pattern(MIB, 8));
        assert!(!fs.grow_to_device().unwrap());
        fs.sync().unwrap();
    }
    disk.resize(300 * MIB);
    let mut fs = EclipseFS::mount(disk.clone(), false).unwrap();
    assert!(fs.grow_to_device().unwrap());
    assert_eq!(fs.fsinfo().total_bytes, 300 * MIB as u64);
    assert!(!fs.grow_to_device().unwrap());
    let root = fs.root_ino();
    let big = fs.create(root, "big", FileKind::Regular, 0o644, 0).unwrap();
    let data = pattern(200 * MIB, 9);
    write_all(&mut fs, big, 0, &data);
    let mut fs = remount(&disk, fs);
    assert_eq!(fs.fsinfo().total_bytes, 300 * MIB as u64);
    assert_eq!(read_all(&mut fs, big), data);
    let before = fs.lookup(root, "before").unwrap();
    assert_eq!(read_all(&mut fs, before), pattern(MIB, 8));
}

#[test]
fn big_directories_and_inode_table_growth() {
    let (disk, mut fs) = fresh(64 * MIB);
    let root = fs.root_ino();
    let d = fs.create(root, "lib", FileKind::Dir, 0o755, 0).unwrap();
    let n = 3000;
    for i in 0..n {
        let f = fs
            .create(
                d,
                &format!("libfoo-{i:05}.so.{}", i % 7),
                FileKind::Regular,
                0o644,
                0,
            )
            .unwrap();
        write_all(&mut fs, f, 0, format!("file {i}").as_bytes());
    }
    // Delete two thirds: the directory compacts itself.
    let size_full = fs.stat(d).unwrap().size;
    for i in (0..n).filter(|i| i % 3 != 0) {
        fs.unlink(d, &format!("libfoo-{i:05}.so.{}", i % 7))
            .unwrap();
    }
    assert!(fs.stat(d).unwrap().size < size_full * 2 / 3);
    let mut fs = remount(&disk, fs);
    let entries = fs.readdir(d).unwrap();
    assert_eq!(entries.len(), n / 3);
    for i in (0..n).step_by(3) {
        let ino = fs
            .lookup(d, &format!("libfoo-{i:05}.so.{}", i % 7))
            .unwrap();
        assert_eq!(read_all(&mut fs, ino), format!("file {i}").as_bytes());
    }
    assert_eq!(fs.lookup(d, "libfoo-00001.so.1"), Err(Error::NotFound));
}

#[test]
fn names_are_validated() {
    let (_disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    for bad in ["", ".", "..", "a/b", "a\0b"] {
        assert_eq!(
            fs.create(root, bad, FileKind::Regular, 0o644, 0),
            Err(Error::Invalid)
        );
    }
    let long = "x".repeat(256);
    assert_eq!(
        fs.create(root, &long, FileKind::Regular, 0o644, 0),
        Err(Error::Invalid)
    );
    fs.create(root, &"y".repeat(255), FileKind::Regular, 0o644, 0)
        .unwrap();
    fs.create(root, "dup", FileKind::Regular, 0o644, 0).unwrap();
    assert_eq!(
        fs.create(root, "dup", FileKind::Dir, 0o755, 0),
        Err(Error::Exists)
    );
    fs.create(root, "ñandú ☀", FileKind::Regular, 0o644, 0)
        .unwrap();
}

#[test]
fn set_attr_and_times() {
    let (disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o600, 0).unwrap();
    fs.set_attr(
        f,
        Some(0o4755),
        Some(1000),
        Some(100),
        Some((5, 6)),
        Some((7, 8)),
    )
    .unwrap();
    let mut fs = remount(&disk, fs);
    let st = fs.stat(f).unwrap();
    assert_eq!(st.mode, 0o104755);
    assert_eq!((st.uid, st.gid), (1000, 100));
    assert_eq!(st.atime, (5, 6));
    assert_eq!(st.mtime, (7, 8));
    assert_eq!(st.ctime, clock());
}

#[test]
fn read_only_mount_refuses_changes() {
    let (disk, fs) = fresh(8 * MIB);
    drop(fs);
    let mut fs = EclipseFS::mount(disk.clone(), true).unwrap();
    let root = fs.root_ino();
    assert!(matches!(
        fs.create(root, "x", FileKind::Regular, 0o644, 0),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(fs.grow_to_device(), Err(Error::Unsupported(_))));
    fs.sync().unwrap();
}

fn victim(disk: &Arc<MemDisk>) -> u64 {
    let mut fs = EclipseFS::mount(disk.clone(), false).unwrap();
    let root = fs.root_ino();
    let ino = fs
        .create(root, "victim", FileKind::Regular, 0o644, 0)
        .unwrap();
    fs.sync().unwrap();
    ino
}

fn superblock_damage_is_detected(disk: &Arc<MemDisk>) {
    // Flip a bit in the superblock's label.
    disk.poke(110, 1);
    assert!(matches!(
        EclipseFS::mount(disk.clone(), false),
        Err(Error::Corrupt(_))
    ));
    disk.poke(110, 1);
}

#[test]
fn corruption_is_detected() {
    let disk = MemDisk::new(8 * MIB);
    assert!(matches!(
        EclipseFS::mount(disk.clone(), false),
        Err(Error::BadSuperblock)
    ));
    mkfs::format(&*disk, &opts()).unwrap();
    let ino = victim(&disk);
    superblock_damage_is_detected(&disk);
    // Flip a bit in the tree leaf that holds the victim's items (the only
    // node naming it), next to the name.
    let mut image = vec![0u8; 8 * MIB];
    disk.read_at(0, &mut image).unwrap();
    let leaf = image
        .chunks(4096)
        .position(|b| &b[..4] == b"EFSN" && b.windows(6).any(|w| w == b"victim"))
        .expect("leaf naming the victim");
    let at = image[leaf * 4096..(leaf + 1) * 4096]
        .windows(6)
        .position(|w| w == b"victim")
        .unwrap();
    disk.poke(leaf * 4096 + at, 4);
    // On a filesystem this small the root directory shares the leaf, so the
    // mount itself may already refuse it.
    match EclipseFS::mount(disk.clone(), false) {
        Err(e) => assert_eq!(e, Error::Corrupt("tree node checksum")),
        Ok(mut fs) => {
            assert!(matches!(fs.stat(ino), Err(Error::Corrupt(_))));
            assert!(fs.check().is_err());
        }
    }
}

#[test]
fn corruption_is_detected_v3() {
    let disk = MemDisk::new(8 * MIB);
    mkfs::format_v3(&*disk, &opts()).unwrap();
    let ino = victim(&disk);
    superblock_damage_is_detected(&disk);
    // Find the victim's inode record through the inode table's first extent
    // and flip a bit in its size.
    let mut sb = vec![0u8; 4096];
    disk.read_at(0, &mut sb).unwrap();
    let itable_first_phys = u64::from_le_bytes(sb[168 + 96 + 8..168 + 96 + 16].try_into().unwrap());
    // The last transaction is still in the journal and would repair the
    // record on mount: drop it first.
    let jstart = u64::from_le_bytes(sb[432..440].try_into().unwrap());
    disk.write_at(jstart * 4096, &[0u8; 4096]).unwrap();
    disk.poke((itable_first_phys * 4096 + ino * 256 + 25) as usize, 4);
    let mut fs = EclipseFS::mount(disk.clone(), false).unwrap();
    assert!(matches!(fs.stat(ino), Err(Error::Corrupt(_))));
}

#[test]
fn reformatting_a_btrfs_disk_clears_its_signature() {
    let disk = MemDisk::new(80 * MIB);
    disk.write_at(0x10040, b"_BHRfS_M").unwrap();
    disk.write_at(0x400_0040, b"_BHRfS_M").unwrap();
    mkfs::format(&*disk, &opts()).unwrap();
    let mut magic = [0u8; 8];
    disk.read_at(0x10040, &mut magic).unwrap();
    assert_ne!(&magic, b"_BHRfS_M");
    disk.read_at(0x400_0040, &mut magic).unwrap();
    assert_ne!(&magic, b"_BHRfS_M");
    EclipseFS::mount(disk, true).unwrap();
}

#[test]
fn file_device_image_roundtrip() {
    use eclipsefs::device::FileDevice;
    let path = std::env::temp_dir().join(format!("eclipsefs-test-{}.img", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    file.set_len(16 * MIB as u64).unwrap();
    let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(file).unwrap());
    mkfs::format(&*dev, &opts()).unwrap();
    {
        let mut fs = EclipseFS::mount(dev.clone(), false).unwrap();
        let root = fs.root_ino();
        let f = fs
            .create(root, "hello", FileKind::Regular, 0o644, 0)
            .unwrap();
        write_all(&mut fs, f, 0, b"hola mundo");
        fs.sync().unwrap();
    }
    let mut fs = EclipseFS::mount(dev, true).unwrap();
    let root = fs.root_ino();
    let f = fs.lookup(root, "hello").unwrap();
    assert_eq!(read_all(&mut fs, f), b"hola mundo");
    let _ = std::fs::remove_file(path);
}
