#![cfg(feature = "std")]
//! Subvolumes, snapshots, reflinks and deduplication (format v0.4).

mod common;

use common::*;
use eclipsefs::{EclipseFS, Error, FileKind};

const RO: Error = Error::Unsupported("read-only subvolume");

fn used(fs: &EclipseFS) -> u64 {
    fs.fsinfo().bytes_used
}

fn file(fs: &mut EclipseFS, dir: u64, name: &str, data: &[u8]) -> u64 {
    let f = fs.create(dir, name, FileKind::Regular, 0o644, 0).unwrap();
    write_all(fs, f, 0, data);
    f
}

fn read_path(fs: &mut EclipseFS, dir: u64, name: &str) -> Vec<u8> {
    let f = fs.lookup(dir, name).unwrap();
    read_all(fs, f)
}

fn names(fs: &mut EclipseFS, dir: u64) -> Vec<String> {
    let mut v: Vec<String> = fs
        .readdir(dir)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    v.sort();
    v
}

fn clean(fs: &mut EclipseFS) {
    fs.check().unwrap();
    let r = fs.scrub().unwrap();
    assert!(r.is_clean(), "{:?}", r);
}

#[test]
fn subvolumes_are_directories_with_their_own_inodes() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let sv = fs.create_subvolume(root, "home").unwrap();
    assert_ne!(fs.subvolume_of(sv), fs.subvolume_of(root));
    assert_eq!(fs.lookup(root, "home").unwrap(), sv);
    let e = fs.readdir(root).unwrap();
    assert_eq!(
        (e[0].name.as_str(), e[0].ino, e[0].kind),
        ("home", sv, FileKind::Dir)
    );
    assert_eq!(fs.stat(sv).unwrap().kind, FileKind::Dir);
    assert_eq!(fs.parent(sv).unwrap(), root);
    let docs = fs.create(sv, "docs", FileKind::Dir, 0o755, 0).unwrap();
    let f = file(&mut fs, docs, "a.txt", b"hello");
    assert_eq!(fs.subvolume_of(f), fs.subvolume_of(sv));
    assert_eq!(fs.stat(f).unwrap().ino, f);
    assert_eq!(fs.parent(docs).unwrap(), sv);
    // Inode numbers are per subvolume; handles are not.
    let g = file(&mut fs, root, "top.txt", b"top");
    assert_eq!(g, 257);
    assert_eq!(f & 0xFF_FFFF_FFFF, 258);
    assert_ne!(f, 258);
    clean(&mut fs);
    let mut fs = remount(&disk, fs);
    let sv = fs.lookup(root, "home").unwrap();
    let docs = fs.lookup(sv, "docs").unwrap();
    assert_eq!(read_path(&mut fs, docs, "a.txt"), b"hello");
    let list = fs.list_subvolumes().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(
        (list[1].name.as_str(), list[1].parent, list[1].dir),
        ("home", 5, root)
    );
    clean(&mut fs);
}

#[test]
fn links_and_renames_stay_inside_a_subvolume() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let sv = fs.create_subvolume(root, "sv").unwrap();
    let f = file(&mut fs, root, "f", b"x");
    let cross = Error::Unsupported("cross-subvolume link or rename");
    assert_eq!(fs.link(sv, "f", f), Err(cross));
    assert_eq!(fs.rename(root, "f", sv, "f"), Err(cross));
    // A subvolume can be renamed within its parent.
    let d = fs.create(root, "d", FileKind::Dir, 0o755, 0).unwrap();
    fs.rename(root, "sv", d, "moved").unwrap();
    assert_eq!(fs.lookup(d, "moved").unwrap(), sv);
    assert_eq!(fs.parent(sv).unwrap(), d);
    assert_eq!(fs.list_subvolumes().unwrap()[1].name, "moved");
    // It cannot replace or be replaced by another directory with content.
    let other = fs.create(d, "other", FileKind::Dir, 0o755, 0).unwrap();
    file(&mut fs, other, "x", b"1");
    assert_eq!(fs.rename(d, "moved", d, "other"), Err(Error::NotEmpty));
    assert_eq!(fs.rename(d, "other", d, "moved"), Err(Error::NotEmpty));
    clean(&mut fs);
}

#[test]
fn removing_subvolumes() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let base = used(&fs);
    let sv = fs.create_subvolume(root, "sv").unwrap();
    file(&mut fs, sv, "big", &pattern(400_000, 1));
    // rmdir only takes an empty subvolume.
    assert_eq!(fs.unlink(root, "sv"), Err(Error::NotEmpty));
    let inner = fs.create_subvolume(sv, "inner").unwrap();
    // Nested subvolumes block deleting the outer one.
    assert_eq!(fs.delete_subvolume(root, "sv"), Err(Error::NotEmpty));
    fs.unlink(sv, "inner").unwrap();
    assert_eq!(fs.stat(inner).err(), Some(Error::NotFound));
    fs.delete_subvolume(root, "sv").unwrap();
    assert_eq!(fs.lookup(root, "sv"), Err(Error::NotFound));
    assert_eq!(fs.stat(sv).err(), Some(Error::NotFound));
    assert_eq!(fs.delete_subvolume(root, "sv"), Err(Error::NotFound));
    fs.sync().unwrap();
    assert!(used(&fs) <= base + 4 * 4096, "{} vs {}", used(&fs), base);
    assert_eq!(fs.list_subvolumes().unwrap().len(), 1);
    file(&mut fs, root, "plain", b"p");
    assert_eq!(fs.delete_subvolume(root, "plain"), Err(Error::Invalid));
    clean(&mut fs);
}

#[test]
fn snapshots_are_isolated_both_ways() {
    let (disk, mut fs) = fresh(32 * MIB);
    let root = fs.root_ino();
    let sv = fs.create_subvolume(root, "data").unwrap();
    let d = fs.create(sv, "dir", FileKind::Dir, 0o755, 0).unwrap();
    let a = pattern(300_000, 1);
    let b = pattern(5_000, 2);
    file(&mut fs, d, "a", &a);
    file(&mut fs, sv, "b", &b);
    fs.symlink(sv, "l", b"dir/a").unwrap();
    let before = used(&fs);
    let snap = fs.snapshot(sv, root, "snap", false).unwrap();
    fs.sync().unwrap();
    // A snapshot costs a few tree nodes, not a copy of the data.
    assert!(used(&fs) < before + 8 * 4096, "{} -> {}", before, used(&fs));
    assert_eq!(names(&mut fs, snap), ["b", "dir", "l"]);
    // Change the source every way.
    let fa = fs.lookup(d, "a").unwrap();
    write_all(&mut fs, fa, 1000, &[0xEE; 9000]);
    fs.truncate(fa, 100_000).unwrap();
    fs.unlink(sv, "b").unwrap();
    file(&mut fs, sv, "new", b"only in source");
    // The snapshot still has the old contents.
    let sd = fs.lookup(snap, "dir").unwrap();
    assert_eq!(read_path(&mut fs, sd, "a"), a);
    assert_eq!(read_path(&mut fs, snap, "b"), b);
    assert_eq!(names(&mut fs, snap), ["b", "dir", "l"]);
    let sl = fs.lookup(snap, "l").unwrap();
    assert_eq!(fs.read_link(sl).unwrap(), b"dir/a");
    // And changing the snapshot leaves the source alone.
    let sa = fs.lookup(sd, "a").unwrap();
    write_all(&mut fs, sa, 0, b"SNAP");
    fs.unlink(sd, "a").unwrap();
    let mut want = a.clone();
    want[1000..10_000].fill(0xEE);
    want.truncate(100_000);
    assert_eq!(read_path(&mut fs, d, "a"), want);
    clean(&mut fs);
    let mut fs = remount(&disk, fs);
    let sv = fs.lookup(root, "data").unwrap();
    let snap = fs.lookup(root, "snap").unwrap();
    let d = fs.lookup(sv, "dir").unwrap();
    assert_eq!(read_path(&mut fs, d, "a"), want);
    assert_eq!(names(&mut fs, snap), ["b", "dir", "l"]);
    clean(&mut fs);
    let info = fs.list_subvolumes().unwrap();
    let s = info.iter().find(|s| s.name == "snap").unwrap();
    assert_eq!(s.snapshot_of, fs.subvolume_of(sv));
    // Dropping the source keeps the snapshot whole.
    fs.delete_subvolume(root, "data").unwrap();
    assert_eq!(read_path(&mut fs, snap, "b"), b);
    clean(&mut fs);
    fs.delete_subvolume(root, "snap").unwrap();
    clean(&mut fs);
}

#[test]
fn read_only_snapshots() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = file(&mut fs, root, "f", &pattern(20_000, 3));
    let snap = fs.snapshot(root, root, "ro", true).unwrap();
    let sf = fs.lookup(snap, "f").unwrap();
    assert_eq!(read_all(&mut fs, sf), pattern(20_000, 3));
    assert_eq!(fs.write(sf, 0, b"x"), Err(RO));
    assert_eq!(fs.truncate(sf, 0), Err(RO));
    assert_eq!(fs.create(snap, "n", FileKind::Regular, 0o644, 0), Err(RO));
    assert_eq!(fs.unlink(snap, "f"), Err(RO));
    assert_eq!(
        fs.set_attr(sf, Some(0o600), None, None, None, None),
        Err(RO)
    );
    // A snapshot of `/` inside `/` does not contain itself.
    assert_eq!(names(&mut fs, snap), ["f"]);
    assert!(fs.list_subvolumes().unwrap()[1].read_only);
    let id = fs.subvolume_of(snap);
    fs.set_subvolume_read_only(id, false).unwrap();
    write_all(&mut fs, sf, 0, b"now writable");
    assert_eq!(read_all(&mut fs, f), pattern(20_000, 3));
    clean(&mut fs);
}

#[test]
fn nested_subvolumes_are_left_out_of_snapshots() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let outer = fs.create_subvolume(root, "outer").unwrap();
    let d = fs.create(outer, "d", FileKind::Dir, 0o755, 0).unwrap();
    let inner = fs.create_subvolume(d, "inner").unwrap();
    file(&mut fs, inner, "x", b"inner file");
    file(&mut fs, d, "y", b"outer file");
    let snap = fs.snapshot(outer, root, "snap", false).unwrap();
    let sd = fs.lookup(snap, "d").unwrap();
    assert_eq!(names(&mut fs, sd), ["y"]);
    assert_eq!(names(&mut fs, d), ["inner", "y"]);
    clean(&mut fs);
    // Snapshots of snapshots.
    let s2 = fs.snapshot(snap, root, "snap2", true).unwrap();
    let s2d = fs.lookup(s2, "d").unwrap();
    assert_eq!(read_path(&mut fs, s2d, "y"), b"outer file");
    clean(&mut fs);
}

#[test]
fn default_subvolume_is_mounted_as_root() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let sv = fs.create_subvolume(root, "rootfs").unwrap();
    file(&mut fs, sv, "etc", b"config");
    let id = fs.subvolume_of(sv);
    fs.set_default_subvolume(id).unwrap();
    assert_eq!(
        fs.delete_subvolume(root, "rootfs"),
        Err(Error::Unsupported("deleting the default subvolume"))
    );
    drop(fs);
    let mut fs = EclipseFS::mount(disk.clone(), false).unwrap();
    let r = fs.root_ino();
    assert_eq!(r, sv);
    assert_eq!(fs.parent(r).unwrap(), r);
    assert_eq!(read_path(&mut fs, r, "etc"), b"config");
    clean(&mut fs);
    fs.set_default_subvolume(5).unwrap();
    drop(fs);
    let fs = EclipseFS::mount(disk, false).unwrap();
    assert_eq!(fs.root_ino(), root);
}

#[test]
fn reflinks_share_blocks_until_written() {
    let (disk, mut fs) = fresh(32 * MIB);
    let root = fs.root_ino();
    let data = pattern(3 * MIB + 1234, 4);
    let src = file(&mut fs, root, "src", &data);
    fs.sync().unwrap();
    let base = used(&fs);
    let dst = fs.create(root, "dst", FileKind::Regular, 0o644, 0).unwrap();
    assert_eq!(
        fs.clone_range(src, 0, dst, 0, 0).unwrap(),
        data.len() as u64
    );
    fs.sync().unwrap();
    assert!(used(&fs) < base + 8 * 4096, "{} -> {}", base, used(&fs));
    assert_eq!(read_all(&mut fs, dst), data);
    assert_eq!(fs.stat(dst).unwrap().size, data.len() as u64);
    // Writing either copy leaves the other alone.
    write_all(&mut fs, dst, 4096 * 10 + 7, b"changed");
    write_all(&mut fs, src, 0, b"SRC");
    let mut want_dst = data.clone();
    want_dst[4096 * 10 + 7..4096 * 10 + 14].copy_from_slice(b"changed");
    let mut want_src = data.clone();
    want_src[..3].copy_from_slice(b"SRC");
    assert_eq!(read_all(&mut fs, dst), want_dst);
    assert_eq!(read_all(&mut fs, src), want_src);
    clean(&mut fs);
    // A range into the middle of another file, and across subvolumes.
    let sv = fs.create_subvolume(root, "sv").unwrap();
    let other = file(&mut fs, sv, "other", &pattern(100_000, 5));
    fs.clone_range(src, 8192, other, 4096, 16384).unwrap();
    let mut want = pattern(100_000, 5);
    want[4096..4096 + 16384].copy_from_slice(&want_src[8192..8192 + 16384]);
    assert_eq!(read_all(&mut fs, other), want);
    let mut fs = remount(&disk, fs);
    assert_eq!(read_all(&mut fs, other), want);
    clean(&mut fs);
    // Deleting the source frees only what nobody else maps.
    fs.unlink(root, "src").unwrap();
    assert_eq!(read_all(&mut fs, dst), want_dst);
    clean(&mut fs);
}

#[test]
fn reflink_arguments_are_checked() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let a = file(&mut fs, root, "a", &pattern(20_000, 1));
    let b = file(&mut fs, root, "b", &pattern(30_000, 2));
    let d = fs.create(root, "d", FileKind::Dir, 0o755, 0).unwrap();
    assert_eq!(fs.clone_range(a, 1, b, 0, 4096), Err(Error::Invalid));
    assert_eq!(fs.clone_range(a, 0, b, 1, 4096), Err(Error::Invalid));
    // An unaligned length only to the end of both files.
    assert_eq!(fs.clone_range(a, 0, b, 0, 5000), Err(Error::Invalid));
    assert_eq!(fs.clone_range(a, 16384, b, 0, 0), Err(Error::Invalid));
    assert_eq!(fs.clone_range(a, 0, b, 0, 30_000), Err(Error::Invalid));
    assert_eq!(fs.clone_range(a, 0, d, 0, 4096), Err(Error::IsDir));
    assert_eq!(fs.clone_range(a, 0, a, 4096, 8192), Err(Error::Invalid));
    // Non-overlapping ranges of one file are fine.
    fs.clone_range(a, 0, a, 8192, 8192).unwrap();
    let mut want = pattern(20_000, 1);
    let head = want[..8192].to_vec();
    want[8192..16384].copy_from_slice(&head);
    assert_eq!(read_all(&mut fs, a), want);
    // The tail to the end of both files.
    fs.clone_range(a, 16384, b, 28672, 0).unwrap();
    assert_eq!(fs.stat(b).unwrap().size, 28672 + 20_000 - 16384);
    // A small (inline) source is copied.
    let s = file(&mut fs, root, "small", b"tiny file");
    let t = fs.create(root, "t", FileKind::Regular, 0o644, 0).unwrap();
    assert_eq!(fs.clone_range(s, 0, t, 0, 0).unwrap(), 9);
    assert_eq!(read_all(&mut fs, t), b"tiny file");
    clean(&mut fs);
}

#[test]
fn dedupe_merges_identical_ranges_only() {
    let (_disk, mut fs) = fresh(32 * MIB);
    let root = fs.root_ino();
    let data = pattern(2 * MIB, 6);
    let a = file(&mut fs, root, "a", &data);
    let b = file(&mut fs, root, "b", &data);
    let mut other = data.clone();
    other[100] ^= 1;
    let c = file(&mut fs, root, "c", &other);
    fs.sync().unwrap();
    let before = used(&fs);
    assert_eq!(fs.dedupe_range(a, 0, c, 0, data.len() as u64).unwrap(), 0);
    assert_eq!(
        fs.dedupe_range(a, 0, b, 0, data.len() as u64).unwrap(),
        data.len() as u64
    );
    fs.sync().unwrap();
    assert!(
        used(&fs) + MIB as u64 <= before,
        "{} -> {}",
        before,
        used(&fs)
    );
    assert_eq!(read_all(&mut fs, b), data);
    assert_eq!(read_all(&mut fs, c), other);
    clean(&mut fs);
}

#[test]
fn v3_has_no_subvolumes() {
    let (_disk, mut fs) = fresh_v3(8 * MIB);
    let root = fs.root_ino();
    assert_eq!(
        fs.create_subvolume(root, "x"),
        Err(Error::Unsupported("needs format v0.4"))
    );
}

#[test]
fn a_read_only_flag_change_alone_is_committed() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let sv = fs.create_subvolume(root, "home").unwrap();
    let id = fs.subvolume_of(sv);
    fs.sync().unwrap();
    fs.set_subvolume_read_only(id, true).unwrap();
    fs.sync().unwrap();
    let mut fs = remount(&disk, fs);
    let list = fs.list_subvolumes().unwrap();
    assert!(list.iter().find(|s| s.id == id).unwrap().read_only);
    assert_eq!(
        fs.create(sv, "x", FileKind::Regular, 0o644, 0).unwrap_err(),
        RO
    );
}
