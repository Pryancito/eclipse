//! Create an empty EclipseFS on a block device.

use alloc::string::String;
use alloc::vec::Vec;

use crate::blocks::{AllocationGroup, BlockAllocator};
use crate::device::BlockDevice;
use crate::format::*;
use crate::fs::{extend_zeroed, write_chain, write_data, zero, ITABLE_GROW};
use crate::{Error, Result};

/// Same fields as `btrfs::mkfs::MkfsOptions`, so callers can switch crates
/// without changing how they build it. EclipseFS has a single identifier:
/// `fsid` becomes the filesystem UUID and the other three are ignored.
#[derive(Debug, Clone)]
pub struct MkfsOptions {
    pub label: String,
    pub fsid: [u8; 16],
    pub chunk_uuid: [u8; 16],
    pub dev_uuid: [u8; 16],
    pub subvol_uuid: [u8; 16],
    /// Creation time (seconds, nanoseconds) for the root directory.
    pub now: (u64, u32),
}

/// Signatures of other filesystems that would make a probe mistake the
/// formatted device for them: the btrfs superblock and its first mirror.
const STALE_SIGNATURES: [u64; 2] = [0x1_0000, 0x400_0000];

/// Format the whole device.
pub fn format(dev: &dyn BlockDevice, opts: &MkfsOptions) -> Result<()> {
    if opts.label.len() >= 64 {
        return Err(Error::Invalid);
    }
    let mut total = dev.size() / BLOCK_SIZE;
    if total < MIN_GROUP_BLOCKS {
        return Err(Error::NoSpace);
    }
    let mut groups = Vec::new();
    let mut start = 0;
    while start < total {
        let count = (total - start).min(GROUP_BLOCKS);
        if count < MIN_GROUP_BLOCKS && start > 0 {
            total = start;
            break;
        }
        groups.push(AllocationGroup::new(groups.len() as u32, count));
        start += count;
    }

    for at in STALE_SIGNATURES {
        if at + BLOCK_SIZE <= total * BLOCK_SIZE {
            zero(dev, at, BLOCK_SIZE)?;
        }
    }

    let mut alloc = BlockAllocator::new(groups);
    let mut itable = Inode::new(FileKind::Regular, 0o600, opts.now);
    extend_zeroed(dev, &mut alloc, &mut itable, ITABLE_GROW)?;

    let mut root = Inode::new(FileKind::Dir, 0o755, opts.now);
    root.nlink = 2;
    root.parent = ROOT_INO;
    let rec = root.encode();
    if write_data(
        dev,
        &mut alloc,
        &mut itable,
        ROOT_INO * INODE_SIZE as u64,
        &rec,
    )? < INODE_SIZE
    {
        return Err(Error::NoSpace);
    }
    if itable.extents_dirty {
        write_chain(dev, &mut alloc, ITABLE_INO, &mut itable)?;
    }

    let mut label = [0u8; 64];
    label[..opts.label.len()].copy_from_slice(opts.label.as_bytes());
    let sb = Superblock {
        version: VERSION,
        block_size: BLOCK_SIZE as u32,
        group_blocks: GROUP_BLOCKS as u32,
        total_blocks: total,
        free_blocks: alloc.free_blocks(),
        next_ino: ROOT_INO + 1,
        inode_count: 1,
        root_ino: ROOT_INO,
        generation: 1,
        created: opts.now.0,
        flags: 0,
        inode_size: INODE_SIZE as u32,
        uuid: opts.fsid,
        label,
        itable: itable.encode(),
    };
    for g in alloc.groups() {
        dev.write_at(g.bitmap_block() * BLOCK_SIZE, &g.to_bytes())?;
    }
    // Backups first, the primary last: until it lands the device is not
    // EclipseFS at all.
    let enc = sb.encode();
    for g in alloc.groups().iter().skip(1) {
        dev.write_at(g.start_block * BLOCK_SIZE, &enc)?;
    }
    dev.write_at(0, &enc)?;
    dev.sync()
}
