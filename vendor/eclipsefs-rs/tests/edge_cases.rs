#![cfg(feature = "std")]
//! Argument validation, boundaries of the on-disk structures, space
//! accounting, and a randomized comparison against an in-memory model.

mod common;

use std::collections::BTreeMap;

use common::*;
use eclipsefs::device::BlockDevice;
use eclipsefs::format::Superblock;
use eclipsefs::fs::COMMIT_OPS;
use eclipsefs::{mkfs, EclipseFS, Error, FileKind};

fn used(fs: &EclipseFS) -> u64 {
    fs.fsinfo().bytes_used
}

// --- mkfs and mount --------------------------------------------------------

#[test]
fn mkfs_rejects_tiny_devices_and_long_labels() {
    let disk = MemDisk::new(63 * BLOCK);
    assert!(matches!(mkfs::format(&*disk, &opts()), Err(Error::NoSpace)));

    let disk = MemDisk::new(8 * MIB);
    let mut o = opts();
    o.label = "x".repeat(64);
    assert!(matches!(mkfs::format(&*disk, &o), Err(Error::Invalid)));
    o.label = "x".repeat(63);
    mkfs::format(&*disk, &o).unwrap();
    let fs = EclipseFS::mount(disk, false).unwrap();
    assert_eq!(fs.label(), o.label);
}

#[test]
fn small_devices_format_and_work() {
    // 64 blocks is one group but leaves no room for the inode table.
    let disk = MemDisk::new(64 * BLOCK);
    assert!(matches!(mkfs::format(&*disk, &opts()), Err(Error::NoSpace)));
    let (_disk, mut fs) = fresh(MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, f, 0, b"tiny");
    assert_eq!(read_all(&mut fs, f), b"tiny");
}

#[test]
fn identity_comes_from_mkfs_options() {
    let (_disk, fs) = fresh(8 * MIB);
    assert_eq!(fs.label(), "eclipse");
    assert_eq!(fs.uuid(), [1; 16]);
    assert_eq!(fs.nodesize(), 4096);
    assert_eq!(fs.sectorsize(), 4096);
    let info = fs.fsinfo();
    assert_eq!(info.block_size, 4096);
    assert_eq!(info.total_bytes, 8 * MIB as u64);
    assert!(info.bytes_used > 0 && info.bytes_used < info.total_bytes);
}

#[test]
fn mount_rejects_blank_short_and_truncated_devices() {
    let blank = MemDisk::new(8 * MIB);
    assert!(matches!(
        EclipseFS::mount(blank, false),
        Err(Error::BadSuperblock)
    ));

    let short = MemDisk::new(100);
    assert!(matches!(
        EclipseFS::mount(short, false),
        Err(Error::BadSuperblock)
    ));

    let (disk, fs) = fresh(8 * MIB);
    drop(fs);
    disk.resize(4 * MIB);
    assert!(matches!(
        EclipseFS::mount(disk, false),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn trailing_space_smaller_than_a_group_is_left_out() {
    // 128 MiB + 32 blocks: too little for a second group.
    let (_disk, fs) = fresh(128 * MIB + 32 * BLOCK);
    assert_eq!(fs.fsinfo().total_bytes, 128 * MIB as u64);
}

#[test]
fn read_only_mount_reads_everything() {
    let (disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    let d = fs.create(root, "d", FileKind::Dir, 0o755, 0).unwrap();
    let f = fs.create(d, "f", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, f, 0, &pattern(10_000, 1));
    let l = fs.symlink(d, "l", b"f").unwrap();
    fs.sync().unwrap();
    drop(fs);

    let mut ro = EclipseFS::mount(disk, true).unwrap();
    assert_eq!(ro.lookup(root, "d").unwrap(), d);
    assert_eq!(ro.readdir(d).unwrap().len(), 2);
    assert_eq!(read_all(&mut ro, f), pattern(10_000, 1));
    assert_eq!(ro.read_link(l).unwrap(), b"f");
    assert_eq!(ro.parent(f).unwrap(), d);
    ro.sync().unwrap();
    assert!(!matches!(ro.grow_to_device(), Ok(true)));
    assert!(ro.truncate(f, 0).is_err());
    assert!(ro.set_attr(f, Some(0o600), None, None, None, None).is_err());
    assert!(ro.link(root, "f2", f).is_err());
    assert!(ro.rename(d, "f", root, "f").is_err());
    assert!(ro.write_symlink(l, 0, b"g").is_err());
    assert_eq!(read_all(&mut ro, f), pattern(10_000, 1));
}

// --- type checks on data operations ----------------------------------------

#[test]
fn data_operations_check_the_node_kind() {
    let (_disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    let d = fs.create(root, "d", FileKind::Dir, 0o755, 0).unwrap();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    let l = fs.symlink(root, "l", b"f").unwrap();
    let fifo = fs.create(root, "p", FileKind::Fifo, 0o644, 0).unwrap();

    assert!(matches!(fs.write(d, 0, b"x"), Err(Error::IsDir)));
    assert!(matches!(fs.read(d, 0, &mut [0; 4]), Err(Error::IsDir)));
    assert!(matches!(fs.truncate(d, 0), Err(Error::IsDir)));
    assert!(matches!(fs.write(l, 0, b"x"), Err(Error::Invalid)));
    assert!(matches!(fs.truncate(l, 0), Err(Error::Invalid)));
    assert!(matches!(fs.write(fifo, 0, b"x"), Err(Error::Invalid)));
    assert!(matches!(fs.read_link(f), Err(Error::Invalid)));
    assert!(matches!(fs.write_symlink(f, 0, b"x"), Err(Error::Invalid)));
    assert!(matches!(fs.link(root, "d2", d), Err(Error::IsDir)));
    assert!(matches!(fs.lookup(f, "x"), Err(Error::NotDir)));
    assert!(matches!(fs.readdir(f), Err(Error::NotDir)));
    assert!(fs.create(f, "x", FileKind::Regular, 0o644, 0).is_err());
}

#[test]
fn offsets_that_overflow_are_invalid() {
    let (_disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    assert!(matches!(
        fs.write(f, u64::MAX - 1, b"abc"),
        Err(Error::Invalid)
    ));
    assert_eq!(fs.write(f, u64::MAX, b"").unwrap(), 0);
    assert_eq!(fs.stat(f).unwrap().size, 0);
}

#[test]
fn reads_stop_at_end_of_file() {
    let (_disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, f, 0, &pattern(5000, 2));
    let mut buf = vec![0u8; 100];
    assert_eq!(fs.read(f, 4950, &mut buf).unwrap(), 50);
    assert_eq!(&buf[..50], &pattern(5000, 2)[4950..]);
    assert_eq!(fs.read(f, 5000, &mut buf).unwrap(), 0);
    assert_eq!(fs.read(f, 1 << 40, &mut buf).unwrap(), 0);
    assert_eq!(fs.read(f, 0, &mut []).unwrap(), 0);
}

#[test]
fn missing_inodes_are_not_found() {
    let (_disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    fs.unlink(root, "f").unwrap();
    assert!(fs.stat(f).is_err());
    assert!(fs.stat(1_000_000).is_err());
    assert!(matches!(fs.lookup(root, "f"), Err(Error::NotFound)));
    assert!(matches!(fs.unlink(root, "f"), Err(Error::NotFound)));
    assert!(matches!(
        fs.rename(root, "f", root, "g"),
        Err(Error::NotFound)
    ));
}

// --- file contents ----------------------------------------------------------

#[test]
fn overwrites_keep_the_surrounding_bytes() {
    let (disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    let mut model = pattern(3 * BLOCK + 123, 3);
    write_all(&mut fs, f, 0, &model);
    for (off, len) in [(0, 1), (4095, 2), (5000, 4000), (3 * BLOCK, 123), (100, 1)] {
        let chunk = vec![0xEE; len];
        write_all(&mut fs, f, off as u64, &chunk);
        model[off..off + len].copy_from_slice(&chunk);
    }
    assert_eq!(read_all(&mut fs, f), model);
    let mut fs = remount(&disk, fs);
    assert_eq!(read_all(&mut fs, f), model);
}

#[test]
fn shrinking_then_growing_reveals_zeros() {
    let (_disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, f, 0, &[0xFF; 3 * BLOCK]);
    fs.truncate(f, 100).unwrap();
    fs.truncate(f, 3 * BLOCK as u64).unwrap();
    let data = read_all(&mut fs, f);
    assert!(data[..100].iter().all(|&b| b == 0xFF));
    assert!(data[100..].iter().all(|&b| b == 0));

    // Same through a write past the end instead of truncate.
    fs.truncate(f, 10).unwrap();
    write_all(&mut fs, f, 2 * BLOCK as u64, b"z");
    let data = read_all(&mut fs, f);
    assert_eq!(data.len(), 2 * BLOCK + 1);
    assert!(data[10..2 * BLOCK].iter().all(|&b| b == 0));
}

#[test]
fn sparse_writes_do_not_allocate_the_holes() {
    let (_disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let before = used(&fs);
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, f, 1 << 30, b"end");
    let st = fs.stat(f).unwrap();
    assert_eq!(st.size, (1 << 30) + 3);
    assert_eq!(st.nbytes, BLOCK as u64);
    assert!(used(&fs) - before <= 2 * BLOCK as u64);
    let mut buf = [1u8; 64];
    assert_eq!(fs.read(f, 12345, &mut buf).unwrap(), 64);
    assert_eq!(buf, [0; 64]);
}

#[test]
fn space_accounting_returns_to_baseline() {
    let (disk, mut fs) = fresh(16 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    // The directory's first block is allocated with its first entry.
    let base = used(&fs);
    write_all(&mut fs, f, 0, &pattern(MIB, 4));
    assert_eq!(used(&fs) - base, MIB as u64);
    assert_eq!(fs.stat(f).unwrap().nbytes, MIB as u64);
    fs.truncate(f, MIB as u64 / 2).unwrap();
    assert_eq!(used(&fs) - base, MIB as u64 / 2);
    fs.truncate(f, 0).unwrap();
    assert_eq!(used(&fs), base);
    fs.unlink(root, "f").unwrap();
    assert_eq!(used(&fs), base);
    let fs = remount(&disk, fs);
    assert_eq!(used(&fs), base);
}

#[test]
fn extent_count_boundaries_round_trip() {
    // Every other block written: n blocks give n extents. Cross the six
    // inline extents and one full overflow block (169).
    for n in [6usize, 7, 175, 176, 400] {
        let (disk, mut fs) = fresh(16 * MIB);
        let root = fs.root_ino();
        let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
        let base = used(&fs);
        for i in 0..n {
            write_all(&mut fs, f, (2 * i * BLOCK) as u64, &pattern(BLOCK, i));
        }
        let mut fs = remount(&disk, fs);
        for i in 0..n {
            let mut buf = vec![0u8; BLOCK];
            fs.read(f, (2 * i * BLOCK) as u64, &mut buf).unwrap();
            assert_eq!(buf, pattern(BLOCK, i), "n={} block {}", n, i);
            if i + 1 < n {
                fs.read(f, ((2 * i + 1) * BLOCK) as u64, &mut buf).unwrap();
                assert!(buf.iter().all(|&b| b == 0));
            }
        }
        assert_eq!(fs.stat(f).unwrap().nbytes, (n * BLOCK) as u64);
        fs.truncate(f, 0).unwrap();
        assert_eq!(used(&fs), base, "n={}", n);
        let fs = remount(&disk, fs);
        assert_eq!(used(&fs), base);
    }
}

// --- symlinks ---------------------------------------------------------------

#[test]
fn symlink_targets_are_validated() {
    let (_disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    assert!(matches!(fs.symlink(root, "e", b""), Err(Error::Invalid)));
    assert!(matches!(
        fs.symlink(root, "big", &[b'a'; 4097]),
        Err(Error::Invalid)
    ));
    let l = fs.symlink(root, "max", &[b'a'; 4096]).unwrap();
    assert_eq!(fs.read_link(l).unwrap().len(), 4096);
    assert!(matches!(
        fs.write_symlink(l, 4096, b"x"),
        Err(Error::Invalid)
    ));
    assert!(matches!(
        fs.write_symlink(l, u64::MAX, b"x"),
        Err(Error::Invalid)
    ));
}

#[test]
fn symlink_inline_boundary() {
    let (disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    let a = fs.symlink(root, "a", &[b'a'; 152]).unwrap();
    let b = fs.symlink(root, "b", &[b'b'; 153]).unwrap();
    assert_eq!(fs.stat(a).unwrap().nbytes, 152);
    assert_eq!(fs.stat(b).unwrap().nbytes, BLOCK as u64);
    let base = used(&fs);
    // Growing an inline target past 152 bytes moves it to a block.
    fs.write_symlink(a, 152, b"!").unwrap();
    assert_eq!(fs.stat(a).unwrap().nbytes, BLOCK as u64);
    assert_eq!(used(&fs), base + BLOCK as u64);
    let mut fs = remount(&disk, fs);
    let mut want = vec![b'a'; 152];
    want.push(b'!');
    assert_eq!(fs.read_link(a).unwrap(), want);
    assert_eq!(fs.read_link(b).unwrap(), vec![b'b'; 153]);
    fs.unlink(root, "a").unwrap();
    fs.unlink(root, "b").unwrap();
    assert_eq!(used(&fs), base - BLOCK as u64);
}

#[test]
fn write_symlink_past_the_end_fills_with_zeros() {
    let (_disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    let l = fs.symlink(root, "l", b"ab").unwrap();
    assert_eq!(fs.write_symlink(l, 5, b"cd").unwrap(), 2);
    assert_eq!(fs.read_link(l).unwrap(), b"ab\0\0\0cd");
    assert_eq!(fs.write_symlink(l, 0, b"X").unwrap(), 1);
    assert_eq!(fs.read_link(l).unwrap(), b"Xb\0\0\0cd");
    assert_eq!(fs.stat(l).unwrap().size, 7);
}

// --- namespace --------------------------------------------------------------

#[test]
fn special_files_keep_kind_mode_and_rdev() {
    let (disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    let kinds = [
        ("c", FileKind::CharDevice, 0o620, 0x0501),
        ("b", FileKind::BlockDevice, 0o660, 0x0803),
        ("p", FileKind::Fifo, 0o644, 0),
        ("s", FileKind::Socket, 0o755, 0),
    ];
    for (name, kind, mode, rdev) in kinds {
        fs.create(root, name, kind, mode, rdev).unwrap();
    }
    let mut fs = remount(&disk, fs);
    for (name, kind, mode, rdev) in kinds {
        let ino = fs.lookup(root, name).unwrap();
        let st = fs.stat(ino).unwrap();
        assert_eq!(st.kind, kind);
        assert_eq!(st.mode & 0o7777, mode);
        assert_eq!(st.rdev, rdev);
        assert_eq!(st.size, 0);
    }
    let kinds: Vec<_> = fs
        .readdir(root)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            FileKind::CharDevice,
            FileKind::BlockDevice,
            FileKind::Fifo,
            FileKind::Socket
        ]
    );
}

#[test]
fn set_attr_keeps_the_file_type_bits() {
    let (_disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    let d = fs.create(root, "d", FileKind::Dir, 0o755, 0).unwrap();
    fs.set_attr(d, Some(0o100_700), Some(5), None, None, None)
        .unwrap();
    let st = fs.stat(d).unwrap();
    assert_eq!(st.kind, FileKind::Dir);
    assert_eq!(st.mode & 0o170000, 0o040000);
    assert_eq!(st.mode & 0o7777, 0o700);
    assert_eq!(st.uid, 5);
    assert_eq!(st.gid, 0);
    assert_eq!(st.ctime, clock());
}

#[test]
fn directory_link_counts() {
    let (disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    assert_eq!(fs.stat(root).unwrap().nlink, 2);
    let a = fs.create(root, "a", FileKind::Dir, 0o755, 0).unwrap();
    let b = fs.create(root, "b", FileKind::Dir, 0o755, 0).unwrap();
    fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    assert_eq!(fs.stat(root).unwrap().nlink, 4);
    let c = fs.create(a, "c", FileKind::Dir, 0o755, 0).unwrap();
    assert_eq!(fs.stat(a).unwrap().nlink, 3);
    fs.rename(a, "c", b, "c").unwrap();
    assert_eq!(fs.stat(a).unwrap().nlink, 2);
    assert_eq!(fs.stat(b).unwrap().nlink, 3);
    assert_eq!(fs.parent(c).unwrap(), b);
    let mut fs = remount(&disk, fs);
    assert_eq!(fs.stat(b).unwrap().nlink, 3);
    fs.unlink(b, "c").unwrap();
    fs.unlink(root, "a").unwrap();
    assert_eq!(fs.stat(b).unwrap().nlink, 2);
    assert_eq!(fs.stat(root).unwrap().nlink, 3);
    assert_eq!(fs.parent(root).unwrap(), root);
}

#[test]
fn rename_type_conflicts() {
    let (_disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    let d = fs.create(root, "d", FileKind::Dir, 0o755, 0).unwrap();
    fs.create(root, "e", FileKind::Dir, 0o755, 0).unwrap();
    fs.create(d, "inside", FileKind::Regular, 0o644, 0).unwrap();
    fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();

    assert!(matches!(
        fs.rename(root, "e", root, "f"),
        Err(Error::NotDir)
    ));
    assert!(matches!(fs.rename(root, "f", root, "d"), Err(Error::IsDir)));
    assert!(matches!(
        fs.rename(root, "e", root, "d"),
        Err(Error::NotEmpty)
    ));
    assert!(matches!(fs.rename(root, "d", d, "x"), Err(Error::Invalid)));
    // Onto an empty directory replaces it.
    fs.unlink(d, "inside").unwrap();
    fs.rename(root, "e", root, "d").unwrap();
    assert_eq!(fs.readdir(root).unwrap().len(), 2);
    assert_eq!(fs.stat(root).unwrap().nlink, 3);
}

#[test]
fn rename_over_a_hard_link() {
    let (_disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    fs.link(root, "g", f).unwrap();
    assert_eq!(fs.stat(f).unwrap().nlink, 2);
    // Both names point at the same inode: nothing to do.
    fs.rename(root, "f", root, "g").unwrap();
    assert_eq!(fs.stat(f).unwrap().nlink, 2);
    assert_eq!(fs.readdir(root).unwrap().len(), 2);

    let h = fs.create(root, "h", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, h, 0, b"hello");
    fs.rename(root, "h", root, "g").unwrap();
    assert_eq!(fs.stat(f).unwrap().nlink, 1);
    assert_eq!(fs.lookup(root, "g").unwrap(), h);
    assert_eq!(read_all(&mut fs, h), b"hello");
}

#[test]
fn many_files_and_directories_beyond_the_caches() {
    // More inodes than the 4096-entry inode cache and more directories than
    // the 256-entry directory cache.
    let (disk, mut fs) = fresh(64 * MIB);
    let root = fs.root_ino();
    let mut files = Vec::new();
    for d in 0..300 {
        let dir = fs
            .create(root, &format!("d{}", d), FileKind::Dir, 0o755, 0)
            .unwrap();
        for i in 0..15 {
            let f = fs
                .create(dir, &format!("f{}", i), FileKind::Regular, 0o644, 0)
                .unwrap();
            if i == 0 {
                write_all(&mut fs, f, 0, format!("{}", d).as_bytes());
            }
            files.push((dir, i, f));
        }
    }
    for &(dir, i, f) in files.iter().step_by(7) {
        assert_eq!(fs.lookup(dir, &format!("f{}", i)).unwrap(), f);
        assert_eq!(fs.parent(f).unwrap(), dir);
    }
    let mut fs = remount(&disk, fs);
    assert_eq!(fs.readdir(root).unwrap().len(), 300);
    for d in (0..300).step_by(13) {
        let dir = fs.lookup(root, &format!("d{}", d)).unwrap();
        assert_eq!(fs.readdir(dir).unwrap().len(), 15);
        let f = fs.lookup(dir, "f0").unwrap();
        assert_eq!(read_all(&mut fs, f), format!("{}", d).as_bytes());
    }
    let report = fs.check().unwrap();
    assert_eq!(report.inodes, 1 + 300 + 300 * 15);
    assert_eq!(report.directories, 301);
}

#[test]
fn create_fails_cleanly_when_the_name_exists() {
    let (_disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    fs.create(root, "x", FileKind::Regular, 0o644, 0).unwrap();
    let base = used(&fs);
    assert!(matches!(
        fs.create(root, "x", FileKind::Dir, 0o755, 0),
        Err(Error::Exists)
    ));
    assert!(matches!(fs.symlink(root, "x", b"t"), Err(Error::Exists)));
    assert_eq!(used(&fs), base);
    assert_eq!(fs.readdir(root).unwrap().len(), 1);
}

// --- journal ----------------------------------------------------------------

fn visible_on_disk(disk: &std::sync::Arc<MemDisk>, name: &str) -> bool {
    // A second, read-only mount sees what is on the device (and in the
    // journal), not what the first mount holds in memory.
    let mut ro = EclipseFS::mount(disk.clone(), true).unwrap();
    let root = ro.root_ino();
    ro.lookup(root, name).is_ok()
}

#[test]
fn operations_are_grouped_until_sync() {
    let (disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    fs.create(root, "a", FileKind::Regular, 0o644, 0).unwrap();
    assert!(!visible_on_disk(&disk, "a"));
    fs.sync().unwrap();
    assert!(visible_on_disk(&disk, "a"));
    // Enough operations commit on their own.
    for i in 0..COMMIT_OPS {
        fs.create(root, &format!("n{}", i), FileKind::Regular, 0o644, 0)
            .unwrap();
    }
    assert!(visible_on_disk(&disk, "n0"));
    fs.create(root, "late", FileKind::Regular, 0o644, 0)
        .unwrap();
    assert!(!visible_on_disk(&disk, "late"));
    // Unmounting commits too.
    drop(fs);
    assert!(visible_on_disk(&disk, "late"));
}

#[test]
fn space_freed_by_an_open_transaction_is_reused_after_commit() {
    let (_disk, mut fs) = fresh(4 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "big", FileKind::Regular, 0o644, 0).unwrap();
    let free = fs.fsinfo().total_bytes - fs.fsinfo().bytes_used;
    let data = vec![1u8; free as usize - 64 * 1024];
    write_all(&mut fs, f, 0, &data);
    fs.unlink(root, "big").unwrap();
    // The device is nearly full on disk until the unlink commits; the next
    // write commits it first instead of failing.
    let g = fs
        .create(root, "again", FileKind::Regular, 0o644, 0)
        .unwrap();
    write_all(&mut fs, g, 0, &data);
    fs.check().unwrap();
}

#[test]
fn images_without_a_journal_are_updated_in_place() {
    let disk = MemDisk::new(8 * MIB);
    mkfs::format(&*disk, &opts()).unwrap();
    // Turn it into a pre-journal (v0.3.0) superblock.
    let mut buf = vec![0u8; 4096];
    disk.read_at(0, &mut buf).unwrap();
    let mut sb = Superblock::decode(&buf).unwrap();
    sb.flags = 0;
    sb.journal_start = 0;
    sb.journal_blocks = 0;
    disk.write_at(0, &sb.encode()).unwrap();

    let mut fs = EclipseFS::mount(disk.clone(), false).unwrap();
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    // Without a journal every operation goes home at once.
    assert!(visible_on_disk(&disk, "f"));
    write_all(&mut fs, f, 0, &pattern(50_000, 5));
    fs.unlink(root, "f").unwrap();
    let g = fs.create(root, "g", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, g, 0, b"old format");
    drop(fs);
    let mut fs = EclipseFS::mount(disk.clone(), false).unwrap();
    assert_eq!(read_all(&mut fs, g), b"old format");
    disk.read_at(0, &mut buf).unwrap();
    assert!(!Superblock::decode(&buf).unwrap().has_journal());
}

#[test]
fn check_reports_damage() {
    let (disk, mut fs) = fresh(8 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, f, 0, &pattern(3 * BLOCK, 1));
    let clean = fs.check().unwrap();
    assert_eq!(clean.inodes, 2);
    assert_eq!(clean.directories, 1);
    drop(fs);
    // Drop the last transaction from the journal so mount does not repair
    // what follows, then mark a free block used in group 0's bitmap.
    let mut buf = vec![0u8; 4096];
    disk.read_at(0, &mut buf).unwrap();
    let jstart = Superblock::decode(&buf).unwrap().journal_start;
    disk.write_at(jstart * 4096, &[0u8; 4096]).unwrap();
    let mut bm = vec![0u8; 4096];
    disk.read_at(4096, &mut bm).unwrap();
    assert_eq!(bm[200] & 1, 0);
    bm[200] |= 1;
    disk.write_at(4096, &bm).unwrap();
    let mut fs = EclipseFS::mount(disk.clone(), true).unwrap();
    assert_eq!(
        fs.check().unwrap_err(),
        Error::Corrupt("fsck: block marked used but unowned")
    );
}

// --- randomized model check -----------------------------------------------

#[derive(Clone, Default)]
struct Model {
    // (dir index, name) -> contents
    files: BTreeMap<(usize, String), Vec<u8>>,
}

#[test]
fn random_operations_match_a_model() {
    let (disk, mut fs) = fresh(32 * MIB);
    let root = fs.root_ino();
    let mut dirs = vec![root];
    for i in 0..3 {
        dirs.push(
            fs.create(root, &format!("dir{}", i), FileKind::Dir, 0o755, 0)
                .unwrap(),
        );
    }
    let base = used(&fs);
    let mut model = Model::default();
    let mut rng = Lcg(0xEC11_95E5);

    for step in 0..3000 {
        let d = rng.below(dirs.len() as u64) as usize;
        let name = format!("n{}", rng.below(24));
        let key = (d, name.clone());
        match rng.below(10) {
            0 | 1 => {
                let r = fs.create(dirs[d], &name, FileKind::Regular, 0o644, 0);
                if let std::collections::btree_map::Entry::Vacant(e) = model.files.entry(key) {
                    r.unwrap();
                    e.insert(Vec::new());
                } else {
                    assert!(matches!(r, Err(Error::Exists)), "step {}", step);
                }
            }
            2..=4 => {
                if let Some(data) = model.files.get_mut(&key) {
                    let ino = fs.lookup(dirs[d], &name).unwrap();
                    let off = rng.below(64 * 1024) as usize;
                    let len = 1 + rng.below(20_000) as usize;
                    let chunk = pattern(len, step);
                    write_all(&mut fs, ino, off as u64, &chunk);
                    if data.len() < off + len {
                        data.resize(off + len, 0);
                    }
                    data[off..off + len].copy_from_slice(&chunk);
                }
            }
            5 => {
                if let Some(data) = model.files.get_mut(&key) {
                    let ino = fs.lookup(dirs[d], &name).unwrap();
                    let size = rng.below(80 * 1024) as usize;
                    fs.truncate(ino, size as u64).unwrap();
                    data.resize(size, 0);
                }
            }
            6 => {
                let r = fs.unlink(dirs[d], &name);
                if model.files.remove(&key).is_some() {
                    r.unwrap();
                } else {
                    assert!(matches!(r, Err(Error::NotFound)), "step {}", step);
                }
            }
            7 => {
                let nd = rng.below(dirs.len() as u64) as usize;
                let nn = format!("n{}", rng.below(24));
                let r = fs.rename(dirs[d], &name, dirs[nd], &nn);
                match model.files.remove(&key) {
                    Some(data) => {
                        r.unwrap();
                        model.files.insert((nd, nn), data);
                    }
                    None => assert!(matches!(r, Err(Error::NotFound))),
                }
            }
            8 => {
                if let Some(data) = model.files.get(&key) {
                    let ino = fs.lookup(dirs[d], &name).unwrap();
                    let off = rng.below(data.len() as u64 + 10) as usize;
                    let mut buf = vec![0u8; 1 + rng.below(9000) as usize];
                    let n = fs.read(ino, off as u64, &mut buf).unwrap();
                    let want = &data[off.min(data.len())..(off + buf.len()).min(data.len())];
                    assert_eq!(&buf[..n], want, "step {}", step);
                }
            }
            _ => {
                if step % 3 == 0 {
                    fs = remount(&disk, fs);
                }
            }
        }
        if step % 500 == 499 {
            check(&mut fs, &dirs, &model);
        }
    }
    let mut fs = remount(&disk, fs);
    check(&mut fs, &dirs, &model);

    // Removing everything gives back every block except the ones the
    // directories grew into (they keep their tombstones until compacted).
    for (d, name) in model.files.keys() {
        fs.unlink(dirs[*d], name).unwrap();
    }
    let dir_bytes: u64 = dirs.iter().map(|&d| fs.stat(d).unwrap().nbytes).sum();
    let root_before = BLOCK as u64; // root already held the dirN entries
    assert!(used(&fs) <= base + dir_bytes - root_before);
    let left = used(&fs);
    let mut fs = remount(&disk, fs);
    assert_eq!(used(&fs), left);
    fs.check().unwrap();
}

fn check(fs: &mut EclipseFS, dirs: &[u64], model: &Model) {
    for (i, &dir) in dirs.iter().enumerate() {
        let mut names: Vec<String> = fs
            .readdir(dir)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == FileKind::Regular)
            .map(|e| e.name)
            .collect();
        names.sort();
        let want: Vec<String> = model
            .files
            .keys()
            .filter(|(d, _)| *d == i)
            .map(|(_, n)| n.clone())
            .collect();
        assert_eq!(names, want);
    }
    for ((d, name), data) in &model.files {
        let ino = fs.lookup(dirs[*d], name).unwrap();
        let got = read_all(fs, ino);
        assert!(got == *data, "{}/{} differs", d, name);
    }
}
