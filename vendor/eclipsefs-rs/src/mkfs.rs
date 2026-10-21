//! Create an empty EclipseFS on a block device.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::blocks::{AllocationGroup, BlockAllocator};
use crate::device::BlockDevice;
use crate::format::*;
use crate::journal::JournalArea;
use crate::v3::{extend_zeroed, write_chain, write_data, zero, ITABLE_GROW};
use crate::volume::{Profile, Tier};
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
    let jstart = alloc.allocate_run(Some(2), jblocks).ok_or(Error::NoSpace)?;
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
    format_devices_tiered(devs, opts, data, meta, &[])
}

/// [`format_devices`] with a [`Tier`] for each device (`tiers[i]` for
/// `devs[i]`; missing entries are untiered): metadata and new data go to
/// the fast devices first, see [`crate::volume`].
pub fn format_devices_tiered(
    devs: &[Arc<dyn BlockDevice>],
    opts: &MkfsOptions,
    data: Profile,
    meta: Profile,
    tiers: &[Tier],
) -> Result<()> {
    if tiers.len() > devs.len() {
        return Err(Error::Invalid);
    }
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
    let mut ids = vec![1u32];
    for d in &devs[1..] {
        ids.push(fs.add_device(d.clone())?);
    }
    for (&id, &t) in ids.iter().zip(tiers) {
        if t != Tier::Any {
            fs.set_device_tier(id, t)?;
        }
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

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::testdisk::{formatted, opts, Ram, MIB};
    use crate::EclipseFS;

    /// 1/256 of the disk within [64, 4096] blocks, but never more than a
    /// quarter of a tiny disk and never fewer than the journal needs.
    /// Monotonic, so a bigger disk never gets a smaller journal.
    #[test]
    fn the_journal_scales_with_the_disk_within_bounds() {
        assert_eq!(journal_blocks(64), 16);
        assert_eq!(journal_blocks(32), MIN_JOURNAL_BLOCKS as u64);
        assert_eq!(journal_blocks(256), 64);
        assert_eq!(journal_blocks(1 << 16), 256);
        assert_eq!(journal_blocks(1 << 20), 4096);
        assert_eq!(journal_blocks(1 << 30), 4096);
        let mut last = 0;
        for total in (64..200_000).step_by(997) {
            let j = journal_blocks(total);
            assert!(j >= last, "shrinks at {}", total);
            assert!(j >= MIN_JOURNAL_BLOCKS as u64);
            assert!(j <= 4096);
            last = j;
        }
    }

    #[test]
    fn a_label_must_fit_the_superblock() {
        let d = Ram::new(8 * MIB);
        let mut o = opts();
        o.label = "x".repeat(64);
        assert_eq!(format(&*d, &o), Err(Error::Invalid));
        o.label = "x".repeat(63);
        format(&*d, &o).unwrap();
        assert_eq!(EclipseFS::mount(d, true).unwrap().label(), o.label);
    }

    #[test]
    fn a_disk_smaller_than_one_group_is_refused() {
        let d = Ram::new((MIN_GROUP_BLOCKS as usize - 1) * BLOCK_SIZE as usize);
        assert_eq!(format(&*d, &opts()), Err(Error::NoSpace));
        assert_eq!(format_v3(&*d, &opts()), Err(Error::NoSpace));
    }

    /// The smallest disk mkfs takes still gives a filesystem that mounts and
    /// passes fsck.
    #[test]
    fn the_smallest_disk_still_works() {
        for blocks in [MIN_GROUP_BLOCKS * 4, 1024] {
            let d = Ram::new((blocks * BLOCK_SIZE) as usize);
            format(&*d, &opts()).unwrap();
            let mut fs = EclipseFS::mount(d, false).unwrap();
            fs.check().unwrap();
        }
    }

    /// A btrfs that was on the disk before must not be found by `blkid`
    /// after mkfs: both of its superblocks are wiped.
    #[test]
    fn stale_btrfs_superblocks_are_wiped() {
        let d = Ram::new(128 * MIB);
        for at in STALE_SIGNATURES {
            d.write_at(at + 0x40, b"_BHRfS_M").unwrap();
        }
        format(&*d, &opts()).unwrap();
        for at in STALE_SIGNATURES {
            assert_ne!(d.peek(at as usize + 0x40, 8), b"_BHRfS_M");
        }
    }

    /// A tail too short to be a group is left out rather than made into a
    /// group the allocator cannot use.
    #[test]
    fn a_short_tail_is_left_out() {
        let blocks = GROUP_BLOCKS + MIN_GROUP_BLOCKS - 1;
        let d = Ram::new((blocks * BLOCK_SIZE) as usize);
        format(&*d, &opts()).unwrap();
        let p = crate::fs::probe(&*d).unwrap();
        assert_eq!(p.min_device_bytes, GROUP_BLOCKS * BLOCK_SIZE);
    }

    #[test]
    fn both_formats_pass_fsck_when_fresh() {
        let mut fs = EclipseFS::mount(formatted(64), false).unwrap();
        let r = fs.check().unwrap();
        assert_eq!(r.directories, 1);
        let d = Ram::new(64 * MIB);
        format_v3(&*d, &opts()).unwrap();
        EclipseFS::mount(d, false).unwrap().check().unwrap();
    }

    /// Formatting twice gives the same bytes: mkfs depends on its options
    /// only, so images built by `xtask` are reproducible.
    #[test]
    fn mkfs_is_deterministic() {
        let a = formatted(16);
        let b = formatted(16);
        assert!(a.peek(0, 16 * MIB) == b.peek(0, 16 * MIB));
    }

    #[test]
    fn a_volume_needs_devices_enough_for_its_profiles() {
        let one: Vec<Arc<dyn BlockDevice>> = vec![Ram::new(64 * MIB)];
        assert!(matches!(
            format_devices(&one, &opts(), Profile::Raid1, Profile::Raid1),
            Err(Error::Unsupported(_))
        ));
        assert_eq!(
            format_devices(&[], &opts(), Profile::Single, Profile::Single),
            Err(Error::Invalid)
        );
        assert_eq!(
            format_devices_tiered(
                &one,
                &opts(),
                Profile::Single,
                Profile::Single,
                &[Tier::Fast, Tier::Slow]
            ),
            Err(Error::Invalid)
        );
    }

    #[test]
    fn prefix_caps_the_size_and_passes_the_rest_through() {
        let d = Ram::new(MIB);
        let p = Prefix(d.clone(), 4096);
        assert_eq!(p.size(), 4096);
        assert_eq!(Prefix(d.clone(), 2 * MIB as u64).size(), MIB as u64);
        p.write_at(10, b"abc").unwrap();
        assert_eq!(d.peek(10, 3), b"abc");
        let mut b = [0u8; 3];
        p.read_at(10, &mut b).unwrap();
        assert_eq!(&b, b"abc");
    }

    /// v0.3 on a disk of several groups: every group after the first gets a
    /// backup superblock.
    #[test]
    fn v3_writes_backup_superblocks_in_every_group() {
        let d = Ram::new(3 * (GROUP_BLOCKS * BLOCK_SIZE) as usize);
        format_v3(&*d, &opts()).unwrap();
        let primary = d.peek(0, 9);
        for g in 1..3 {
            let at = (g * GROUP_BLOCKS * BLOCK_SIZE) as usize;
            assert_eq!(d.peek(at, 9), primary, "group {}", g);
        }
        EclipseFS::mount(d, false).unwrap().check().unwrap();
    }

    /// Devices left untiered in a tiered volume stay `Any`.
    #[test]
    fn a_tiered_volume_may_leave_devices_untiered() {
        let devs: Vec<Arc<dyn BlockDevice>> = vec![Ram::new(64 * MIB), Ram::new(64 * MIB)];
        format_devices_tiered(
            &devs,
            &opts(),
            Profile::Single,
            Profile::Single,
            &[Tier::Any, Tier::Fast],
        )
        .unwrap();
        let fs = EclipseFS::mount_devices(devs, false).unwrap();
        assert_eq!(fs.device_tier(1), Tier::Any);
        assert_eq!(fs.device_tier(2), Tier::Fast);
    }
}
