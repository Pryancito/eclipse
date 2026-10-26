#![cfg(feature = "std")]
//! The B-tree engine's own contract, in the places an image walk does not
//! reach it.
//!
//! `tree.rs` had no tests, and a wide mutation lot explains why that was not
//! obvious: 24 mutations against the crate's suite and fifteen died with no
//! test in the file at all, because mounting and walking real images reads
//! whatever the descent produces. What lived through it is what no walk
//! happens to ask for: the greatest item at or below a key when the answer
//! lies in the previous leaf, the ends of a key range, and the item that
//! fills a leaf exactly.
//!
//! These therefore build a tree deep enough to have leaf boundaries in the
//! middle of the keys, and then ask the engine directly, because `Btrfs`
//! keeps its volume private and the questions are about the engine.
//!
//! The half of the contract that has to *insert* items -- the largest item a
//! leaf can hold, and how full a node gets before the tree grows a level --
//! cannot be asked from here: writing needs a loaded allocator, and the scan
//! that loads it is private to `Btrfs`. Those live in `fs.rs` next to the
//! mount that builds one.

use std::path::PathBuf;
use std::sync::Arc;

use btrfs::alloc_ext::FreeSpace;
use btrfs::device::{BlockDevice, FileDevice};
use btrfs::structs::{header, leaf, node, Key, FS_TREE, LAST_FREE_OBJECTID};
use btrfs::tree::{RootCache, Tree};
use btrfs::volume::Volume;
use btrfs::{mkfs, Btrfs, Error, FileKind};

/// Big enough for mkfs and for a few thousand directory entries.
const MKFS_SIZE: u64 = 48 * 1024 * 1024;

/// Long enough that each entry costs a few hundred bytes of metadata, so a
/// few hundred of them span several leaves.
const NAME_LEN: usize = 180;

/// Entries to create. Each one writes an INODE_ITEM, an INODE_REF, a DIR_ITEM
/// and a DIR_INDEX, the last two carrying the name, so this is comfortably
/// more than one leaf's worth at any node size this reader accepts.
const ENTRIES: u64 = 300;

fn opts() -> mkfs::MkfsOptions {
    mkfs::MkfsOptions {
        label: "btree".into(),
        fsid: [0x99; 16],
        chunk_uuid: [0xaa; 16],
        dev_uuid: [0xbb; 16],
        subvol_uuid: [0xcc; 16],
        now: (1_700_000_000, 0),
    }
}

fn tmpfile(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("btrfs-btree-{}-{}", std::process::id(), name))
}

fn open_dev(path: &PathBuf) -> Arc<dyn BlockDevice> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // Explicitly not truncating: every helper here reopens the same image.
        .truncate(false)
        .open(path)
        .unwrap();
    f.set_len(MKFS_SIZE).unwrap();
    Arc::new(FileDevice::open(f).unwrap())
}

/// A mounted volume plus the two pieces of state `Tree` borrows, so a test can
/// ask the engine a question without going through `Btrfs`.
struct Engine {
    vol: Volume,
    roots: RootCache,
    alloc: FreeSpace,
}

impl Engine {
    fn open(path: &PathBuf) -> Self {
        let vol = Volume::open(open_dev(path)).unwrap();
        let alloc = FreeSpace {
            nodesize: vol.nodesize as u64,
            sectorsize: vol.sectorsize as u64,
            ..Default::default()
        };
        Self {
            vol,
            roots: RootCache::new(),
            alloc,
        }
    }

    fn tree(&mut self) -> Tree<'_> {
        Tree {
            vol: &mut self.vol,
            roots: &mut self.roots,
            alloc: &mut self.alloc,
        }
    }

    /// Every key of `FS_TREE`, in order.
    fn all_keys(&mut self) -> Vec<Key> {
        let mut keys = Vec::new();
        self.tree()
            .iter_from(FS_TREE, Key::MIN, |k, _| {
                keys.push(*k);
                Ok(true)
            })
            .unwrap();
        keys
    }
}

/// Format an image and fill its root directory, so the fs tree is more than
/// one block deep. Returns the image path.
fn filled(name: &str) -> PathBuf {
    let path = tmpfile(name);
    let _ = std::fs::remove_file(&path);
    let dev = open_dev(&path);
    mkfs::format(&*dev, &opts()).unwrap();
    drop(dev);

    let mut fs = Btrfs::mount(open_dev(&path), false).unwrap();
    let root = fs.root_ino();
    for i in 0..ENTRIES {
        let name = format!("{}{:06}", "n".repeat(NAME_LEN), i);
        fs.create(root, &name, FileKind::Regular, 0o644, 0)
            .unwrap_or_else(|e| panic!("create #{} failed with {:?}", i, e));
    }
    fs.sync().unwrap();
    drop(fs);
    path
}

/// The greatest key strictly below `k`, or `None` when `k` is `Key::MIN`.
fn just_below(k: Key) -> Option<Key> {
    if k.offset > 0 {
        Some(Key::new(k.objectid, k.item_type, k.offset - 1))
    } else if k.item_type > 0 {
        Some(Key::new(k.objectid, k.item_type - 1, u64::MAX))
    } else if k.objectid > 0 {
        Some(Key::new(k.objectid - 1, u8::MAX, u64::MAX))
    } else {
        None
    }
}

/// Rewrite one tree block on the image: read it, let `edit` touch it,
/// recompute the checksum exactly as a forger would, and write it back to
/// every stripe. crc32c is not a MAC, so this is what a prepared disk looks
/// like to the reader.
fn craft(path: &PathBuf, logical: u64, edit: impl FnOnce(&mut [u8])) {
    let vol = Volume::open(open_dev(path)).unwrap();
    let mut raw = vec![0u8; vol.nodesize];
    vol.read_logical(logical, &mut raw).unwrap();
    edit(&mut raw);
    header::update_csum(&mut raw);
    assert!(
        header::csum_ok(&raw),
        "the crafted block has to pass the checksum, or it never reaches the walk"
    );
    vol.write_logical(logical, &raw).unwrap();
}

/// Check every separator of every node below `bytenr` against the first key
/// of the child it points at, counting the children checked. The recursion is
/// bounded by `level`, which drops by one per step.
fn separators_are_honest(e: &mut Engine, bytenr: u64, level: u8, checked: &mut usize) {
    if level == 0 {
        return;
    }
    let block = e.vol.read_block(bytenr).unwrap();
    let n = header::nritems(&block) as usize;
    let mut kids = Vec::with_capacity(n);
    for slot in 0..n {
        kids.push((node::key(&block, slot), node::blockptr(&block, slot)));
    }
    drop(block);
    for (sep, child) in kids {
        let child_block = e.vol.read_block(child).unwrap();
        let kids_n = header::nritems(&child_block) as usize;
        assert!(
            kids_n > 0,
            "block {} points at {}, which has no items at all",
            bytenr,
            child
        );
        let first = if header::level(&child_block) == 0 {
            leaf::key(&child_block, 0)
        } else {
            node::key(&child_block, 0)
        };
        drop(child_block);
        assert_eq!(
            sep, first,
            "block {} names its child {} by {:?}, but that child starts at {:?}",
            bytenr, child, sep, first
        );
        *checked += 1;
        separators_are_honest(e, child, level - 1, checked);
    }
}

#[test]
fn the_greatest_item_at_or_below_a_key_is_found_across_leaf_boundaries() {
    let path = filled("prev");
    let mut e = Engine::open(&path);

    let (_, level) = e.tree().root(FS_TREE).unwrap();
    assert!(
        level > 0,
        "{} entries did not make the fs tree deeper than one block, so this \
         test would not cross a leaf boundary and would prove nothing",
        ENTRIES
    );

    let keys = e.all_keys();
    assert!(
        keys.len() > 2,
        "an fs tree with {} entries has items",
        ENTRIES
    );

    // What `load_next_ino` asks at every mount: the greatest key in the tree.
    // Answered from the previous leaf whenever the bound falls past the last
    // one, which is exactly where the walk into the left sibling runs.
    let bound = Key::new(LAST_FREE_OBJECTID, u8::MAX, u64::MAX);
    let want = keys.iter().filter(|k| **k <= bound).max().copied();
    let got = e.tree().prev_item(FS_TREE, bound).unwrap().map(|(k, _)| k);
    assert_eq!(got, want, "the greatest key at or below {:?}", bound);

    // The case that matters is the FIRST key of a leaf. A search for anything
    // just below one of those lands at slot 0, with nothing to its left in
    // the block, so the answer is the last item of the leaf before -- reached
    // by walking up to an ancestor with a left sibling and descending its
    // rightmost edge. Nothing a mount or a file read does asks for that, so
    // the slots have to be looked up rather than guessed: `search` returns
    // the path, and its last element says which slot the key sits in.
    let mut heads = Vec::new();
    for (i, k) in keys.iter().enumerate().skip(1) {
        let (path, exact) = e.tree().search(FS_TREE, *k).unwrap();
        assert!(
            exact,
            "{:?} came out of this tree and has to be found in it",
            k
        );
        if path.last().unwrap().slot == 0 {
            heads.push(i);
        }
    }
    assert!(
        !heads.is_empty(),
        "no key but the first was at slot 0, so the fs tree is one leaf and \
         this test would prove nothing"
    );

    let mut probed = 0usize;
    for i in heads {
        let (lo, hi) = (keys[i - 1], keys[i]);
        let Some(mid) = just_below(hi) else { continue };
        if mid < lo {
            // Adjacent keys: there is no key between them to ask about.
            continue;
        }
        let got = e
            .tree()
            .prev_item(FS_TREE, mid)
            .unwrap()
            .map(|(k, _)| k)
            .unwrap_or_else(|| panic!("nothing at or below {:?}", mid));
        assert_eq!(
            got, lo,
            "at or below {:?}: {:?} is the last item of the leaf before the \
             one starting at {:?}",
            mid, lo, hi
        );
        probed += 1;
    }
    assert!(
        probed > 0,
        "every leaf started on a key adjacent to the one before it, so no \
         search landed at slot 0 inexactly"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_key_range_takes_both_its_ends_and_answers_with_the_first_item_in_it() {
    let path = filled("range");
    let mut e = Engine::open(&path);
    let keys = e.all_keys();
    assert!(
        keys.len() >= 3,
        "an fs tree with {} entries has items",
        ENTRIES
    );
    let (a, b) = (keys[0], keys[1]);

    // Both ends are inclusive. Every caller in this crate passes an upper end
    // of `u64::MAX`, so no item has ever sat exactly on it and nothing would
    // have noticed the end turning exclusive.
    let got = e
        .tree()
        .find_first_in_range(FS_TREE, a, a)
        .unwrap()
        .map(|(k, _)| k);
    assert_eq!(got, Some(a), "the range [{:?}, {:?}] holds that item", a, a);

    // With two items inside it, the answer is the FIRST, which is what the
    // name says and what a root lookup depends on. Every caller passes a range
    // holding one item at most, so that too has never been asked.
    let got = e
        .tree()
        .find_first_in_range(FS_TREE, a, b)
        .unwrap()
        .map(|(k, _)| k);
    assert_eq!(
        got,
        Some(a),
        "the range [{:?}, {:?}] holds both, and {:?} is the first",
        a,
        b,
        a
    );

    // And a range that falls in a hole between two items holds nothing.
    let hole = keys
        .windows(2)
        .find_map(|w| just_below(w[1]).filter(|m| *m > w[0]))
        .expect("two adjacent keys of this tree with a gap between them");
    assert!(
        e.tree()
            .find_first_in_range(FS_TREE, hole, hole)
            .unwrap()
            .is_none(),
        "{:?} is not in this tree, so the range [{:?}, {:?}] is empty",
        hole,
        hole,
        hole
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn every_separator_in_a_node_is_the_first_key_of_the_child_it_points_at() {
    // The invariant a descent rests on: the key a node stores for a child is
    // that child's own first key. `insert_rec` and `delete_rec` each refresh
    // it after the child changes, and deleting the first item of a leaf is the
    // case that needs it, because the leaf's first key moves and the parent is
    // the only place that still claims the old one.
    //
    // It is also why `prev_item` never walks into a left sibling: with honest
    // separators a search either lands on its key or past the last slot of the
    // leaf before it, never at slot 0 of a leaf that starts above the key.
    let path = filled("sep");
    {
        let mut fs = Btrfs::mount(open_dev(&path), false).unwrap();
        let root = fs.root_ino();
        // One entry in three. Each `unlink` removes four items spread across
        // the tree, so a good share of leaves lose the item they are named by.
        for i in (0..ENTRIES).step_by(3) {
            let name = format!("{}{:06}", "n".repeat(NAME_LEN), i);
            fs.unlink(root, &name)
                .unwrap_or_else(|e| panic!("unlink #{} failed with {:?}", i, e));
        }
        fs.sync().unwrap();
    }

    let mut e = Engine::open(&path);
    let (bytenr, level) = e.tree().root(FS_TREE).unwrap();
    assert!(
        level > 0,
        "the fs tree collapsed to a single block, so there is no separator \
         left to check and this test would prove nothing"
    );
    let mut checked = 0usize;
    separators_are_honest(&mut e, bytenr, level, &mut checked);
    assert!(
        checked > 1,
        "only {} child was reached, so the walk did not cross a node",
        checked
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_crafted_level_in_a_tree_block_is_refused_where_it_is_wrong() {
    let path = filled("level");
    let (bytenr, level) = {
        let mut e = Engine::open(&path);
        e.tree().root(FS_TREE).unwrap()
    };
    assert!(level > 0, "the fs tree root of a filled image is a node");

    craft(&path, bytenr, |b| header::set_level(b, level + 1));

    // The root item still records `level` and the block now claims one more.
    // A node and a leaf are read with different strides, so taking the
    // block's own word for it would read the slots of one as the other, and
    // the pointer that came out would take the walk off the tree. The message
    // is part of the statement: refused at the block where the two disagree,
    // not further down once a bogus address has already been followed.
    assert_eq!(
        Btrfs::mount(open_dev(&path), false).err(),
        Some(Error::Corrupt("tree level mismatch")),
        "mounting an image whose fs tree root claims a level its root item does not"
    );

    let _ = std::fs::remove_file(&path);
}
