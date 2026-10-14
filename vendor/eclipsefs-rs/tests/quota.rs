#![cfg(feature = "std")]
//! Quotas (format v0.4): per-subvolume counters of file bytes, limits
//! that refuse growth, and what each subvolume holds on disk alone.

mod common;

use common::*;
use eclipsefs::cowfmt::FALLOC_FL_KEEP_SIZE;
use eclipsefs::{Compression, EclipseFS, Error, FileKind, QgroupInfo};

const OFF: Error = Error::Unsupported("quotas are off");
const KIB: u64 = 1024;

fn file(fs: &mut EclipseFS, dir: u64, name: &str, data: &[u8]) -> u64 {
    let f = fs.create(dir, name, FileKind::Regular, 0o644, 0).unwrap();
    write_all(fs, f, 0, data);
    f
}

fn group(fs: &mut EclipseFS, h: u64) -> QgroupInfo {
    let id = fs.subvolume_of(h);
    fs.qgroup_show()
        .unwrap()
        .into_iter()
        .find(|q| q.id == id)
        .unwrap()
}

/// What the counter of `h`'s subvolume should be: its files' nbytes.
fn counted(fs: &mut EclipseFS, dir: u64) -> u64 {
    let mut total = fs.stat(dir).unwrap().nbytes;
    for e in fs.readdir(dir).unwrap() {
        if fs.subvolume_of(e.ino) != fs.subvolume_of(dir) {
            continue;
        }
        total += match e.kind {
            FileKind::Dir => counted(fs, e.ino),
            _ => fs.stat(e.ino).unwrap().nbytes,
        };
    }
    total
}

/// Counter as kept equals a recount, and fsck agrees.
fn consistent(fs: &mut EclipseFS) {
    let before = fs.qgroup_show().unwrap();
    fs.quota_rescan().unwrap();
    assert_eq!(fs.qgroup_show().unwrap(), before);
    fs.check().unwrap();
}

#[test]
fn quotas_are_off_until_enabled() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    assert!(!fs.quota_enabled());
    assert_eq!(fs.qgroup_show().unwrap_err(), OFF);
    assert_eq!(fs.set_qgroup_limit(5, Some(MIB as u64)).unwrap_err(), OFF);
    assert_eq!(fs.quota_rescan().unwrap_err(), OFF);
    // No limit applies while they are off.
    file(&mut fs, root, "a", &pattern(256 * 1024, 1));
    fs.quota_enable().unwrap();
    assert!(fs.quota_enabled());
    let fs = remount(&disk, fs);
    assert!(fs.quota_enabled());
}

#[test]
fn enabling_counts_what_is_already_there() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let d = fs.create(root, "d", FileKind::Dir, 0o755, 0).unwrap();
    file(&mut fs, d, "big", &pattern(300 * 1024, 2));
    file(&mut fs, root, "small", b"tiny");
    let sv = fs.create_subvolume(root, "home").unwrap();
    file(&mut fs, sv, "h", &pattern(40 * 1024, 3));
    fs.quota_enable().unwrap();
    let top = counted(&mut fs, root);
    let home = counted(&mut fs, sv);
    assert_eq!(group(&mut fs, root).referenced, top);
    assert_eq!(group(&mut fs, sv).referenced, home);
    assert_eq!(home, 40 * KIB);
    assert!(top >= 300 * KIB);
    consistent(&mut fs);
    let mut fs = remount(&disk, fs);
    assert_eq!(group(&mut fs, root).referenced, top);
    assert_eq!(group(&mut fs, sv).referenced, home);
    consistent(&mut fs);
}

#[test]
fn the_counter_follows_every_change() {
    let (disk, mut fs) = fresh(32 * MIB);
    fs.quota_enable().unwrap();
    let root = fs.root_ino();
    let mut rng = Lcg(7);
    let mut files = Vec::new();
    for step in 0..300 {
        match rng.below(7) {
            0 | 1 => {
                let n = rng.below(200_000) as usize;
                files.push(file(&mut fs, root, &format!("f{step}"), &pattern(n, step)));
            }
            2 if !files.is_empty() => {
                let f = files[rng.below(files.len() as u64) as usize];
                let off = rng.below(400_000);
                let n = rng.below(70_000) as usize;
                write_all(&mut fs, f, off, &pattern(n, step));
            }
            3 if !files.is_empty() => {
                let f = files[rng.below(files.len() as u64) as usize];
                fs.truncate(f, rng.below(300_000)).unwrap();
            }
            4 if !files.is_empty() => {
                let f = files[rng.below(files.len() as u64) as usize];
                let off = rng.below(200_000);
                fs.fallocate(f, FALLOC_FL_KEEP_SIZE, off, 1 + rng.below(90_000))
                    .unwrap();
            }
            5 if files.len() > 1 => {
                let i = rng.below(files.len() as u64) as usize;
                let f = files.swap_remove(i);
                let name = fs
                    .readdir(root)
                    .unwrap()
                    .into_iter()
                    .find(|e| e.ino == f)
                    .unwrap()
                    .name;
                fs.unlink(root, &name).unwrap();
            }
            _ if files.len() > 1 => {
                let a = files[rng.below(files.len() as u64) as usize];
                let b = files[rng.below(files.len() as u64) as usize];
                let size = fs.stat(a).unwrap().size;
                if a != b && size >= 4096 {
                    let off = fs.stat(b).unwrap().size.next_multiple_of(4096);
                    fs.clone_range(a, 0, b, off, size / 4096 * 4096).unwrap();
                }
            }
            _ => {}
        }
        if step % 50 == 0 {
            assert_eq!(group(&mut fs, root).referenced, counted(&mut fs, root));
            consistent(&mut fs);
        }
    }
    let n = group(&mut fs, root).referenced;
    assert_eq!(n, counted(&mut fs, root));
    let mut fs = remount(&disk, fs);
    assert_eq!(group(&mut fs, root).referenced, n);
    consistent(&mut fs);
}

#[test]
fn a_limit_refuses_growth_but_not_rewrites() {
    let (disk, mut fs) = fresh(16 * MIB);
    fs.quota_enable().unwrap();
    let root = fs.root_ino();
    let sv = fs.create_subvolume(root, "jail").unwrap();
    let id = fs.subvolume_of(sv);
    fs.set_qgroup_limit(id, Some(64 * KIB)).unwrap();
    let a = file(&mut fs, sv, "a", &pattern(48 * 1024, 1));
    // 48 KiB used: 32 more is too much, 16 fits.
    let b = fs.create(sv, "b", FileKind::Regular, 0o644, 0).unwrap();
    assert_eq!(
        fs.write(b, 0, &pattern(32 * 1024, 2)).unwrap_err(),
        Error::NoSpace
    );
    assert_eq!(fs.stat(b).unwrap().size, 0);
    write_all(&mut fs, b, 0, &pattern(16 * 1024, 2));
    assert_eq!(group(&mut fs, sv).referenced, 64 * KIB);
    // Full: rewriting what is there still works, growing does not.
    write_all(&mut fs, a, 4096, &pattern(20 * 1024, 3));
    assert_eq!(fs.write(a, 48 * 1024, b"x").unwrap_err(), Error::NoSpace);
    assert_eq!(
        fs.fallocate(b, 0, 16 * 1024, 4096).unwrap_err(),
        Error::NoSpace
    );
    let mut expect = pattern(48 * 1024, 1);
    expect[4096..4096 + 20 * 1024].copy_from_slice(&pattern(20 * 1024, 3));
    assert_eq!(read_all(&mut fs, a), expect);
    // Freeing makes room again.
    fs.unlink(sv, "b").unwrap();
    write_all(&mut fs, a, 48 * 1024, &pattern(16 * 1024, 4));
    // Other subvolumes are not limited.
    file(&mut fs, root, "free", &pattern(512 * 1024, 5));
    consistent(&mut fs);
    // The limit survives a remount, and can be lifted.
    let mut fs = remount(&disk, fs);
    let a = fs.lookup(sv, "a").unwrap();
    assert_eq!(group(&mut fs, sv).limit, Some(64 * KIB));
    assert_eq!(fs.write(a, 64 * 1024, b"x").unwrap_err(), Error::NoSpace);
    fs.set_qgroup_limit(id, None).unwrap();
    write_all(&mut fs, a, 64 * 1024, &pattern(64 * 1024, 6));
    assert_eq!(group(&mut fs, sv).limit, None);
    consistent(&mut fs);
}

#[test]
fn clones_count_only_the_blocks_they_add() {
    let (_disk, mut fs) = fresh(16 * MIB);
    fs.quota_enable().unwrap();
    let root = fs.root_ino();
    let src = file(&mut fs, root, "src", &pattern(64 * 1024, 1));
    let base = group(&mut fs, root).referenced;
    fs.set_qgroup_limit(5, Some(base + 64 * KIB)).unwrap();
    // A copy of 64 KiB fits exactly; a second does not.
    let c1 = fs.create(root, "c1", FileKind::Regular, 0o644, 0).unwrap();
    fs.clone_range(src, 0, c1, 0, 64 * 1024).unwrap();
    let c2 = fs.create(root, "c2", FileKind::Regular, 0o644, 0).unwrap();
    assert_eq!(
        fs.clone_range(src, 0, c2, 0, 64 * 1024).unwrap_err(),
        Error::NoSpace
    );
    // Cloning over blocks the file already maps adds nothing.
    fs.clone_range(src, 0, c1, 0, 64 * 1024).unwrap();
    assert_eq!(group(&mut fs, root).referenced, base + 64 * KIB);
    // On disk the copy costs nothing: both are shared.
    let q = group(&mut fs, root);
    assert!(q.disk_referenced < q.referenced, "{q:?}");
    consistent(&mut fs);
}

#[test]
fn snapshots_start_with_the_same_counter_and_no_limit() {
    let (disk, mut fs) = fresh(32 * MIB);
    fs.quota_enable().unwrap();
    let root = fs.root_ino();
    let sv = fs.create_subvolume(root, "data").unwrap();
    file(&mut fs, sv, "a", &pattern(200 * 1024, 1));
    file(&mut fs, sv, "b", &pattern(100 * 1024, 2));
    let id = fs.subvolume_of(sv);
    fs.set_qgroup_limit(id, Some(MIB as u64)).unwrap();
    let snap = fs.snapshot(sv, root, "snap", false).unwrap();
    let (q, s) = (group(&mut fs, sv), group(&mut fs, snap));
    assert_eq!(s.referenced, q.referenced);
    assert_eq!(s.disk_referenced, q.disk_referenced);
    assert_eq!(s.limit, None);
    // Everything is shared: neither owns anything alone.
    assert_eq!((q.exclusive, s.exclusive), (0, 0));
    // Rewriting `a` in the snapshot gives each its own copy of it.
    let a = fs.lookup(snap, "a").unwrap();
    write_all(&mut fs, a, 0, &pattern(200 * 1024, 3));
    let (q, s) = (group(&mut fs, sv), group(&mut fs, snap));
    assert_eq!(q.exclusive, 200 * KIB);
    assert_eq!(s.exclusive, 200 * KIB);
    assert_eq!(q.disk_referenced, 300 * KIB);
    // Deleting the original leaves the snapshot owning everything.
    fs.delete_subvolume(root, "data").unwrap();
    fs.sync().unwrap();
    let s = group(&mut fs, snap);
    assert_eq!(s.exclusive, s.disk_referenced);
    assert_eq!(fs.qgroup_show().unwrap().len(), 2);
    consistent(&mut fs);
    let mut fs = remount(&disk, fs);
    consistent(&mut fs);
}

#[test]
fn disabling_stops_limits_and_enabling_recounts() {
    let (disk, mut fs) = fresh(16 * MIB);
    fs.quota_enable().unwrap();
    let root = fs.root_ino();
    fs.set_qgroup_limit(5, Some(32 * KIB)).unwrap();
    let f = file(&mut fs, root, "f", &pattern(32 * 1024, 1));
    assert_eq!(fs.write(f, 32 * 1024, b"x").unwrap_err(), Error::NoSpace);
    fs.quota_disable().unwrap();
    assert!(!fs.quota_enabled());
    write_all(&mut fs, f, 32 * 1024, &pattern(96 * 1024, 2));
    fs.unlink(root, "f").unwrap();
    file(&mut fs, root, "g", &pattern(10 * 1024, 3));
    let mut fs = remount(&disk, fs);
    assert!(!fs.quota_enabled());
    fs.check().unwrap();
    fs.quota_enable().unwrap();
    // Recounted, and the old limit is back.
    assert_eq!(group(&mut fs, root).referenced, counted(&mut fs, root));
    assert_eq!(group(&mut fs, root).limit, Some(32 * KIB));
    consistent(&mut fs);
}

#[test]
fn bad_limits_are_refused() {
    let (_disk, mut fs) = fresh(16 * MIB);
    fs.quota_enable().unwrap();
    assert_eq!(fs.set_qgroup_limit(5, Some(0)).unwrap_err(), Error::Invalid);
    assert_eq!(
        fs.set_qgroup_limit(4242, Some(MIB as u64)).unwrap_err(),
        Error::NotFound
    );
}

#[test]
fn compressed_files_count_file_bytes() {
    let (_disk, mut fs) = fresh(16 * MIB);
    fs.quota_enable().unwrap();
    fs.set_mount_compression(Some(Compression::Zstd)).unwrap();
    let root = fs.root_ino();
    file(&mut fs, root, "z", &vec![b'a'; 256 * 1024]);
    fs.sync().unwrap();
    let q = group(&mut fs, root);
    assert!(q.referenced >= 256 * KIB, "{q:?}");
    assert!(q.disk_referenced < 64 * KIB, "{q:?}");
    consistent(&mut fs);
}

#[test]
fn received_subvolumes_are_counted() {
    let (_disk, mut fs) = fresh(32 * MIB);
    let root = fs.root_ino();
    let sv = fs.create_subvolume(root, "src").unwrap();
    file(&mut fs, sv, "a", &pattern(100 * 1024, 1));
    let d = fs.create(sv, "d", FileKind::Dir, 0o755, 0).unwrap();
    file(&mut fs, d, "b", b"inline");
    let snap = fs.snapshot(sv, root, "snap", true).unwrap();
    let mut stream = Vec::new();
    fs.send(snap, None, &mut |b| {
        stream.extend_from_slice(b);
        Ok(())
    })
    .unwrap();
    let (_disk2, mut dst) = fresh(32 * MIB);
    dst.quota_enable().unwrap();
    let droot = dst.root_ino();
    let mut pos = 0;
    let got = dst
        .receive(droot, &mut |buf| {
            buf.copy_from_slice(&stream[pos..pos + buf.len()]);
            pos += buf.len();
            Ok(())
        })
        .unwrap();
    assert_eq!(group(&mut dst, got).referenced, counted(&mut dst, got));
    consistent(&mut dst);
}

#[test]
fn the_old_format_has_no_quotas() {
    let (_disk, mut fs) = fresh_v3(16 * MIB);
    assert!(!fs.quota_enabled());
    assert!(matches!(
        fs.quota_enable().unwrap_err(),
        Error::Unsupported(_)
    ));
}
