//! On-disk records of the copy-on-write format (v0.4).
//!
//! Superblock (block 0, backups at block 0 of every other group):
//!
//! ```text
//! 0   "ECLIPSEFS"       12  version 0x0004_0000   16  block size
//! 20  group blocks      24  total blocks          32  free blocks
//! 40  generation        48  created (s)           56  feature flags
//! 64  uuid [16]         80  label [64]
//! 144 root tree block   152 root tree level       160 root tree generation
//! 168 journal start     176 journal blocks        180 checksum type
//! 184 default subvolume
//! 508 crc32c of bytes 0..508
//! ```
//!
//! Trees and item keys `(objectid, type, offset)`:
//!
//! - root tree (1): `(tree id, ROOT_ITEM, 0)` for every other tree;
//! - reference tree (2): `(first block, REF_ITEM, 0)` -> length, count, for
//!   blocks shared by more than one owner;
//! - one tree per subvolume (the default one is 5), with per inode:
//!   `(ino, INODE_ITEM, 0)`, `(ino, INODE_REF, hash(dir, index))` per name,
//!   `(ino, EXTENT_DATA, file offset)` per extent, and per directory
//!   `(dir, DIR_ITEM, hash(name))` for lookups and `(dir, DIR_INDEX, n)` for
//!   readdir in creation order;
//! - checksum tree (7): `(first block of a bucket, CSUM_ITEM, 0)` -> the
//!   CRC32C of every data block in use among the bucket's
//!   [`CSUM_BUCKET`] blocks (a presence bitmap, then the checksums).

use alloc::vec;
use alloc::vec::Vec;

use crate::btree::{Key, Root};
pub use crate::compress::{Compression, ZCHUNK};
use crate::crc::{crc32, crc32c};
use crate::format::{
    le16, le32, le64, put32, put64, FileKind, BLOCK_SIZE, GROUP_BLOCKS, MAGIC, MIN_GROUP_BLOCKS,
    MIN_JOURNAL_BLOCKS,
};
use crate::{Error, Result};

pub const VERSION4: u32 = 0x0004_0000;
pub const CSUM_CRC32C: u8 = 1;
const SB_CRC: usize = 508;

pub const ROOT_TREE: u64 = 1;
pub const REFS_TREE: u64 = 2;
/// The default subvolume (same number as btrfs's FS_TREE).
pub const FS_TREE: u64 = 5;
/// Checksums of file data, by disk block (btrfs's CSUM_TREE number).
pub const CSUM_TREE: u64 = 7;
/// First inode number of every subvolume: its root directory.
pub const FIRST_INO: u64 = 256;
/// Subvolumes and snapshots created later get ids from here on.
pub const FIRST_SUBVOL: u64 = 256;
/// Largest subvolume id (it has to fit in the top bits of a handle).
pub const MAX_SUBVOL: u64 = (1 << (64 - SUBVOL_SHIFT)) - 1;
/// Inode handles carry the subvolume above this bit: `id << 40 | ino`, and
/// plain `ino` for the top subvolume (5), so its handles are its inode
/// numbers.
pub const SUBVOL_SHIFT: u32 = 40;
pub const INO_MASK: u64 = (1 << SUBVOL_SHIFT) - 1;

/// The handle of inode `ino` of subvolume `subvol`.
pub fn handle(subvol: u64, ino: u64) -> u64 {
    if subvol == FS_TREE {
        ino
    } else {
        (subvol << SUBVOL_SHIFT) | ino
    }
}

/// (subvolume, inode) of a handle.
pub fn split(h: u64) -> (u64, u64) {
    match h >> SUBVOL_SHIFT {
        0 => (FS_TREE, h),
        sv => (sv, h & INO_MASK),
    }
}

/// Whether a directory entry's target is another subvolume (a handle, not
/// a local inode number).
pub fn is_link(ino: u64) -> bool {
    ino > INO_MASK
}

pub const INODE_ITEM: u8 = 1;
pub const INODE_REF: u8 = 12;
pub const XATTR_ITEM: u8 = 24;
pub const DIR_ITEM: u8 = 84;
pub const DIR_INDEX: u8 = 96;
pub const EXTENT_DATA: u8 = 108;
pub const CSUM_ITEM: u8 = 128;
pub const ROOT_ITEM: u8 = 132;
/// `(subvolume, ROOT_BACKREF, parent subvolume)` in the root tree -> the
/// directory entry naming it there (directory, index, name).
pub const ROOT_BACKREF: u8 = 144;
pub const REF_ITEM: u8 = 200;

/// Superblock feature: quotas are on, every subvolume's `referenced`
/// counter is kept (and checked by fsck).
pub const FEAT_QUOTA: u64 = 1;

/// Largest file kept inside its extent item.
pub const INLINE_MAX: usize = 1024;
/// Longest extent item (128 MiB).
pub const MAX_EXTENT_BLOCKS: u64 = GROUP_BLOCKS;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuperV4 {
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub generation: u64,
    pub created: u64,
    pub features: u64,
    pub uuid: [u8; 16],
    pub label: [u8; 64],
    pub root: Root,
    pub journal_start: u64,
    pub journal_blocks: u32,
    pub default_subvol: u64,
}

impl SuperV4 {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = vec![0u8; BLOCK_SIZE as usize];
        b[0..9].copy_from_slice(MAGIC);
        put32(&mut b, 12, VERSION4);
        put32(&mut b, 16, BLOCK_SIZE as u32);
        put32(&mut b, 20, GROUP_BLOCKS as u32);
        put64(&mut b, 24, self.total_blocks);
        put64(&mut b, 32, self.free_blocks);
        put64(&mut b, 40, self.generation);
        put64(&mut b, 48, self.created);
        put64(&mut b, 56, self.features);
        b[64..80].copy_from_slice(&self.uuid);
        b[80..144].copy_from_slice(&self.label);
        put64(&mut b, 144, self.root.bytenr);
        b[152] = self.root.level;
        put64(&mut b, 160, self.root.gen);
        put64(&mut b, 168, self.journal_start);
        put32(&mut b, 176, self.journal_blocks);
        b[180] = CSUM_CRC32C;
        put64(&mut b, 184, self.default_subvol);
        let c = crc32c(&b[..SB_CRC]);
        put32(&mut b, SB_CRC, c);
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() < BLOCK_SIZE as usize || &b[0..9] != MAGIC {
            return Err(Error::BadSuperblock);
        }
        if le32(b, 12) >> 16 != VERSION4 >> 16 {
            return Err(Error::Unsupported("EclipseFS format version"));
        }
        if crc32c(&b[..SB_CRC]) != le32(b, SB_CRC) {
            return Err(Error::Corrupt("superblock checksum"));
        }
        if le32(b, 16) as u64 != BLOCK_SIZE || le32(b, 20) as u64 != GROUP_BLOCKS {
            return Err(Error::Unsupported("EclipseFS geometry"));
        }
        if b[180] != CSUM_CRC32C {
            return Err(Error::Unsupported("EclipseFS checksum type"));
        }
        let sb = Self {
            total_blocks: le64(b, 24),
            free_blocks: le64(b, 32),
            generation: le64(b, 40),
            created: le64(b, 48),
            features: le64(b, 56),
            uuid: b[64..80].try_into().unwrap(),
            label: b[80..144].try_into().unwrap(),
            root: Root {
                bytenr: le64(b, 144),
                level: b[152],
                gen: le64(b, 160),
            },
            journal_start: le64(b, 168),
            journal_blocks: le32(b, 176),
            default_subvol: le64(b, 184),
        };
        if sb.total_blocks < MIN_GROUP_BLOCKS
            || sb.root.bytenr >= sb.total_blocks
            || sb.root.gen > sb.generation
        {
            return Err(Error::Corrupt("superblock fields"));
        }
        if sb.journal_blocks < MIN_JOURNAL_BLOCKS
            || sb.journal_start < 2
            || sb.journal_start + sb.journal_blocks as u64 > sb.total_blocks.min(GROUP_BLOCKS)
        {
            return Err(Error::Corrupt("journal location"));
        }
        Ok(sb)
    }

    pub fn label(&self) -> &[u8] {
        let end = self.label.iter().position(|&c| c == 0).unwrap_or(64);
        &self.label[..end]
    }

    pub fn groups(&self) -> (usize, u64) {
        let n = self.total_blocks.div_ceil(GROUP_BLOCKS);
        (n as usize, self.total_blocks - (n - 1) * GROUP_BLOCKS)
    }
}

/// 64-bit hash of a directory entry name (lookup key of DIR_ITEM).
pub fn name_hash(name: &[u8]) -> u64 {
    (crc32c(name) as u64) | ((crc32(name) as u64) << 32)
}

/// Key offset of an INODE_REF: one item per (directory, index) name.
pub fn ref_hash(dir: u64, index: u64) -> u64 {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&dir.to_le_bytes());
    b[8..].copy_from_slice(&index.to_le_bytes());
    name_hash(&b)
}

// ---------------------------------------------------------------------------
// Inode
// ---------------------------------------------------------------------------

pub const INODE_ITEM_SIZE: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InodeItem {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub size: u64,
    pub nbytes: u64,
    pub rdev: u64,
    pub flags: u64,
    /// Transaction that created the inode.
    pub generation: u64,
    /// Containing directory (for directories, the target of `..`).
    pub parent: u64,
    /// Next DIR_INDEX number (directories).
    pub dir_next: u64,
    pub atime: (u64, u32),
    pub mtime: (u64, u32),
    pub ctime: (u64, u32),
    /// Creation time.
    pub otime: (u64, u32),
    /// Bumped on every change.
    pub seq: u64,
}

impl InodeItem {
    pub fn new(kind: FileKind, mode: u32, now: (u64, u32), generation: u64) -> Self {
        Self {
            mode: kind.mode_bits() | (mode & 0o7777),
            uid: 0,
            gid: 0,
            nlink: if kind == FileKind::Dir { 2 } else { 1 },
            size: 0,
            nbytes: 0,
            rdev: 0,
            flags: 0,
            generation,
            parent: 0,
            dir_next: 2,
            atime: now,
            mtime: now,
            ctime: now,
            otime: now,
            seq: 0,
        }
    }

    pub fn kind(&self) -> FileKind {
        FileKind::from_mode(self.mode)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = vec![0u8; INODE_ITEM_SIZE];
        put32(&mut b, 0, self.mode);
        put32(&mut b, 4, self.uid);
        put32(&mut b, 8, self.gid);
        put32(&mut b, 12, self.nlink);
        put64(&mut b, 16, self.size);
        put64(&mut b, 24, self.nbytes);
        put64(&mut b, 32, self.rdev);
        put64(&mut b, 40, self.flags);
        put64(&mut b, 48, self.generation);
        put64(&mut b, 56, self.parent);
        put64(&mut b, 64, self.dir_next);
        put64(&mut b, 72, self.atime.0);
        put32(&mut b, 80, self.atime.1);
        put32(&mut b, 84, self.mtime.1);
        put64(&mut b, 88, self.mtime.0);
        put64(&mut b, 96, self.ctime.0);
        put32(&mut b, 104, self.ctime.1);
        put32(&mut b, 108, self.otime.1);
        put64(&mut b, 112, self.otime.0);
        put64(&mut b, 120, self.seq);
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != INODE_ITEM_SIZE {
            return Err(Error::Corrupt("inode item size"));
        }
        let i = Self {
            mode: le32(b, 0),
            uid: le32(b, 4),
            gid: le32(b, 8),
            nlink: le32(b, 12),
            size: le64(b, 16),
            nbytes: le64(b, 24),
            rdev: le64(b, 32),
            flags: le64(b, 40),
            generation: le64(b, 48),
            parent: le64(b, 56),
            dir_next: le64(b, 64),
            atime: (le64(b, 72), le32(b, 80)),
            mtime: (le64(b, 88), le32(b, 84)),
            ctime: (le64(b, 96), le32(b, 104)),
            otime: (le64(b, 112), le32(b, 108)),
            seq: le64(b, 120),
        };
        if FileKind::from_disk(i.kind().to_disk()).is_none() || i.mode & 0o170000 == 0 {
            return Err(Error::Corrupt("inode mode"));
        }
        Ok(i)
    }
}

// ---------------------------------------------------------------------------
// File extents
// ---------------------------------------------------------------------------

pub const EXT_INLINE: u8 = 0;
pub const EXT_REG: u8 = 1;
/// Allocated by `fallocate`, never written: reads as zeros, no checksums.
pub const EXT_PREALLOC: u8 = 2;
/// Compressed: `disk_blocks` blocks at `disk` hold `ram_blocks` file
/// blocks; the item maps `blocks` of them from `offset` on.
pub const EXT_COMPRESSED: u8 = 3;
const EXT_HDR: usize = 8;
const EXT_REG_SIZE: usize = 24;
const EXT_COMP_SIZE: usize = 48;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileExtent {
    /// The file's bytes, inside the item (only at offset 0).
    Inline(Vec<u8>),
    /// `blocks` blocks at `disk`, mapped at the item's file offset.
    Reg { disk: u64, blocks: u64 },
    /// Same, reserved by `fallocate` and not written yet (reads as zeros).
    Prealloc { disk: u64, blocks: u64 },
    /// File blocks `offset..offset+blocks` of a compressed extent.
    Comp(Comp),
}

/// Where a compressed piece of a file lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Comp {
    pub algo: Compression,
    pub disk: u64,
    pub disk_blocks: u64,
    /// File blocks the whole extent holds.
    pub ram_blocks: u64,
    /// First of them this item maps, and how many.
    pub offset: u64,
    pub blocks: u64,
}

impl FileExtent {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            FileExtent::Inline(d) => {
                let mut b = vec![0u8; EXT_HDR + d.len()];
                b[0] = EXT_INLINE;
                b[EXT_HDR..].copy_from_slice(d);
                b
            }
            FileExtent::Reg { disk, blocks } | FileExtent::Prealloc { disk, blocks } => {
                let mut b = vec![0u8; EXT_REG_SIZE];
                b[0] = if matches!(self, FileExtent::Reg { .. }) {
                    EXT_REG
                } else {
                    EXT_PREALLOC
                };
                put64(&mut b, 8, *disk);
                put64(&mut b, 16, *blocks);
                b
            }
            FileExtent::Comp(c) => {
                let mut b = vec![0u8; EXT_COMP_SIZE];
                b[0] = EXT_COMPRESSED;
                b[1] = c.algo.to_disk();
                put64(&mut b, 8, c.disk);
                put64(&mut b, 16, c.disk_blocks);
                put64(&mut b, 24, c.ram_blocks);
                put64(&mut b, 32, c.offset);
                put64(&mut b, 40, c.blocks);
                b
            }
        }
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        match b.first() {
            Some(&EXT_INLINE) if b.len() >= EXT_HDR && b[1] == 0 => {
                Ok(FileExtent::Inline(b[EXT_HDR..].to_vec()))
            }
            Some(&t @ (EXT_REG | EXT_PREALLOC)) if b.len() == EXT_REG_SIZE && b[1] == 0 => {
                let blocks = le64(b, 16);
                if blocks == 0 || blocks > MAX_EXTENT_BLOCKS {
                    return Err(Error::Corrupt("file extent length"));
                }
                let disk = le64(b, 8);
                Ok(if t == EXT_REG {
                    FileExtent::Reg { disk, blocks }
                } else {
                    FileExtent::Prealloc { disk, blocks }
                })
            }
            Some(&EXT_COMPRESSED) if b.len() == EXT_COMP_SIZE => {
                let c = Comp {
                    algo: Compression::from_disk(b[1])
                        .ok_or(Error::Corrupt("compression algorithm"))?,
                    disk: le64(b, 8),
                    disk_blocks: le64(b, 16),
                    ram_blocks: le64(b, 24),
                    offset: le64(b, 32),
                    blocks: le64(b, 40),
                };
                if c.disk_blocks == 0
                    || c.disk_blocks >= c.ram_blocks
                    || c.ram_blocks > ZCHUNK
                    || c.blocks == 0
                    || c.offset + c.blocks > c.ram_blocks
                {
                    return Err(Error::Corrupt("compressed extent item"));
                }
                Ok(FileExtent::Comp(c))
            }
            _ => Err(Error::Corrupt("file extent item")),
        }
    }
}

/// Data blocks an item references (for reference counting).
/// A compressed piece references every block of its extent.
pub fn extent_of(k: &Key, d: &[u8]) -> Option<(u64, u64)> {
    if k.ty != EXTENT_DATA {
        return None;
    }
    match (d.first(), d.len()) {
        (Some(&(EXT_REG | EXT_PREALLOC)), EXT_REG_SIZE)
        | (Some(&EXT_COMPRESSED), EXT_COMP_SIZE) => Some((le64(d, 8), le64(d, 16))),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Directory entries and names
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEnt {
    pub ino: u64,
    pub index: u64,
    pub kind: FileKind,
    pub name: Vec<u8>,
}

/// Encode a DIR_ITEM value (every name with this hash).
pub fn encode_dir_item(list: &[DirEnt]) -> Vec<u8> {
    let mut b = Vec::new();
    for e in list {
        b.extend_from_slice(&e.ino.to_le_bytes());
        b.extend_from_slice(&e.index.to_le_bytes());
        b.push(e.kind.to_disk());
        b.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
        b.extend_from_slice(&e.name);
    }
    b
}

pub fn decode_dir_item(b: &[u8]) -> Result<Vec<DirEnt>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < b.len() {
        if b.len() - at < 19 {
            return Err(Error::Corrupt("directory item"));
        }
        let n = le16(b, at + 17) as usize;
        let kind = FileKind::from_disk(b[at + 16]).ok_or(Error::Corrupt("directory item kind"))?;
        if n == 0 || b.len() - at - 19 < n {
            return Err(Error::Corrupt("directory item name"));
        }
        out.push(DirEnt {
            ino: le64(b, at),
            index: le64(b, at + 8),
            kind,
            name: b[at + 19..at + 19 + n].to_vec(),
        });
        at += 19 + n;
    }
    Ok(out)
}

/// DIR_INDEX value and INODE_REF value share a layout: `(u64, u64, kind,
/// name)`. For DIR_INDEX the numbers are (child ino, 0); for INODE_REF
/// (directory, index).
pub fn encode_pair(a: u64, b: u64, kind: FileKind, name: &[u8]) -> Vec<u8> {
    encode_dir_item(&[DirEnt {
        ino: a,
        index: b,
        kind,
        name: name.to_vec(),
    }])
}

pub fn decode_pair(b: &[u8]) -> Result<DirEnt> {
    let mut l = decode_dir_item(b)?;
    if l.len() != 1 {
        return Err(Error::Corrupt("name item"));
    }
    Ok(l.pop().unwrap())
}

// ---------------------------------------------------------------------------
// Tree roots and reference counts
// ---------------------------------------------------------------------------

pub const ROOT_ITEM_SIZE: usize = 168;
/// Read-only subvolume or snapshot.
pub const ROOT_FLAG_RDONLY: u64 = 1;
/// Being received: not named anywhere yet, dropped at mount if left over.
pub const ROOT_FLAG_RECEIVING: u64 = 2;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootItem {
    pub root: Root,
    pub next_ino: u64,
    pub inode_count: u64,
    pub root_dir: u64,
    pub flags: u64,
    pub otime: (u64, u32),
    /// Subvolume this one is a snapshot of (0 if none).
    pub parent: u64,
    /// Generation it was snapshotted at.
    pub parent_gen: u64,
    pub uuid: [u8; 16],
    /// UUID of the subvolume this one is a snapshot of.
    pub parent_uuid: [u8; 16],
    /// UUID of the subvolume a `receive` made this one from (zero if none).
    pub received_uuid: [u8; 16],
    /// When it was received.
    pub rtime: (u64, u32),
    /// Quota: most file bytes the subvolume may reference (0: no limit).
    pub limit: u64,
    /// File bytes the subvolume references, kept while quotas are on.
    pub referenced: u64,
}

impl RootItem {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = vec![0u8; ROOT_ITEM_SIZE];
        put64(&mut b, 0, self.root.bytenr);
        b[8] = self.root.level;
        put64(&mut b, 16, self.root.gen);
        put64(&mut b, 24, self.next_ino);
        put64(&mut b, 32, self.inode_count);
        put64(&mut b, 40, self.root_dir);
        put64(&mut b, 48, self.flags);
        put64(&mut b, 56, self.otime.0);
        put32(&mut b, 64, self.otime.1);
        put64(&mut b, 72, self.parent);
        put64(&mut b, 80, self.parent_gen);
        b[88..104].copy_from_slice(&self.uuid);
        b[104..120].copy_from_slice(&self.parent_uuid);
        b[120..136].copy_from_slice(&self.received_uuid);
        put64(&mut b, 136, self.rtime.0);
        put32(&mut b, 144, self.rtime.1);
        put64(&mut b, 152, self.limit);
        put64(&mut b, 160, self.referenced);
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() != ROOT_ITEM_SIZE {
            return Err(Error::Corrupt("root item"));
        }
        Ok(Self {
            root: Root {
                bytenr: le64(b, 0),
                level: b[8],
                gen: le64(b, 16),
            },
            next_ino: le64(b, 24),
            inode_count: le64(b, 32),
            root_dir: le64(b, 40),
            flags: le64(b, 48),
            otime: (le64(b, 56), le32(b, 64)),
            parent: le64(b, 72),
            parent_gen: le64(b, 80),
            uuid: b[88..104].try_into().unwrap(),
            parent_uuid: b[104..120].try_into().unwrap(),
            received_uuid: b[120..136].try_into().unwrap(),
            rtime: (le64(b, 136), le32(b, 144)),
            limit: le64(b, 152),
            referenced: le64(b, 160),
        })
    }
}

/// A UUID for subvolume `id` made at generation `gen` and time `now` on
/// the filesystem `fs` (unique per filesystem and moment; no randomness
/// is needed).
pub fn subvol_uuid(fs: &[u8; 16], id: u64, gen: u64, now: (u64, u32)) -> [u8; 16] {
    fn mix(mut z: u64) -> u64 {
        z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    let a = u64::from_le_bytes(fs[..8].try_into().unwrap());
    let b = u64::from_le_bytes(fs[8..].try_into().unwrap());
    let t = now.0 ^ ((now.1 as u64) << 32).rotate_left(17);
    let h1 = mix(a ^ mix(id ^ mix(gen ^ mix(t))));
    let h2 = mix(b ^ mix(h1 ^ mix(id.rotate_left(29) ^ gen)));
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&h1.to_le_bytes());
    out[8..].copy_from_slice(&h2.to_le_bytes());
    // RFC 4122 version 4 / variant bits, so tools show it as a UUID.
    out[6] = (out[6] & 0x0F) | 0x40;
    out[8] = (out[8] & 0x3F) | 0x80;
    out
}

pub fn encode_ref(len: u64, count: u32) -> Vec<u8> {
    let mut b = vec![0u8; 12];
    put64(&mut b, 0, len);
    put32(&mut b, 8, count);
    b
}

pub fn decode_ref(b: &[u8]) -> Result<(u64, u32)> {
    if b.len() != 12 {
        return Err(Error::Corrupt("reference item"));
    }
    Ok((le64(b, 0), le32(b, 8)))
}

// ---------------------------------------------------------------------------
// Inode flags (the values of Linux's FS_IOC_GETFLAGS / chattr)
// ---------------------------------------------------------------------------

pub const FL_SYNC: u32 = 0x0000_0008;
pub const FL_IMMUTABLE: u32 = 0x0000_0010;
pub const FL_APPEND: u32 = 0x0000_0020;
pub const FL_NODUMP: u32 = 0x0000_0040;
pub const FL_NOATIME: u32 = 0x0000_0080;
pub const FL_COMPR: u32 = 0x0000_0004;
pub const FL_NOCOMP: u32 = 0x0000_0400;
pub const FL_NOCOW: u32 = 0x0080_0000;
/// Flags `set_flags` accepts.
pub const FL_SETTABLE: u32 =
    FL_SYNC | FL_IMMUTABLE | FL_APPEND | FL_NODUMP | FL_NOATIME | FL_COMPR | FL_NOCOMP | FL_NOCOW;

/// Bits of `InodeItem::flags` past the Linux ones: the algorithm new data
/// of the inode is compressed with (`btrfs property set ... compression`).
const ZALGO_SHIFT: u32 = 32;
const ZALGO_MASK: u64 = 0xFF << ZALGO_SHIFT;

/// Flags a new inode takes from its directory (as on btrfs), compression
/// property included.
pub const FL_INHERITED: u64 = (FL_NOCOW | FL_COMPR | FL_NOCOMP) as u64 | ZALGO_MASK;

pub fn inode_compression(i: &InodeItem) -> Option<Compression> {
    Compression::from_disk(((i.flags & ZALGO_MASK) >> ZALGO_SHIFT) as u8)
}

pub fn set_inode_compression(i: &mut InodeItem, algo: Option<Compression>) {
    let v = algo.map_or(0, |a| a.to_disk() as u64);
    i.flags = (i.flags & !ZALGO_MASK) | v << ZALGO_SHIFT;
}

// ---------------------------------------------------------------------------
// Extended attributes
// ---------------------------------------------------------------------------

/// Longest attribute name (Linux's XATTR_NAME_MAX).
pub const XATTR_NAME_MAX: usize = 255;
/// Largest attribute value (Linux's XATTR_SIZE_MAX); a value must also fit
/// in a tree leaf with the other attributes sharing its name hash.
pub const XATTR_SIZE_MAX: usize = 65536;
/// `set_xattr` flags (Linux's values).
pub const XATTR_CREATE: u32 = 1;
pub const XATTR_REPLACE: u32 = 2;
/// `fallocate` modes (Linux's values).
pub const FALLOC_FL_KEEP_SIZE: u32 = 0x01;
pub const FALLOC_FL_PUNCH_HOLE: u32 = 0x02;
pub const FALLOC_FL_ZERO_RANGE: u32 = 0x10;
/// Namespaces an attribute name must start with.
pub const XATTR_PREFIXES: [&str; 4] = ["user.", "trusted.", "security.", "system."];

/// `(ino, XATTR_ITEM, hash(name))` -> every attribute of `ino` whose name
/// has that hash: [name len u16 | value len u16 | name | value]...
pub fn encode_xattrs(list: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut b = Vec::new();
    for (n, v) in list {
        b.extend_from_slice(&(n.len() as u16).to_le_bytes());
        b.extend_from_slice(&(v.len() as u16).to_le_bytes());
        b.extend_from_slice(n);
        b.extend_from_slice(v);
    }
    b
}

/// (name, value) pairs of one xattr item.
pub type Xattrs = Vec<(Vec<u8>, Vec<u8>)>;

pub fn decode_xattrs(b: &[u8]) -> Result<Xattrs> {
    let mut out = Vec::new();
    let mut p = 0;
    while p < b.len() {
        if p + 4 > b.len() {
            return Err(Error::Corrupt("xattr item"));
        }
        let n = le16(b, p) as usize;
        let v = le16(b, p + 2) as usize;
        let end = p + 4 + n + v;
        if n == 0 || end > b.len() {
            return Err(Error::Corrupt("xattr item"));
        }
        out.push((b[p + 4..p + 4 + n].to_vec(), b[p + 4 + n..end].to_vec()));
        p = end;
    }
    if out.is_empty() {
        return Err(Error::Corrupt("empty xattr item"));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Data checksums
// ---------------------------------------------------------------------------

/// Data blocks per checksum item.
pub const CSUM_BUCKET: u64 = 256;
const CSUM_MAP: usize = CSUM_BUCKET as usize / 8;

/// The checksums of one bucket of [`CSUM_BUCKET`] disk blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsumBucket(pub Vec<Option<u32>>);

impl Default for CsumBucket {
    fn default() -> Self {
        Self(vec![None; CSUM_BUCKET as usize])
    }
}

impl CsumBucket {
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(Option::is_none)
    }

    /// Presence bitmap, then a CRC32C per block up to the last present one.
    pub fn encode(&self) -> Vec<u8> {
        let n = self
            .0
            .iter()
            .rposition(Option::is_some)
            .map_or(0, |i| i + 1);
        let mut b = vec![0u8; CSUM_MAP + 4 * n];
        for (i, c) in self.0[..n].iter().enumerate() {
            if let Some(c) = c {
                b[i / 8] |= 1 << (i % 8);
                put32(&mut b, CSUM_MAP + 4 * i, *c);
            }
        }
        b
    }

    /// The checksum of block `i` of an encoded bucket, without decoding
    /// the rest of it.
    pub fn lookup(b: &[u8], i: usize) -> Result<Option<u32>> {
        if b.len() < CSUM_MAP || i >= CSUM_BUCKET as usize {
            return Err(Error::Corrupt("checksum item"));
        }
        if b[i / 8] & (1 << (i % 8)) == 0 {
            return Ok(None);
        }
        if CSUM_MAP + 4 * i + 4 > b.len() {
            return Err(Error::Corrupt("checksum item"));
        }
        Ok(Some(le32(b, CSUM_MAP + 4 * i)))
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() < CSUM_MAP || !(b.len() - CSUM_MAP).is_multiple_of(4) {
            return Err(Error::Corrupt("checksum item"));
        }
        let n = (b.len() - CSUM_MAP) / 4;
        if n > CSUM_BUCKET as usize {
            return Err(Error::Corrupt("checksum item"));
        }
        let mut out = Self::default();
        for i in 0..CSUM_BUCKET as usize {
            if b[i / 8] & (1 << (i % 8)) != 0 {
                if i >= n {
                    return Err(Error::Corrupt("checksum item"));
                }
                out.0[i] = Some(le32(b, CSUM_MAP + 4 * i));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xattr_items_round_trip() {
        let list = vec![
            (b"user.a".to_vec(), b"1".to_vec()),
            (b"security.selinux".to_vec(), Vec::new()),
        ];
        let e = encode_xattrs(&list);
        assert_eq!(decode_xattrs(&e).unwrap(), list);
        assert!(decode_xattrs(&e[..e.len() - 1]).is_err());
        assert!(decode_xattrs(&[]).is_err());
        assert!(decode_xattrs(&[0, 0, 1, 0, 9]).is_err());
    }

    #[test]
    fn prealloc_extents_round_trip() {
        let e = FileExtent::Prealloc {
            disk: 77,
            blocks: 3,
        };
        let b = e.encode();
        assert_eq!(FileExtent::decode(&b).unwrap(), e);
        assert_eq!(extent_of(&Key::new(300, EXTENT_DATA, 0), &b), Some((77, 3)));
        let r = FileExtent::Reg {
            disk: 77,
            blocks: 3,
        }
        .encode();
        assert_ne!(b, r);
    }

    #[test]
    fn checksum_buckets_round_trip() {
        let mut c = CsumBucket::default();
        assert!(c.is_empty());
        assert_eq!(c.encode().len(), 32);
        c.0[0] = Some(7);
        c.0[9] = Some(0xDEAD_BEEF);
        c.0[255] = Some(0);
        let e = c.encode();
        assert_eq!(e.len(), 32 + 4 * 256);
        assert_eq!(CsumBucket::decode(&e).unwrap(), c);
        c.0[255] = None;
        let e = c.encode();
        assert_eq!(e.len(), 32 + 4 * 10);
        assert_eq!(CsumBucket::decode(&e).unwrap(), c);
        assert_eq!(CsumBucket::lookup(&e, 9).unwrap(), Some(0xDEAD_BEEF));
        assert_eq!(CsumBucket::lookup(&e, 8).unwrap(), None);
        assert_eq!(CsumBucket::lookup(&e, 200).unwrap(), None);
        assert!(CsumBucket::lookup(&e, 256).is_err());
        // A present bit past the stored checksums, or a ragged length.
        let mut bad = e.clone();
        bad[20] = 1;
        assert!(CsumBucket::decode(&bad).is_err());
        assert!(CsumBucket::decode(&e[..33]).is_err());
        assert!(CsumBucket::decode(&[0; 10]).is_err());
    }

    fn sb() -> SuperV4 {
        SuperV4 {
            total_blocks: 1000,
            free_blocks: 900,
            generation: 7,
            created: 1,
            features: 0,
            uuid: [4; 16],
            label: [0; 64],
            root: Root {
                bytenr: 30,
                level: 1,
                gen: 7,
            },
            journal_start: 2,
            journal_blocks: 64,
            default_subvol: FS_TREE,
        }
    }

    #[test]
    fn superblock_roundtrip_and_checks() {
        let b = sb().encode();
        assert_eq!(SuperV4::decode(&b).unwrap(), sb());
        for at in [0usize, 13, 30, 100, 150, 170, 507, 508] {
            let mut c = b.clone();
            c[at] ^= 1;
            assert!(SuperV4::decode(&c).is_err(), "byte {}", at);
        }
        let mut bad = sb();
        bad.root.gen = 8;
        assert!(SuperV4::decode(&bad.encode()).is_err());
        let mut bad = sb();
        bad.journal_start = 990;
        assert!(SuperV4::decode(&bad.encode()).is_err());
    }

    #[test]
    fn items_roundtrip() {
        let mut i = InodeItem::new(FileKind::Dir, 0o755, (5, 6), 3);
        i.size = 77;
        i.rdev = 9;
        i.otime = (1, 2);
        assert_eq!(InodeItem::decode(&i.encode()).unwrap(), i);
        assert_eq!(i.kind(), FileKind::Dir);
        assert!(InodeItem::decode(&[0u8; INODE_ITEM_SIZE]).is_err());
        assert!(InodeItem::decode(&[0u8; 10]).is_err());

        for e in [
            FileExtent::Inline(vec![1, 2, 3]),
            FileExtent::Inline(vec![]),
            FileExtent::Reg {
                disk: 99,
                blocks: 5,
            },
        ] {
            assert_eq!(FileExtent::decode(&e.encode()).unwrap(), e);
        }
        assert!(FileExtent::decode(&FileExtent::Reg { disk: 1, blocks: 0 }.encode()).is_err());
        assert!(FileExtent::decode(&[7]).is_err());
        let k = Key::new(300, EXTENT_DATA, 0);
        assert_eq!(
            extent_of(&k, &FileExtent::Reg { disk: 9, blocks: 2 }.encode()),
            Some((9, 2))
        );
        assert_eq!(
            extent_of(&k, &FileExtent::Inline(vec![0; 24]).encode()),
            None
        );

        let list = vec![
            DirEnt {
                ino: 300,
                index: 2,
                kind: FileKind::Regular,
                name: b"a".to_vec(),
            },
            DirEnt {
                ino: 301,
                index: 3,
                kind: FileKind::Symlink,
                name: vec![b'x'; 255],
            },
        ];
        assert_eq!(decode_dir_item(&encode_dir_item(&list)).unwrap(), list);
        let enc = encode_dir_item(&list);
        assert!(decode_dir_item(&enc[..enc.len() - 1]).is_err());

        let r = RootItem {
            root: Root {
                bytenr: 5,
                level: 2,
                gen: 4,
            },
            next_ino: 400,
            inode_count: 3,
            root_dir: FIRST_INO,
            flags: ROOT_FLAG_RDONLY,
            otime: (1, 1),
            parent: 5,
            parent_gen: 3,
            uuid: [1; 16],
            parent_uuid: [2; 16],
            received_uuid: [3; 16],
            rtime: (9, 8),
            limit: 1 << 30,
            referenced: 12345,
        };
        assert_eq!(RootItem::decode(&r.encode()).unwrap(), r);
        assert_eq!(decode_ref(&encode_ref(8, 3)).unwrap(), (8, 3));
    }

    #[test]
    fn hashes_spread() {
        assert_ne!(name_hash(b"a"), name_hash(b"b"));
        assert_ne!(ref_hash(1, 2), ref_hash(2, 1));
        let mut seen = alloc::collections::BTreeSet::new();
        for i in 0..10000u32 {
            assert!(seen.insert(name_hash(&i.to_le_bytes())));
        }
    }
}
