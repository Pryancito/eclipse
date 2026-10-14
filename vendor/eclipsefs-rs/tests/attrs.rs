#![cfg(feature = "std")]
//! Extended attributes, inode flags (chattr) and fallocate (format v0.4).

mod common;

use common::*;
use eclipsefs::cowfmt::{
    FALLOC_FL_KEEP_SIZE, FALLOC_FL_PUNCH_HOLE, FALLOC_FL_ZERO_RANGE, FL_APPEND, FL_COMPR,
    FL_IMMUTABLE, FL_NOATIME, FL_NOCOMP, FL_NOCOW, XATTR_CREATE, XATTR_REPLACE, XATTR_SIZE_MAX,
};
use eclipsefs::device::BlockDevice;
use eclipsefs::{EclipseFS, Error, FileKind};

const EPERM: Error = Error::Unsupported("immutable or append-only inode");

fn file(fs: &mut EclipseFS, dir: u64, name: &str, data: &[u8]) -> u64 {
    let f = fs.create(dir, name, FileKind::Regular, 0o644, 0).unwrap();
    write_all(fs, f, 0, data);
    f
}

fn random(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = Lcg(seed);
    (0..len).map(|_| rng.next() as u8).collect()
}

fn find_block(disk: &MemDisk, content: &[u8]) -> Option<usize> {
    let mut image = vec![0u8; disk.size() as usize];
    disk.read_at(0, &mut image).unwrap();
    image.chunks(BLOCK).position(|b| b == content)
}

fn clean(fs: &mut EclipseFS) {
    fs.check().unwrap();
    let r = fs.scrub().unwrap();
    assert!(r.is_clean(), "{:?}", r);
}

// ---------------------------------------------------------------------------
// Extended attributes
// ---------------------------------------------------------------------------

#[test]
fn xattrs_are_set_read_listed_and_removed() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = file(&mut fs, root, "a", b"data");
    assert_eq!(fs.list_xattrs(f).unwrap(), Vec::<String>::new());
    assert_eq!(fs.get_xattr(f, "user.x"), Err(Error::NotFound));
    fs.set_xattr(f, "user.x", b"1", 0).unwrap();
    fs.set_xattr(f, "user.y", b"", 0).unwrap();
    fs.set_xattr(f, "security.selinux", b"system_u:object_r:etc_t", 0)
        .unwrap();
    fs.set_xattr(root, "trusted.dir", b"on a directory", 0)
        .unwrap();
    assert_eq!(fs.get_xattr(f, "user.x").unwrap(), b"1");
    assert_eq!(fs.get_xattr(f, "user.y").unwrap(), b"");
    fs.set_xattr(f, "user.x", b"22", 0).unwrap();
    assert_eq!(fs.get_xattr(f, "user.x").unwrap(), b"22");
    clean(&mut fs);
    let mut fs = remount(&disk, fs);
    assert_eq!(
        fs.list_xattrs(f).unwrap(),
        ["security.selinux", "user.x", "user.y"]
    );
    assert_eq!(
        fs.get_xattr(root, "trusted.dir").unwrap(),
        b"on a directory"
    );
    fs.remove_xattr(f, "user.x").unwrap();
    assert_eq!(fs.remove_xattr(f, "user.x"), Err(Error::NotFound));
    assert_eq!(fs.list_xattrs(f).unwrap(), ["security.selinux", "user.y"]);
    fs.remove_xattr(f, "user.y").unwrap();
    fs.remove_xattr(f, "security.selinux").unwrap();
    assert!(fs.list_xattrs(f).unwrap().is_empty());
    clean(&mut fs);
}

#[test]
fn xattr_create_and_replace_flags() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = file(&mut fs, root, "a", b"");
    assert_eq!(
        fs.set_xattr(f, "user.k", b"v", XATTR_REPLACE),
        Err(Error::NotFound)
    );
    fs.set_xattr(f, "user.k", b"v", XATTR_CREATE).unwrap();
    assert_eq!(
        fs.set_xattr(f, "user.k", b"w", XATTR_CREATE),
        Err(Error::Exists)
    );
    fs.set_xattr(f, "user.k", b"w", XATTR_REPLACE).unwrap();
    assert_eq!(fs.get_xattr(f, "user.k").unwrap(), b"w");
    assert_eq!(fs.set_xattr(f, "user.k", b"w", 4), Err(Error::Invalid));
}

#[test]
fn xattr_names_need_a_known_namespace_and_a_bounded_length() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = file(&mut fs, root, "a", b"");
    for bad in ["x", "user.", "os2.x", "", "USER.x"] {
        assert!(fs.set_xattr(f, bad, b"v", 0).is_err(), "{:?}", bad);
        assert!(fs.get_xattr(f, bad).is_err(), "{:?}", bad);
    }
    let long = format!("user.{}", "n".repeat(250));
    fs.set_xattr(f, &long, b"v", 0).unwrap();
    let too_long = format!("user.{}", "n".repeat(251));
    assert!(fs.set_xattr(f, &too_long, b"v", 0).is_err());
    assert!(fs
        .set_xattr(f, "user.big", &vec![0u8; XATTR_SIZE_MAX + 1], 0)
        .is_err());
    // A value too large for one tree item is refused (E2BIG on Linux).
    assert_eq!(
        fs.set_xattr(f, "user.big", &vec![7u8; 8000], 0),
        Err(Error::NoSpace)
    );
    fs.set_xattr(f, "user.mid", &vec![7u8; 1500], 0).unwrap();
    assert_eq!(fs.get_xattr(f, "user.mid").unwrap(), vec![7u8; 1500]);
    fs.check().unwrap();
}

#[test]
fn posix_acls_are_stored_as_system_xattrs() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let d = fs.create(root, "shared", FileKind::Dir, 0o755, 0).unwrap();
    // A posix_acl_xattr blob: version 2, then (tag, perm, id) entries.
    let mut acl = 2u32.to_le_bytes().to_vec();
    for (tag, perm, id) in [(1u16, 7u16, u32::MAX), (2, 6, 1000), (4, 5, u32::MAX)] {
        acl.extend_from_slice(&tag.to_le_bytes());
        acl.extend_from_slice(&perm.to_le_bytes());
        acl.extend_from_slice(&id.to_le_bytes());
    }
    fs.set_xattr(d, "system.posix_acl_access", &acl, 0).unwrap();
    fs.set_xattr(d, "system.posix_acl_default", &acl, 0)
        .unwrap();
    let mut fs = remount(&disk, fs);
    assert_eq!(fs.get_xattr(d, "system.posix_acl_access").unwrap(), acl);
    assert_eq!(
        fs.list_xattrs(d).unwrap(),
        ["system.posix_acl_access", "system.posix_acl_default"]
    );
}

#[test]
fn many_xattrs_on_many_inodes() {
    let (disk, mut fs) = fresh(32 * MIB);
    let root = fs.root_ino();
    let mut files = Vec::new();
    for i in 0..40 {
        let f = file(&mut fs, root, &format!("f{}", i), b"x");
        for j in 0..25 {
            fs.set_xattr(
                f,
                &format!("user.attr{}", j),
                format!("{}-{}", i, j).as_bytes(),
                0,
            )
            .unwrap();
        }
        files.push(f);
    }
    clean(&mut fs);
    let mut fs = remount(&disk, fs);
    for (i, &f) in files.iter().enumerate() {
        assert_eq!(fs.list_xattrs(f).unwrap().len(), 25);
        assert_eq!(
            fs.get_xattr(f, "user.attr13").unwrap(),
            format!("{}-13", i).as_bytes()
        );
    }
}

#[test]
fn deleting_a_file_drops_its_xattrs() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = file(&mut fs, root, "a", b"x");
    fs.set_xattr(f, "user.a", &[1; 1000], 0).unwrap();
    fs.sync().unwrap();
    let before = fs.fsinfo().bytes_used;
    for i in 0..30 {
        let g = file(&mut fs, root, &format!("t{}", i), b"");
        fs.set_xattr(g, "user.a", &[1; 1500], 0).unwrap();
        fs.unlink(root, &format!("t{}", i)).unwrap();
    }
    fs.unlink(root, "a").unwrap();
    let g = fs.create(root, "a", FileKind::Regular, 0o644, 0).unwrap();
    assert!(fs.list_xattrs(g).unwrap().is_empty());
    clean(&mut fs);
    let mut fs = remount(&disk, fs);
    assert!(fs.fsinfo().bytes_used <= before);
    clean(&mut fs);
}

#[test]
fn xattrs_follow_snapshots_copy_on_write() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let sv = fs.create_subvolume(root, "sv").unwrap();
    let f = file(&mut fs, sv, "a", b"x");
    fs.set_xattr(f, "user.v", b"old", 0).unwrap();
    let snap = fs.snapshot(sv, root, "snap", false).unwrap();
    fs.set_xattr(f, "user.v", b"new", 0).unwrap();
    let g = fs.lookup(snap, "a").unwrap();
    assert_eq!(fs.get_xattr(g, "user.v").unwrap(), b"old");
    assert_eq!(fs.get_xattr(f, "user.v").unwrap(), b"new");
    let ro = fs.snapshot(sv, root, "ro", true).unwrap();
    let h = fs.lookup(ro, "a").unwrap();
    assert_eq!(fs.get_xattr(h, "user.v").unwrap(), b"new");
    assert!(fs.set_xattr(h, "user.v", b"x", 0).is_err());
    assert!(fs.remove_xattr(h, "user.v").is_err());
    clean(&mut fs);
}

#[test]
fn xattrs_are_v4_only() {
    let (_disk, mut fs) = fresh_v3(16 * MIB);
    let root = fs.root_ino();
    assert!(matches!(
        fs.set_xattr(root, "user.a", b"", 0),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(fs.get_flags(root), Err(Error::Unsupported(_))));
    assert!(matches!(
        fs.fallocate(root, 0, 0, 1),
        Err(Error::Unsupported(_))
    ));
}

// ---------------------------------------------------------------------------
// Inode flags
// ---------------------------------------------------------------------------

#[test]
fn flags_round_trip_and_unknown_bits_are_refused() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = file(&mut fs, root, "a", b"x");
    assert_eq!(fs.get_flags(f).unwrap(), 0);
    fs.set_flags(f, FL_NOATIME | FL_COMPR).unwrap();
    let mut fs = remount(&disk, fs);
    assert_eq!(fs.get_flags(f).unwrap(), FL_NOATIME | FL_COMPR);
    assert_eq!(fs.set_flags(f, 0x4000_0000), Err(Error::Invalid));
    assert_eq!(fs.set_flags(f, FL_COMPR | FL_NOCOMP), Err(Error::Invalid));
    fs.set_flags(f, 0).unwrap();
    assert_eq!(fs.get_flags(f).unwrap(), 0);
    fs.check().unwrap();
}

#[test]
fn immutable_files_refuse_every_change() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = file(&mut fs, root, "a", b"keep me");
    fs.set_xattr(f, "user.a", b"1", 0).unwrap();
    fs.set_flags(f, FL_IMMUTABLE).unwrap();
    assert_eq!(fs.write(f, 0, b"x"), Err(EPERM));
    assert_eq!(fs.write(f, 7, b"x"), Err(EPERM));
    assert_eq!(fs.truncate(f, 0), Err(EPERM));
    assert_eq!(fs.unlink(root, "a"), Err(EPERM));
    assert_eq!(fs.rename(root, "a", root, "b"), Err(EPERM));
    assert_eq!(fs.link(root, "b", f), Err(EPERM));
    assert_eq!(
        fs.set_attr(f, Some(0o600), None, None, None, None),
        Err(EPERM)
    );
    assert_eq!(fs.set_xattr(f, "user.a", b"2", 0), Err(EPERM));
    assert_eq!(fs.remove_xattr(f, "user.a"), Err(EPERM));
    assert_eq!(fs.fallocate(f, 0, 0, 4096), Err(EPERM));
    let g = file(&mut fs, root, "g", b"other");
    assert_eq!(fs.rename(root, "g", root, "a"), Err(EPERM));
    assert_eq!(fs.clone_range(g, 0, f, 0, 0), Err(EPERM));
    // Reads still work, and the flag itself can be cleared.
    assert_eq!(read_all(&mut fs, f), b"keep me");
    let mut fs = remount(&disk, fs);
    assert_eq!(fs.unlink(root, "a"), Err(EPERM));
    fs.set_flags(f, 0).unwrap();
    fs.write(f, 0, b"K").unwrap();
    fs.unlink(root, "a").unwrap();
    clean(&mut fs);
}

#[test]
fn immutable_directories_refuse_new_and_removed_names() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let d = fs.create(root, "d", FileKind::Dir, 0o755, 0).unwrap();
    let f = file(&mut fs, d, "in", b"x");
    fs.set_flags(d, FL_IMMUTABLE).unwrap();
    assert_eq!(fs.create(d, "new", FileKind::Regular, 0o644, 0), Err(EPERM));
    assert_eq!(fs.unlink(d, "in"), Err(EPERM));
    assert_eq!(fs.symlink(d, "l", b"in").map(|_| ()), Err(EPERM));
    assert_eq!(fs.rename(d, "in", root, "out"), Err(EPERM));
    assert_eq!(fs.create_subvolume(d, "sv").map(|_| ()), Err(EPERM));
    // Files inside can still change.
    write_all(&mut fs, f, 0, b"changed");
    assert_eq!(read_all(&mut fs, f), b"changed");
    fs.set_flags(d, 0).unwrap();
    fs.unlink(d, "in").unwrap();
    fs.check().unwrap();
}

#[test]
fn append_only_files_only_grow() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = file(&mut fs, root, "log", b"line 1\n");
    fs.set_flags(f, FL_APPEND).unwrap();
    assert_eq!(fs.write(f, 0, b"X"), Err(EPERM));
    assert_eq!(fs.write(f, 3, b"X"), Err(EPERM));
    assert_eq!(fs.write(f, 100, b"X"), Err(EPERM));
    fs.write(f, 7, b"line 2\n").unwrap();
    let mut big = Vec::new();
    for i in 3..2000 {
        big.extend_from_slice(format!("line {}\n", i).as_bytes());
    }
    write_all(&mut fs, f, 14, &big);
    assert_eq!(fs.truncate(f, 0), Err(EPERM));
    assert_eq!(fs.unlink(root, "log"), Err(EPERM));
    assert_eq!(
        fs.fallocate(f, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 0, 4096),
        Err(EPERM)
    );
    let mut fs = remount(&disk, fs);
    let mut want = b"line 1\nline 2\n".to_vec();
    want.extend_from_slice(&big);
    assert_eq!(read_all(&mut fs, f), want);
    clean(&mut fs);
}

#[test]
fn new_files_inherit_nocow_and_compression_flags() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let d = fs.create(root, "vm", FileKind::Dir, 0o755, 0).unwrap();
    fs.set_flags(d, FL_NOCOW | FL_NOATIME).unwrap();
    let f = file(&mut fs, d, "disk.img", b"");
    let sub = fs.create(d, "sub", FileKind::Dir, 0o755, 0).unwrap();
    let g = file(&mut fs, sub, "deep", b"");
    assert_eq!(fs.get_flags(f).unwrap(), FL_NOCOW);
    assert_eq!(fs.get_flags(sub).unwrap(), FL_NOCOW);
    assert_eq!(fs.get_flags(g).unwrap(), FL_NOCOW);
    let c = fs.create(root, "c", FileKind::Dir, 0o755, 0).unwrap();
    fs.set_flags(c, FL_COMPR).unwrap();
    let h = file(&mut fs, c, "h", b"");
    assert_eq!(fs.get_flags(h).unwrap(), FL_COMPR);
}

#[test]
fn nocow_only_changes_on_empty_files() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = file(&mut fs, root, "a", b"data");
    assert_eq!(fs.set_flags(f, FL_NOCOW), Err(Error::Invalid));
    let g = file(&mut fs, root, "b", b"");
    fs.set_flags(g, FL_NOCOW).unwrap();
    write_all(&mut fs, g, 0, &random(20_000, 1));
    assert_eq!(fs.set_flags(g, 0), Err(Error::Invalid));
    // Other flags change freely on it.
    fs.set_flags(g, FL_NOCOW | FL_NOATIME).unwrap();
    fs.truncate(g, 0).unwrap();
    fs.set_flags(g, 0).unwrap();
    fs.check().unwrap();
}

// ---------------------------------------------------------------------------
// No-COW files
// ---------------------------------------------------------------------------

fn nocow_file(fs: &mut EclipseFS, name: &str, data: &[u8]) -> u64 {
    let root = fs.root_ino();
    let f = fs.create(root, name, FileKind::Regular, 0o644, 0).unwrap();
    fs.set_flags(f, FL_NOCOW).unwrap();
    write_all(fs, f, 0, data);
    f
}

#[test]
fn nocow_files_are_overwritten_in_place() {
    let (disk, mut fs) = fresh(16 * MIB);
    let data = random(8 * BLOCK, 7);
    let f = nocow_file(&mut fs, "db", &data);
    let mut fs = remount(&disk, fs);
    let at = find_block(&disk, &data[3 * BLOCK..4 * BLOCK]).unwrap();
    let new = random(BLOCK, 8);
    write_all(&mut fs, f, 3 * BLOCK as u64, &new);
    let fs = remount(&disk, fs);
    assert_eq!(find_block(&disk, &new), Some(at));
    let mut fs = fs;
    let mut want = data.clone();
    want[3 * BLOCK..4 * BLOCK].copy_from_slice(&new);
    assert_eq!(read_all(&mut fs, f), want);
    clean(&mut fs);
}

#[test]
fn regular_files_are_not_overwritten_in_place() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let data = random(8 * BLOCK, 7);
    let f = file(&mut fs, root, "a", &data);
    let mut fs = remount(&disk, fs);
    let at = find_block(&disk, &data[3 * BLOCK..4 * BLOCK]).unwrap();
    let new = random(BLOCK, 8);
    write_all(&mut fs, f, 3 * BLOCK as u64, &new);
    let _fs = remount(&disk, fs);
    assert_ne!(find_block(&disk, &new), Some(at));
}

#[test]
fn nocow_data_has_no_checksums() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let data = random(4 * BLOCK, 21);
    let f = nocow_file(&mut fs, "raw", &data);
    let other = random(4 * BLOCK, 22);
    let g = file(&mut fs, root, "summed", &other);
    let mut fs = remount(&disk, fs);
    let a = find_block(&disk, &data[BLOCK..2 * BLOCK]).unwrap();
    let b = find_block(&disk, &other[BLOCK..2 * BLOCK]).unwrap();
    disk.poke(a * BLOCK + 10, 0xFF);
    disk.poke(b * BLOCK + 10, 0xFF);
    let mut fs2 = remount(&disk, fs);
    // The no-COW file reads back what is on the disk; the other one notices.
    let got = read_all(&mut fs2, f);
    assert_eq!(got[BLOCK + 10], data[BLOCK + 10] ^ 0xFF);
    let mut buf = vec![0u8; BLOCK];
    assert!(fs2.read(g, BLOCK as u64, &mut buf).is_err());
    let r = fs2.scrub().unwrap();
    assert_eq!(r.bad_data_blocks.len(), 1, "{:?}", r);
    fs = fs2;
    let _ = &mut fs;
}

#[test]
fn nocow_files_still_cow_blocks_shared_with_a_snapshot() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let sv = fs.create_subvolume(root, "sv").unwrap();
    let f = fs.create(sv, "db", FileKind::Regular, 0o644, 0).unwrap();
    fs.set_flags(f, FL_NOCOW).unwrap();
    let data = random(6 * BLOCK, 31);
    write_all(&mut fs, f, 0, &data);
    let snap = fs.snapshot(sv, root, "snap", true).unwrap();
    let new = random(BLOCK, 32);
    write_all(&mut fs, f, 2 * BLOCK as u64, &new);
    // A second write to the now private block goes in place.
    let newer = random(BLOCK, 33);
    let fs = remount(&disk, fs);
    let at = find_block(&disk, &new).unwrap();
    let mut fs = fs;
    write_all(&mut fs, f, 2 * BLOCK as u64, &newer);
    let mut fs = remount(&disk, fs);
    assert_eq!(find_block(&disk, &newer), Some(at));
    let s = fs.lookup(snap, "db").unwrap();
    assert_eq!(read_all(&mut fs, s), data);
    let mut want = data.clone();
    want[2 * BLOCK..3 * BLOCK].copy_from_slice(&newer);
    assert_eq!(read_all(&mut fs, f), want);
    clean(&mut fs);
}

#[test]
fn reflinks_between_nocow_and_cow_files_are_refused() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = nocow_file(&mut fs, "n", &random(2 * BLOCK, 1));
    let g = file(&mut fs, root, "c", &random(2 * BLOCK, 2));
    let h = nocow_file(&mut fs, "n2", b"");
    assert_eq!(fs.clone_range(f, 0, g, 0, 0), Err(Error::Invalid));
    assert_eq!(fs.clone_range(g, 0, f, 0, 0), Err(Error::Invalid));
    assert_eq!(fs.clone_range(f, 0, h, 0, 0).unwrap(), 2 * BLOCK as u64);
    assert_eq!(read_all(&mut fs, h), read_all(&mut fs, f));
    // The clone is shared, so writing to it copies.
    write_all(&mut fs, h, 0, b"zz");
    assert_ne!(read_all(&mut fs, h), read_all(&mut fs, f));
    clean(&mut fs);
}

// ---------------------------------------------------------------------------
// fallocate
// ---------------------------------------------------------------------------

#[test]
fn fallocate_extends_with_zeros_and_reserves_space() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = file(&mut fs, root, "a", b"head");
    fs.sync().unwrap();
    let used0 = fs.fsinfo().bytes_used;
    fs.fallocate(f, 0, 0, 1 << 20).unwrap();
    let st = fs.stat(f).unwrap();
    assert_eq!(st.size, 1 << 20);
    assert!(fs.fsinfo().bytes_used >= used0 + 255 * BLOCK as u64);
    let mut want = vec![0u8; 1 << 20];
    want[..4].copy_from_slice(b"head");
    assert_eq!(read_all(&mut fs, f), want);
    clean(&mut fs);
    let mut fs = remount(&disk, fs);
    assert_eq!(read_all(&mut fs, f), want);
    // Writing into reserved space needs no more of it.
    let used = fs.fsinfo().bytes_used;
    write_all(&mut fs, f, 4096 * 7, &random(4096 * 10, 3));
    fs.sync().unwrap();
    assert!(fs.fsinfo().bytes_used <= used + 64 * 1024);
    want[4096 * 7..4096 * 17].copy_from_slice(&random(4096 * 10, 3));
    assert_eq!(read_all(&mut fs, f), want);
    clean(&mut fs);
}

#[test]
fn fallocate_keep_size_reserves_past_the_end() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = file(&mut fs, root, "a", &random(5000, 4));
    let nbytes = fs.stat(f).unwrap().nbytes;
    fs.fallocate(f, FALLOC_FL_KEEP_SIZE, 0, 64 * 1024).unwrap();
    let st = fs.stat(f).unwrap();
    assert_eq!(st.size, 5000);
    assert!(st.nbytes > nbytes);
    assert_eq!(read_all(&mut fs, f), random(5000, 4));
    // Growing the file reveals zeros, not stale bytes.
    fs.truncate(f, 40_000).unwrap();
    let got = read_all(&mut fs, f);
    assert_eq!(&got[..5000], &random(5000, 4)[..]);
    assert!(got[5000..].iter().all(|&b| b == 0));
    fs.truncate(f, 0).unwrap();
    assert_eq!(fs.stat(f).unwrap().nbytes, 0);
    clean(&mut fs);
}

#[test]
fn punching_holes_frees_whole_blocks_and_zeros_the_edges() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let data = random(16 * BLOCK, 5);
    let f = file(&mut fs, root, "a", &data);
    fs.sync().unwrap();
    let blocks = fs.stat(f).unwrap().nbytes;
    let mode = FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE;
    fs.fallocate(f, mode, 3 * BLOCK as u64 + 100, 6 * BLOCK as u64)
        .unwrap();
    let mut want = data.clone();
    want[3 * BLOCK + 100..9 * BLOCK + 100].fill(0);
    assert_eq!(read_all(&mut fs, f), want);
    let st = fs.stat(f).unwrap();
    assert_eq!(st.size, 16 * BLOCK as u64);
    assert_eq!(st.nbytes, blocks - 5 * BLOCK as u64);
    // Past the end changes nothing; a hole in a hole is fine.
    fs.fallocate(f, mode, 20 * BLOCK as u64, BLOCK as u64)
        .unwrap();
    fs.fallocate(f, mode, 4 * BLOCK as u64, BLOCK as u64)
        .unwrap();
    assert_eq!(fs.stat(f).unwrap().size, 16 * BLOCK as u64);
    clean(&mut fs);
    let mut fs = remount(&disk, fs);
    assert_eq!(read_all(&mut fs, f), want);
    clean(&mut fs);
}

#[test]
fn punching_a_hole_in_a_snapshot_shared_file_leaves_the_snapshot_alone() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let sv = fs.create_subvolume(root, "sv").unwrap();
    let data = random(10 * BLOCK, 6);
    let f = file(&mut fs, sv, "a", &data);
    let snap = fs.snapshot(sv, root, "snap", false).unwrap();
    let mode = FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE;
    fs.fallocate(f, mode, 0, 10 * BLOCK as u64).unwrap();
    assert!(read_all(&mut fs, f).iter().all(|&b| b == 0));
    let s = fs.lookup(snap, "a").unwrap();
    assert_eq!(read_all(&mut fs, s), data);
    assert_eq!(fs.stat(f).unwrap().nbytes, 0);
    clean(&mut fs);
}

#[test]
fn zero_range_zeros_and_keeps_space() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let data = random(8 * BLOCK, 9);
    let f = file(&mut fs, root, "a", &data);
    fs.fallocate(
        f,
        FALLOC_FL_ZERO_RANGE,
        BLOCK as u64 - 1,
        4 * BLOCK as u64 + 2,
    )
    .unwrap();
    let mut want = data.clone();
    want[BLOCK - 1..5 * BLOCK + 1].fill(0);
    assert_eq!(read_all(&mut fs, f), want);
    assert_eq!(fs.stat(f).unwrap().nbytes, 8 * BLOCK as u64);
    // Zeroing past the end grows the file unless KEEP_SIZE.
    fs.fallocate(f, FALLOC_FL_ZERO_RANGE, 8 * BLOCK as u64, BLOCK as u64)
        .unwrap();
    assert_eq!(fs.stat(f).unwrap().size, 9 * BLOCK as u64);
    fs.fallocate(
        f,
        FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE,
        9 * BLOCK as u64,
        BLOCK as u64,
    )
    .unwrap();
    assert_eq!(fs.stat(f).unwrap().size, 9 * BLOCK as u64);
    clean(&mut fs);
}

#[test]
fn fallocate_on_small_inline_files() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = file(&mut fs, root, "a", b"tiny");
    fs.fallocate(f, 0, 0, 3 * BLOCK as u64).unwrap();
    let mut want = vec![0u8; 3 * BLOCK];
    want[..4].copy_from_slice(b"tiny");
    assert_eq!(read_all(&mut fs, f), want);
    let g = file(&mut fs, root, "b", b"0123456789");
    fs.fallocate(g, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 2, 3)
        .unwrap();
    assert_eq!(read_all(&mut fs, g), b"01\0\0\x0056789");
    clean(&mut fs);
}

#[test]
fn fallocate_arguments_are_checked() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = file(&mut fs, root, "a", b"x");
    assert_eq!(fs.fallocate(f, 0, 0, 0), Err(Error::Invalid));
    assert_eq!(
        fs.fallocate(f, FALLOC_FL_PUNCH_HOLE, 0, 1),
        Err(Error::Invalid)
    );
    assert_eq!(
        fs.fallocate(
            f,
            FALLOC_FL_PUNCH_HOLE | FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE,
            0,
            1
        ),
        Err(Error::Invalid)
    );
    assert_eq!(fs.fallocate(f, 0, u64::MAX, 2), Err(Error::Invalid));
    assert!(matches!(
        fs.fallocate(f, 0x8, 0, 1),
        Err(Error::Unsupported(_))
    ));
    assert_eq!(fs.fallocate(root, 0, 0, 1), Err(Error::IsDir));
    let l = fs.symlink(root, "l", b"a").unwrap();
    assert_eq!(fs.fallocate(l, 0, 0, 1), Err(Error::Invalid));
}

#[test]
fn fallocate_fails_cleanly_when_the_disk_is_full() {
    let (disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    let f = file(&mut fs, root, "a", b"x");
    assert_eq!(fs.fallocate(f, 0, 0, 64 * MIB as u64), Err(Error::NoSpace));
    clean(&mut fs);
    let mut fs = remount(&disk, fs);
    clean(&mut fs);
    fs.truncate(f, 0).unwrap();
    fs.unlink(root, "a").unwrap();
    clean(&mut fs);
}

#[test]
fn preallocated_extents_survive_snapshots_and_clones() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let sv = fs.create_subvolume(root, "sv").unwrap();
    let f = file(&mut fs, sv, "a", b"");
    fs.fallocate(f, 0, 0, 8 * BLOCK as u64).unwrap();
    let snap = fs.snapshot(sv, root, "snap", false).unwrap();
    let g = file(&mut fs, root, "clone", b"");
    fs.clone_range(f, 0, g, 0, 0).unwrap();
    write_all(&mut fs, f, BLOCK as u64, &random(BLOCK, 1));
    let s = fs.lookup(snap, "a").unwrap();
    assert_eq!(read_all(&mut fs, s), vec![0u8; 8 * BLOCK]);
    assert_eq!(read_all(&mut fs, g), vec![0u8; 8 * BLOCK]);
    let mut want = vec![0u8; 8 * BLOCK];
    want[BLOCK..2 * BLOCK].copy_from_slice(&random(BLOCK, 1));
    assert_eq!(read_all(&mut fs, f), want);
    clean(&mut fs);
    let mut fs = remount(&disk, fs);
    fs.delete_subvolume(root, "snap").unwrap();
    fs.unlink(root, "clone").unwrap();
    clean(&mut fs);
}

/// A read fills every byte of the caller's buffer: data from extents and
/// zeros for holes, preallocated blocks and the unaligned edges, even when
/// the buffer held something else before (reads zero only the gaps).
#[test]
fn reads_overwrite_a_dirty_buffer_with_holes_and_preallocated_zeros() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "sparse", FileKind::Regular, 0o644, 0).unwrap();
    let a = random(3 * BLOCK, 1);
    let b = random(BLOCK + 100, 2);
    // data | hole | prealloc | data | hole up to the size
    write_all(&mut fs, f, 0, &a);
    fs.fallocate(f, FALLOC_FL_KEEP_SIZE, (6 * BLOCK) as u64, (2 * BLOCK) as u64)
        .unwrap();
    write_all(&mut fs, f, (9 * BLOCK + 7) as u64, &b);
    fs.truncate(f, (14 * BLOCK) as u64).unwrap();
    let mut want = vec![0u8; 14 * BLOCK];
    want[..a.len()].copy_from_slice(&a);
    want[9 * BLOCK + 7..9 * BLOCK + 7 + b.len()].copy_from_slice(&b);
    let mut fs = remount(&disk, fs);
    let mut rng = Lcg(9);
    for _ in 0..200 {
        let off = rng.below(want.len() as u64) as usize;
        let len = 1 + rng.below((want.len() - off) as u64) as usize;
        let mut buf = vec![0xA5u8; len];
        assert_eq!(fs.read(f, off as u64, &mut buf).unwrap(), len);
        assert_eq!(buf, want[off..off + len], "read {off}+{len}");
    }
    clean(&mut fs);
}
