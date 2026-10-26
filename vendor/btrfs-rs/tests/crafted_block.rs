#![cfg(feature = "std")]
//! A hand-crafted image: what a tree block's header claims about its own
//! contents, against the block it arrived in.
//!
//! `nritems`, and in a leaf each item's `(data_off, data_size)`, come off the
//! disk and decide a walk: every `0..nritems` in `tree.rs` and `volume.rs`
//! indexes into the block buffer with them. Random corruption does not reach
//! that code -- `Volume::read_block` checks the crc32c first -- but crc32c is
//! not a MAC, and the Eclipse installer mounts btrfs filesystems it did not
//! create. So a prepared USB stick does reach it, and a panic inside a kernel
//! is not a mount error.
//!
//! These tests therefore work on a real image: format one, then rewrite a
//! single header field and recompute the checksum exactly as a forger would,
//! and require that mounting it comes back as an error.

use std::path::PathBuf;
use std::sync::Arc;

use btrfs::device::{BlockDevice, FileDevice};
use btrfs::structs::{header, leaf, node, CHUNK_ITEM_KEY, HEADER_SIZE, ITEM_SIZE, KEY_SIZE};
use btrfs::volume::Volume;
use btrfs::{mkfs, Btrfs, Error};

/// Big enough for mkfs (1 MiB reserved + 4 MiB system + 8 MiB metadata +
/// a data chunk).
const MKFS_SIZE: u64 = 24 * 1024 * 1024;

fn opts() -> mkfs::MkfsOptions {
    mkfs::MkfsOptions {
        label: "crafted".into(),
        fsid: [0x55; 16],
        chunk_uuid: [0x66; 16],
        dev_uuid: [0x77; 16],
        subvol_uuid: [0x88; 16],
        now: (1_700_000_000, 0),
    }
}

fn tmpfile(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("btrfs-crafted-{}-{}", std::process::id(), name))
}

fn open_dev(path: &PathBuf) -> Arc<dyn BlockDevice> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // Explicitly not truncating: `craft` and `chunk_root` reopen the same
        // image, and wiping it there would leave nothing to craft.
        .truncate(false)
        .open(path)
        .unwrap();
    f.set_len(MKFS_SIZE).unwrap();
    Arc::new(FileDevice::open(f).unwrap())
}

/// Format a fresh image and check it mounts, so a later refusal is the
/// crafting and not the image.
fn fresh(name: &str) -> PathBuf {
    let path = tmpfile(name);
    let _ = std::fs::remove_file(&path);
    let dev = open_dev(&path);
    mkfs::format(&*dev, &opts()).unwrap();
    drop(dev);
    let dev = open_dev(&path);
    let mut fs = Btrfs::mount(dev, false).unwrap();
    // Reading the root directory walks a tree, so the bound has already run
    // on a block that is not crafted: a later refusal is the crafting.
    fs.readdir(fs.root_ino()).unwrap();
    path
}

/// Which block the chunk tree starts at, and what the header says about it.
/// The chunk root is the first tree block `Volume::open` reads, so crafting
/// it is the outermost case there is: the mount fails before a single file is
/// looked up.
fn chunk_root(path: &PathBuf) -> (u64, usize, u8, u32) {
    let vol = Volume::open(open_dev(path)).unwrap();
    let logical = vol.sb.chunk_root();
    let nodesize = vol.nodesize;
    let mut raw = vec![0u8; nodesize];
    vol.read_logical(logical, &mut raw).unwrap();
    (
        logical,
        nodesize,
        header::level(&raw),
        header::nritems(&raw),
    )
}

/// Read a tree block, hand it to `edit`, recompute its checksum and write it
/// back to every stripe -- the whole forgery, in the one place that knows how
/// to address a tree block. Returns the block as it reads back off the image
/// afterwards, because once the chunk root is crafted `Volume::open` refuses
/// it and there is no second chance to translate that address.
fn craft(path: &PathBuf, logical: u64, edit: impl FnOnce(&mut [u8])) -> Vec<u8> {
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
    // Straight back off the device, so what the next test looks at is what is
    // on the image and not what this function had in hand.
    let mut back = vec![0u8; vol.nodesize];
    vol.read_logical(logical, &mut back).unwrap();
    back
}

/// Mount the image at `path` and return what came back.
fn mount(path: &PathBuf) -> Result<Btrfs, Error> {
    Btrfs::mount(open_dev(path), false)
}

#[test]
fn a_crafted_item_count_in_the_chunk_root_is_a_mount_error_not_a_panic() {
    let path = fresh("nritems");
    let (logical, nodesize, level, _) = chunk_root(&path);
    assert_eq!(
        level, 0,
        "a fresh image of this size has a one-block chunk tree"
    );

    // Larger than the item table of any block size this reader accepts, and
    // still a plausible u32: a forger picks the number, not the format.
    let crafted = 60_000u32;
    assert!(
        (crafted as usize) * ITEM_SIZE > nodesize - HEADER_SIZE,
        "the crafted count has to be past the table to be the case under test"
    );
    craft(&path, logical, |b| header::set_nritems(b, crafted));

    match mount(&path) {
        Err(Error::Corrupt(_)) => {}
        Err(e) => panic!(
            "mounting the crafted image failed with {:?}, not Corrupt",
            e
        ),
        Ok(_) => panic!(
            "mounted an image whose chunk root claims {} items in room for {}",
            crafted,
            (nodesize - HEADER_SIZE) / ITEM_SIZE
        ),
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_crafted_item_extent_in_the_chunk_root_is_a_mount_error_not_a_panic() {
    let path = fresh("extent");
    let (logical, nodesize, level, nritems) = chunk_root(&path);
    assert_eq!(
        level, 0,
        "a fresh image of this size has a one-block chunk tree"
    );
    assert!(nritems > 0, "the chunk root of a fresh image has items");

    // This one leaves `nritems` alone: the table is honest and only one
    // item's data extent is a lie, which is the other half of the bound. It
    // has to be an item the walk actually slices, so a chunk item: a lie on a
    // slot nothing reads would be caught by the bound and by nothing else,
    // which would not say what this test means to say.
    let mut crafted_slot = None;
    craft(&path, logical, |b| {
        for slot in 0..nritems as usize {
            if leaf::key(b, slot).item_type == CHUNK_ITEM_KEY {
                leaf::set_data_size(b, slot, nodesize);
                crafted_slot = Some(slot);
                return;
            }
        }
    });
    assert!(
        crafted_slot.is_some(),
        "the chunk root of a fresh image has to hold at least one chunk item"
    );

    match mount(&path) {
        Err(Error::Corrupt(_)) => {}
        Err(e) => panic!(
            "mounting the crafted image failed with {:?}, not Corrupt",
            e
        ),
        Ok(_) => panic!(
            "mounted an image whose chunk item at slot {:?} claims a whole block of data",
            crafted_slot
        ),
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn that_same_crafted_image_is_what_would_walk_off_the_buffer() {
    // The two tests above would also pass if `read_block` refused the block
    // for some unrelated reason. This is the other side of the statement: the
    // bytes now on that image are the ones that take the walk out of the
    // buffer, read back off the disk and handed to the accessor the walk uses.
    let path = fresh("lethal");
    let (logical, _nodesize, _level, _nritems) = chunk_root(&path);
    let crafted = 60_000u32;
    let raw = craft(&path, logical, |b| header::set_nritems(b, crafted));
    assert_eq!(
        header::nritems(&raw),
        crafted,
        "the crafted count has to survive the round trip through the image"
    );
    let last = crafted as usize - 1;
    assert!(
        HEADER_SIZE + last * ITEM_SIZE + KEY_SIZE > raw.len(),
        "the last slot of the crafted table has to be past the end of the block"
    );

    // `load_chunk_node` does exactly this for every slot in `0..nritems`.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let walked = std::panic::catch_unwind(|| leaf::key(&raw, last));
    std::panic::set_hook(hook);
    assert!(
        walked.is_err(),
        "the crafted block was supposed to index out of the buffer, and did not"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_chunk_tree_that_points_back_at_itself_is_a_mount_error_not_a_dead_stack() {
    // The other field of the same header that decides a walk. `load_chunk_node`
    // read each block's own level and recursed on its children while that
    // level was above zero, so a node whose block pointer names itself
    // recursed for ever -- and because `read_block` caches, not even slowly.
    // A stack that runs out inside a kernel is worse than a panic: there is no
    // message. The bound is a ceiling that drops by one on the way down, so
    // the level has to strictly decrease and a cycle cannot be built; the walk
    // still reads each block at the level that block itself claims, which is
    // the level `check_block_bounds` measured its item count against, so the
    // two can never disagree about which table this block holds.
    let path = fresh("cycle");
    let (logical, _nodesize, level, _nritems) = chunk_root(&path);
    assert_eq!(level, 0, "the chunk root of a fresh image is a leaf");

    craft(&path, logical, |b| {
        // One child, which is this very block, still claiming to be a node.
        header::set_level(b, 1);
        header::set_nritems(b, 1);
        node::set_blockptr(b, 0, logical);
    });

    match mount(&path) {
        Err(Error::Corrupt(_)) => {}
        Err(e) => panic!(
            "mounting the crafted image failed with {:?}, not Corrupt",
            e
        ),
        Ok(_) => panic!("mounted an image whose chunk tree is a one-block cycle"),
    }
    let _ = std::fs::remove_file(&path);
}
