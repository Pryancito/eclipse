//! The two sides of "there is no room": the blocks the bitmap hands out, and
//! the ceiling the format puts on one file.
//!
//! `alloc_block` writes the bitmap through to the device on every allocation,
//! which is what lets a kernel that never calls `sync` survive a reboot. What
//! it writes is one 4 KiB chunk of the bitmap, chosen by the block id, and
//! nothing checked that the chunk it picks is the one holding the bit -- or
//! that the last chunk gets written at all.

extern crate std;

use crate::*;
use rcore_fs::vfs::{FileSystem, FileType, Result};
use std::sync::{Arc, Mutex};

fn _new_sfs(space: usize) -> (Arc<dyn Device>, Arc<SimpleFileSystem>) {
    let file = tempfile::tempfile().expect("failed to create file");
    let device: Arc<dyn Device> = Arc::new(Mutex::new(file));
    let sfs = SimpleFileSystem::create(device.clone(), space).expect("failed to create SFS");
    (device, sfs)
}

/// Every block the filesystem reports free can really be handed out.
///
/// `alloc_block` takes the bit out of the bitmap first and only then looks at
/// the super block's counter, so the two have to agree about the last block:
/// a counter checked one too early keeps a block back for ever, and
/// `info()`'s `bfree` promises it is there. Growing one block at a time is
/// what makes the end of the device exact -- the growth that finally fails
/// asks for a single block, so its rollback gives nothing back.
#[test]
fn every_block_reported_free_can_be_allocated() -> Result<()> {
    // 64 blocks: small enough that filling it is cheap, large enough for
    // `SimpleFileSystem::create`'s `blocks >= 16`.
    let (_device, sfs) = _new_sfs(64 * BLKSIZE);
    let root = sfs.root_inode();
    let free_at_start = sfs.info().bfree;
    assert!(free_at_start > 0, "a fresh filesystem has free blocks");

    let file = root.create("big", FileType::File, 0o644)?;
    let mut blocks = 0usize;
    while file.resize((blocks + 1) * BLKSIZE).is_ok() {
        blocks += 1;
    }
    assert!(
        blocks > MAX_NBLOCK_DIRECT,
        "the file outgrew its direct blocks"
    );
    assert_eq!(
        sfs.info().bfree,
        0,
        "the filesystem stopped at {} blocks with free ones still on its own count",
        blocks
    );

    // And the count is the truth, not just zero: give one block back and the
    // filesystem takes it again.
    file.resize((blocks - 1) * BLKSIZE)?;
    assert_eq!(sfs.info().bfree, 1, "shrinking by one block frees one");
    file.resize(blocks * BLKSIZE)?;
    assert_eq!(sfs.info().bfree, 0, "and the last free block is usable");
    Ok(())
}

/// The chunk of the bitmap written to the device is the one holding the bit,
/// including when it is the last chunk.
///
/// A chunk is `BLKBITS` bits, so the index is `block_id / BLKBITS`; dividing
/// by `BLKSIZE` instead is off by eight for every id past the first 4096, and
/// an off-by-one `end > data.len()` check skips the final chunk altogether --
/// on a filesystem with a one-block bitmap, that is every write. Either way
/// the device keeps a bitmap that calls a block free while a file holds it,
/// and the next mount hands the same block out twice.
#[test]
fn the_bitmap_chunk_written_through_is_the_one_holding_the_bit() -> Result<()> {
    // One bitmap block (so the only chunk is also the last one) and more than
    // 4096 blocks, so that the two divisors part ways.
    let (device, sfs) = _new_sfs(4300 * BLKSIZE);
    assert_eq!(
        sfs.super_block.read().freemap_blocks,
        1,
        "this test wants a one-block bitmap"
    );
    let root = sfs.root_inode();

    let file = root.create("big", FileType::File, 0o644)?;
    file.resize(4100 * BLKSIZE)?;

    let free_map = sfs.free_map.read();
    let in_memory = free_map.as_buf();
    assert!(in_memory.len() >= BLKSIZE, "the bitmap is a block long");
    let highest_taken = (0..sfs.info().blocks)
        .filter(|&id| !free_map[id])
        .max()
        .expect("something is allocated");
    assert!(
        highest_taken >= BLKSIZE,
        "this test needs a block past the {}th, where `/ BLKBITS` and `/ BLKSIZE` part ways; the highest one taken is {}",
        BLKSIZE,
        highest_taken
    );

    // Nothing has been synced: what the device holds is what `alloc_block`
    // wrote through, one chunk at a time.
    let mut on_device = vec![0u8; BLKSIZE];
    let read = device
        .read_at(BLKSIZE * BLKN_FREEMAP, &mut on_device)
        .expect("failed to read the bitmap back");
    assert_eq!(read, BLKSIZE, "short read of the bitmap block");
    if let Some(byte) = (0..BLKSIZE).find(|&i| on_device[i] != in_memory[i]) {
        panic!(
            "the bitmap on the device disagrees with the one in memory at byte {}, which covers blocks {} to {}: {:#04x} there, {:#04x} here",
            byte,
            byte * 8,
            byte * 8 + 7,
            on_device[byte],
            in_memory[byte]
        );
    }
    Ok(())
}

/// A file as long as the format allows is turned away for want of space, not
/// rejected out of hand; one byte longer is rejected.
///
/// `MAX_FILE_SIZE` is what the block map can address, and the existing test
/// asks for `1 << 40`, which is far above it -- so it says nothing about
/// where the line is. A `>=` here would make the largest file the format
/// describes impossible to create on any device.
#[test]
fn a_file_of_exactly_the_largest_size_is_not_refused_as_invalid() -> Result<()> {
    let (_device, sfs) = _new_sfs(64 * BLKSIZE);
    let root = sfs.root_inode();
    let file = root.create("big", FileType::File, 0o644)?;

    assert_eq!(
        file.resize(MAX_FILE_SIZE).unwrap_err(),
        FsError::NoDeviceSpace,
        "the largest file the format allows is a question of room, not of validity"
    );
    assert_eq!(
        file.resize(MAX_FILE_SIZE + 1).unwrap_err(),
        FsError::InvalidParam,
        "one byte past the largest size is invalid"
    );
    // The failed growths gave everything back.
    assert_eq!(file.metadata()?.size, 0);
    assert_eq!(file.metadata()?.blocks, 0);
    Ok(())
}
