//! Create an empty EclipseFS on a block device.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::blocks::{AllocationGroup, BlockAllocator};
use crate::device::BlockDevice;
use crate::format::*;
use crate::journal::JournalArea;
use crate::v3::{extend_zeroed, write_chain, write_data, zero, ITABLE_GROW};
use crate::volume::Profile;
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

/// Journal size for a filesystem of `total` blocks: 1/256 of it (16 MiB per
/// 4 GiB), at least 64 blocks and at most 4096, and never more than a
/// quarter of a tiny device.
pub fn journal_blocks(total: u64) -> u64 {
    (total / 256)
        .clamp(64, 4096)
        .min(total / 4)
        .max(MIN_JOURNAL_BLOCKS as u64)
}

/// Common to both formats: allocation groups over the whole device, stale
/// signatures wiped, and an empty journal right after group 0's bitmap.
fn layout(dev: &dyn BlockDevice, opts: &MkfsOptions) -> Result<(u64, BlockAllocator, u64, u64)> {
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
    // The journal sits right after group 0's bitmap, in one piece.
    let jblocks = journal_blocks(total);
    let jstart = match alloc.allocate(Some(2), jblocks) {
        Some((s, n)) if n == jblocks => s,
        _ => return Err(Error::NoSpace),
    };
    JournalArea {
        start: jstart,
        blocks: jblocks,
    }
    .invalidate(dev)?;
    Ok((total, alloc, jstart, jblocks))
}

fn label_bytes(opts: &MkfsOptions) -> [u8; 64] {
    let mut label = [0u8; 64];
    label[..opts.label.len()].copy_from_slice(opts.label.as_bytes());
    label
}

/// Format the whole device (copy-on-write v0.4).
pub fn format(dev: &dyn BlockDevice, opts: &MkfsOptions) -> Result<()> {
    let (total, alloc, jstart, jblocks) = layout(dev, opts)?;
    crate::cow::format(
        dev,
        alloc,
        total,
        JournalArea {
            start: jstart,
            blocks: jblocks,
        },
        label_bytes(opts),
        opts.fsid,
        opts.now,
    )
}

/// Format the whole device with the older v0.3 layout (in-place updates
/// under a metadata journal). Kept for compatibility tests and tools.
/// Make a filesystem over several devices (`mkfs.btrfs -d PROFILE -m
/// PROFILE dev...`): group 0 (journal, chunk map, first trees) on the
/// first device, then every device added and group 0 converted to the
/// metadata profile; data chunks are made as data comes.
pub fn format_devices(
    devs: &[Arc<dyn BlockDevice>],
    opts: &MkfsOptions,
    data: Profile,
    meta: Profile,
) -> Result<()> {
    let first = devs.first().ok_or(Error::Invalid)?;
    if data.min_devices() > devs.len() || meta.min_devices() > devs.len() {
        return Err(Error::Unsupported("not enough devices for that profile"));
    }
    // Group 0 holds metadata only: a slice of the first device.
    let size = first.size() / BLOCK_SIZE;
    let sys = (size / 16).clamp(1024, GROUP_BLOCKS).min(size);
    format(&Prefix(first.clone(), sys * BLOCK_SIZE), opts)?;
    let mut fs = crate::cow::CowFs::mount(first.clone(), false)?;
    fs.make_volume_metadata()?;
    for d in &devs[1..] {
        fs.add_device(d.clone())?;
    }
    fs.balance(Some(data), Some(meta))?;
    fs.sync()
}

/// The first `.1` bytes of a device.
struct Prefix(Arc<dyn BlockDevice>, u64);

impl BlockDevice for Prefix {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.0.read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        self.0.write_at(offset, buf)
    }
    fn sync(&self) -> Result<()> {
        self.0.sync()
    }
    fn size(&self) -> u64 {
        self.1.min(self.0.size())
    }
}

pub fn format_v3(dev: &dyn BlockDevice, opts: &MkfsOptions) -> Result<()> {
    let (total, mut alloc, jstart, jblocks) = layout(dev, opts)?;
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

    let label = label_bytes(opts);
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
        flags: SB_FLAG_JOURNAL,
        inode_size: INODE_SIZE as u32,
        uuid: opts.fsid,
        label,
        itable: itable.encode(),
        journal_start: jstart,
        journal_blocks: jblocks as u32,
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
