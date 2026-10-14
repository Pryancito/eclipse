#![cfg(feature = "std")]
//! Per-directory encryption (fscrypt v2 policies): contents and names are
//! encrypted on disk, and without the key a directory still lists, finds
//! and deletes entries by their no-key names.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};

use common::*;
use eclipsefs::cowfmt::{FALLOC_FL_KEEP_SIZE, FALLOC_FL_PUNCH_HOLE, FL_ENCRYPT, FL_NOATIME};
use eclipsefs::{Compression, EclipseFS, EncryptionPolicy, Error, FileKind};

const KEY: [u8; 64] = [0x42; 64];
const OTHER: [u8; 32] = [0x17; 32];

fn random(buf: &mut [u8]) {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    for b in buf {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        *b = (n.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as u8;
    }
}

fn setup(size: usize) -> (std::sync::Arc<MemDisk>, EclipseFS, u64, [u8; 16]) {
    let (disk, mut fs) = fresh(size);
    fs.set_random(random);
    let id = fs.add_encryption_key(&KEY).unwrap();
    let root = fs.root_ino();
    let dir = fs.create(root, "vault", FileKind::Dir, 0o700, 0).unwrap();
    fs.set_encryption_policy(dir, &EncryptionPolicy::new(id))
        .unwrap();
    (disk, fs, dir, id)
}

fn names(fs: &mut EclipseFS, dir: u64) -> Vec<String> {
    let mut v: Vec<_> = fs
        .readdir(dir)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    v.sort();
    v
}

fn disk_has(disk: &MemDisk, needle: &[u8]) -> bool {
    use eclipsefs::device::BlockDevice;
    let mut all = vec![0u8; disk.size() as usize];
    disk.read_at(0, &mut all).unwrap();
    all.windows(needle.len()).any(|w| w == needle)
}

fn mounted(disk: &std::sync::Arc<MemDisk>, fs: EclipseFS) -> EclipseFS {
    let mut fs = remount(disk, fs);
    fs.set_random(random);
    fs
}

#[test]
fn key_identifier_is_stable() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let a = fs.add_encryption_key(&KEY).unwrap();
    assert_eq!(fs.add_encryption_key(&KEY).unwrap(), a);
    assert_ne!(fs.add_encryption_key(&OTHER).unwrap(), a);
    assert!(fs.encryption_key_present(&a).unwrap());
    fs.remove_encryption_key(&a).unwrap();
    assert!(!fs.encryption_key_present(&a).unwrap());
    assert_eq!(fs.remove_encryption_key(&a), Err(Error::NotFound));
    assert_eq!(fs.add_encryption_key(&[1; 8]), Err(Error::Invalid));
}

#[test]
fn policy_needs_an_empty_directory_and_its_key() {
    let (_disk, mut fs) = fresh(16 * MIB);
    fs.set_random(random);
    let root = fs.root_ino();
    let id = fs.add_encryption_key(&KEY).unwrap();
    let other = fs.add_encryption_key(&OTHER).unwrap();
    let p = EncryptionPolicy::new(id);

    let full = fs.create(root, "full", FileKind::Dir, 0o755, 0).unwrap();
    fs.create(full, "x", FileKind::Regular, 0o644, 0).unwrap();
    assert_eq!(fs.set_encryption_policy(full, &p), Err(Error::NotEmpty));

    let f = fs
        .create(root, "file", FileKind::Regular, 0o644, 0)
        .unwrap();
    assert_eq!(fs.set_encryption_policy(f, &p), Err(Error::NotDir));

    let d = fs.create(root, "d", FileKind::Dir, 0o755, 0).unwrap();
    fs.remove_encryption_key(&id).unwrap();
    assert_eq!(fs.set_encryption_policy(d, &p), Err(Error::NoKey));
    fs.add_encryption_key(&KEY).unwrap();

    let mut bad = p;
    bad.contents_mode = 9;
    assert_eq!(fs.set_encryption_policy(d, &bad), Err(Error::Invalid));

    assert_eq!(fs.encryption_policy(d).unwrap(), None);
    fs.set_encryption_policy(d, &p).unwrap();
    assert_eq!(fs.encryption_policy(d).unwrap(), Some(p));
    // The same policy again is fine, another one is not.
    fs.set_encryption_policy(d, &p).unwrap();
    assert_eq!(
        fs.set_encryption_policy(d, &EncryptionPolicy::new(other)),
        Err(Error::Exists)
    );
    assert_ne!(fs.get_flags(d).unwrap() & FL_ENCRYPT, 0);
    assert_eq!(fs.get_flags(root).unwrap() & FL_ENCRYPT, 0);
    assert_eq!(fs.encryption_nonce(root), Err(Error::NotFound));
}

#[test]
fn no_random_source_no_policy() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let id = fs.add_encryption_key(&KEY).unwrap();
    let d = fs.create(root, "d", FileKind::Dir, 0o755, 0).unwrap();
    assert!(matches!(
        fs.set_encryption_policy(d, &EncryptionPolicy::new(id)),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn children_inherit_the_policy_with_their_own_nonce() {
    let (_disk, mut fs, dir, id) = setup(16 * MIB);
    let f = fs.create(dir, "a", FileKind::Regular, 0o644, 0).unwrap();
    let sub = fs.create(dir, "sub", FileKind::Dir, 0o755, 0).unwrap();
    let g = fs.create(sub, "b", FileKind::Regular, 0o644, 0).unwrap();
    let p = Some(EncryptionPolicy::new(id));
    for ino in [f, sub, g] {
        assert_eq!(fs.encryption_policy(ino).unwrap(), p);
        assert_ne!(fs.get_flags(ino).unwrap() & FL_ENCRYPT, 0);
    }
    let nonces = [dir, f, sub, g].map(|i| fs.encryption_nonce(i).unwrap());
    for (i, a) in nonces.iter().enumerate() {
        for b in &nonces[i + 1..] {
            assert_ne!(a, b);
        }
    }
    fs.check().unwrap();
}

#[test]
fn contents_and_names_are_not_on_disk_in_the_clear() {
    let (disk, mut fs, dir, _) = setup(32 * MIB);
    let secret = b"the treasure is buried under the old oak tree!".repeat(300);
    let f = fs
        .create(
            dir,
            "plans-for-world-domination",
            FileKind::Regular,
            0o644,
            0,
        )
        .unwrap();
    write_all(&mut fs, f, 0, &secret);
    // A small file too: encrypted files are never kept inline.
    let s = fs
        .create(dir, "tiny-note", FileKind::Regular, 0o644, 0)
        .unwrap();
    write_all(&mut fs, s, 0, b"meet at midnight by the fountain");
    let mut fs = mounted(&disk, fs);
    assert!(!disk_has(&disk, b"buried under the old oak"));
    assert!(!disk_has(&disk, b"plans-for-world-domination"));
    assert!(!disk_has(&disk, b"midnight by the fountain"));
    assert!(!disk_has(&disk, b"tiny-note"));

    // The same file outside the vault is in the clear (the search works).
    let root = fs.root_ino();
    let p = fs
        .create(
            root,
            "plans-for-world-domination",
            FileKind::Regular,
            0o644,
            0,
        )
        .unwrap();
    write_all(&mut fs, p, 0, &secret);
    let fs = mounted(&disk, fs);
    assert!(disk_has(&disk, b"buried under the old oak"));
    assert!(disk_has(&disk, b"plans-for-world-domination"));
    drop(fs);
}

#[test]
fn with_the_key_everything_works_across_remounts() {
    let (disk, mut fs, dir, _) = setup(32 * MIB);
    let mut files = vec![];
    for (i, len) in [0, 1, 15, 16, 17, 4095, 4096, 4097, 100_000]
        .into_iter()
        .enumerate()
    {
        let name = format!("file-{i}-{}", "x".repeat(i * 20));
        let f = fs.create(dir, &name, FileKind::Regular, 0o644, 0).unwrap();
        let data = pattern(len, i);
        write_all(&mut fs, f, 0, &data);
        files.push((name, data));
    }
    let sub = fs.create(dir, "sub", FileKind::Dir, 0o755, 0).unwrap();
    let deep = fs.create(sub, "deep", FileKind::Regular, 0o600, 0).unwrap();
    write_all(&mut fs, deep, 0, b"deep");

    let mut fs = mounted(&disk, fs);
    // The key is gone after a remount until it is added again.
    let root = fs.root_ino();
    let dir = fs.lookup(root, "vault").unwrap();
    assert!(fs.lookup(dir, "sub").is_err());
    fs.add_encryption_key(&KEY).unwrap();
    for (name, data) in &files {
        let f = fs.lookup(dir, name).unwrap();
        assert_eq!(&read_all(&mut fs, f), data, "{name}");
    }
    let mut want: Vec<_> = files.iter().map(|(n, _)| n.clone()).collect();
    want.push("sub".into());
    want.sort();
    assert_eq!(names(&mut fs, dir), want);
    let sub = fs.lookup(dir, "sub").unwrap();
    let deep = fs.lookup(sub, "deep").unwrap();
    assert_eq!(read_all(&mut fs, deep), b"deep");
    assert_eq!(fs.lookup(dir, "nope"), Err(Error::NotFound));
    fs.check().unwrap();
    assert!(fs.scrub().unwrap().bad_data_blocks.is_empty());
}

#[test]
fn without_the_key_names_are_hidden_but_usable() {
    let (disk, mut fs, dir, id) = setup(32 * MIB);
    let long = "L".repeat(250);
    for name in ["a", "hello.txt", "sixteen-bytes-ab", long.as_str()] {
        let f = fs.create(dir, name, FileKind::Regular, 0o644, 0).unwrap();
        write_all(&mut fs, f, 0, name.as_bytes());
    }
    let sub = fs.create(dir, "sub", FileKind::Dir, 0o755, 0).unwrap();
    fs.create(sub, "inner", FileKind::Regular, 0o644, 0)
        .unwrap();
    let plain = names(&mut fs, dir);
    fs.remove_encryption_key(&id).unwrap();

    let hidden = names(&mut fs, dir);
    assert_eq!(hidden.len(), plain.len());
    for n in &hidden {
        assert!(!plain.contains(n), "{n} shown in the clear");
        assert!(n.len() <= 255);
        assert!(n
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        // Every no-key name finds its entry, long names included.
        let ino = fs.lookup(dir, n).unwrap();
        fs.stat(ino).unwrap();
    }
    assert_eq!(fs.lookup(dir, "hello.txt"), Err(Error::NotFound));
    assert_eq!(fs.lookup(dir, "!!!"), Err(Error::NotFound));

    // Data, new names and new files need the key.
    let f = fs.lookup(dir, &hidden[0]).unwrap();
    let mut buf = [0u8; 8];
    let kind = fs.stat(f).unwrap().kind;
    if kind == FileKind::Regular {
        assert_eq!(fs.read(f, 0, &mut buf), Err(Error::NoKey));
        assert_eq!(fs.write(f, 0, b"x"), Err(Error::NoKey));
    }
    assert_eq!(
        fs.create(dir, "new", FileKind::Regular, 0o644, 0),
        Err(Error::NoKey)
    );
    assert_eq!(
        fs.rename(dir, &hidden[0], dir, "renamed"),
        Err(Error::NoKey)
    );
    let root = fs.root_ino();
    let plainf = fs
        .create(root, "plain", FileKind::Regular, 0o644, 0)
        .unwrap();
    assert_eq!(fs.link(dir, "lnk", plainf), Err(Error::NoKey));

    // Deleting by no-key name works (rm -rf of a locked directory).
    let inos: Vec<u64> = hidden.iter().map(|n| fs.lookup(dir, n).unwrap()).collect();
    let sub = inos
        .into_iter()
        .find(|&i| fs.stat(i).unwrap().kind == FileKind::Dir)
        .unwrap();
    let inner = names(&mut fs, sub);
    assert_eq!(inner.len(), 1);
    fs.unlink(sub, &inner[0]).unwrap();
    for n in &hidden {
        fs.unlink(dir, n).unwrap();
    }
    assert!(names(&mut fs, dir).is_empty());
    fs.check().unwrap();
    let mut fs = mounted(&disk, fs);
    fs.check().unwrap();
    assert!(fs.scrub().unwrap().bad_data_blocks.is_empty());
}

#[test]
fn adding_the_key_back_shows_everything_again() {
    let (_disk, mut fs, dir, id) = setup(32 * MIB);
    let f = fs.create(dir, "doc", FileKind::Regular, 0o644, 0).unwrap();
    let data = pattern(50_000, 3);
    write_all(&mut fs, f, 0, &data);
    fs.remove_encryption_key(&id).unwrap();
    assert_eq!(fs.lookup(dir, "doc"), Err(Error::NotFound));
    let mut buf = vec![0u8; 10];
    assert_eq!(fs.read(f, 0, &mut buf), Err(Error::NoKey));
    assert_eq!(fs.add_encryption_key(&KEY).unwrap(), id);
    assert_eq!(names(&mut fs, dir), ["doc"]);
    assert_eq!(read_all(&mut fs, f), data);
}

#[test]
fn a_wrong_key_finds_nothing() {
    let (disk, fs, _, id) = setup(16 * MIB);
    let mut fs = mounted(&disk, fs);
    let other = fs.add_encryption_key(&OTHER).unwrap();
    assert_ne!(other, id);
    let root = fs.root_ino();
    let dir = fs.lookup(root, "vault").unwrap();
    assert_eq!(
        fs.create(dir, "x", FileKind::Regular, 0o644, 0),
        Err(Error::NoKey)
    );
}

#[test]
fn symlinks_keep_their_target_encrypted() {
    let (disk, mut fs, dir, id) = setup(16 * MIB);
    let target = b"/very/secret/destination/of/the/link";
    let l = fs.symlink(dir, "ln", target).unwrap();
    assert_eq!(fs.read_link(l).unwrap(), target);
    let mut fs = mounted(&disk, fs);
    assert!(!disk_has(&disk, b"secret/destination"));
    fs.add_encryption_key(&KEY).unwrap();
    assert_eq!(fs.read_link(l).unwrap(), target);
    fs.remove_encryption_key(&id).unwrap();
    assert_eq!(fs.read_link(l), Err(Error::NoKey));
}

#[test]
fn truncate_punch_and_defrag_keep_contents() {
    let (disk, mut fs, dir, _) = setup(32 * MIB);
    let f = fs.create(dir, "f", FileKind::Regular, 0o644, 0).unwrap();
    let mut data = pattern(64 * 1024 + 300, 9);
    write_all(&mut fs, f, 0, &data);

    // Unaligned truncate: the tail of the last block reads as zeros.
    fs.truncate(f, 10_000).unwrap();
    data.truncate(10_000);
    fs.truncate(f, 20_000).unwrap();
    data.resize(20_000, 0);
    assert_eq!(read_all(&mut fs, f), data);

    fs.fallocate(f, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 4096, 8192)
        .unwrap();
    data[4096..12288].fill(0);
    assert_eq!(read_all(&mut fs, f), data);

    // Fragment the file, then defrag (and ask for compression, which an
    // encrypted file never gets).
    for i in 0..5u64 {
        write_all(&mut fs, f, i * 4096, &[i as u8 + 1; 100]);
        data[(i * 4096) as usize..(i * 4096 + 100) as usize].fill(i as u8 + 1);
        fs.sync().unwrap();
    }
    fs.defrag(f, 0, u64::MAX, 0, Some(Compression::Zlib))
        .unwrap();
    assert_eq!(read_all(&mut fs, f), data);
    let c = fs.compsize(f).unwrap();
    assert_eq!(c.disk_bytes, c.uncompressed_bytes);
    let mut fs = mounted(&disk, fs);
    fs.add_encryption_key(&KEY).unwrap();
    assert_eq!(read_all(&mut fs, f), data);
    fs.check().unwrap();
}

#[test]
fn compression_is_never_applied() {
    let (disk, mut fs, dir, _) = setup(32 * MIB);
    fs.set_mount_compression(Some(Compression::Zstd)).unwrap();
    let f = fs
        .create(dir, "zeros", FileKind::Regular, 0o644, 0)
        .unwrap();
    write_all(&mut fs, f, 0, &vec![0u8; 256 * 1024]);
    let mut fs = mounted(&disk, fs);
    fs.add_encryption_key(&KEY).unwrap();
    let c = fs.compsize(f).unwrap();
    assert_eq!(c.disk_bytes, c.uncompressed_bytes);
    assert_eq!(read_all(&mut fs, f), vec![0u8; 256 * 1024]);
    fs.check().unwrap();
}

#[test]
fn sharing_and_sending_are_refused() {
    let (_disk, mut fs, dir, _) = setup(32 * MIB);
    let root = fs.root_ino();
    let a = fs.create(dir, "a", FileKind::Regular, 0o644, 0).unwrap();
    let b = fs.create(dir, "b", FileKind::Regular, 0o644, 0).unwrap();
    let p = fs.create(root, "p", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, a, 0, &pattern(8192, 1));
    write_all(&mut fs, b, 0, &pattern(8192, 1));
    write_all(&mut fs, p, 0, &pattern(8192, 1));
    for (s, d) in [(a, b), (a, p), (p, a)] {
        assert!(matches!(
            fs.clone_range(s, 0, d, 0, 8192),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            fs.dedupe_range(s, 0, d, 0, 8192),
            Err(Error::Unsupported(_))
        ));
    }
    assert!(matches!(
        fs.create_subvolume(dir, "sv"),
        Err(Error::Unsupported(_))
    ));
    let snap = fs.snapshot(root, root, "snap", true).unwrap();
    assert!(matches!(
        fs.send(snap, None, &mut |_| Ok(())),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn rename_and_link_stay_within_one_policy() {
    let (_disk, mut fs, dir, _) = setup(32 * MIB);
    let root = fs.root_ino();
    let other_id = fs.add_encryption_key(&OTHER).unwrap();
    let other = fs.create(root, "other", FileKind::Dir, 0o700, 0).unwrap();
    fs.set_encryption_policy(other, &EncryptionPolicy::new(other_id))
        .unwrap();

    let f = fs.create(dir, "f", FileKind::Regular, 0o644, 0).unwrap();
    let sub = fs.create(dir, "sub", FileKind::Dir, 0o755, 0).unwrap();
    let plain = fs
        .create(root, "plain", FileKind::Regular, 0o644, 0)
        .unwrap();

    // Inside one policy, anything goes.
    fs.rename(dir, "f", sub, "g").unwrap();
    fs.link(dir, "f2", f).unwrap();
    assert_eq!(fs.lookup(sub, "g").unwrap(), f);
    // Plain files and files of another policy cannot come in.
    assert!(matches!(
        fs.rename(root, "plain", dir, "plain"),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        fs.link(dir, "p", plain),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        fs.rename(dir, "f2", other, "f"),
        Err(Error::Unsupported(_))
    ));
    // Moving out to an unencrypted directory is allowed, as on Linux.
    fs.rename(dir, "f2", root, "out").unwrap();
    assert_eq!(fs.lookup(root, "out").unwrap(), f);
    assert_eq!(read_all(&mut fs, f), b"");
    fs.check().unwrap();
}

#[test]
fn set_flags_keeps_the_encryption_flag() {
    let (_disk, mut fs, dir, _) = setup(16 * MIB);
    let root = fs.root_ino();
    let f = fs.create(dir, "f", FileKind::Regular, 0o644, 0).unwrap();
    fs.set_flags(f, FL_NOATIME).unwrap();
    assert_eq!(fs.get_flags(f).unwrap(), FL_NOATIME | FL_ENCRYPT);
    let p = fs.create(root, "p", FileKind::Regular, 0o644, 0).unwrap();
    // FL_ENCRYPT cannot be set by chattr either.
    fs.set_flags(p, FL_ENCRYPT | FL_NOATIME).unwrap();
    assert_eq!(fs.get_flags(p).unwrap(), FL_NOATIME);
    fs.check().unwrap();
}

#[test]
fn snapshots_of_encrypted_data_read_with_the_key() {
    let (disk, mut fs) = fresh(32 * MIB);
    fs.set_random(random);
    let root = fs.root_ino();
    let id = fs.add_encryption_key(&KEY).unwrap();
    let sv = fs.create_subvolume(root, "home").unwrap();
    let dir = fs.create(sv, "vault", FileKind::Dir, 0o700, 0).unwrap();
    fs.set_encryption_policy(dir, &EncryptionPolicy::new(id))
        .unwrap();
    let f = fs.create(dir, "f", FileKind::Regular, 0o644, 0).unwrap();
    let data = pattern(30_000, 4);
    write_all(&mut fs, f, 0, &data);
    let snap = fs.snapshot(sv, root, "snap", false).unwrap();
    write_all(&mut fs, f, 0, b"changed");

    let mut fs = mounted(&disk, fs);
    fs.add_encryption_key(&KEY).unwrap();
    let sdir = fs.lookup(snap, "vault").unwrap();
    let sf = fs.lookup(sdir, "f").unwrap();
    assert_eq!(read_all(&mut fs, sf), data);
    // The snapshot is writable, with the same key.
    fs.create(sdir, "new", FileKind::Regular, 0o644, 0).unwrap();
    fs.remove_encryption_key(&id).unwrap();
    assert_eq!(names(&mut fs, sdir).len(), 2);
    fs.delete_subvolume(root, "snap").unwrap();
    fs.check().unwrap();
}

#[test]
fn hard_link_counts_and_unlinks_survive_without_the_key() {
    let (disk, mut fs, dir, id) = setup(32 * MIB);
    for i in 0..50 {
        let f = fs
            .create(dir, &format!("n{i:03}"), FileKind::Regular, 0o644, 0)
            .unwrap();
        write_all(&mut fs, f, 0, &pattern(5000, i));
    }
    let mut fs = mounted(&disk, fs);
    let root = fs.root_ino();
    let dir = fs.lookup(root, "vault").unwrap();
    let before = fs.fsinfo();
    for n in names(&mut fs, dir) {
        fs.unlink(dir, &n).unwrap();
    }
    fs.sync().unwrap();
    assert!(fs.fsinfo().bytes_used < before.bytes_used);
    assert!(!fs.encryption_key_present(&id).unwrap());
    fs.check().unwrap();
}
