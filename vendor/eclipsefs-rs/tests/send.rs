#![cfg(feature = "std")]
//! send/receive (format v0.4): full and incremental streams between
//! filesystems, reflinks kept, and broken streams leaving nothing behind.

mod common;

use std::collections::BTreeMap;

use common::*;
use eclipsefs::cowfmt::{FALLOC_FL_KEEP_SIZE, FL_NOATIME, FL_NOCOW, INO_MASK};
use eclipsefs::{Compression, EclipseFS, Error, FileKind};

/// Everything a path shows: kind, mode, owner, size, links, device,
/// mtime, local inode number, flags, xattrs, contents.
#[derive(Debug, PartialEq)]
struct Node {
    kind: FileKind,
    mode: u32,
    owner: (u32, u32),
    size: u64,
    nlink: u32,
    rdev: u64,
    mtime: (u64, u32),
    ino: u64,
    flags: u32,
    xattrs: Vec<(String, Vec<u8>)>,
    data: Vec<u8>,
}

fn tree(fs: &mut EclipseFS, top: u64) -> BTreeMap<String, Node> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(top, String::from("."))];
    while let Some((dir, path)) = stack.pop() {
        for e in fs.readdir(dir).unwrap() {
            let p = format!("{}/{}", path, e.name);
            let st = fs.stat(e.ino).unwrap();
            let data = match e.kind {
                FileKind::Dir => {
                    stack.push((e.ino, p.clone()));
                    Vec::new()
                }
                FileKind::Symlink => fs.read_link(e.ino).unwrap(),
                FileKind::Regular => read_all(fs, e.ino),
                _ => Vec::new(),
            };
            let xattrs = fs
                .list_xattrs(e.ino)
                .unwrap()
                .into_iter()
                .map(|n| {
                    let v = fs.get_xattr(e.ino, &n).unwrap();
                    (n, v)
                })
                .collect();
            let node = Node {
                kind: st.kind,
                mode: st.mode,
                owner: (st.uid, st.gid),
                size: st.size,
                nlink: st.nlink,
                rdev: st.rdev,
                mtime: st.mtime,
                ino: e.ino & INO_MASK,
                flags: fs.get_flags(e.ino).unwrap(),
                xattrs,
                data,
            };
            out.insert(p, node);
        }
    }
    out
}

fn text(len: usize, seed: usize) -> Vec<u8> {
    let mut v = Vec::new();
    let mut i = 0;
    while v.len() < len {
        v.extend_from_slice(format!("{}:{} some text to send\n", seed, i).as_bytes());
        i += 1;
    }
    v.truncate(len);
    v
}

fn file(fs: &mut EclipseFS, dir: u64, name: &str, data: &[u8]) -> u64 {
    let f = fs.create(dir, name, FileKind::Regular, 0o644, 0).unwrap();
    write_all(fs, f, 0, data);
    f
}

fn send(fs: &mut EclipseFS, snap: u64, parent: Option<u64>) -> Vec<u8> {
    let mut v = Vec::new();
    fs.send(snap, parent, &mut |b| {
        v.extend_from_slice(b);
        Ok(())
    })
    .unwrap();
    v
}

fn receive(fs: &mut EclipseFS, dir: u64, stream: &[u8]) -> Result<u64, Error> {
    let mut pos = 0;
    fs.receive(dir, &mut |buf| {
        let end = pos + buf.len();
        if end > stream.len() {
            return Err(Error::Corrupt("short stream"));
        }
        buf.copy_from_slice(&stream[pos..end]);
        pos = end;
        Ok(())
    })
}

fn clean(fs: &mut EclipseFS) {
    fs.check().unwrap();
    let r = fs.scrub().unwrap();
    assert!(r.is_clean(), "{:?}", r);
}

/// A subvolume with a bit of everything in it.
fn populate(fs: &mut EclipseFS, sv: u64) {
    let docs = fs.create(sv, "docs", FileKind::Dir, 0o750, 0).unwrap();
    let deep = fs.create(docs, "deep", FileKind::Dir, 0o700, 0).unwrap();
    file(fs, docs, "small", b"inline bytes");
    file(fs, deep, "big", &text(700_000, 1));
    let sparse = fs
        .create(sv, "sparse", FileKind::Regular, 0o600, 0)
        .unwrap();
    write_all(fs, sparse, 5 << 20, b"far away");
    write_all(fs, sparse, 12_345, b"near");
    fs.symlink(sv, "link", b"docs/deep/big").unwrap();
    let h = file(fs, sv, "hard", b"two names");
    fs.link(docs, "hard2", h).unwrap();
    fs.create(sv, "fifo", FileKind::Fifo, 0o644, 0).unwrap();
    fs.create(sv, "tty", FileKind::CharDevice, 0o620, 0x0405)
        .unwrap();
    fs.set_xattr(h, "user.color", b"blue", 0).unwrap();
    fs.set_xattr(docs, "security.selinux", b"ctx", 0).unwrap();
    fs.set_attr(
        h,
        Some(0o640),
        Some(1000),
        Some(100),
        None,
        Some((1_600_000_000, 7)),
    )
    .unwrap();
    let n = fs
        .create(sv, "nocow.img", FileKind::Regular, 0o644, 0)
        .unwrap();
    fs.set_flags(n, FL_NOCOW | FL_NOATIME).unwrap();
    write_all(fs, n, 0, &text(50_000, 2));
    let p = file(fs, sv, "prealloc", b"start");
    fs.fallocate(p, FALLOC_FL_KEEP_SIZE, 0, 64 * 1024).unwrap();
    let z = fs
        .create(sv, "packed", FileKind::Regular, 0o644, 0)
        .unwrap();
    fs.set_compression(z, Some(Compression::Zstd)).unwrap();
    write_all(fs, z, 0, &text(400_000, 3));
}

#[test]
fn a_full_stream_recreates_the_snapshot_on_another_filesystem() {
    let (_a, mut src) = fresh(64 * MIB);
    let root = src.root_ino();
    let sv = src.create_subvolume(root, "data").unwrap();
    populate(&mut src, sv);
    let snap = src.snapshot(sv, root, "data.1", true).unwrap();
    let stream = send(&mut src, snap, None);
    let want = tree(&mut src, snap);

    let (b, mut dst) = fresh(64 * MIB);
    let droot = dst.root_ino();
    let backups = dst
        .create(droot, "backups", FileKind::Dir, 0o755, 0)
        .unwrap();
    let got = receive(&mut dst, backups, &stream).unwrap();
    assert_eq!(dst.lookup(backups, "data.1").unwrap(), got);
    assert_eq!(tree(&mut dst, got), want);
    clean(&mut dst);
    // Read-only, and it remembers where it came from.
    assert!(dst.create(got, "x", FileKind::Regular, 0o644, 0).is_err());
    let src_info = src.list_subvolumes().unwrap();
    let s = src_info.iter().find(|s| s.name == "data.1").unwrap();
    let mut dst = remount(&b, dst);
    let info = dst.list_subvolumes().unwrap();
    let r = info.iter().find(|i| i.name == "data.1").unwrap();
    assert!(r.read_only);
    assert_eq!(r.received_uuid, s.uuid);
    assert_ne!(r.uuid, s.uuid);
    assert_eq!(tree(&mut dst, got), want);
    clean(&mut dst);
}

#[test]
fn incremental_streams_carry_only_the_changes() {
    let (_a, mut src) = fresh(64 * MIB);
    let root = src.root_ino();
    let sv = src.create_subvolume(root, "data").unwrap();
    populate(&mut src, sv);
    // Lots of untouched files.
    let bulk = src.create(sv, "bulk", FileKind::Dir, 0o755, 0).unwrap();
    for i in 0..300 {
        file(&mut src, bulk, &format!("f{}", i), &text(3000 + i, i));
    }
    let s1 = src.snapshot(sv, root, "s1", true).unwrap();
    let full = send(&mut src, s1, None);

    let (_b, mut dst) = fresh(64 * MIB);
    let droot = dst.root_ino();
    receive(&mut dst, droot, &full).unwrap();

    // Renames (a swap included), deletes, new files, overwrites, truncates,
    // appends, hard links, xattrs, modes.
    let docs = src.lookup(sv, "docs").unwrap();
    let deep = src.lookup(docs, "deep").unwrap();
    src.rename(sv, "docs", sv, "tmp").unwrap();
    src.rename(docs, "deep", sv, "docs").unwrap();
    src.rename(sv, "tmp", deep, "was_docs").unwrap();
    let big = src.lookup(deep, "big").unwrap();
    write_all(&mut src, big, 300_000, b"overwritten in the middle");
    write_all(&mut src, big, 700_000, &text(9000, 9));
    src.unlink(bulk, "f7").unwrap();
    src.rename(bulk, "f8", bulk, "f9").unwrap();
    file(&mut src, bulk, "new", &text(5000, 5));
    let sparse = src.lookup(sv, "sparse").unwrap();
    src.truncate(sparse, 20_000).unwrap();
    let h = src.lookup(sv, "hard").unwrap();
    src.unlink(sv, "hard").unwrap();
    src.link(bulk, "hard3", h).unwrap();
    src.set_xattr(h, "user.color", b"red", 0).unwrap();
    src.remove_xattr(docs, "security.selinux").ok();
    src.set_attr(bulk, Some(0o700), None, None, None, None)
        .unwrap();
    src.unlink(sv, "fifo").unwrap();
    let s2 = src.snapshot(sv, root, "s2", true).unwrap();
    let inc = send(&mut src, s2, Some(s1));
    assert!(
        inc.len() * 10 < full.len(),
        "{} vs {}",
        inc.len(),
        full.len()
    );
    let got = receive(&mut dst, droot, &inc).unwrap();
    assert_eq!(tree(&mut dst, got), tree(&mut src, s2));
    // The older one is still there, unchanged.
    let old = dst.lookup(droot, "s1").unwrap();
    assert_eq!(tree(&mut dst, old), tree(&mut src, s1));
    clean(&mut dst);

    // And a third, on top of the received second.
    src.unlink(bulk, "new").unwrap();
    file(&mut src, sv, "third", b"3");
    let s3 = src.snapshot(sv, root, "s3", true).unwrap();
    let inc = send(&mut src, s3, Some(s2));
    let got = receive(&mut dst, droot, &inc).unwrap();
    assert_eq!(tree(&mut dst, got), tree(&mut src, s3));
    clean(&mut dst);
}

#[test]
fn unchanged_snapshots_send_an_empty_increment() {
    let (_a, mut src) = fresh(32 * MIB);
    let root = src.root_ino();
    let sv = src.create_subvolume(root, "data").unwrap();
    populate(&mut src, sv);
    let s1 = src.snapshot(sv, root, "s1", true).unwrap();
    let s2 = src.snapshot(sv, root, "s2", true).unwrap();
    let st = src.send(s2, Some(s1), &mut |_| Ok(())).unwrap();
    assert_eq!((st.inodes, st.data_bytes), (0, 0));
    assert!(st.bytes < 200, "{:?}", st);
}

#[test]
fn reflinks_are_sent_as_clones() {
    let (_a, mut src) = fresh(64 * MIB);
    let root = src.root_ino();
    let sv = src.create_subvolume(root, "data").unwrap();
    let data = text(2 << 20, 4);
    let a = file(&mut src, sv, "a", &data);
    let b = src.create(sv, "b", FileKind::Regular, 0o644, 0).unwrap();
    src.clone_range(a, 0, b, 0, 0).unwrap();
    let c = src.create(sv, "c", FileKind::Regular, 0o644, 0).unwrap();
    src.clone_range(a, 4096 * 10, c, 0, 4096 * 20).unwrap();
    let snap = src.snapshot(sv, root, "snap", true).unwrap();
    let mut stream = Vec::new();
    let st = src
        .send(snap, None, &mut |x| {
            stream.extend_from_slice(x);
            Ok(())
        })
        .unwrap();
    assert_eq!(st.data_bytes, data.len() as u64);
    assert_eq!(st.cloned_bytes, data.len() as u64 + 4096 * 20);

    let (_b, mut dst) = fresh(64 * MIB);
    let droot = dst.root_ino();
    dst.sync().unwrap();
    let before = dst.fsinfo().bytes_used;
    let got = receive(&mut dst, droot, &stream).unwrap();
    dst.sync().unwrap();
    assert!(dst.fsinfo().bytes_used - before < data.len() as u64 + 256 * 1024);
    assert_eq!(tree(&mut dst, got), tree(&mut src, snap));
    clean(&mut dst);
}

#[test]
fn compressed_files_stay_compressed() {
    let (_a, mut src) = fresh(64 * MIB);
    let root = src.root_ino();
    let sv = src.create_subvolume(root, "data").unwrap();
    src.set_compression(sv, Some(Compression::Lz4)).unwrap();
    let f = file(&mut src, sv, "log", &text(1 << 20, 6));
    let snap = src.snapshot(sv, root, "snap", true).unwrap();
    let stream = send(&mut src, snap, None);
    let (_b, mut dst) = fresh(64 * MIB);
    let droot = dst.root_ino();
    let got = receive(&mut dst, droot, &stream).unwrap();
    let g = dst.lookup(got, "log").unwrap();
    assert_eq!(dst.compression(g).unwrap(), Some(Compression::Lz4));
    let c = dst.compsize(g).unwrap();
    assert!(c.disk_bytes * 3 < c.uncompressed_bytes, "{:?}", c);
    assert_eq!(read_all(&mut dst, g), read_all(&mut src, f));
    clean(&mut dst);
}

#[test]
fn a_snapshot_can_be_received_on_the_same_filesystem() {
    let (disk, mut fs) = fresh(64 * MIB);
    let root = fs.root_ino();
    let sv = fs.create_subvolume(root, "data").unwrap();
    populate(&mut fs, sv);
    let snap = fs.snapshot(sv, root, "snap", true).unwrap();
    let stream = send(&mut fs, snap, None);
    let copies = fs.create(root, "copies", FileKind::Dir, 0o755, 0).unwrap();
    let got = receive(&mut fs, copies, &stream).unwrap();
    assert_eq!(tree(&mut fs, got), tree(&mut fs, snap));
    clean(&mut fs);
    let mut fs = remount(&disk, fs);
    clean(&mut fs);
}

#[test]
fn only_read_only_snapshots_can_be_sent() {
    let (_a, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let sv = fs.create_subvolume(root, "data").unwrap();
    assert!(matches!(
        fs.send(sv, None, &mut |_| Ok(())),
        Err(Error::Unsupported(_))
    ));
    let d = fs.create(sv, "d", FileKind::Dir, 0o755, 0).unwrap();
    assert!(matches!(
        fs.send(d, None, &mut |_| Ok(())),
        Err(Error::Invalid)
    ));
    let snap = fs.snapshot(sv, root, "snap", true).unwrap();
    assert!(matches!(
        fs.send(snap, Some(sv), &mut |_| Ok(())),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn an_increment_needs_its_parent_on_the_other_side() {
    let (_a, mut src) = fresh(32 * MIB);
    let root = src.root_ino();
    let sv = src.create_subvolume(root, "data").unwrap();
    populate(&mut src, sv);
    let s1 = src.snapshot(sv, root, "s1", true).unwrap();
    file(&mut src, sv, "more", b"x");
    let s2 = src.snapshot(sv, root, "s2", true).unwrap();
    let inc = send(&mut src, s2, Some(s1));
    let (_b, mut dst) = fresh(32 * MIB);
    let droot = dst.root_ino();
    assert_eq!(receive(&mut dst, droot, &inc), Err(Error::NotFound));
    assert_eq!(dst.list_subvolumes().unwrap().len(), 1);
    clean(&mut dst);
    // A received subvolume made writable is no longer a parent.
    let full = send(&mut src, s1, None);
    let got = receive(&mut dst, droot, &full).unwrap();
    let id = dst.subvolume_of(got);
    dst.set_subvolume_read_only(id, false).unwrap();
    assert_eq!(receive(&mut dst, droot, &inc), Err(Error::NotFound));
    clean(&mut dst);
}

#[test]
fn damaged_or_cut_streams_leave_nothing_behind() {
    let (_a, mut src) = fresh(32 * MIB);
    let root = src.root_ino();
    let sv = src.create_subvolume(root, "data").unwrap();
    populate(&mut src, sv);
    let snap = src.snapshot(sv, root, "snap", true).unwrap();
    let stream = send(&mut src, snap, None);
    let (disk, mut dst) = fresh(32 * MIB);
    let droot = dst.root_ino();
    dst.sync().unwrap();
    let used = dst.fsinfo().bytes_used;
    let mut rng = Lcg(5);
    for i in 0..60 {
        let mut bad = stream.clone();
        if i % 2 == 0 {
            bad.truncate(rng.below(stream.len() as u64) as usize);
        } else {
            let at = rng.below(stream.len() as u64) as usize;
            bad[at] ^= 1 << rng.below(8);
        }
        assert!(receive(&mut dst, droot, &bad).is_err(), "case {}", i);
        assert!(dst.readdir(droot).unwrap().is_empty());
    }
    assert!(receive(&mut dst, droot, b"not a stream at all").is_err());
    dst.sync().unwrap();
    assert_eq!(dst.fsinfo().bytes_used, used);
    clean(&mut dst);
    let mut dst = remount(&disk, dst);
    assert_eq!(dst.list_subvolumes().unwrap().len(), 1);
    // The real one still works.
    let got = receive(&mut dst, droot, &stream).unwrap();
    assert_eq!(tree(&mut dst, got), tree(&mut src, snap));
    clean(&mut dst);
}

#[test]
fn a_power_cut_during_receive_leaves_nothing_behind() {
    let (_a, mut src) = fresh(32 * MIB);
    let root = src.root_ino();
    let sv = src.create_subvolume(root, "data").unwrap();
    // Enough operations for the receive to commit on the way.
    for i in 0..300 {
        file(&mut src, sv, &format!("f{i}"), &text(3000 + i * 7, i));
    }
    let snap = src.snapshot(sv, root, "snap", true).unwrap();
    let stream = send(&mut src, snap, None);
    let (disk, mut dst) = fresh(32 * MIB);
    let droot = dst.root_ino();
    dst.sync().unwrap();
    let used = dst.fsinfo().bytes_used;
    // Images of the disk taken while the stream is read.
    let mut images = Vec::new();
    let mut pos = 0;
    let mut reads = 0;
    let got = dst
        .receive(droot, &mut |buf| {
            reads += 1;
            if reads % 97 == 0 {
                images.push(disk.copy());
            }
            buf.copy_from_slice(&stream[pos..pos + buf.len()]);
            pos += buf.len();
            Ok(())
        })
        .unwrap();
    let want = tree(&mut src, snap);
    assert_eq!(tree(&mut dst, got), want);
    assert!(images.len() > 3);
    let mut partial = 0;
    for img in images {
        let ro = EclipseFS::mount(img.clone(), true).unwrap();
        partial += usize::from(ro.fsinfo().bytes_used > used);
        drop(ro);
        let mut fs = EclipseFS::mount(img.clone(), false).unwrap();
        fs.set_clock(clock);
        assert!(fs.readdir(droot).unwrap().is_empty());
        assert_eq!(fs.list_subvolumes().unwrap().len(), 1);
        fs.sync().unwrap();
        assert_eq!(fs.fsinfo().bytes_used, used);
        clean(&mut fs);
        let got = receive(&mut fs, droot, &stream).unwrap();
        assert_eq!(tree(&mut fs, got), want);
    }
    // Some cuts did land after the receive had committed part of it.
    assert!(partial > 0);
}

#[test]
fn a_name_that_exists_is_refused() {
    let (_a, mut src) = fresh(16 * MIB);
    let root = src.root_ino();
    let sv = src.create_subvolume(root, "data").unwrap();
    let snap = src.snapshot(sv, root, "snap", true).unwrap();
    let stream = send(&mut src, snap, None);
    let (_b, mut dst) = fresh(16 * MIB);
    let droot = dst.root_ino();
    dst.create(droot, "snap", FileKind::Dir, 0o755, 0).unwrap();
    assert_eq!(receive(&mut dst, droot, &stream), Err(Error::Exists));
    clean(&mut dst);
}

#[test]
fn send_and_receive_are_v4_only() {
    let (_a, mut fs) = fresh_v3(16 * MIB);
    let root = fs.root_ino();
    assert!(matches!(
        fs.send(root, None, &mut |_| Ok(())),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        receive(&mut fs, root, b"EFSSEND1"),
        Err(Error::Unsupported(_))
    ));
}

/// Random changes, a snapshot after each round, each sent as an increment
/// on top of the last: the received copies match every time.
#[test]
fn random_rounds_of_increments_stay_in_step() {
    for seed in [1u64, 2, 3] {
        let (_a, mut src) = fresh(64 * MIB);
        let root = src.root_ino();
        let sv = src.create_subvolume(root, "data").unwrap();
        let (_b, mut dst) = fresh(64 * MIB);
        let droot = dst.root_ino();
        let mut rng = Lcg(seed);
        let mut dirs = vec![sv];
        let mut prev: Option<u64> = None;
        for round in 0..8 {
            for _ in 0..60 {
                let d = dirs[rng.below(dirs.len() as u64) as usize];
                let name = format!("n{}", rng.below(10));
                let _ = match rng.below(14) {
                    0 | 1 => src
                        .create(d, &name, FileKind::Regular, 0o644, 0)
                        .map(|_| ()),
                    2 => src
                        .create(d, &name, FileKind::Dir, 0o755, 0)
                        .map(|x| dirs.push(x)),
                    3..=5 => match src.lookup(d, &name) {
                        Ok(f) if src.stat(f).unwrap().kind == FileKind::Regular => {
                            let size = src.stat(f).unwrap().size;
                            let at = rng.below(size + 20_000);
                            let len = 1 + rng.below(70_000) as usize;
                            src.write(f, at, &text(len, rng.below(1000) as usize))
                                .map(|_| ())
                        }
                        _ => Ok(()),
                    },
                    6 => match src.lookup(d, &name) {
                        Ok(f) if src.stat(f).unwrap().kind == FileKind::Regular => {
                            let size = src.stat(f).unwrap().size;
                            src.truncate(f, rng.below(size + 10_000))
                        }
                        _ => Ok(()),
                    },
                    7 => {
                        let nd = dirs[rng.below(dirs.len() as u64) as usize];
                        src.rename(d, &name, nd, &format!("n{}", rng.below(10)))
                    }
                    8 => src.unlink(d, &name),
                    9 => match src.lookup(d, &name) {
                        Ok(f) if src.stat(f).unwrap().kind != FileKind::Dir => src.link(
                            dirs[rng.below(dirs.len() as u64) as usize],
                            &format!("h{}", rng.below(5)),
                            f,
                        ),
                        _ => Ok(()),
                    },
                    10 => match src.lookup(d, &name) {
                        Ok(f) => src.set_xattr(
                            f,
                            &format!("user.k{}", rng.below(3)),
                            &text(rng.below(300) as usize, 1),
                            0,
                        ),
                        _ => Ok(()),
                    },
                    11 => match (
                        src.lookup(d, &name),
                        src.lookup(d, &format!("n{}", rng.below(10))),
                    ) {
                        (Ok(a), Ok(b))
                            if a != b
                                && src.stat(a).unwrap().kind == FileKind::Regular
                                && src.stat(b).unwrap().kind == FileKind::Regular =>
                        {
                            let len = src.stat(a).unwrap().size / 4096 * 4096;
                            if len > 0 {
                                src.clone_range(a, 0, b, 0, len).map(|_| ())
                            } else {
                                Ok(())
                            }
                        }
                        _ => Ok(()),
                    },
                    12 => src
                        .symlink(d, &name, format!("t{}", rng.below(99)).as_bytes())
                        .map(|_| ()),
                    _ => match src.lookup(d, &name) {
                        Ok(f) => src.set_attr(
                            f,
                            Some(0o600 + rng.below(0o100) as u32),
                            None,
                            None,
                            None,
                            None,
                        ),
                        _ => Ok(()),
                    },
                };
                dirs.retain(|&x| src.stat(x).is_ok());
            }
            let name = format!("r{}", round);
            let snap = src.snapshot(sv, root, &name, true).unwrap();
            let stream = send(&mut src, snap, prev);
            let got = receive(&mut dst, droot, &stream).unwrap();
            assert_eq!(
                tree(&mut dst, got),
                tree(&mut src, snap),
                "seed {} round {}",
                seed,
                round
            );
            prev = Some(snap);
        }
        clean(&mut dst);
        clean(&mut src);
    }
}
