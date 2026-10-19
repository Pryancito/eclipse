#![cfg(feature = "std")]
//! Repair (`btrfs check --repair`): damage is made on purpose by editing
//! tree leaves on the disk of an unmounted filesystem, `check()` has to see
//! it, `repair()` has to fix it, and every file that is still reachable has
//! to keep its contents.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use common::*;
use eclipsefs::btree::{Body, Key, Node};
use eclipsefs::cowfmt::*;
use eclipsefs::device::BlockDevice;
use eclipsefs::{mkfs, ChunkKind, EclipseFS, Error, FileKind, Profile, Tier};

const B: u64 = BLOCK as u64;

fn sb(disk: &MemDisk) -> SuperV4 {
    let mut b = vec![0u8; BLOCK];
    disk.read_at(0, &mut b).unwrap();
    SuperV4::decode(&b).unwrap()
}

/// A leaf's items.
type Items = Vec<(Key, Vec<u8>)>;

fn read_node(disk: &MemDisk, b: u64, fsid: &[u8; 16]) -> Node {
    let mut buf = vec![0u8; BLOCK];
    disk.read_at(b * B, &mut buf).unwrap();
    Node::decode(&buf, b, fsid).unwrap()
}

/// Apply `f(owner, items)` to every leaf below the node at `b` (once per
/// node); a leaf `f` changes is written back with its items sorted, and the
/// keys pointing at it are kept in step. Returns the node's new first key
/// if it moved.
fn rewrite(
    disk: &MemDisk,
    fsid: &[u8; 16],
    b: u64,
    seen: &mut BTreeSet<u64>,
    f: &mut dyn FnMut(u64, &mut Items) -> bool,
) -> Option<Key> {
    if !seen.insert(b) {
        return None;
    }
    let mut node = read_node(disk, b, fsid);
    let owner = node.owner;
    let (changed, old_first, new_first) = match &mut node.body {
        Body::Leaf(items) => {
            let old = items.first().map(|i| i.0);
            let c = f(owner, items);
            items.sort_by_key(|i| i.0);
            (c, old, items.first().map(|i| i.0))
        }
        Body::Internal(ptrs) => {
            let old = ptrs.first().map(|p| p.key);
            let mut c = false;
            for p in ptrs.iter_mut() {
                if let Some(k) = rewrite(disk, fsid, p.bytenr, seen, f) {
                    p.key = k;
                    c = true;
                }
            }
            (c, old, ptrs.first().map(|p| p.key))
        }
    };
    if changed {
        disk.write_at(b * B, &node.encode(b, fsid)).unwrap();
    }
    (new_first != old_first).then_some(new_first).flatten()
}

/// Edit the leaves of every tree on the (unmounted) disk.
fn edit(disk: &MemDisk, mut f: impl FnMut(u64, &mut Vec<(Key, Vec<u8>)>) -> bool) {
    // Clean unmount: the journal's transaction is already home, and replaying
    // it would only rewrite the superblock and bitmaps we read here.
    let s = sb(disk);
    disk.write_at(s.journal_start * B, &[0u8; BLOCK]).unwrap();
    let mut seen = BTreeSet::new();
    rewrite(disk, &s.uuid, s.root.bytenr, &mut seen, &mut f);
    let mut roots = Vec::new();
    let mut stack = vec![s.root.bytenr];
    while let Some(b) = stack.pop() {
        match read_node(disk, b, &s.uuid).body {
            Body::Internal(p) => stack.extend(p.iter().map(|p| p.bytenr)),
            Body::Leaf(items) => {
                for (k, d) in items {
                    if k.ty == ROOT_ITEM {
                        roots.push(RootItem::decode(&d).unwrap().root.bytenr);
                    }
                }
            }
        }
    }
    for r in roots {
        rewrite(disk, &s.uuid, r, &mut seen, &mut f);
    }
}

/// Edit the items of the top subvolume only.
fn edit_fs(disk: &MemDisk, mut f: impl FnMut(&mut Vec<(Key, Vec<u8>)>) -> bool) {
    edit(disk, |owner, items| owner == FS_TREE && f(items));
}

/// Add `item` to the top subvolume, in the leaf its key belongs in.
fn add_fs_item(disk: &MemDisk, key: Key, value: Vec<u8>) {
    let mut firsts = Vec::new();
    edit_fs(disk, |items| {
        firsts.extend(items.first().map(|i| i.0));
        false
    });
    let target = firsts
        .into_iter()
        .filter(|k| *k <= key)
        .max()
        .expect("a leaf for the key");
    let mut value = Some(value);
    edit_fs(disk, |items| {
        if items.first().map(|i| i.0) != Some(target) {
            return false;
        }
        items.push((key, value.take().unwrap()));
        true
    });
}

fn unmount(fs: EclipseFS) {
    let mut fs = fs;
    fs.sync().unwrap();
    drop(fs);
}

fn open(disk: &Arc<MemDisk>) -> EclipseFS {
    let mut fs = EclipseFS::mount(disk.clone(), false).unwrap();
    fs.set_clock(clock);
    fs
}

/// Every file and symlink under `dir`, by path.
fn tree(fs: &mut EclipseFS, dir: u64, path: &str, out: &mut BTreeMap<String, Vec<u8>>) {
    for e in fs.readdir(dir).unwrap() {
        if e.name == "." || e.name == ".." {
            continue;
        }
        let p = format!("{path}/{}", e.name);
        match e.kind {
            FileKind::Dir => tree(fs, e.ino, &p, out),
            FileKind::Symlink => {
                out.insert(p, fs.read_link(e.ino).unwrap());
            }
            _ => {
                out.insert(p, read_capped(fs, e.ino));
            }
        }
    }
}

/// The file's contents, or its first MiB and its size when it is larger
/// (damage can make a sparse file of any size).
fn read_capped(fs: &mut EclipseFS, ino: u64) -> Vec<u8> {
    let size = fs.stat(ino).unwrap().size;
    if size <= MIB as u64 {
        return read_all(fs, ino);
    }
    let mut buf = vec![0u8; MIB];
    let n = fs.read(ino, 0, &mut buf).unwrap();
    buf.truncate(n);
    buf.extend_from_slice(&size.to_le_bytes());
    buf
}

fn contents(fs: &mut EclipseFS) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let root = fs.root_ino();
    tree(fs, root, "", &mut out);
    out
}

/// A small tree: files of every size class (inline, one block, many
/// blocks), a directory with files, a symlink and a hard link.
fn sample(fs: &mut EclipseFS) {
    let root = fs.root_ino();
    for (i, size) in [100usize, 4096, 50_000, 300_000].into_iter().enumerate() {
        let f = fs
            .create(root, &format!("f{i}"), FileKind::Regular, 0o644, 0)
            .unwrap();
        write_all(fs, f, 0, &pattern(size, i));
    }
    let d = fs.create(root, "d", FileKind::Dir, 0o755, 0).unwrap();
    for i in 0..5 {
        let f = fs
            .create(d, &format!("g{i}"), FileKind::Regular, 0o644, 0)
            .unwrap();
        write_all(fs, f, 0, &pattern(20_000 + i * 7000, 10 + i));
    }
    fs.symlink(root, "s", b"d/g1").unwrap();
    let f1 = fs.lookup(root, "f1").unwrap();
    fs.link(d, "hard", f1).unwrap();
}

/// After the damage: check sees it, repair fixes it, and the repaired
/// filesystem is still clean after a remount and a second repair has
/// nothing left to do.
fn repair(disk: &Arc<MemDisk>) -> (EclipseFS, eclipsefs::RepairReport) {
    let mut fs = open(disk);
    assert!(fs.check().is_err(), "the damage is not visible to check");
    let r = fs.repair().unwrap();
    assert!(r.changed(), "{:?}", r);
    assert_eq!(r.still_damaged, None, "{:?}", r);
    fs.check().unwrap();
    unmount(fs);
    let mut fs = open(disk);
    fs.check().unwrap();
    let again = fs.repair().unwrap();
    assert!(
        !again.changed(),
        "second repair changed things: {:?}",
        again
    );
    assert!(again.is_clean(), "{:?}", again);
    (fs, r)
}

fn setup() -> (Arc<MemDisk>, BTreeMap<String, Vec<u8>>, u64) {
    let (disk, mut fs) = fresh(64 * MIB);
    sample(&mut fs);
    let want = contents(&mut fs);
    let root = fs.root_ino();
    unmount(fs);
    (disk, want, root)
}

fn ino_of(disk: &Arc<MemDisk>, path: &[&str]) -> u64 {
    let mut fs = open(disk);
    let mut at = fs.root_ino();
    for p in path {
        at = fs.lookup(at, p).unwrap();
    }
    drop(fs);
    at
}

fn inode_of(items: &mut [(Key, Vec<u8>)], ino: u64) -> Option<&mut Vec<u8>> {
    items
        .iter_mut()
        .find(|(k, _)| *k == Key::new(ino, INODE_ITEM, 0))
        .map(|(_, d)| d)
}

/// Change an inode item in place.
fn edit_inode(disk: &MemDisk, ino: u64, mut f: impl FnMut(&mut InodeItem)) {
    edit_fs(disk, |items| match inode_of(items, ino) {
        Some(d) => {
            let mut i = InodeItem::decode(d).unwrap();
            f(&mut i);
            *d = i.encode();
            true
        }
        None => false,
    });
}

#[test]
fn a_clean_filesystem_is_left_alone() {
    let (disk, want, _) = setup();
    let mut fs = open(&disk);
    let gen = fs.generation();
    let r = fs.repair().unwrap();
    assert!(!r.changed(), "{:?}", r);
    assert!(r.is_clean(), "{:?}", r);
    assert_eq!(contents(&mut fs), want);
    // Nothing to commit either.
    assert_eq!(fs.generation(), gen);
}

#[test]
fn a_read_only_mount_cannot_repair() {
    let (disk, _, _) = setup();
    let mut fs = EclipseFS::mount(disk.clone(), true).unwrap();
    assert!(matches!(fs.repair(), Err(Error::Unsupported(_))));
}

#[test]
fn the_old_format_cannot_repair() {
    let (_, mut fs) = fresh_v3(16 * MIB);
    assert!(matches!(fs.repair(), Err(Error::Unsupported(_))));
}

/// Bitmaps that agree with their checksums but not with the trees: a leak
/// (free blocks marked in use) and blocks in use marked free.
#[test]
fn bitmaps_are_rebuilt_from_the_trees() {
    let (disk, want, _) = setup();
    let f = ino_of(&disk, &["f3"]);
    let mut at = None;
    edit_fs(&disk, |items| {
        for (k, d) in items.iter() {
            if *k == Key::new(f, EXTENT_DATA, 0) {
                if let Ok(FileExtent::Reg { disk, .. }) = FileExtent::decode(d) {
                    at = Some(disk);
                }
            }
        }
        false
    });
    let at = at.unwrap();
    let s = sb(&disk);
    let bitmap = B; // group 0's bitmap block
    let mut bm = vec![0u8; BLOCK];
    disk.read_at(bitmap, &mut bm).unwrap();
    // In use, marked free: the first 16 blocks of f3's data.
    for b in at..at + 16 {
        bm[(b / 8) as usize] &= !(1 << (b % 8));
    }
    // Leaked: 64 free blocks near the end of the group, marked in use.
    let total = s.total_blocks.min(32768);
    for b in total - 200..total - 136 {
        bm[(b / 8) as usize] |= 1 << (b % 8);
    }
    disk.write_at(bitmap, &bm).unwrap();
    // Keep the checksum table in step, so mount takes the bitmap as is.
    let (table, _) = s.bitmap_sums.unwrap();
    let mut tb = vec![0u8; BLOCK];
    disk.read_at(table * B, &mut tb).unwrap();
    let mut sums = decode_sums_block(&tb).unwrap();
    sums[0] = eclipsefs::crc::crc32c(&bm);
    disk.write_at(table * B, &encode_sums_block(&sums)).unwrap();
    let (mut fs, r) = repair(&disk);
    assert_eq!(r.bitmap_blocks_fixed, 16 + 64, "{:?}", r);
    assert_eq!(contents(&mut fs), want);
}

#[test]
fn a_wrong_link_count_is_fixed() {
    let (disk, want, _) = setup();
    let f0 = ino_of(&disk, &["f0"]);
    let f1 = ino_of(&disk, &["f1"]);
    edit_inode(&disk, f0, |i| i.nlink = 7);
    edit_inode(&disk, f1, |i| i.nlink = 1);
    let (mut fs, r) = repair(&disk);
    assert_eq!(r.inodes_fixed, 2);
    assert_eq!(fs.stat(f0).unwrap().nlink, 1);
    assert_eq!(fs.stat(f1).unwrap().nlink, 2);
    assert_eq!(contents(&mut fs), want);
}

#[test]
fn a_wrong_directory_link_count_and_size_are_fixed() {
    let (disk, want, root) = setup();
    let d = ino_of(&disk, &["d"]);
    edit_inode(&disk, root, |i| i.nlink = 9);
    edit_inode(&disk, d, |i| {
        i.size = 1;
        i.nlink = 0;
    });
    let (mut fs, _) = repair(&disk);
    assert_eq!(fs.stat(root).unwrap().nlink, 3);
    assert_eq!(fs.stat(d).unwrap().nlink, 2);
    let names: u64 = fs
        .readdir(d)
        .unwrap()
        .iter()
        .filter(|e| e.name != "." && e.name != "..")
        .map(|e| 2 * e.name.len() as u64)
        .sum();
    assert_eq!(fs.stat(d).unwrap().size, names);
    assert_eq!(contents(&mut fs), want);
}

#[test]
fn a_wrong_byte_count_is_fixed() {
    let (disk, want, _) = setup();
    let f = ino_of(&disk, &["f3"]);
    let real = {
        let mut fs = open(&disk);
        fs.stat(f).unwrap().nbytes
    };
    edit_inode(&disk, f, |i| i.nbytes = 12345);
    let (mut fs, _) = repair(&disk);
    assert_eq!(fs.stat(f).unwrap().nbytes, real);
    assert_eq!(contents(&mut fs), want);
}

#[test]
fn a_missing_inode_takes_its_names_and_blocks_with_it() {
    let (disk, mut want, _) = setup();
    let f = ino_of(&disk, &["f3"]);
    let used = open(&disk).fsinfo().bytes_used;
    edit_fs(&disk, |items| {
        let n = items.len();
        items.retain(|(k, _)| *k != Key::new(f, INODE_ITEM, 0));
        items.len() != n
    });
    let (mut fs, r) = repair(&disk);
    want.remove("/f3");
    assert!(r.entries_removed >= 1 && r.items_removed >= 1, "{:?}", r);
    assert_eq!(fs.lookup(fs.root_ino(), "f3"), Err(Error::NotFound));
    assert!(fs.fsinfo().bytes_used + 300_000 / B * B <= used);
    assert_eq!(contents(&mut fs), want);
}

#[test]
fn a_file_without_a_name_goes_to_lost_found() {
    let (disk, mut want, root) = setup();
    let f = ino_of(&disk, &["f2"]);
    edit_fs(&disk, |items| {
        let n = items.len();
        items.retain(|(k, d)| {
            !(k.objectid == root
                && ((k.ty == DIR_INDEX && decode_pair(d).unwrap().ino == f)
                    || (k.ty == DIR_ITEM && k.offset == name_hash(b"f2"))))
        });
        items.len() != n
    });
    let (mut fs, r) = repair(&disk);
    assert_eq!(r.lost_found, 1, "{:?}", r);
    let data = want.remove("/f2").unwrap();
    want.insert(format!("/lost+found/#{f}"), data);
    assert_eq!(contents(&mut fs), want);
    let lf = fs.lookup(root, "lost+found").unwrap();
    assert_eq!(fs.lookup(lf, &format!("#{f}")).unwrap(), f);
    assert_eq!(fs.parent(lf).unwrap(), root);
}

#[test]
fn a_directory_without_a_name_keeps_its_children() {
    let (disk, want, root) = setup();
    let d = ino_of(&disk, &["d"]);
    edit_fs(&disk, |items| {
        let n = items.len();
        items.retain(|(k, d2)| {
            !(k.objectid == root
                && ((k.ty == DIR_INDEX && decode_pair(d2).unwrap().ino == d)
                    || (k.ty == DIR_ITEM && k.offset == name_hash(b"d"))))
        });
        items.len() != n
    });
    let (mut fs, _) = repair(&disk);
    let mut moved = BTreeMap::new();
    for (p, v) in want {
        let p = match p.strip_prefix("/d/") {
            Some(rest) => format!("/lost+found/#{d}/{rest}"),
            None => p,
        };
        moved.insert(p, v);
    }
    assert_eq!(contents(&mut fs), moved);
    let lf = fs.lookup(root, "lost+found").unwrap();
    assert_eq!(fs.parent(d).unwrap(), lf);
    assert_eq!(fs.stat(lf).unwrap().nlink, 3);
}

#[test]
fn a_missing_name_hash_item_is_rebuilt() {
    let (disk, want, root) = setup();
    edit_fs(&disk, |items| {
        let n = items.len();
        items.retain(|(k, _)| *k != Key::new(root, DIR_ITEM, name_hash(b"f1")));
        items.len() != n
    });
    let (mut fs, r) = repair(&disk);
    assert!(r.entries_fixed >= 1);
    assert_eq!(
        fs.lookup(root, "f1").unwrap(),
        ino_of_fs(&mut fs, "d", "hard")
    );
    assert_eq!(contents(&mut fs), want);
}

fn ino_of_fs(fs: &mut EclipseFS, dir: &str, name: &str) -> u64 {
    let root = fs.root_ino();
    let d = fs.lookup(root, dir).unwrap();
    fs.lookup(d, name).unwrap()
}

#[test]
fn back_references_are_rebuilt_and_stray_ones_dropped() {
    let (disk, want, root) = setup();
    let f0 = ino_of(&disk, &["f0"]);
    let f1 = ino_of(&disk, &["f1"]);
    // f0 loses its only back reference; f1 gets one for a name it does
    // not have.
    edit_fs(&disk, |items| {
        let n = items.len();
        items.retain(|(k, _)| !(k.objectid == f0 && k.ty == INODE_REF));
        n != items.len()
    });
    add_fs_item(
        &disk,
        Key::new(f1, INODE_REF, ref_hash(root, 999)),
        encode_pair(root, 999, FileKind::Regular, b"ghost"),
    );
    let (mut fs, r) = repair(&disk);
    assert!(r.entries_fixed >= 2, "{:?}", r);
    assert_eq!(fs.stat(f1).unwrap().nlink, 2);
    assert_eq!(contents(&mut fs), want);
}

#[test]
fn a_lost_extent_item_leaks_nothing() {
    let (disk, mut want, _) = setup();
    let f = ino_of(&disk, &["f3"]);
    edit_fs(&disk, |items| {
        let n = items.len();
        // The file's first extent (data from offset 0).
        items.retain(|(k, _)| *k != Key::new(f, EXTENT_DATA, 0));
        items.len() != n
    });
    let (mut fs, r) = repair(&disk);
    assert!(r.bitmap_blocks_fixed > 0, "{:?}", r);
    assert!(r.csums_dropped > 0, "{:?}", r);
    assert!(r.inodes_fixed > 0, "{:?}", r);
    // What the extent held (all of it, written in one go) is a hole now.
    let got = read_all(&mut fs, f);
    let old = want.remove("/f3").unwrap();
    assert_eq!(got.len(), old.len());
    assert!(got.iter().all(|&b| b == 0));
    let mut rest = contents(&mut fs);
    rest.remove("/f3");
    assert_eq!(rest, want);
}

#[test]
fn blocks_claimed_twice_become_shared() {
    let (disk, want, root) = setup();
    let src = ino_of(&disk, &["f2"]);
    let size = want["/f2"].len() as u64;
    // An empty file the damage will point at f2's blocks.
    let dst = {
        let mut fs = open(&disk);
        let dst = fs
            .create(root, "copy", FileKind::Regular, 0o644, 0)
            .unwrap();
        unmount(fs);
        dst
    };
    let mut extents = Vec::new();
    edit_fs(&disk, |items| {
        extents.extend(
            items
                .iter()
                .filter(|(k, _)| k.objectid == src && k.ty == EXTENT_DATA)
                .cloned(),
        );
        false
    });
    assert!(!extents.is_empty());
    edit_fs(&disk, |items| {
        let Some(d) = inode_of(items, dst) else {
            return false;
        };
        let mut i = InodeItem::decode(d).unwrap();
        i.size = size;
        *d = i.encode();
        for (k, v) in &extents {
            items.push((Key::new(dst, EXTENT_DATA, k.offset), v.clone()));
        }
        true
    });
    let (mut fs, r) = repair(&disk);
    assert!(r.refcounts_fixed > 0, "{:?}", r);
    let copy = fs.lookup(root, "copy").unwrap();
    assert_eq!(read_all(&mut fs, copy), want["/f2"]);
    // Shared now: a change to one side leaves the other alone.
    write_all(&mut fs, copy, 0, &[0xEE; 5000]);
    let f2 = fs.lookup(root, "f2").unwrap();
    assert_eq!(read_all(&mut fs, f2), want["/f2"]);
    fs.check().unwrap();
    let mut got = contents(&mut fs);
    got.remove("/copy");
    assert_eq!(got, want);
}

#[test]
fn missing_checksums_are_computed() {
    let (disk, want, _) = setup();
    let mut gone = 0;
    edit(&disk, |owner, items| {
        if owner != CSUM_TREE {
            return false;
        }
        for (k, d) in items.iter_mut() {
            if k.ty != CSUM_ITEM {
                continue;
            }
            // Every other checksum of the bucket goes.
            let mut cb = CsumBucket::decode(d).unwrap();
            for c in cb.0.iter_mut().step_by(2) {
                gone += c.take().is_some() as usize;
            }
            *d = cb.encode();
        }
        gone > 0
    });
    assert!(gone > 0);
    let (mut fs, r) = repair(&disk);
    assert!(r.csums_added > 0, "{:?}", r);
    assert_eq!(contents(&mut fs), want);
    assert!(fs.scrub().unwrap().is_clean());
}

#[test]
fn an_unknown_item_is_dropped() {
    let (disk, want, _) = setup();
    let f = ino_of(&disk, &["f0"]);
    add_fs_item(&disk, Key::new(f, 77, 3), vec![1, 2, 3]);
    let (mut fs, r) = repair(&disk);
    assert_eq!(r.items_removed, 1);
    assert_eq!(contents(&mut fs), want);
}

#[test]
fn items_of_an_inode_that_never_was_are_dropped() {
    let (disk, want, _) = setup();
    let ghost = 100_000;
    add_fs_item(
        &disk,
        Key::new(ghost, XATTR_ITEM, name_hash(b"user.x")),
        encode_xattrs(&[(b"user.x".to_vec(), b"y".to_vec())]),
    );
    let (mut fs, r) = repair(&disk);
    assert_eq!(r.items_removed, 1);
    assert_eq!(contents(&mut fs), want);
}

#[test]
fn an_overlapping_extent_is_dropped() {
    let (disk, want, _) = setup();
    let f = ino_of(&disk, &["f3"]);
    let mut first = None;
    edit_fs(&disk, |items| {
        first = first.take().or_else(|| {
            items
                .iter()
                .find(|(k, _)| k.objectid == f && k.ty == EXTENT_DATA)
                .cloned()
        });
        false
    });
    let first = first.unwrap();
    let FileExtent::Reg { disk: at, blocks } = FileExtent::decode(&first.1).unwrap() else {
        panic!("not a regular extent");
    };
    assert!(blocks >= 2);
    // A second item mapping the extent's second block again.
    add_fs_item(
        &disk,
        Key::new(f, EXTENT_DATA, first.0.offset + B),
        FileExtent::Reg {
            disk: at + 1,
            blocks: 1,
        }
        .encode(),
    );
    let (mut fs, r) = repair(&disk);
    assert_eq!(r.items_removed, 1, "{:?}", r);
    assert_eq!(contents(&mut fs), want);
}

#[test]
fn an_extent_pointing_at_metadata_is_dropped_without_freeing_it() {
    let (disk, mut want, _) = setup();
    let f = ino_of(&disk, &["f2"]);
    let node = sb(&disk).root.bytenr;
    edit_fs(&disk, |items| {
        for (k, d) in items.iter_mut() {
            if k.objectid == f && k.ty == EXTENT_DATA {
                if let Ok(FileExtent::Reg { blocks, .. }) = FileExtent::decode(d) {
                    *d = FileExtent::Reg { disk: node, blocks }.encode();
                    return true;
                }
            }
        }
        false
    });
    let (mut fs, r) = repair(&disk);
    assert!(r.items_removed >= 1, "{:?}", r);
    // The extent's own blocks leaked and are back.
    assert!(r.bitmap_blocks_fixed > 0, "{:?}", r);
    let got = read_all(&mut fs, f);
    assert!(got.iter().take(BLOCK).all(|&b| b == 0));
    want.remove("/f2");
    let mut rest = contents(&mut fs);
    rest.remove("/f2");
    assert_eq!(rest, want);
}

#[test]
fn a_directory_cycle_is_broken() {
    let (disk, mut fs) = fresh(64 * MIB);
    let root = fs.root_ino();
    let a = fs.create(root, "a", FileKind::Dir, 0o755, 0).unwrap();
    let b = fs.create(a, "b", FileKind::Dir, 0o755, 0).unwrap();
    let fa = fs.create(a, "in-a", FileKind::Regular, 0o644, 0).unwrap();
    let fb = fs.create(b, "in-b", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, fa, 0, &pattern(9000, 1));
    write_all(&mut fs, fb, 0, &pattern(7000, 2));
    unmount(fs);
    // Move a's entry from the root into b: a holds b holds a.
    edit_fs(&disk, |items| {
        for (k, d) in items.iter_mut() {
            if k.objectid == root && k.ty == DIR_INDEX && decode_pair(d).unwrap().ino == a {
                *k = Key::new(b, DIR_INDEX, 100);
            } else if *k == Key::new(root, DIR_ITEM, name_hash(b"a")) {
                let mut list = decode_dir_item(d).unwrap();
                for e in &mut list {
                    e.index = 100;
                }
                *d = encode_dir_item(&list);
                *k = Key::new(b, DIR_ITEM, name_hash(b"a"));
            } else if k.objectid == a && k.ty == INODE_REF {
                *k = Key::new(a, INODE_REF, ref_hash(b, 100));
                *d = encode_pair(b, 100, FileKind::Dir, b"a");
            }
        }
        if let Some(d) = inode_of(items, a) {
            let mut i = InodeItem::decode(d).unwrap();
            i.parent = b;
            *d = i.encode();
        }
        true
    });
    let (mut fs, r) = repair(&disk);
    assert_eq!(r.lost_found, 1, "{:?}", r);
    let got = contents(&mut fs);
    assert_eq!(got.len(), 2, "{:?}", got.keys());
    assert!(got.values().any(|v| *v == pattern(9000, 1)));
    assert!(got.values().any(|v| *v == pattern(7000, 2)));
    assert!(got.keys().all(|k| k.starts_with("/lost+found/#")));
}

#[test]
fn subvolume_counters_are_recomputed() {
    let (disk, mut fs) = fresh(64 * MIB);
    fs.quota_enable().unwrap();
    sample(&mut fs);
    let want = contents(&mut fs);
    let referenced = fs.qgroup_show().unwrap()[0].referenced;
    unmount(fs);
    edit(&disk, |owner, items| {
        if owner != ROOT_TREE {
            return false;
        }
        for (k, d) in items.iter_mut() {
            if *k == Key::new(FS_TREE, ROOT_ITEM, 0) {
                let mut it = RootItem::decode(d).unwrap();
                it.inode_count += 5;
                it.next_ino = FIRST_INO + 2;
                it.referenced = 1;
                *d = it.encode();
                return true;
            }
        }
        false
    });
    let (mut fs, _) = repair(&disk);
    assert_eq!(fs.qgroup_show().unwrap()[0].referenced, referenced);
    assert_eq!(contents(&mut fs), want);
    // New inodes do not collide with the ones that were past next_ino.
    let root = fs.root_ino();
    let n = fs.create(root, "new", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, n, 0, b"new file");
    assert_eq!(read_all(&mut fs, n), b"new file");
    fs.check().unwrap();
    let mut got = contents(&mut fs);
    got.remove("/new");
    assert_eq!(got, want);
}

#[test]
fn snapshots_and_reflinks_are_left_alone() {
    let (_disk, mut fs) = fresh(64 * MIB);
    sample(&mut fs);
    let root = fs.root_ino();
    let f3 = fs.lookup(root, "f3").unwrap();
    let c = fs
        .create(root, "clone", FileKind::Regular, 0o644, 0)
        .unwrap();
    fs.clone_range(f3, 0, c, 0, 300_000).unwrap();
    let sv = fs.create_subvolume(root, "sv").unwrap();
    let g = fs.create(sv, "g", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, g, 0, &pattern(70_000, 3));
    fs.snapshot(sv, root, "snap", true).unwrap();
    fs.snapshot(root, root, "top-snap", false).unwrap();
    let want = contents(&mut fs);
    let r = fs.repair().unwrap();
    assert!(!r.changed(), "{:?}", r);
    assert!(r.is_clean(), "{:?}", r);
    assert_eq!(contents(&mut fs), want);
}

#[test]
fn damage_in_a_snapshot_is_repaired_there() {
    let (disk, mut fs) = fresh(64 * MIB);
    let root = fs.root_ino();
    let sv = fs.create_subvolume(root, "sv").unwrap();
    let g = fs.create(sv, "g", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, g, 0, &pattern(70_000, 3));
    unmount(fs);
    let (_, local) = (0, g & ((1 << 40) - 1));
    edit(&disk, |owner, items| {
        if owner < FIRST_SUBVOL {
            return false;
        }
        match inode_of(items, local) {
            Some(d) => {
                let mut i = InodeItem::decode(d).unwrap();
                i.nlink = 4;
                *d = i.encode();
                true
            }
            None => false,
        }
    });
    let (mut fs, r) = repair(&disk);
    assert_eq!(r.inodes_fixed, 1, "{:?}", r);
    assert_eq!(fs.stat(g).unwrap().nlink, 1);
    assert_eq!(read_all(&mut fs, g), pattern(70_000, 3));
}

#[test]
fn compressed_files_survive_a_repair() {
    let (disk, mut fs) = fresh(64 * MIB);
    fs.set_mount_compression(Some(eclipsefs::Compression::Zstd))
        .unwrap();
    sample(&mut fs);
    let want = contents(&mut fs);
    unmount(fs);
    let f = ino_of(&disk, &["f3"]);
    edit_inode(&disk, f, |i| i.nlink = 3);
    let (mut fs, _) = repair(&disk);
    assert_eq!(contents(&mut fs), want);
}

#[test]
fn many_kinds_of_damage_at_once() {
    let (disk, mut want, root) = setup();
    let f0 = ino_of(&disk, &["f0"]);
    let f2 = ino_of(&disk, &["f2"]);
    let f3 = ino_of(&disk, &["f3"]);
    let d = ino_of(&disk, &["d"]);
    add_fs_item(&disk, Key::new(f0, 99, 0), vec![0]);
    edit_fs(&disk, |items| {
        items.retain(|(k, _)| *k != Key::new(f3, INODE_ITEM, 0));
        items.retain(|(k, v)| {
            !(k.objectid == root && k.ty == DIR_INDEX && decode_pair(v).unwrap().ino == f2)
        });
        if let Some(v) = inode_of(items, d) {
            let mut i = InodeItem::decode(v).unwrap();
            i.nlink = 40;
            i.parent = f0;
            *v = i.encode();
        }
        true
    });
    let (mut fs, _) = repair(&disk);
    want.remove("/f3");
    let data = want.remove("/f2").unwrap();
    want.insert(format!("/lost+found/#{f2}"), data);
    assert_eq!(contents(&mut fs), want);
    assert_eq!(fs.parent(d).unwrap(), root);
}

#[test]
fn a_repaired_filesystem_keeps_working() {
    let (disk, _, root) = setup();
    let f = ino_of(&disk, &["f3"]);
    edit_fs(&disk, |items| {
        let n = items.len();
        items.retain(|(k, _)| *k != Key::new(f, INODE_ITEM, 0));
        items.len() != n
    });
    let (mut fs, _) = repair(&disk);
    // Freed blocks are reused without trouble.
    let mut files = Vec::new();
    for i in 0..20 {
        let n = fs
            .create(root, &format!("after{i}"), FileKind::Regular, 0o644, 0)
            .unwrap();
        let data = pattern(40_000 + i * 3000, 50 + i);
        write_all(&mut fs, n, 0, &data);
        files.push((n, data));
    }
    unmount(fs);
    let mut fs = open(&disk);
    fs.check().unwrap();
    for (n, data) in files {
        assert_eq!(read_all(&mut fs, n), data);
    }
}

/// Bad copies on a mirrored volume are rewritten from the good ones before
/// anything else is looked at.
#[test]
fn rotten_copies_are_rewritten_from_their_mirror() {
    let d = MemDisk::new(64 * MIB);
    let devs: Vec<Arc<dyn BlockDevice>> = vec![d.clone()];
    mkfs::format_devices(&devs, &opts(), Profile::Dup, Profile::Dup).unwrap();
    let mut fs = EclipseFS::mount_devices(devs.clone(), false).unwrap();
    fs.set_clock(clock);
    sample(&mut fs);
    let want = contents(&mut fs);
    fs.sync().unwrap();
    let chunks: Vec<_> = fs
        .chunks()
        .into_iter()
        .filter(|c| c.kind == ChunkKind::Data && c.logical > 0)
        .collect();
    assert!(!chunks.is_empty());
    for c in &chunks {
        let (_, at) = c.stripes[1];
        for b in (at + 2 * B..at + c.size_bytes).step_by(BLOCK) {
            d.poke(b as usize + 100, 0x5A);
        }
    }
    let r = fs.repair().unwrap();
    assert!(r.scrub.repaired > 0, "{:?}", r);
    assert!(r.is_clean(), "{:?}", r);
    assert_eq!(fs.scrub().unwrap().repaired, 0);
    assert_eq!(contents(&mut fs), want);
}

/// Without a second copy, a rotten data block is reported and stays an
/// error to read, while the rest of the repair goes on.
#[test]
fn rotten_data_without_a_copy_is_reported() {
    let (disk, want, _) = setup();
    let f = ino_of(&disk, &["f3"]);
    let mut at = None;
    edit_fs(&disk, |items| {
        for (k, d) in items.iter() {
            if *k == Key::new(f, EXTENT_DATA, 0) {
                if let Ok(FileExtent::Reg { disk, .. }) = FileExtent::decode(d) {
                    at = Some(disk);
                }
            }
        }
        false
    });
    disk.poke((at.unwrap() * B) as usize + 10, 0xFF);
    let f0 = ino_of(&disk, &["f0"]);
    edit_inode(&disk, f0, |i| i.nlink = 5);
    let mut fs = open(&disk);
    let r = fs.repair().unwrap();
    assert_eq!(r.scrub.bad_data_blocks, vec![at.unwrap()]);
    assert_eq!(r.still_damaged, None, "{:?}", r);
    assert!(!r.is_clean());
    assert_eq!(fs.stat(f0).unwrap().nlink, 1);
    let mut buf = vec![0u8; BLOCK];
    assert!(fs.read(f, 0, &mut buf).is_err());
    let mut got = contents_except(&mut fs, "/f3");
    got.remove("/f3");
    let mut want = want;
    want.remove("/f3");
    assert_eq!(got, want);
}

fn contents_except(fs: &mut EclipseFS, skip: &str) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let root = fs.root_ino();
    for e in fs.readdir(root).unwrap() {
        if e.name == "." || e.name == ".." || format!("/{}", e.name) == skip {
            continue;
        }
        let p = format!("/{}", e.name);
        match e.kind {
            FileKind::Dir => tree(fs, e.ino, &p, &mut out),
            FileKind::Symlink => {
                out.insert(p, fs.read_link(e.ino).unwrap());
            }
            _ => {
                out.insert(p, read_all(fs, e.ino));
            }
        }
    }
    out
}

/// Damage a leaf's items at random: remove one, scramble an inode's
/// counters, flip a byte of a value, or move an item to the next object.
fn scramble(rng: &mut Lcg, items: &mut Vec<(Key, Vec<u8>)>) -> bool {
    if items.len() < 3 {
        return false;
    }
    for _ in 0..1 + rng.below(4) {
        let i = 1 + rng.below(items.len() as u64 - 1) as usize;
        match rng.below(4) {
            0 => {
                items.remove(i);
            }
            1 => {
                if items[i].0.ty == INODE_ITEM {
                    if let Ok(mut it) = InodeItem::decode(&items[i].1) {
                        it.nlink = rng.below(5) as u32;
                        it.nbytes = rng.below(1 << 20);
                        items[i].1 = it.encode();
                    }
                }
            }
            2 => {
                let v = &mut items[i].1;
                if !v.is_empty() {
                    let at = rng.below(v.len() as u64) as usize;
                    v[at] ^= 1 << rng.below(8);
                }
            }
            _ => {
                let k = items[i].0;
                let to = Key::new(k.objectid + 1, k.ty, k.offset);
                // Only where it still sorts inside this leaf.
                if !items.iter().any(|(k2, _)| *k2 == to) && items.iter().any(|(k2, _)| *k2 > to) {
                    items[i].0 = to;
                }
            }
        }
    }
    true
}

/// Repair whatever `damage` did: it always ends with a filesystem that
/// passes check, reads the same after a remount, and needs nothing on a
/// second run.
fn survives(seed: u64, disk: &Arc<MemDisk>) {
    let mut fs = open(disk);
    let r = fs
        .repair()
        .unwrap_or_else(|e| panic!("seed {seed}: {:?}", e));
    assert_eq!(r.still_damaged, None, "seed {seed}: {:?}", r);
    fs.check().unwrap();
    let before = contents(&mut fs);
    unmount(fs);
    let mut fs = open(disk);
    fs.check().unwrap();
    let again = fs.repair().unwrap();
    assert!(!again.changed(), "seed {seed}: {:?}", again);
    assert_eq!(contents(&mut fs), before, "seed {seed}");
}

/// Random damage to the items of the top subvolume.
#[test]
fn random_damage_always_ends_consistent() {
    for seed in 1..SEEDS {
        let (disk, _, _) = setup();
        let mut rng = Lcg(seed);
        edit_fs(&disk, |items| scramble(&mut rng, items));
        survives(seed, &disk);
    }
}

/// Random damage to every subvolume, snapshot and the checksum tree of a
/// filesystem with shared trees and reflinked extents.
#[test]
fn random_damage_with_snapshots_always_ends_consistent() {
    for seed in 1..SEEDS {
        let (disk, mut fs) = fresh(64 * MIB);
        sample(&mut fs);
        let root = fs.root_ino();
        let sv = fs.create_subvolume(root, "sv").unwrap();
        let g = fs.create(sv, "g", FileKind::Regular, 0o644, 0).unwrap();
        write_all(&mut fs, g, 0, &pattern(70_000, 3));
        let f3 = fs.lookup(root, "f3").unwrap();
        let c = fs
            .create(root, "clone", FileKind::Regular, 0o644, 0)
            .unwrap();
        fs.clone_range(f3, 0, c, 0, 300_000).unwrap();
        fs.snapshot(sv, root, "snap", true).unwrap();
        fs.snapshot(root, root, "top-snap", false).unwrap();
        write_all(&mut fs, g, 5000, &pattern(9000, 4));
        unmount(fs);
        let mut rng = Lcg(seed * 7919);
        edit(&disk, |owner, items| {
            let target = owner == FS_TREE || owner >= FIRST_SUBVOL || owner == CSUM_TREE;
            target && rng.below(2) == 0 && scramble(&mut rng, items)
        });
        survives(seed, &disk);
    }
}

const SEEDS: u64 = 150;

type Write = (u64, Vec<u8>);

/// Device that applies writes and logs them with the syncs between, so a
/// power cut can be replayed at any point of a repair.
struct Recorder {
    data: std::sync::Mutex<Vec<u8>>,
    /// Writes as (offset, bytes), `None` for a sync.
    log: std::sync::Mutex<Vec<Option<Write>>>,
}

impl BlockDevice for Recorder {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> eclipsefs::Result<()> {
        let d = self.data.lock().unwrap();
        let o = offset as usize;
        buf.copy_from_slice(d.get(o..o + buf.len()).ok_or(Error::Io)?);
        Ok(())
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> eclipsefs::Result<()> {
        let mut d = self.data.lock().unwrap();
        let o = offset as usize;
        d.get_mut(o..o + buf.len())
            .ok_or(Error::Io)?
            .copy_from_slice(buf);
        self.log.lock().unwrap().push(Some((offset, buf.to_vec())));
        Ok(())
    }
    fn sync(&self) -> eclipsefs::Result<()> {
        self.log.lock().unwrap().push(None);
        Ok(())
    }
    fn size(&self) -> u64 {
        self.data.lock().unwrap().len() as u64
    }
}

/// A power cut anywhere in a repair leaves a filesystem that mounts, and a
/// second repair ends where an uninterrupted one does.
#[test]
fn a_crash_during_repair_is_finished_by_the_next_one() {
    let (disk, _, root) = setup();
    let f2 = ino_of(&disk, &["f2"]);
    let f3 = ino_of(&disk, &["f3"]);
    let d = ino_of(&disk, &["d"]);
    add_fs_item(&disk, Key::new(f2, 99, 0), vec![0]);
    edit_fs(&disk, |items| {
        items.retain(|(k, _)| *k != Key::new(f3, INODE_ITEM, 0));
        items.retain(|(k, v)| {
            !(k.objectid == root && k.ty == DIR_INDEX && decode_pair(v).unwrap().ino == f2)
        });
        if let Some(v) = inode_of(items, d) {
            let mut i = InodeItem::decode(v).unwrap();
            i.nlink = 40;
            *v = i.encode();
        }
        true
    });
    let mut base = vec![0u8; disk.size() as usize];
    disk.read_at(0, &mut base).unwrap();
    // The uninterrupted repair, recorded.
    let rec = Arc::new(Recorder {
        data: std::sync::Mutex::new(base.clone()),
        log: std::sync::Mutex::new(Vec::new()),
    });
    let want = {
        let mut fs = EclipseFS::mount(rec.clone(), false).unwrap();
        fs.set_clock(clock);
        let r = fs.repair().unwrap();
        assert!(r.is_clean(), "{:?}", r);
        let c = contents(&mut fs);
        unmount(fs);
        c
    };
    let log = rec.log.lock().unwrap().clone();
    let mut image = base;
    let stride = (log.len() / 150).max(1);
    for (i, ev) in log.iter().enumerate() {
        if i % stride == 0 {
            let disk = MemDisk::new(image.len());
            disk.write_at(0, &image).unwrap();
            let mut fs = open(&disk);
            let r = fs.repair().unwrap();
            assert!(r.is_clean(), "cut at {i}: {:?}", r);
            assert_eq!(contents(&mut fs), want, "cut at {i}");
        }
        if let Some((off, data)) = ev {
            let o = *off as usize;
            image[o..o + data.len()].copy_from_slice(data);
        }
    }
}

// ---------------------------------------------------------------------------
// The fsync log, parity and tiers
// ---------------------------------------------------------------------------

/// The fsync log area (first block, blocks) in the root tree, if any.
fn log_area(disk: &MemDisk) -> Option<(u64, u64)> {
    let mut at = None;
    edit(disk, |_, items| {
        for (k, d) in items.iter() {
            if *k == Key::new(LOG_TREE, EXTENT_DATA, 0) {
                if let Ok(FileExtent::Prealloc { disk, blocks }) = FileExtent::decode(d) {
                    at = Some((disk, blocks));
                }
            }
        }
        false
    });
    at
}

/// A filesystem whose root tree holds an fsync log area.
fn with_log() -> (Arc<MemDisk>, u64, Vec<u8>) {
    let (disk, mut fs) = fresh(64 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    let data = pattern(20_000, 7);
    write_all(&mut fs, f, 0, &data);
    fs.fsync(f).unwrap();
    unmount(fs);
    assert!(log_area(&disk).is_some(), "no log area to test with");
    (disk, f, data)
}

#[test]
fn the_fsync_log_area_is_kept() {
    let (disk, f, data) = with_log();
    let area = log_area(&disk).unwrap();
    // Mark the log area free in group 0's bitmap (checksum kept in step).
    let s = sb(&disk);
    let mut bm = vec![0u8; BLOCK];
    disk.read_at(B, &mut bm).unwrap();
    for b in area.0..area.0 + area.1 {
        bm[(b / 8) as usize] &= !(1 << (b % 8));
    }
    disk.write_at(B, &bm).unwrap();
    let (table, _) = s.bitmap_sums.unwrap();
    let mut tb = vec![0u8; BLOCK];
    disk.read_at(table * B, &mut tb).unwrap();
    let mut sums = decode_sums_block(&tb).unwrap();
    sums[0] = eclipsefs::crc::crc32c(&bm);
    disk.write_at(table * B, &encode_sums_block(&sums)).unwrap();
    let (mut fs, r) = repair(&disk);
    assert_eq!(r.bitmap_blocks_fixed, area.1, "{:?}", r);
    assert_eq!(r.items_removed, 0, "{:?}", r);
    assert_eq!(read_all(&mut fs, f), data);
    // The area is where it was, and the log still works: new data does
    // not land on it and an fsync survives a power cut.
    let more = pattern(50_000, 8);
    write_all(&mut fs, f, 0, &more);
    fs.fsync(f).unwrap();
    let mut after = EclipseFS::mount(disk.copy(), false).unwrap();
    after.set_clock(clock);
    assert_eq!(read_all(&mut after, f), more);
    after.check().unwrap();
    unmount(fs);
    assert_eq!(log_area(&disk), Some(area));
}

#[test]
fn a_filesystem_with_an_fsync_log_needs_no_repair() {
    let (disk, mut fs) = fresh(64 * MIB);
    let root = fs.root_ino();
    let f = fs.create(root, "f", FileKind::Regular, 0o644, 0).unwrap();
    write_all(&mut fs, f, 0, &pattern(10_000, 1));
    fs.fsync(f).unwrap();
    // A record, not a commit.
    let want = pattern(30_000, 2);
    write_all(&mut fs, f, 0, &want);
    fs.fsync(f).unwrap();
    assert!(fs.fsync_log_records() > 0);
    // The crashed copy replays the log at mount.
    let mut after = EclipseFS::mount(disk.copy(), false).unwrap();
    after.set_clock(clock);
    let r = after.repair().unwrap();
    assert!(!r.changed(), "{:?}", r);
    assert!(r.is_clean(), "{:?}", r);
    assert_eq!(read_all(&mut after, f), want);
    // And the live one, log and all.
    let r = fs.repair().unwrap();
    assert!(!r.changed(), "{:?}", r);
    assert!(r.is_clean(), "{:?}", r);
    assert_eq!(read_all(&mut fs, f), want);
}

#[test]
fn a_log_area_on_metadata_is_reported_and_left_alone() {
    let (disk, _, _) = with_log();
    let area = log_area(&disk).unwrap();
    let node = sb(&disk).root.bytenr;
    edit(&disk, |_, items| {
        let mut hit = false;
        for (k, d) in items.iter_mut() {
            if *k == Key::new(LOG_TREE, EXTENT_DATA, 0) {
                *d = FileExtent::Prealloc {
                    disk: node,
                    blocks: area.1,
                }
                .encode();
                hit = true;
            }
        }
        hit
    });
    let mut fs = open(&disk);
    let r = fs.repair().unwrap();
    assert_eq!(
        r.still_damaged,
        Some("the fsync log area shares its blocks"),
        "{:?}",
        r
    );
    assert!(!r.is_clean());
    assert_eq!(r.bitmap_blocks_fixed, 0, "{:?}", r);
    assert_eq!(r.refcounts_fixed, 0, "{:?}", r);
}

fn volume(n: usize) -> Vec<Arc<MemDisk>> {
    (0..n).map(|_| MemDisk::new(64 * MIB)).collect()
}

fn vol_devs(d: &[Arc<MemDisk>]) -> Vec<Arc<dyn BlockDevice>> {
    d.iter()
        .map(|d| d.clone() as Arc<dyn BlockDevice>)
        .collect()
}

fn reopen_volume(d: &[Arc<MemDisk>], fs: EclipseFS) -> EclipseFS {
    unmount(fs);
    let mut fs = EclipseFS::mount_devices(vol_devs(d), false).unwrap();
    fs.set_clock(clock);
    fs
}

fn vol_files(fs: &mut EclipseFS, n: usize, seed: usize) -> Vec<(u64, Vec<u8>)> {
    let root = fs.root_ino();
    (0..n)
        .map(|i| {
            let f = fs
                .create(root, &format!("v{seed}-{i}"), FileKind::Regular, 0o644, 0)
                .unwrap();
            let data = pattern(150_000 + i * 999, seed * 10 + i);
            write_all(fs, f, 0, &data);
            (f, data)
        })
        .collect()
}

fn vol_same(fs: &mut EclipseFS, files: &[(u64, Vec<u8>)]) {
    for (f, data) in files {
        assert_eq!(&read_all(fs, *f), data);
    }
}

#[test]
fn parity_volumes_are_repaired_through_the_volume_layer() {
    for (n, p) in [(3, Profile::Raid5), (4, Profile::Raid6)] {
        let d = volume(n);
        mkfs::format_devices(&vol_devs(&d), &opts(), p, Profile::Raid1).unwrap();
        let mut fs = EclipseFS::mount_devices(vol_devs(&d), false).unwrap();
        fs.set_clock(clock);
        let files = vol_files(&mut fs, 6, 1);
        fs.sync().unwrap();
        // Clean: nothing to do.
        let r = fs.repair().unwrap();
        assert!(!r.changed() && r.is_clean(), "{p:?}: {r:?}");
        // Rot part of one device's share of the data chunks: repair's
        // scrub puts it back from parity.
        for c in fs
            .chunks()
            .into_iter()
            .filter(|c| c.kind == ChunkKind::Data && c.profile == p)
        {
            let (dev, at) = c.stripes[1];
            for b in (at + B..at + c.size_bytes / 4).step_by(BLOCK) {
                d[dev as usize - 1].poke(b as usize + 100, 0x5A);
            }
        }
        let mut fs = reopen_volume(&d, fs);
        let r = fs.repair().unwrap();
        assert!(r.scrub.repaired > 0, "{p:?}: {r:?}");
        assert!(r.is_clean(), "{p:?}: {r:?}");
        vol_same(&mut fs, &files);
        // New data after a repair still goes to fresh rows, and the volume
        // stays consistent.
        let more = vol_files(&mut fs, 4, 2);
        fs.sync().unwrap();
        let mut fs = reopen_volume(&d, fs);
        vol_same(&mut fs, &files);
        vol_same(&mut fs, &more);
        fs.check().unwrap();
        assert!(fs.scrub().unwrap().is_clean());
        let r = fs.repair().unwrap();
        assert!(!r.changed() && r.is_clean(), "{p:?}: {r:?}");
    }
}

#[test]
fn tiered_volumes_keep_their_placement() {
    let d = volume(2);
    let tiers = [Tier::Fast, Tier::Slow];
    mkfs::format_devices_tiered(
        &vol_devs(&d),
        &opts(),
        Profile::Single,
        Profile::Single,
        &tiers,
    )
    .unwrap();
    let mut fs = EclipseFS::mount_devices(vol_devs(&d), false).unwrap();
    fs.set_clock(clock);
    let files = vol_files(&mut fs, 4, 3);
    let r = fs.repair().unwrap();
    assert!(!r.changed() && r.is_clean(), "{r:?}");
    let mut fs = reopen_volume(&d, fs);
    let r = fs.repair().unwrap();
    assert!(!r.changed() && r.is_clean(), "{r:?}");
    let more = vol_files(&mut fs, 4, 4);
    fs.sync().unwrap();
    // Metadata stays on the fast device.
    for c in fs.chunks().iter().filter(|c| c.kind == ChunkKind::Metadata) {
        assert!(c.stripes.iter().all(|s| fs.device_tier(s.0) == Tier::Fast));
    }
    let mut fs = reopen_volume(&d, fs);
    vol_same(&mut fs, &files);
    vol_same(&mut fs, &more);
    fs.check().unwrap();
}
