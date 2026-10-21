//! What `check` finds when a tree or the space accounting is wrong: one
//! test per kind of damage, made by editing the trees or the in-memory
//! state of a mounted filesystem directly.

use super::*;
use crate::testdisk::{clock, formatted, Ram, MIB};
use crate::volume::{ChunkKind, Profile};

fn mounted(read_only: bool) -> (Arc<Ram>, CowFs) {
    let d = formatted(16);
    let mut fs = CowFs::mount(d.clone(), read_only).unwrap();
    fs.set_clock(clock);
    (d, fs)
}

fn file(fs: &mut CowFs, name: &str, data: &[u8]) -> u64 {
    let h = fs
        .create(fs.root_ino(), name, FileKind::Regular, 0o644, 0)
        .unwrap();
    fs.write(h, 0, data).unwrap();
    h
}

/// The current subvolume's namespace check, on the tree as it is now.
fn subvol_check(fs: &mut CowFs) -> Result<()> {
    let mut item = fs.fs_item.clone();
    item.root = fs.fs_root;
    fs.check_subvol(&item).map(|_| ())
}

fn with_item(fs: &mut CowFs, f: impl FnOnce(&mut RootItem)) -> Result<()> {
    let mut item = fs.fs_item.clone();
    item.root = fs.fs_root;
    f(&mut item);
    fs.check_subvol(&item).map(|_| ())
}

/// Items of `ino` of type `ty` in the current subvolume.
fn items(fs: &mut CowFs, ino: u64, ty: u8) -> Vec<(Key, Vec<u8>)> {
    fs.st
        .range(
            &fs.fs_root,
            &Key::new(ino, ty, 0),
            &Key::new(ino, ty, u64::MAX),
            usize::MAX,
        )
        .unwrap()
}

fn corrupt(r: Result<()>, what: &'static str) {
    assert_eq!(r, Err(Error::Corrupt(what)));
}

/// An item of `ino` copied to another offset, which breaks whatever the
/// offset has to agree with.
fn moved(fs: &mut CowFs, ino: u64, ty: u8, by: u64) {
    let (k, v) = items(fs, ino, ty).remove(0);
    fs.upsert(Key::new(ino, ty, k.offset + by), v).unwrap();
}

#[test]
fn keys_that_disagree_with_their_items() {
    let root = FIRST_INO;
    for (ino_of, ty, what) in [
        (1u8, INODE_REF, "fsck: inode ref key"),
        (0, DIR_ITEM, "fsck: name hash"),
        (1, XATTR_ITEM, "fsck: xattr item"),
    ] {
        let (_, mut fs) = mounted(false);
        let h = file(&mut fs, "f", b"x");
        fs.set_xattr(h, "user.a", b"1", 0).unwrap();
        let ino = if ino_of == 1 { split(h).1 } else { root };
        moved(&mut fs, ino, ty, 1);
        corrupt(subvol_check(&mut fs), what);
    }
}

#[test]
fn an_encryption_context_lives_at_offset_zero_and_needs_the_feature() {
    let (_, mut fs) = mounted(false);
    fs.upsert(Key::new(FIRST_INO, ENC_CONTEXT, 1), vec![0; 8])
        .unwrap();
    corrupt(subvol_check(&mut fs), "fsck: encryption context key");
    let (_, mut fs) = mounted(false);
    fs.upsert(Key::new(FIRST_INO, ENC_CONTEXT, 0), vec![0; 8])
        .unwrap();
    corrupt(
        subvol_check(&mut fs),
        "fsck: encryption without the feature",
    );
}

#[test]
fn an_entry_naming_a_subvolume_names_its_root_directory() {
    let (_, mut fs) = mounted(false);
    fs.create_subvolume(fs.root_ino(), "sv").unwrap();
    let (k, v) = items(&mut fs, FIRST_INO, DIR_INDEX).remove(0);
    let e = decode_pair(&v).unwrap();
    fs.upsert(k, encode_pair(e.ino, e.index, FileKind::Regular, &e.name))
        .unwrap();
    corrupt(subvol_check(&mut fs), "fsck: subvolume entry");
}

/// Extents of every kind must start on a block, past the end of the one
/// before; inline data only at offset 0 and no bigger than `INLINE_MAX`.
#[test]
fn extents_out_of_place() {
    let (_, mut fs) = mounted(false);
    let h = file(&mut fs, "reg", &[1; 8192]);
    moved(&mut fs, split(h).1, EXTENT_DATA, 1);
    corrupt(subvol_check(&mut fs), "fsck: overlapping extents");

    let (_, mut fs) = mounted(false);
    let h = fs
        .create(fs.root_ino(), "z", FileKind::Regular, 0o644, 0)
        .unwrap();
    fs.set_compression(h, Some(Compression::Zlib)).unwrap();
    fs.write(h, 0, &[3; 65536]).unwrap();
    fs.sync().unwrap();
    moved(&mut fs, split(h).1, EXTENT_DATA, 1);
    corrupt(subvol_check(&mut fs), "fsck: overlapping extents");

    let (_, mut fs) = mounted(false);
    let h = fs
        .create(fs.root_ino(), "p", FileKind::Regular, 0o644, 0)
        .unwrap();
    fs.fallocate(h, 0, 0, 8192).unwrap();
    moved(&mut fs, split(h).1, EXTENT_DATA, 1);
    corrupt(subvol_check(&mut fs), "fsck: overlapping extents");

    let (_, mut fs) = mounted(false);
    let h = file(&mut fs, "small", b"tiny");
    moved(&mut fs, split(h).1, EXTENT_DATA, BS);
    corrupt(subvol_check(&mut fs), "fsck: inline extent offset");

    let (_, mut fs) = mounted(false);
    let h = file(&mut fs, "big", b"tiny");
    fs.upsert(
        Key::new(split(h).1, EXTENT_DATA, 0),
        FileExtent::Inline(vec![0; INLINE_MAX + 1]).encode(),
    )
    .unwrap();
    corrupt(subvol_check(&mut fs), "fsck: inline extent too large");
}

#[test]
fn inodes_that_cannot_hold_what_they_hold() {
    // Data on a device node.
    let (_, mut fs) = mounted(false);
    let h = fs
        .create(fs.root_ino(), "dev", FileKind::CharDevice, 0o644, 0x0103)
        .unwrap();
    let ino = split(h).1;
    fs.upsert(
        Key::new(ino, EXTENT_DATA, 0),
        FileExtent::Inline(vec![1; 4]).encode(),
    )
    .unwrap();
    let mut i = fs.inode(ino).unwrap();
    i.nbytes = 4;
    fs.put_inode(ino, i).unwrap();
    corrupt(subvol_check(&mut fs), "fsck: data on a special file");

    // Data of an inode that is not there.
    let (_, mut fs) = mounted(false);
    fs.upsert(
        Key::new(FIRST_INO + 100, EXTENT_DATA, 0),
        FileExtent::Inline(vec![1; 4]).encode(),
    )
    .unwrap();
    corrupt(subvol_check(&mut fs), "fsck: extent of a free inode");
}

#[test]
fn counters_in_the_root_item() {
    let (_, mut fs) = mounted(false);
    file(&mut fs, "f", &[1; 5000]);
    fs.quota_enable().unwrap();
    corrupt(
        with_item(&mut fs, |i| i.referenced += 1),
        "fsck: quota counter",
    );
    corrupt(
        with_item(&mut fs, |i| i.inode_count += 1),
        "fsck: inode count",
    );
}

#[test]
fn directories_out_of_their_parents() {
    let root = FIRST_INO;
    let mkdir = |fs: &mut CowFs, name: &str| {
        split(
            fs.create(handle(FS_TREE, root), name, FileKind::Dir, 0o755, 0)
                .unwrap(),
        )
        .1
    };
    // Two directories each the other's parent.
    let (_, mut fs) = mounted(false);
    let a = mkdir(&mut fs, "a");
    let b = mkdir(&mut fs, "b");
    for (x, p) in [(a, b), (b, a)] {
        let mut i = fs.inode(x).unwrap();
        i.parent = p;
        fs.put_inode(x, i).unwrap();
    }
    corrupt(subvol_check(&mut fs), "fsck: directory cycle");

    // A directory whose `..` is not the one it is in.
    let (_, mut fs) = mounted(false);
    let a = mkdir(&mut fs, "a");
    let b = mkdir(&mut fs, "b");
    let mut i = fs.inode(b).unwrap();
    i.parent = a;
    fs.put_inode(b, i).unwrap();
    corrupt(subvol_check(&mut fs), "fsck: directory parent");
}

/// A subvolume with no root directory at all.
#[test]
fn a_subvolume_without_its_root() {
    let (_, mut fs) = mounted(false);
    let empty = fs.st.create(&mut fs.sp, FS_TREE, 0).unwrap();
    corrupt(
        with_item(&mut fs, |i| {
            i.root = empty;
            i.inode_count = 0;
        }),
        "fsck: root",
    );
}

// ---------------------------------------------------------------------------
// The whole-filesystem pass
// ---------------------------------------------------------------------------

#[test]
fn space_accounting_that_does_not_add_up() {
    let (_, mut fs) = mounted(true);
    fs.sb.total_blocks = 4;
    corrupt(fs.check().map(|_| ()), "fsck: block past the end");

    let (_, mut fs) = mounted(true);
    fs.sb.free_blocks += 1;
    corrupt(fs.check().map(|_| ()), "fsck: free block count");

    // The checksum table's blocks, no longer accounted for.
    let (_, mut fs) = mounted(true);
    fs.sb.bitmap_sums = None;
    corrupt(
        fs.check().map(|_| ()),
        "fsck: block marked used but unowned",
    );
}

#[test]
fn a_bad_root_tree_fails_the_check() {
    let (d, mut fs) = mounted(true);
    fs.drop_caches();
    d.poke((fs.root_tree.bytenr * BS + 100) as usize, 1);
    assert!(matches!(fs.check(), Err(Error::Corrupt(_))));
}

/// Entries naming subvolumes and the backreferences that answer them.
#[test]
fn subvolume_names_and_backrefs_must_agree() {
    // A backref gone.
    let (_, mut fs) = mounted(false);
    fs.create_subvolume(fs.root_ino(), "sv").unwrap();
    let id = FIRST_SUBVOL;
    fs.st
        .delete(
            &mut fs.sp,
            &mut fs.root_tree,
            Key::new(id, ROOT_BACKREF, FS_TREE),
        )
        .unwrap();
    corrupt(
        fs.check().map(|_| ()),
        "fsck: subvolume entries and backrefs differ",
    );

    // A subvolume still being received but already named.
    let (_, mut fs) = mounted(false);
    fs.create_subvolume(fs.root_ino(), "sv").unwrap();
    let k = Key::new(id, ROOT_ITEM, 0);
    let mut item = RootItem::decode(&fs.st.get(&fs.root_tree, &k).unwrap().unwrap()).unwrap();
    item.flags |= ROOT_FLAG_RECEIVING;
    fs.st
        .update(&mut fs.sp, &mut fs.root_tree, k, item.encode())
        .unwrap();
    corrupt(fs.check().map(|_| ()), "fsck: subvolume not named once");

    // An entry and backref for a subvolume that does not exist.
    let (_, mut fs) = mounted(false);
    fs.link_subvol(FIRST_INO, b"ghost", 999).unwrap();
    corrupt(
        fs.check().map(|_| ()),
        "fsck: entry for a missing subvolume",
    );
}

/// A receive in progress is skipped: its subvolume is checked when the
/// receive ends, and has no name until then.
#[test]
fn a_subvolume_being_received_is_left_out() {
    let (_, mut fs) = mounted(false);
    let root = fs.st.create(&mut fs.sp, FIRST_SUBVOL, 0).unwrap();
    let item = RootItem {
        root,
        next_ino: FIRST_INO,
        root_dir: FIRST_INO,
        flags: ROOT_FLAG_RECEIVING,
        ..Default::default()
    };
    fs.st
        .insert(
            &mut fs.sp,
            &mut fs.root_tree,
            Key::new(FIRST_SUBVOL, ROOT_ITEM, 0),
            item.encode(),
        )
        .unwrap();
    fs.check().unwrap();
}

/// A RAID1 volume with a data chunk besides the system one, mounted
/// read-only (so `check` does not commit what the tests change).
fn volume() -> CowFs {
    let devs: Vec<Arc<dyn BlockDevice>> = vec![Ram::new(64 * MIB), Ram::new(64 * MIB)];
    crate::mkfs::format_devices(
        &devs,
        &crate::testdisk::opts(),
        Profile::Raid1,
        Profile::Raid1,
    )
    .unwrap();
    let mut fs = CowFs::mount_devices(devs.clone(), false, false).unwrap();
    fs.set_clock(clock);
    file(&mut fs, "f", &[7; 1 << 20]);
    fs.sync().unwrap();
    drop(fs);
    CowFs::mount_devices(devs, true, false).unwrap()
}

/// The chunk map against the superblock and the allocation groups.
#[test]
fn a_volume_whose_chunks_do_not_add_up() {
    let mut fs = volume();
    fs.check().unwrap();
    fs.sb.vol.as_mut().unwrap().num_devices += 1;
    corrupt(fs.check().map(|_| ()), "fsck: system chunk");

    let mut fs = volume();
    let vs = fs.sb.vol.as_mut().unwrap();
    vs.chunk_map = fs.sb.total_blocks - 1;
    vs.chunk_map_len = 2 * BS as u32;
    corrupt(fs.check().map(|_| ()), "fsck: block past the end");

    let mut fs = volume();
    let vs = fs.sb.vol.as_mut().unwrap();
    vs.chunk_map = vs.sys.blocks;
    corrupt(fs.check().map(|_| ()), "fsck: chunk map outside group 0");

    let mut fs = volume();
    fs.vol.clone().unwrap().with_map(|m| {
        let c = m
            .chunks
            .values_mut()
            .find(|c| c.kind == ChunkKind::Data)
            .unwrap();
        c.kind = ChunkKind::Metadata;
    });
    corrupt(fs.check().map(|_| ()), "fsck: chunks and groups disagree");

    // The journal moved past the end of the system chunk.
    let mut fs = volume();
    fs.journal.start = fs.sb.vol.as_ref().unwrap().sys.blocks;
    corrupt(fs.check().map(|_| ()), "fsck: block outside every chunk");
}
