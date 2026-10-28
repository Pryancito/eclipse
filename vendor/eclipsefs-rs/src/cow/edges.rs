//! Corner cases of mounting, the namespace, file data and subvolumes that
//! the scenario tests do not reach: refusals, damaged images, a full disk,
//! and defensive checks fed the state they guard against.

use super::*;
use crate::fs::FIEMAP_EXTENT_DATA_INLINE;
use crate::mkfs;
use crate::testdisk::{clock, formatted, opts, Ram, MIB};
use crate::volume::{ChunkKind, Profile, Stripe, Tier};

fn mounted() -> (Arc<Ram>, CowFs) {
    let d = formatted(16);
    let mut fs = CowFs::mount(d.clone(), false).unwrap();
    fs.set_clock(clock);
    (d, fs)
}

fn remount(d: &Arc<Ram>) -> Result<CowFs> {
    CowFs::mount(d.clone(), false)
}

fn create(fs: &mut CowFs, name: &str, kind: FileKind) -> u64 {
    fs.create(fs.root_ino(), name, kind, 0o644, 0).unwrap()
}

fn file(fs: &mut CowFs, name: &str, data: &[u8]) -> u64 {
    let h = create(fs, name, FileKind::Regular);
    fs.write(h, 0, data).unwrap();
    h
}

/// Write to a new file until the data runs out.
fn fill(fs: &mut CowFs) {
    let h = create(fs, "filler", FileKind::Regular);
    let mut off = 0;
    // Big writes first, then single blocks for what they leave.
    for size in [256 * 1024, BS as usize] {
        let chunk = vec![0x5a; size];
        while let Ok(n) = fs.write(h, off, &chunk) {
            if n == 0 {
                break;
            }
            off += n as u64;
        }
    }
}

/// Rewrite the superblock at the start of `d` through `f`.
fn edit_super(d: &Ram, f: impl FnOnce(&mut SuperV4)) {
    let mut buf = vec![0u8; BS as usize];
    d.read_at(0, &mut buf).unwrap();
    let mut sb = SuperV4::decode(&buf).unwrap();
    f(&mut sb);
    d.write_at(0, &sb.encode()).unwrap();
}

// ---------------------------------------------------------------------------
// Mounting
// ---------------------------------------------------------------------------

#[test]
fn mount_refuses_a_set_that_is_not_one_filesystem() {
    assert_eq!(
        CowFs::mount_devices(Vec::new(), false, false).err(),
        Some(Error::Invalid)
    );
    let a = formatted(16);
    let b = formatted(16);
    assert_eq!(
        CowFs::mount_devices(vec![a, b], false, false).err(),
        Some(Error::Invalid)
    );
}

/// Without bitmap checksums there is nothing to repair a bad bitmap from.
#[test]
fn a_bad_bitmap_without_checksums_fails_the_mount() {
    let d = formatted(16);
    edit_super(&d, |sb| sb.bitmap_sums = None);
    // Group 0's header blocks marked free.
    d.poke(BS as usize, 0x03);
    assert!(matches!(remount(&d), Err(Error::Corrupt(_))));
}

#[test]
fn a_filesystem_larger_than_its_device_does_not_mount() {
    let d = formatted(16);
    let small = Ram::new(8 * MIB);
    small.write_at(0, &d.peek(0, 8 * MIB)).unwrap();
    assert_eq!(
        CowFs::mount(small, false).err(),
        Some(Error::Corrupt("filesystem larger than device"))
    );
}

#[test]
fn the_reference_tree_holds_only_references() {
    let (d, mut fs) = mounted();
    fs.st
        .insert(&mut fs.sp, &mut fs.refs_root, Key::new(9, 99, 0), vec![1])
        .unwrap();
    fs.store_root(REFS_TREE, fs.refs_root).unwrap();
    fs.sync().unwrap();
    drop(fs);
    assert_eq!(
        remount(&d).err(),
        Some(Error::Corrupt("reference tree item"))
    );
}

#[test]
fn the_root_of_the_default_subvolume_is_a_directory() {
    let (d, mut fs) = mounted();
    let mut i = fs.inode(FIRST_INO).unwrap();
    i.mode = (i.mode & 0o7777) | crate::format::S_IFREG;
    fs.put_inode(FIRST_INO, i).unwrap();
    fs.sync().unwrap();
    drop(fs);
    assert_eq!(
        remount(&d).err(),
        Some(Error::Corrupt("root is not a directory"))
    );
}

/// RAID10 keeps every block while one of each mirrored pair is there.
#[test]
fn raid10_is_readable_with_one_of_each_pair() {
    let c = Chunk {
        kind: ChunkKind::Data,
        profile: Profile::Raid10,
        blocks: 64,
        stripes: (1..=4).map(|devid| Stripe { devid, phys: 0 }).collect(),
    };
    assert!(readable(&c, &[1, 3]));
    assert!(!readable(&c, &[1, 2]));
}

/// The devices of a RAID1 volume.
fn volume() -> Vec<Arc<dyn BlockDevice>> {
    let devs: Vec<Arc<dyn BlockDevice>> = (0..2)
        .map(|_| Ram::new(64 * MIB) as Arc<dyn BlockDevice>)
        .collect();
    mkfs::format_devices(&devs, &opts(), Profile::Raid1, Profile::Raid1).unwrap();
    // One commit of its own, so the journal no longer holds the chunk map
    // mkfs wrote.
    let mut fs = CowFs::mount_devices(devs.clone(), false, false).unwrap();
    fs.set_clock(clock);
    file(&mut fs, "x", b"tiny");
    fs.sync().unwrap();
    drop(fs);
    devs
}

/// The chunk map comes from another copy when the first is bad, and has
/// to agree with the superblock about the system chunk.
#[test]
fn the_chunk_map_is_read_from_a_good_copy() {
    let devs = volume();
    let mut buf = vec![0u8; BS as usize];
    devs[0].read_at(0, &mut buf).unwrap();
    let vs = SuperV4::decode(&buf).unwrap().vol.unwrap();
    // Device 1 holds the system chunk at physical block 0.
    let at = vs.chunk_map * BS;
    let fs = CowFs::mount_devices(devs.clone(), true, false).unwrap();
    let v = fs.vol.clone().unwrap();
    drop(fs);
    v.write_copy(at, &[0xff; BS as usize], 0).unwrap();
    let fs = CowFs::mount_devices(devs.clone(), true, false).unwrap();
    assert!(fs.vol.clone().unwrap().map().chunks.contains_key(&0));

    // Both copies say the system chunk is smaller than the superblock does.
    let devs = volume();
    let fs = CowFs::mount_devices(devs.clone(), true, false).unwrap();
    let v = fs.vol.clone().unwrap();
    drop(fs);
    let mut map = v.map();
    map.chunks.get_mut(&0).unwrap().blocks -= 1;
    let mut bytes = map.encode();
    bytes.resize(BS as usize, 0);
    for k in 0..2 {
        v.write_copy(at, &bytes, k).unwrap();
    }
    assert_eq!(
        CowFs::mount_devices(devs, true, false).err(),
        Some(Error::Corrupt("system chunk"))
    );
}

/// A chunk map that outgrows its blocks moves to a new run and frees
/// the old one.
#[test]
fn a_growing_chunk_map_moves() {
    let devs = volume();
    let mut fs = CowFs::mount_devices(devs, false, false).unwrap();
    let old = fs.sb.vol.as_ref().unwrap().chunk_map;
    let free = fs.sp.alloc.free_blocks();
    fs.vol.clone().unwrap().with_map(|m| {
        let sys = m.chunks[&0].clone();
        for g in 1..200 {
            m.chunks.insert(g, sys.clone());
        }
    });
    fs.sp.chunks_dirty = true;
    fs.store_chunk_map().unwrap();
    let vs = fs.sb.vol.as_ref().unwrap();
    assert_ne!(vs.chunk_map, old);
    assert!(vs.chunk_map_len as u64 > BS);
    assert!(fs.sp.alloc.free_blocks() < free);
}

#[test]
fn growing_a_volume_to_its_devices() {
    let devs = volume();
    let mut fs = CowFs::mount_devices(devs, false, false).unwrap();
    assert!(!fs.grow_to_device().unwrap());
}

/// The last group already full and too little room after it for another:
/// the filesystem stays as it is.
#[test]
fn growing_by_less_than_a_group_adds_nothing() {
    let d = Ram::new((GROUP_BLOCKS * BS) as usize);
    mkfs::format(&*d, &opts()).unwrap();
    let mut fs = CowFs::mount(d, false).unwrap();
    let total = fs.sb.total_blocks;
    assert_eq!(total, GROUP_BLOCKS);
    assert!(!fs.grow_single(total + MIN_GROUP_BLOCKS - 1).unwrap());
    assert_eq!(fs.sb.total_blocks, total);
}

// ---------------------------------------------------------------------------
// Transactions
// ---------------------------------------------------------------------------

#[test]
fn timed_commits_need_a_clock() {
    let (_, mut fs) = mounted();
    assert_eq!(fs.commit_interval(), None);
    fs.clock = None;
    fs.set_commit_interval(Some(5));
    assert_eq!(fs.commit_interval(), Some(5));
    // Without a clock the interval cannot pass: only the op count counts.
    file(&mut fs, "f", b"x");
    assert!(!fs.commit_if_due().unwrap());
}

/// More bitmap blocks than the journal holds are written in place, the
/// journal emptied first so a replay cannot undo them.
#[test]
fn bitmaps_past_the_journal_are_written_in_place() {
    let (d, mut fs) = mounted();
    fs.journal.blocks = 1;
    file(&mut fs, "f", &[1; 100_000]);
    fs.sync().unwrap();
    drop(fs);
    let mut fs = remount(&d).unwrap();
    fs.check().unwrap();
}

#[test]
fn store_root_needs_a_readable_item() {
    let (_, mut fs) = mounted();
    fs.root_insert(Key::new(77, ROOT_ITEM, 0), vec![1, 2, 3])
        .unwrap();
    assert!(fs.store_root(77, fs.fs_root).is_err());
    assert_eq!(
        fs.store_root(78, fs.fs_root).err(),
        Some(Error::Corrupt("missing tree"))
    );
}

// ---------------------------------------------------------------------------
// File data
// ---------------------------------------------------------------------------

#[test]
fn no_blocks_asked_for_no_checksums_read() {
    let (_, mut fs) = mounted();
    assert!(fs.csums(100, 0).unwrap().is_empty());
}

#[test]
fn a_written_block_without_its_checksum_does_not_read() {
    let (_, mut fs) = mounted();
    let h = file(&mut fs, "f", &[9; 8192]);
    fs.sync().unwrap();
    let e = fs.extents(split(h).1, 0, 2).unwrap()[0];
    let bucket = e.pblk / CSUM_BUCKET * CSUM_BUCKET;
    fs.st
        .delete(
            &mut fs.sp,
            &mut fs.csum_root,
            Key::new(bucket, CSUM_ITEM, 0),
        )
        .unwrap();
    fs.drop_caches();
    let mut buf = vec![0u8; 8192];
    assert_eq!(
        fs.read(h, 0, &mut buf).err(),
        Some(Error::Corrupt("missing data checksum"))
    );
}

#[test]
fn an_inode_that_is_not_there_cannot_be_updated() {
    let (_, mut fs) = mounted();
    let i = InodeItem::new(FileKind::Regular, 0o644, clock(), 1);
    assert_eq!(fs.put_inode(FIRST_INO + 50, i).err(), Some(Error::NotFound));
}

#[test]
fn an_extent_off_the_block_grid_is_corrupt() {
    let (_, mut fs) = mounted();
    let h = file(&mut fs, "f", &[9; 8192]);
    let ino = split(h).1;
    let (k, v) = fs.items_of(ino, EXTENT_DATA, 1).unwrap().remove(0);
    fs.delete(k).unwrap();
    fs.insert(Key::new(ino, EXTENT_DATA, 7), v).unwrap();
    let mut buf = vec![0u8; 100];
    assert_eq!(
        fs.read(h, 0, &mut buf).err(),
        Some(Error::Corrupt("unaligned file extent"))
    );
}

/// A cached compressed extent of the wrong size is not used.
#[test]
fn a_stale_decompressed_extent_is_refused() {
    let (_, mut fs) = mounted();
    let h = create(&mut fs, "z", FileKind::Regular);
    fs.set_compression(h, Some(Compression::Zstd)).unwrap();
    fs.write(h, 0, &[4; 65536]).unwrap();
    fs.sync().unwrap();
    let e = fs.extents(split(h).1, 0, 1).unwrap()[0];
    assert!(e.z.is_some());
    fs.zcache = Some((e.pblk, vec![0; 10]));
    let mut buf = vec![0u8; 100];
    assert_eq!(
        fs.read(h, 0, &mut buf).err(),
        Some(Error::Corrupt("compressed extent"))
    );
}

#[test]
fn reading_nothing_reads_nothing() {
    let (_, mut fs) = mounted();
    let h = file(&mut fs, "f", &[1; 5000]);
    let ino = split(h).1;
    fs.read_blocks(ino, 0, &mut []).unwrap();
    fs.read_blocks_with(ino, true, None, 0, &mut []).unwrap();
}

/// Compressed data waiting for the commit, of an inode gone meanwhile.
#[test]
fn pending_compression_of_a_vanished_inode_is_dropped() {
    let (_, mut fs) = mounted();
    let h = create(&mut fs, "z", FileKind::Regular);
    fs.set_compression(h, Some(Compression::Zlib)).unwrap();
    fs.write(h, 0, &[4; 3000]).unwrap();
    let ino = split(h).1;
    assert!(!fs.zpending.is_empty());
    fs.delete(Key::new(ino, INODE_ITEM, 0)).unwrap();
    fs.compress_pending().unwrap();
}

#[test]
fn uninlining_a_file_without_inline_data_is_a_no_op() {
    let (_, mut fs) = mounted();
    let h = file(&mut fs, "f", &[1; 8192]);
    let ino = split(h).1;
    let mut i = fs.inode(ino).unwrap();
    fs.uninline(ino, &mut i).unwrap();
    assert_eq!(i, fs.inode(ino).unwrap());
}

#[test]
fn truncating_inline_data_past_the_inline_limit_moves_it_out() {
    let (_, mut fs) = mounted();
    let h = file(&mut fs, "f", b"short");
    fs.truncate(h, 3 * BS).unwrap();
    let ino = split(h).1;
    assert!(fs.inline_data(ino).unwrap().is_none());
    let mut buf = vec![0u8; 8];
    fs.read(h, 0, &mut buf).unwrap();
    assert_eq!(&buf, b"short\0\0\0");
}

/// On a full disk an inline file cannot grow out of its item.
#[test]
fn inline_data_cannot_move_out_on_a_full_disk() {
    let (_, mut fs) = mounted();
    let h = file(&mut fs, "f", b"short");
    fill(&mut fs);
    assert!(fs.truncate(h, 3 * BS).is_err());
    assert!(fs.write(h, 2 * BS, b"far").is_err());
    fs.check().unwrap();
}

#[test]
fn a_small_file_in_a_block_is_mapped_as_a_block() {
    let (_, mut fs) = mounted();
    let h = file(&mut fs, "f", &[1; 8192]);
    fs.truncate(h, 100).unwrap();
    let m = fs.fiemap(h, 0, 100).unwrap();
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].flags & FIEMAP_EXTENT_DATA_INLINE, 0);
}

// ---------------------------------------------------------------------------
// Directories
// ---------------------------------------------------------------------------

#[test]
fn entries_only_go_in_directories_at_free_indexes() {
    let (_, mut fs) = mounted();
    let f = split(file(&mut fs, "f", b"x")).1;
    let root = FIRST_INO;
    assert_eq!(
        fs.add_entry_at(f, b"n", f, FileKind::Regular, None).err(),
        Some(Error::NotDir)
    );
    let used = fs.items_of(root, DIR_INDEX, 1).unwrap()[0].0.offset;
    assert_eq!(
        fs.add_entry_at(root, b"n", f, FileKind::Regular, Some(used))
            .err(),
        Some(Error::Exists)
    );
    for bad in [0, 1, u64::MAX] {
        assert_eq!(
            fs.add_entry_at(root, b"n", f, FileKind::Regular, Some(bad))
                .err(),
            Some(Error::Invalid)
        );
    }
    assert_eq!(
        fs.add_entry_at(root, b"f", f, FileKind::Regular, None)
            .err(),
        Some(Error::Exists)
    );
}

/// Names whose hashes collide share one item, which may fill up.
#[test]
fn colliding_names_share_an_item() {
    let (_, mut fs) = mounted();
    let root = FIRST_INO;
    let f = split(file(&mut fs, "f", b"x")).1;
    let other = DirEnt {
        ino: f,
        index: 999,
        kind: FileKind::Regular,
        name: b"collides".to_vec(),
    };
    let hkey = Key::new(root, DIR_ITEM, name_hash(b"b"));
    fs.upsert(hkey, encode_dir_item(core::slice::from_ref(&other)))
        .unwrap();
    fs.add_entry_at(root, b"b", f, FileKind::Regular, None)
        .unwrap();
    assert_eq!(
        decode_dir_item(&fs.get(hkey).unwrap().unwrap())
            .unwrap()
            .len(),
        2
    );
    // Taking one name out leaves the other in the item.
    fs.remove_entry(root, b"b").unwrap();
    assert_eq!(
        decode_dir_item(&fs.get(hkey).unwrap().unwrap())
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        fs.remove_entry(root, b"nothing").err(),
        Some(Error::NotFound)
    );

    // An item with no room left for another name.
    let ent = |i: u64, len: usize| DirEnt {
        ino: f,
        index: 1000 + i,
        kind: FileKind::Regular,
        name: alloc::vec![b'a' + (i % 26) as u8; len],
    };
    let per = encode_dir_item(&[ent(0, 100)]).len();
    let n = (MAX_ITEM / per - 1) as u64;
    let mut big: Vec<DirEnt> = (0..n).map(|i| ent(i, 100)).collect();
    let used = encode_dir_item(&big).len();
    big.push(ent(n, MAX_ITEM - used - (per - 100) - 5));
    assert!(encode_dir_item(&big).len() <= MAX_ITEM);
    let hkey = Key::new(root, DIR_ITEM, name_hash(b"c"));
    fs.upsert(hkey, encode_dir_item(&big)).unwrap();
    assert_eq!(
        fs.add_entry_at(root, b"c", f, FileKind::Regular, None)
            .err(),
        Some(Error::NoSpace)
    );
}

/// A failed entry takes the new inode back out.
#[test]
fn a_create_whose_entry_fails_leaves_nothing() {
    let (_, mut fs) = mounted();
    let root = FIRST_INO;
    let next = fs.inode(root).unwrap().dir_next;
    fs.insert(
        Key::new(root, DIR_INDEX, next),
        encode_pair(FIRST_INO, 0, FileKind::Dir, b"squatter"),
    )
    .unwrap();
    assert_eq!(
        fs.create(fs.root_ino(), "new", FileKind::Regular, 0o644, 0)
            .err(),
        Some(Error::Exists)
    );
    assert_eq!(fs.lookup(fs.root_ino(), "new").err(), Some(Error::NotFound));

    // The back reference already there.
    let (_, mut fs) = mounted();
    let next = fs.inode(root).unwrap().dir_next;
    let ino = fs.fs_item.next_ino;
    fs.insert(
        Key::new(ino, INODE_REF, ref_hash(root, next)),
        encode_pair(root, next, FileKind::Regular, b"new"),
    )
    .unwrap();
    assert_eq!(
        fs.create(fs.root_ino(), "new", FileKind::Regular, 0o644, 0)
            .err(),
        Some(Error::Exists)
    );
}

/// Dropping one name of a file named in two directories.
#[test]
fn unlinking_names_of_a_hard_link() {
    let (_, mut fs) = mounted();
    let root = fs.root_ino();
    let d = fs.create(root, "d", FileKind::Dir, 0o755, 0).unwrap();
    let h = file(&mut fs, "f", b"x");
    fs.link(d, "g", h).unwrap();
    // Not its parent: the parent stays.
    fs.unlink(d, "g").unwrap();
    assert_eq!(fs.inode(split(h).1).unwrap().parent, FIRST_INO);

    // A link count higher than the names there are.
    let mut i = fs.inode(split(h).1).unwrap();
    i.nlink = 2;
    fs.put_inode(split(h).1, i).unwrap();
    fs.unlink(root, "f").unwrap();
}

#[test]
fn lookups_in_a_filesystem_with_encryption_check_names() {
    let (_, mut fs) = mounted();
    fs.sb.features |= FEAT_ENCRYPT;
    assert_eq!(fs.lookup(fs.root_ino(), "a/b").err(), Some(Error::NotFound));
}

#[test]
fn a_symlink_too_long_to_be_one_is_corrupt() {
    let (_, mut fs) = mounted();
    let h = fs.symlink(fs.root_ino(), "l", b"target").unwrap();
    let ino = split(h).1;
    let mut i = fs.inode(ino).unwrap();
    i.size = MAX_SYMLINK_LEN as u64 + 1;
    fs.put_inode(ino, i).unwrap();
    assert_eq!(fs.read_link(h).err(), Some(Error::Corrupt("symlink size")));
}

#[test]
fn symlinks_on_a_full_disk() {
    let (_, mut fs) = mounted();
    let h = fs.symlink(fs.root_ino(), "l", b"t").unwrap();
    fill(&mut fs);
    let long = vec![b'x'; 3000];
    assert!(fs.symlink(fs.root_ino(), "m", &long).is_err());
    assert!(fs.write_symlink(h, 0, &long).is_err());
}

#[test]
fn parents_of_roots_and_orphans() {
    let (_, mut fs) = mounted();
    assert_eq!(fs.parent_l(FIRST_INO).unwrap(), FIRST_INO);
    let h = file(&mut fs, "f", b"x");
    let ino = split(h).1;
    let mut i = fs.inode(ino).unwrap();
    i.parent = 0;
    fs.put_inode(ino, i).unwrap();
    assert_eq!(fs.parent_l(ino).err(), Some(Error::NotFound));
}

#[test]
fn renaming_into_a_file_is_not_a_directory() {
    let (_, mut fs) = mounted();
    file(&mut fs, "a", b"x");
    let b = file(&mut fs, "b", b"y");
    assert_eq!(
        fs.rename(fs.root_ino(), "a", b, "c").err(),
        Some(Error::NotDir)
    );
}

#[test]
fn xattrs_whose_names_collide_share_an_item() {
    let (_, mut fs) = mounted();
    let h = file(&mut fs, "f", b"x");
    let ino = split(h).1;
    let key = Key::new(ino, XATTR_ITEM, name_hash(b"user.a"));
    fs.upsert(
        key,
        encode_xattrs(&[
            (b"user.a".to_vec(), b"1".to_vec()),
            (b"user.zz".to_vec(), b"2".to_vec()),
        ]),
    )
    .unwrap();
    fs.remove_xattr(h, "user.a").unwrap();
    assert_eq!(
        decode_xattrs(&fs.get(key).unwrap().unwrap()).unwrap().len(),
        1
    );
}

// ---------------------------------------------------------------------------
// Subvolumes
// ---------------------------------------------------------------------------

#[test]
fn only_real_subvolumes_can_be_entered() {
    let (_, mut fs) = mounted();
    assert_eq!(fs.stat(handle(3, FIRST_INO)).err(), Some(Error::NotFound));
}

#[test]
fn a_subvolume_named_twice_is_corrupt() {
    let (_, mut fs) = mounted();
    fs.create_subvolume(fs.root_ino(), "sv").unwrap();
    fs.root_insert(
        Key::new(FIRST_SUBVOL, ROOT_BACKREF, 77),
        encode_pair(FIRST_INO, 9, FileKind::Dir, b"again"),
    )
    .unwrap();
    assert_eq!(
        fs.parent(handle(FIRST_SUBVOL, FIRST_INO)).err(),
        Some(Error::Corrupt("subvolume named twice"))
    );
}

#[test]
fn subvolume_ids_run_out() {
    let (_, mut fs) = mounted();
    let item = fs.fs_item.clone();
    fs.root_insert(Key::new(MAX_SUBVOL, ROOT_ITEM, 0), item.encode())
        .unwrap();
    assert_eq!(
        fs.create_subvolume(fs.root_ino(), "sv").err(),
        Some(Error::NoSpace)
    );
}

#[test]
fn snapshots_are_of_subvolume_roots() {
    let (_, mut fs) = mounted();
    let h = file(&mut fs, "f", b"x");
    assert_eq!(
        fs.snapshot(h, fs.root_ino(), "s", false).err(),
        Some(Error::Invalid)
    );
}

/// A receive in progress: not listed, not limited, not shown, no parent.
#[test]
fn a_subvolume_being_received_is_invisible() {
    let (_, mut fs) = mounted();
    fs.quota_enable().unwrap();
    let root = fs.st.create(&mut fs.sp, FIRST_SUBVOL, 0).unwrap();
    let item = RootItem {
        root,
        next_ino: FIRST_INO,
        root_dir: FIRST_INO,
        flags: ROOT_FLAG_RECEIVING,
        ..Default::default()
    };
    fs.root_insert(Key::new(FIRST_SUBVOL, ROOT_ITEM, 0), item.encode())
        .unwrap();
    assert_eq!(fs.list_subvolumes().unwrap().len(), 1);
    assert_eq!(fs.qgroup_show().unwrap().len(), 1);
    assert_eq!(
        fs.set_qgroup_limit(FIRST_SUBVOL, Some(1 << 20)).err(),
        Some(Error::NotFound)
    );
    assert_eq!(
        fs.parent(handle(FIRST_SUBVOL, FIRST_INO)).err(),
        Some(Error::NotFound)
    );
}

/// With another subvolume mounted as `/`, the top one has no parent but
/// itself.
#[test]
fn the_top_subvolume_is_its_own_parent() {
    let (d, mut fs) = mounted();
    let sv = fs.create_subvolume(fs.root_ino(), "sv").unwrap();
    fs.set_default_subvolume(split(sv).0).unwrap();
    drop(fs);
    let mut fs = remount(&d).unwrap();
    assert_eq!(fs.root_ino(), sv);
    let top = handle(FS_TREE, FIRST_INO);
    assert_eq!(fs.parent(top).unwrap(), top);
}

#[test]
fn a_read_only_mount_changes_no_subvolume() {
    let d = formatted(16);
    let mut fs = CowFs::mount(d, true).unwrap();
    assert!(matches!(
        fs.set_subvolume_read_only(FS_TREE, true),
        Err(Error::Unsupported(_))
    ));
}

// ---------------------------------------------------------------------------
// Clone and dedupe
// ---------------------------------------------------------------------------

#[test]
fn clone_takes_regular_files_only() {
    let (_, mut fs) = mounted();
    let root = fs.root_ino();
    let f = file(&mut fs, "f", &[1; 8192]);
    let l = fs.symlink(root, "l", b"t").unwrap();
    assert_eq!(fs.clone_range(root, 0, f, 0, 0).err(), Some(Error::IsDir));
    assert_eq!(fs.clone_range(l, 0, f, 0, 0).err(), Some(Error::Invalid));
    assert_eq!(fs.clone_range(f, 0, l, 0, 0).err(), Some(Error::Invalid));
    // Nothing from the end on.
    assert_eq!(fs.clone_range(f, 8192, f, 0, 0).unwrap(), 0);
    assert_eq!(fs.dedupe_range(f, 0, f, 0, 0).unwrap(), 0);
}

/// Blocks cloned over a file still inline move its data out first.
#[test]
fn cloning_into_an_inline_file() {
    let (_, mut fs) = mounted();
    let src = file(&mut fs, "src", &[1; 8192]);
    let dst = file(&mut fs, "dst", b"inline");
    fs.clone_range(src, 0, dst, BS, BS).unwrap();
    let mut buf = vec![0u8; 6];
    fs.read(dst, 0, &mut buf).unwrap();
    assert_eq!(&buf, b"inline");
    fs.check().unwrap();
}

#[test]
fn cloning_inline_data_onto_a_full_disk() {
    let (_, mut fs) = mounted();
    let src = file(&mut fs, "src", b"inline");
    let dst = file(&mut fs, "dst", &[1; 100]);
    fill(&mut fs);
    assert!(fs.clone_range(src, 0, dst, 4 * BS, 0).is_err());
}

// ---------------------------------------------------------------------------
// Defragmentation and scrub
// ---------------------------------------------------------------------------

/// No run long enough for a window: the data stays where it is.
#[test]
fn defrag_on_a_full_disk_leaves_data_in_place() {
    let (_, mut fs) = mounted();
    let h = create(&mut fs, "f", FileKind::Regular);
    for i in 0..8u64 {
        fs.write(h, i * 2 * BS, &[i as u8; BS as usize]).unwrap();
        fs.sync().unwrap();
        fs.write(h, (i * 2 + 1) * BS, &[i as u8; BS as usize])
            .unwrap();
    }
    fill(&mut fs);
    assert_eq!(fs.defrag(h, 0, 16 * BS, 0, None).unwrap(), 0);
}

/// A tree node bad in one copy of a volume is read from the other and
/// rewritten.
#[test]
fn scrub_repairs_a_bad_copy_of_a_tree_node() {
    let devs = volume();
    let mut fs = CowFs::mount_devices(devs.clone(), false, false).unwrap();
    fs.set_clock(clock);
    file(&mut fs, "f", &[3; 20000]);
    fs.sync().unwrap();
    let b = fs.fs_root.bytenr;
    fs.vol
        .clone()
        .unwrap()
        .write_copy(b * BS, &[0xff; BS as usize], 0)
        .unwrap();
    fs.drop_caches();
    let r = fs.scrub().unwrap();
    assert!(r.repaired > 0, "{:?}", r);
}

#[test]
fn a_shared_range_over_a_bad_node_fails() {
    let (d, mut fs) = mounted();
    file(&mut fs, "f", b"x");
    fs.sync().unwrap();
    d.write_at(fs.fs_root.bytenr * BS, &[0xff; BS as usize])
        .unwrap();
    fs.drop_caches();
    let first = Key::new(0, 0, 0);
    assert!(fs
        .st
        .range_shared(&fs.fs_root, &first, &Key::MAX, 10)
        .is_err());
}

/// A device whose sync fails once `fail` is set.
struct SyncFails {
    ram: Arc<dyn BlockDevice>,
    fail: core::sync::atomic::AtomicBool,
}

impl BlockDevice for SyncFails {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.ram.read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        self.ram.write_at(offset, buf)
    }
    fn sync(&self) -> Result<()> {
        match self.fail.load(core::sync::atomic::Ordering::Relaxed) {
            true => Err(Error::Io),
            false => Ok(()),
        }
    }
    fn size(&self) -> u64 {
        self.ram.size()
    }
}

/// A scrub that repairs a copy but cannot sync the repair fails.
#[test]
fn scrub_repairs_that_cannot_be_synced() {
    for node in [true, false] {
        let wrapped: Vec<Arc<SyncFails>> = volume()
            .into_iter()
            .map(|ram| {
                Arc::new(SyncFails {
                    ram,
                    fail: false.into(),
                })
            })
            .collect();
        let devs = wrapped
            .iter()
            .map(|d| d.clone() as Arc<dyn BlockDevice>)
            .collect();
        let mut fs = CowFs::mount_devices(devs, false, false).unwrap();
        let h = file(&mut fs, "f", &[3; 20000]);
        fs.sync().unwrap();
        let b = match node {
            true => fs.fs_root.bytenr,
            false => fs.fiemap(h, 0, 20000).unwrap()[0].physical / BS,
        };
        fs.vol
            .clone()
            .unwrap()
            .write_copy(b * BS, &[0xff; BS as usize], 0)
            .unwrap();
        fs.drop_caches();
        for d in &wrapped {
            d.fail.store(true, core::sync::atomic::Ordering::Relaxed);
        }
        assert_eq!(fs.scrub().err(), Some(Error::Io), "node: {node}");
    }
}

#[test]
fn the_formatting_stand_in_device_is_never_read() {
    let d = NullDev(5);
    assert_eq!(d.size(), 5);
    assert_eq!(d.read_at(0, &mut [0; 4]).err(), Some(Error::Io));
    assert_eq!(d.write_at(0, &[0; 4]).err(), Some(Error::Io));
    d.sync().unwrap();
}

#[test]
fn mount_compression_is_kept() {
    let (_, mut fs) = mounted();
    assert_eq!(fs.mount_compression(), None);
    fs.set_mount_compression(Some(Compression::Lz4));
    assert_eq!(fs.mount_compression(), Some(Compression::Lz4));
}

#[test]
fn compsize_counts_inline_data() {
    let (_, mut fs) = mounted();
    let h = file(&mut fs, "f", &[1; 300]);
    let c = fs.compsize(h).unwrap();
    assert_eq!((c.disk_bytes, c.uncompressed_bytes), (300, 300));
}

/// A superblock in the journal for a bigger filesystem than the device
/// holds (a grow whose superblock never reached its home).
#[test]
fn a_journal_cannot_make_the_filesystem_larger_than_its_device() {
    let d = formatted(16);
    let big = Ram::new(32 * MIB);
    big.write_at(0, &d.peek(0, 16 * MIB)).unwrap();
    let home = big.peek(0, BS as usize);
    let mut fs = CowFs::mount(big.clone(), false).unwrap();
    assert!(fs.grow_to_device().unwrap());
    drop(fs);
    let small = Ram::new(16 * MIB);
    small.write_at(0, &big.peek(0, 16 * MIB)).unwrap();
    small.write_at(0, &home).unwrap();
    assert_eq!(
        CowFs::mount(small, false).err(),
        Some(Error::Corrupt("filesystem larger than device"))
    );
}

// ---------------------------------------------------------------------------
// Tree nodes from mirrors
// ---------------------------------------------------------------------------

/// A node bad in one of three copies is read from the next good one and,
/// on a writable mount, rewritten.
#[test]
fn a_node_is_read_around_a_bad_copy() {
    let devs: Vec<Arc<dyn BlockDevice>> = (0..3)
        .map(|_| Ram::new(64 * MIB) as Arc<dyn BlockDevice>)
        .collect();
    mkfs::format_devices(&devs, &opts(), Profile::Raid1c3, Profile::Raid1c3).unwrap();
    for read_only in [false, true] {
        let mut fs = CowFs::mount_devices(devs.clone(), read_only, false).unwrap();
        let b = fs.fs_root.bytenr;
        let v = fs.vol.clone().unwrap();
        v.write_copy(b * BS, &[0xff; BS as usize], 0).unwrap();
        fs.drop_caches();
        fs.stat(fs.root_ino()).unwrap();
        let mut img = vec![0u8; BS as usize];
        v.read_copy(b * BS, &mut img, 0).unwrap();
        assert_eq!(img[0] == 0xff, read_only, "repaired: {}", !read_only);
    }
}

// ---------------------------------------------------------------------------
// Shared reads
// ---------------------------------------------------------------------------

#[test]
fn shared_reads_of_a_subvolume_not_loaded_take_the_exclusive_path() {
    let (_, fs) = mounted();
    let h = handle(FIRST_SUBVOL + 9, FIRST_INO);
    assert!(fs.stat_shared(h).unwrap().is_none());
    assert_eq!(fs.read_shared(h, 0, &mut [0; 8]).unwrap(), None);
}

#[test]
fn shared_reads_of_small_files_in_blocks_and_of_preallocated_space() {
    let (_, mut fs) = mounted();
    let h = file(&mut fs, "small", &[7; 8192]);
    fs.truncate(h, 100).unwrap();
    let mut buf = [0u8; 100];
    assert_eq!(fs.read_shared(h, 0, &mut buf).unwrap(), Some(100));
    assert_eq!(buf, [7; 100]);

    let p = create(&mut fs, "pre", FileKind::Regular);
    fs.fallocate(p, 0, 0, 3 * BS).unwrap();
    fs.write(p, 3 * BS, &[1; 10]).unwrap();
    let mut buf = vec![9u8; (3 * BS + 10) as usize];
    assert_eq!(fs.read_shared(p, 0, &mut buf).unwrap(), Some(buf.len()));
    assert!(buf[..(3 * BS) as usize].iter().all(|&b| b == 0));

    // Small and sparse: no item at all.
    let z = create(&mut fs, "sparse", FileKind::Regular);
    fs.truncate(z, 100).unwrap();
    let mut buf = [9u8; 100];
    assert_eq!(fs.read_shared(z, 0, &mut buf).unwrap(), Some(100));
    assert_eq!(buf, [0; 100]);
}

/// Damage the shared reader meets: an inline item past the inline size,
/// an extent off the block grid, a block that fails its checksum, one
/// freed by the open transaction.
#[test]
fn shared_reads_of_damaged_files() {
    let (d, mut fs) = mounted();
    let h = file(&mut fs, "f", &[7; 3 * BS as usize]);
    let ino = split(h).1;
    fs.sync().unwrap();
    let e = fs.extents(ino, 0, 1).unwrap()[0];

    // A block freed in this transaction has no checksum to read it by.
    fs.sp.csums_freed.insert(e.pblk, 1);
    assert_eq!(fs.read_shared(h, 0, &mut [0; 8]).unwrap(), None);
    fs.sp.csums_freed = RangeSet::default();

    // Bad data, read whole and in part.
    d.poke((e.pblk * BS + 5) as usize, 1);
    assert_eq!(fs.read_shared(h, 0, &mut [0; 8]).unwrap(), None);
    assert_eq!(fs.read_shared(h, 100, &mut [0; 8]).unwrap(), None);
    let mut whole = vec![0u8; BS as usize];
    assert_eq!(fs.read_shared(h, 0, &mut whole).unwrap(), None);

    let (k, v) = fs.items_of(ino, EXTENT_DATA, 1).unwrap().remove(0);
    fs.delete(k).unwrap();
    fs.insert(Key::new(ino, EXTENT_DATA, 7), v.clone()).unwrap();
    assert_eq!(
        fs.read_shared(h, 0, &mut [0; 8]).err(),
        Some(Error::Corrupt("unaligned file extent"))
    );
    fs.delete(Key::new(ino, EXTENT_DATA, 7)).unwrap();
    fs.insert(k, FileExtent::Inline(vec![1; 10]).encode())
        .unwrap();
    let mut buf = [5u8; 8];
    assert_eq!(fs.read_shared(h, 0, &mut buf).unwrap(), Some(8));
    assert_eq!(buf, [0; 8]);
}

// ---------------------------------------------------------------------------
// Encryption
// ---------------------------------------------------------------------------

fn random(buf: &mut [u8]) {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(37).wrapping_add(11);
    }
}

/// A filesystem with an encrypted directory "vault", its handle and key.
fn vault() -> (CowFs, u64, [u8; 16]) {
    let (_, mut fs) = mounted();
    fs.set_random(random);
    let id = fs.add_encryption_key(&[0x42; 64]).unwrap();
    let dir = fs
        .create(fs.root_ino(), "vault", FileKind::Dir, 0o700, 0)
        .unwrap();
    fs.set_encryption_policy(dir, &EncryptionPolicy::new(id))
        .unwrap();
    (fs, dir, id)
}

#[test]
fn shared_reads_of_encrypted_blocks() {
    let (mut fs, dir, _) = vault();
    let h = fs.create(dir, "f", FileKind::Regular, 0o600, 0).unwrap();
    let data: Vec<u8> = (0..2 * BS).map(|i| i as u8).collect();
    fs.write(h, 0, &data).unwrap();
    let mut buf = vec![0u8; data.len()];
    assert_eq!(fs.read_shared(h, 0, &mut buf).unwrap(), Some(data.len()));
    assert_eq!(buf, data);
}

#[test]
fn a_subvolume_cannot_move_into_an_encrypted_directory() {
    let (mut fs, dir, _) = vault();
    fs.create_subvolume(fs.root_ino(), "sv").unwrap();
    assert!(fs.rename(fs.root_ino(), "sv", dir, "sv").is_err());
}

#[test]
fn no_key_names_that_cannot_be_there() {
    let (mut fs, dir, id) = vault();
    fs.create(dir, "a", FileKind::Regular, 0o600, 0).unwrap();
    fs.remove_encryption_key(&id).unwrap();
    // The longest name that decodes to more than a whole one.
    let too_long = crate::crypt::base64url(&[1; 190]);
    let whole = crate::crypt::base64url(&[1; 189]);
    for name in [too_long, whole] {
        let name = core::str::from_utf8(&name).unwrap();
        assert_eq!(fs.lookup(dir, name).err(), Some(Error::NotFound));
    }
}

#[test]
fn an_encrypted_directory_without_its_context_does_not_list() {
    let (mut fs, dir, _) = vault();
    let ino = split(dir).1;
    fs.delete(Key::new(ino, ENC_CONTEXT, 0)).unwrap();
    fs.keyring.files.clear();
    assert!(matches!(fs.readdir(dir), Err(Error::Corrupt(_))));
}

#[test]
fn fsck_of_encryption() {
    let (mut fs, dir, _) = vault();
    let f = fs
        .create(dir, "secret-file-name", FileKind::Regular, 0o600, 0)
        .unwrap();
    let (d, f) = (split(dir).1, split(f).1);
    let ctx = fs.get(Key::new(d, ENC_CONTEXT, 0)).unwrap().unwrap();
    let mut inodes = BTreeMap::new();
    inodes.insert(d, fs.inode(d).unwrap());
    inodes.insert(f, fs.inode(f).unwrap());
    let mut contexts = BTreeMap::new();
    contexts.insert(d, ctx.clone());
    contexts.insert(f, ctx.clone());
    let none = BTreeSet::new();
    encrypt::check(&inodes, &contexts, &none, &BTreeSet::new()).unwrap();

    let mut missing = contexts.clone();
    missing.remove(&f);
    let what = |r: Result<()>| match r {
        Err(Error::Corrupt(w)) => w,
        r => panic!("{:?}", r),
    };
    assert_eq!(
        what(encrypt::check(&inodes, &missing, &none, &BTreeSet::new())),
        "fsck: encryption flag and context differ"
    );
    let packed: BTreeSet<u64> = [f].into_iter().collect();
    assert_eq!(
        what(encrypt::check(
            &inodes,
            &contexts,
            &packed,
            &BTreeSet::new()
        )),
        "fsck: inline or compressed encrypted data"
    );
    let mut extra = contexts.clone();
    extra.insert(FIRST_INO + 999, ctx);
    assert_eq!(
        what(encrypt::check(&inodes, &extra, &none, &BTreeSet::new())),
        "fsck: encryption context of a missing inode"
    );
    let short: BTreeSet<(u64, u64, Vec<u8>, u64)> =
        [(d, 2, b"short".to_vec(), f)].into_iter().collect();
    assert_eq!(
        what(encrypt::check(&inodes, &contexts, &none, &short)),
        "fsck: entry of an encrypted directory"
    );
}

// ---------------------------------------------------------------------------
// Bitmap checksums
// ---------------------------------------------------------------------------

/// A bitmap bad on every copy is rebuilt from the trees.
#[test]
fn bitmaps_bad_everywhere_are_rebuilt_from_the_trees() {
    let devs = volume();
    let mut fs = CowFs::mount_devices(devs.clone(), false, false).unwrap();
    fs.set_clock(clock);
    file(&mut fs, "f", &[1; 3 * BS as usize]);
    fs.sync().unwrap();
    let v = fs.vol.clone().unwrap();
    let js = fs.journal.start;
    drop(fs);
    // The journal would hand the last bitmaps back.
    for k in 0..2 {
        v.write_copy(js * BS, &[0; BS as usize], k).unwrap();
        v.write_copy(BS, &[0x0f; BS as usize], k).unwrap();
    }
    let mut fs = CowFs::mount_devices(devs, false, false).unwrap();
    fs.check().unwrap();
}

/// Internal nodes, nodes a snapshot shares and the fsync log area all
/// hold blocks of a rebuilt bitmap.
#[test]
fn a_rebuilt_bitmap_counts_every_kind_of_block() {
    let (d, mut fs) = mounted();
    for i in 0..300 {
        file(&mut fs, &alloc::format!("f{}", i), &[i as u8; 600]);
    }
    fs.snapshot(fs.root_ino(), fs.root_ino(), "snap", true)
        .unwrap();
    let h = file(&mut fs, "logged", &[1; 100]);
    fs.fsync(h).unwrap();
    assert!(fs.log_area().unwrap().is_some());
    let js = fs.journal.start;
    drop(fs);
    d.write_at(js * BS, &[0; BS as usize]).unwrap();
    d.write_at(BS, &[0x0f; BS as usize]).unwrap();
    let mut fs = remount(&d).unwrap();
    fs.check().unwrap();
}

#[test]
fn the_checksum_table_is_resized_or_skipped() {
    // Moved when its size changes.
    let (_, mut fs) = mounted();
    let at = fs.sp.alloc.allocate_run(None, 2).unwrap();
    fs.sb.bitmap_sums = Some((at, 2));
    fs.place_bitmap_sums().unwrap();
    assert_eq!(fs.sb.bitmap_sums.map(|s| s.1), Some(1));

    // No room in group 0 for it: the filesystem goes on without one.
    let (_, mut fs) = mounted();
    fs.sb.bitmap_sums = None;
    while fs.sp.alloc.groups_mut()[0].allocate_run(2, 1).is_some() {}
    fs.place_bitmap_sums().unwrap();
    assert_eq!(fs.sb.bitmap_sums, None);
    fs.write_bitmap_sums().unwrap();
}

// ---------------------------------------------------------------------------
// Space on volumes
// ---------------------------------------------------------------------------

#[test]
fn a_single_device_has_no_chunks_to_grow() {
    let (_, mut fs) = mounted();
    assert!(!fs.sp.grow_on(ChunkKind::Data, None));
}

/// Data room and runs past what the chunks hold make new data chunks;
/// nodes past the metadata ones make metadata chunks, until the devices
/// are full.
#[test]
fn space_on_a_volume_grows_by_chunks() {
    let devs = volume();
    let mut fs = CowFs::mount_devices(devs, false, false).unwrap();
    let room = fs.sp.data_room();
    assert!(fs.sp.ensure_data_room(room + 100));
    while fs.sp.data_room() > 50 {
        fs.sp
            .alloc
            .allocate_for(None, 50, Some(ChunkKind::Data))
            .unwrap();
    }
    assert!(fs.sp.alloc_data_run(None, 200).is_some());

    while fs
        .sp
        .alloc
        .allocate_for(None, u64::MAX, Some(ChunkKind::Metadata))
        .is_some()
    {}
    crate::btree::Space::alloc_node(&mut fs.sp, 0).unwrap();
    while fs.sp.grow(ChunkKind::Data) {}
    while fs
        .sp
        .alloc
        .allocate_for(None, u64::MAX, Some(ChunkKind::Metadata))
        .is_some()
    {}
    assert_eq!(
        crate::btree::Space::alloc_node(&mut fs.sp, 0).err(),
        Some(Error::NoSpace)
    );

    // Data cannot grow either: metadata needs its reserve first.
    assert!(!fs.sp.grow(ChunkKind::Data));
}

/// On a tiered volume nodes go to the fast devices, which grow a chunk
/// when theirs are full.
#[test]
fn nodes_grow_the_fast_tier() {
    let devs: Vec<Arc<dyn BlockDevice>> = (0..2)
        .map(|_| Ram::new(64 * MIB) as Arc<dyn BlockDevice>)
        .collect();
    mkfs::format_devices_tiered(
        &devs,
        &opts(),
        Profile::Single,
        Profile::Single,
        &[Tier::Fast, Tier::Slow],
    )
    .unwrap();
    let mut fs = CowFs::mount_devices(devs, false, false).unwrap();
    while fs
        .sp
        .alloc
        .allocate_tier(None, u64::MAX, Some(ChunkKind::Metadata), Some(Tier::Fast))
        .is_some()
    {}
    let b = crate::btree::Space::alloc_node(&mut fs.sp, 0).unwrap();
    assert_eq!(
        fs.sp.alloc.groups()[(b / GROUP_BLOCKS) as usize].tier,
        Tier::Fast
    );
}

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

fn devices(n: usize) -> Vec<Arc<dyn BlockDevice>> {
    (0..n)
        .map(|_| Ram::new(64 * MIB) as Arc<dyn BlockDevice>)
        .collect()
}

#[test]
fn device_operations_that_do_nothing_or_are_refused() {
    let (_, mut fs) = mounted();
    assert_eq!(fs.tier_migrate(true).unwrap(), BalanceReport::default());

    let mut fs = CowFs::mount_devices(volume(), false, false).unwrap();
    fs.drop_chunk(999).unwrap();
    assert_eq!(
        fs.add_device(Ram::new(MIB) as Arc<dyn BlockDevice>),
        Err(Error::NoSpace)
    );
    assert_eq!(
        fs.replace_device(99, Ram::new(64 * MIB)),
        Err(Error::NotFound)
    );
    // A device holding another filesystem is taken.
    let other = formatted(64);
    edit_super(&other, |sb| sb.uuid[0] ^= 1);
    assert_eq!(fs.add_device(other).unwrap(), 3);
    fs.check().unwrap();
}

#[test]
fn a_device_larger_than_its_map_grows_the_volume() {
    let mut fs = CowFs::mount_devices(volume(), false, false).unwrap();
    let vol = fs.vol.clone().unwrap();
    let size = vol.map().devices[&1];
    vol.with_map(|m| m.devices.insert(1, size - 1000));
    assert!(fs.grow_volume(&vol).unwrap());
    assert_eq!(vol.map().devices[&1], size);
    assert!(!fs.grow_volume(&vol).unwrap());
}

/// Chunks of groups the allocator does not have yet are skipped.
#[test]
fn layouts_skip_chunks_past_the_groups() {
    let mut fs = CowFs::mount_devices(volume(), false, false).unwrap();
    let vol = fs.vol.clone().unwrap();
    let c = vol.map().chunks[&0].clone();
    vol.with_map(|m| m.chunks.insert(999, c));
    fs.refresh_layouts();
    vol.with_map(|m| m.chunks.remove(&999));
    fs.refresh_layouts();
    fs.check().unwrap();
}

/// A missing device whose chunks cannot be read again cannot be removed.
#[test]
fn removing_a_missing_device_without_copies_fails() {
    let devs = devices(3);
    mkfs::format_devices(&devs, &opts(), Profile::Single, Profile::Raid1).unwrap();
    let mut fs = CowFs::mount_devices(devs, false, false).unwrap();
    fs.set_clock(clock);
    for i in 0..6 {
        file(&mut fs, &alloc::format!("f{}", i), &vec![i as u8; 4 << 20]);
    }
    fs.sync().unwrap();
    let vol = fs.vol.clone().unwrap();
    let on3 = vol
        .map()
        .chunks
        .values()
        .any(|c| c.stripes.iter().any(|s| s.devid == 3));
    assert!(on3);
    vol.detach(3);
    assert!(fs.remove_device(3).is_err());
    assert!(fs.sp.avoid.contains(&3));
}

/// Metadata on the slow tier moves to the fast one, if it has room.
#[test]
fn metadata_migrates_to_the_fast_tier_when_it_can() {
    let devs = devices(2);
    mkfs::format_devices_tiered(
        &devs,
        &opts(),
        Profile::Single,
        Profile::Single,
        &[Tier::Fast, Tier::Slow],
    )
    .unwrap();
    let mut fs = CowFs::mount_devices(devs, false, false).unwrap();
    fs.set_clock(clock);
    assert!(fs.sp.grow_on(ChunkKind::Metadata, Some(Tier::Slow)));
    fs.sync().unwrap();
    let on_slow = |fs: &CowFs| {
        let map = fs.vol.as_ref().unwrap().map();
        map.chunks
            .values()
            .filter(|c| c.kind == ChunkKind::Metadata && map.chunk_tier(c) == Tier::Slow)
            .count()
    };
    assert_eq!(on_slow(&fs), 1);
    fs.tier_migrate(false).unwrap();
    assert_eq!(on_slow(&fs), 0);
    // No room on the fast device: it stays.
    while fs.sp.grow_on(ChunkKind::Data, Some(Tier::Fast)) {}
    assert!(fs.sp.grow_on(ChunkKind::Metadata, Some(Tier::Slow)));
    fs.sync().unwrap();
    fs.tier_migrate(false).unwrap();
    assert_eq!(on_slow(&fs), 1);
    fs.check().unwrap();
}

/// The smallest device that can be added has room for the blocks it
/// reserves plus one smallest chunk, and that exact size is taken: one
/// block less is refused.
#[test]
fn a_device_of_exactly_the_minimum_size_is_taken() {
    let mut fs = CowFs::mount_devices(volume(), false, false).unwrap();
    fs.set_clock(clock);
    let min = (DEV_RESERVED + MIN_CHUNK) * BS;
    assert_eq!(
        fs.add_device(Ram::new((min - BS) as usize) as Arc<dyn BlockDevice>),
        Err(Error::NoSpace)
    );
    assert_eq!(
        fs.add_device(Ram::new(min as usize) as Arc<dyn BlockDevice>)
            .unwrap(),
        3
    );
    assert_eq!(
        fs.vol.as_ref().unwrap().map().devices[&3],
        DEV_RESERVED + MIN_CHUNK
    );
    fs.check().unwrap();
}

/// A device that reads back SMALLER than its map is not followed: the
/// volume only ever grows, because shrinking has to move the chunks at the
/// end first (`resize`), and a map that shrank on its own would leave them
/// past the device's end.
#[test]
fn a_device_smaller_than_its_map_does_not_shrink_the_volume() {
    let mut fs = CowFs::mount_devices(volume(), false, false).unwrap();
    let vol = fs.vol.clone().unwrap();
    let size = vol.map().devices[&1];
    vol.with_map(|m| m.devices.insert(1, size + 1000));
    assert!(!fs.grow_volume(&vol).unwrap());
    assert_eq!(vol.map().devices[&1], size + 1000);
}

/// A data chunk goes down to the slow tier once less than an eighth of it
/// is free. An eighth exactly is not less than an eighth, so that chunk
/// stays, and so does one that is three quarters full.
#[test]
fn a_data_chunk_moves_down_once_less_than_an_eighth_of_it_is_free() {
    let devs = devices(2);
    mkfs::format_devices_tiered(
        &devs,
        &opts(),
        Profile::Single,
        Profile::Single,
        &[Tier::Fast, Tier::Slow],
    )
    .unwrap();
    let mut fs = CowFs::mount_devices(devs, false, false).unwrap();
    fs.set_clock(clock);
    assert!(fs.sp.grow_on(ChunkKind::Data, Some(Tier::Fast)));
    fs.sync().unwrap();
    let on_fast = |fs: &CowFs| -> Vec<u64> {
        let map = fs.vol.as_ref().unwrap().map();
        map.chunks
            .iter()
            .filter(|(_, c)| c.kind == ChunkKind::Data && map.chunk_tier(c) == Tier::Fast)
            .map(|(&g, _)| g)
            .collect()
    };
    let fast = on_fast(&fs);
    assert_eq!(fast.len(), 1, "{fast:?}");
    let g = fast[0] as usize;
    let count = fs.sp.alloc.groups()[g].block_count;
    // Three quarters full: nowhere near, whichever fraction is read.
    fs.sp.alloc.groups_mut()[g].free_blocks = count / 4;
    assert_eq!(fs.tier_migrate(false).unwrap().relocated, 0);
    assert_eq!(on_fast(&fs).len(), 1);
    // An eighth free is not LESS than an eighth, so it stays too.
    fs.sp.alloc.groups_mut()[g].free_blocks = count / 8;
    assert_eq!(fs.tier_migrate(false).unwrap().relocated, 0);
    assert_eq!(on_fast(&fs).len(), 1);
    // One block less, and it goes.
    fs.sp.alloc.groups_mut()[g].free_blocks = count / 8 - 1;
    assert_eq!(fs.tier_migrate(false).unwrap().relocated, 1);
    assert!(on_fast(&fs).is_empty());
}

/// A parity chunk hands blocks out by parity row, so its group's row is
/// the chunk's data column count: for RAID5, with its one parity column,
/// as much as for RAID6.
#[test]
fn a_raid5_chunk_allocates_by_its_parity_row() {
    let devs = devices(3);
    mkfs::format_devices(&devs, &opts(), Profile::Raid5, Profile::Raid1).unwrap();
    let mut fs = CowFs::mount_devices(devs, false, false).unwrap();
    fs.set_clock(clock);
    // Group 0 is forced to RAID1, so the RAID5 chunk is a later one.
    assert!(fs.sp.grow_on(ChunkKind::Data, None));
    let vol = fs.vol.clone().unwrap();
    let (g, cols) = {
        let map = vol.map();
        map.chunks
            .iter()
            .find(|(_, c)| c.profile == Profile::Raid5)
            .map(|(&g, c)| (g as usize, c.data_cols() as u64))
            .expect("a raid5 chunk")
    };
    assert_eq!(cols, 2, "three devices, one of them parity");
    fs.refresh_layouts();
    assert_eq!(fs.sp.alloc.groups()[g].row(), cols);
}

/// A chunk is scrubbed before it moves, so the copies the move makes are of
/// good blocks. `copy_between` takes the first copy that reads without an
/// I/O error and never looks at its checksum, so without that scrub the
/// move of a mirror with one rotten copy can write the rot over the good
/// one. Removing a device is the worst place for that: the copy left
/// behind is the only one there will be.
#[test]
fn a_rotten_copy_is_repaired_before_its_chunk_moves() {
    let devs: Vec<Arc<dyn BlockDevice>> = (0..3)
        .map(|_| Ram::new(32 * MIB) as Arc<dyn BlockDevice>)
        .collect();
    mkfs::format_devices(&devs, &opts(), Profile::Raid1, Profile::Raid1).unwrap();
    let mut fs = CowFs::mount_devices(devs.clone(), false, false).unwrap();
    fs.set_clock(clock);
    let h = file(&mut fs, "f", &[7; 3 * BS as usize]);
    fs.sync().unwrap();
    let b = fs.fiemap(h, 0, 3 * BS).unwrap()[0].physical / BS;
    let vol = fs.vol.clone().unwrap();

    // The data chunk's two stripes are the block's two copies, in order.
    let (g, away) = {
        let map = vol.map();
        let (&g, c) = map
            .chunks
            .iter()
            .find(|(_, c)| c.kind == ChunkKind::Data)
            .expect("a data chunk");
        assert_eq!(c.stripes.len(), 2, "mirrored on two of the three devices");
        (g, c.stripes[1].devid)
    };

    // Rot copy 0 and nothing else: copy 1 still holds the block. Reading it
    // would repair it, so nothing reads it between here and the move.
    vol.write_copy(b * BS, &[0xff; BS as usize], 0).unwrap();
    fs.drop_caches();

    // Take away the device copy 1 is on. The chunk has to move, because
    // `place_moving` only keeps the stripes it is allowed to keep, and the
    // copy it is made from is the one left: copy 0, the rotten one.
    let before = vol.map().chunks.get(&g).cloned().expect("the data chunk");
    fs.remove_device(away).unwrap();
    let after = vol.map().chunks.get(&g).cloned().expect("the data chunk");
    assert_ne!(after.stripes, before.stripes, "the chunk did not move");
    assert_eq!(after.stripes.len(), 2, "still mirrored after the move");

    // Both copies of the moved block have to hold the file. Read them one
    // by one: `read_copy` has no fallback, so no read-repair can stand in
    // for the scrub that should have run before the move.
    for k in 0..2 {
        let mut got = vec![0u8; BS as usize];
        vol.read_copy(b * BS, &mut got, k).unwrap();
        assert_eq!(got, vec![7u8; BS as usize], "copy {} of the moved block", k);
    }
}

/// A disk that dies: every operation counts, and from `at` on they all fail.
/// `at` is armed after the setup, so the failure lands in the work under
/// test and not in getting there.
struct Dying {
    disk: Arc<Ram>,
    ops: core::sync::atomic::AtomicU64,
    at: core::sync::atomic::AtomicU64,
}

impl Dying {
    fn new(disk: Arc<Ram>) -> Arc<Self> {
        Arc::new(Self {
            disk,
            ops: 0.into(),
            at: u64::MAX.into(),
        })
    }
    fn arm(&self, at: u64) {
        let o = core::sync::atomic::Ordering::Relaxed;
        self.ops.store(0, o);
        self.at.store(at, o);
    }
    fn used(&self) -> u64 {
        self.ops.load(core::sync::atomic::Ordering::Relaxed)
    }
    fn tick(&self) -> Result<()> {
        let o = core::sync::atomic::Ordering::Relaxed;
        if self.ops.fetch_add(1, o) >= self.at.load(o) {
            return Err(Error::Io);
        }
        Ok(())
    }
}

impl BlockDevice for Dying {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.tick()?;
        self.disk.read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        self.tick()?;
        self.disk.write_at(offset, buf)
    }
    fn sync(&self) -> Result<()> {
        self.tick()?;
        self.disk.sync()
    }
    fn size(&self) -> u64 {
        self.disk.size()
    }
}

/// Three devices, mirrored, with a file on them; `at` armed on every device.
fn dying_volume(at: u64) -> (Vec<Arc<Dying>>, CowFs) {
    let raw: Vec<Arc<Ram>> = (0..3).map(|_| Ram::new(32 * MIB)).collect();
    let plain: Vec<Arc<dyn BlockDevice>> = raw
        .iter()
        .map(|d| d.clone() as Arc<dyn BlockDevice>)
        .collect();
    mkfs::format_devices(&plain, &opts(), Profile::Raid1, Profile::Raid1).unwrap();
    let disks: Vec<Arc<Dying>> = raw.into_iter().map(Dying::new).collect();
    let devs: Vec<Arc<dyn BlockDevice>> = disks
        .iter()
        .map(|d| d.clone() as Arc<dyn BlockDevice>)
        .collect();
    let mut fs = CowFs::mount_devices(devs, false, false).unwrap();
    fs.set_clock(clock);
    file(&mut fs, "f", &[7; 200_000]);
    fs.sync().unwrap();
    for d in &disks {
        d.arm(at);
    }
    (disks, fs)
}

/// The stripes a chunk is moved away from stay held until the move commits,
/// so a placement in the same transaction cannot land on the blocks that
/// are still the committed state. The hold is released by the commit, which
/// is why it can only be seen when the commit fails: the device dies part
/// way through removing it, and what is held then has to be exactly the
/// stripes on that device, where they were and as long as they were.
#[test]
fn a_failed_move_keeps_holding_the_stripes_it_moved_off() {
    const AWAY: u32 = 2;
    // What a whole removal costs, to know how far to sweep.
    let (disks, mut fs) = dying_volume(u64::MAX);
    fs.remove_device(AWAY).unwrap();
    let cost = disks.iter().map(|d| d.used()).max().unwrap();
    assert!(cost > 4, "a removal that cheap measures nothing: {}", cost);

    let mut seen = 0;
    for at in 1..=cost {
        let (_disks, mut fs) = dying_volume(at);
        let vol = fs.vol.clone().unwrap();
        // Every stripe on the device being taken away, with the length its
        // own chunk holds it by.
        let want: Vec<(u32, u64, u64)> = vol
            .map()
            .chunks
            .values()
            .flat_map(|c| {
                let len = c.stripe_len();
                c.stripes
                    .iter()
                    .filter(|s| s.devid == AWAY)
                    .map(move |s| (s.devid, s.phys, len))
                    .collect::<Vec<_>>()
            })
            .collect();
        let _ = fs.remove_device(AWAY);
        let held = vol.held();
        if held.is_empty() {
            continue;
        }
        seen += 1;
        for r in &held {
            assert!(
                want.contains(r),
                "at {}: held {:?} is not a stripe of the device going away, \
                 which are {:?}",
                at,
                r,
                want
            );
        }
    }
    assert!(seen > 0, "no failure left anything held, so nothing was measured");
}
