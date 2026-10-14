//! EclipseFS v0.3 on-disk format.
//!
//! eclipsefs-lib's v0.2 image (header, inode table, TLV node records with the
//! file contents inline) was written once by `mkfs-eclipsefs` and could not be
//! modified in place. v0.3 keeps its identity -- the `ECLIPSEFS` magic, the
//! CRC32 checksums, root inode 1 and the node kinds 1/2/3 -- on a layout that
//! can be updated block by block:
//!
//! ```text
//! group k (GROUP_BLOCKS blocks, 128 MiB):
//!   block 0      superblock (primary in group 0, backup copy elsewhere)
//!   block 1      free-space bitmap of the group
//!   block 2..    data, directory contents, inode table, extent blocks
//! ```
//!
//! Inodes are fixed 256-byte records in the *inode table*, itself an
//! extent-mapped file whose inode record lives in the superblock. Inode `n`
//! sits at byte `n * INODE_SIZE` of that file. All integers are little-endian.

use alloc::vec;
use alloc::vec::Vec;

use crate::crc::crc32;
use crate::extent::{Extent, ExtentTree, EXTENT_DISK_SIZE};
use crate::{Error, Result};

pub const MAGIC: &[u8; 9] = b"ECLIPSEFS";
/// v0.3.0, encoded like eclipsefs-lib's `ECLIPSEFS_VERSION`.
pub const VERSION: u32 = 0x0003_0000;
pub const VERSION_MAJOR: u32 = VERSION >> 16;

pub const BLOCK_SIZE: u64 = 4096;
/// Blocks per allocation group: one bitmap block's worth of bits.
pub const GROUP_BLOCKS: u64 = BLOCK_SIZE * 8;
/// A trailing group smaller than this is not worth its two header blocks.
pub const MIN_GROUP_BLOCKS: u64 = 64;

pub const INODE_SIZE: usize = 256;
/// The root directory (eclipsefs-lib's `ROOT_INODE`).
pub const ROOT_INO: u64 = 1;
/// Pseudo inode number of the inode table file itself.
pub const ITABLE_INO: u64 = 0;
pub const MAX_NAME_LEN: usize = 255;
/// Longest symlink target accepted (PATH_MAX).
pub const MAX_SYMLINK_LEN: usize = 4096;

/// Inode flag: the content lives in the record's inline area.
pub const INODE_FLAG_INLINE: u8 = 0x01;
/// Bytes available inline in an inode record (extents or data).
pub const INLINE_LEN: usize = 152;
/// Extents that fit in the inline area.
pub const INLINE_EXTENTS: usize = INLINE_LEN / EXTENT_DISK_SIZE;

pub const EXTENT_BLOCK_MAGIC: u32 = u32::from_le_bytes(*b"EFSX");
const EXTENT_BLOCK_HDR: usize = 32;
/// Extents per overflow block.
pub const EXTENTS_PER_BLOCK: usize = (BLOCK_SIZE as usize - EXTENT_BLOCK_HDR) / EXTENT_DISK_SIZE;

pub const S_IFMT: u32 = 0o170000;
pub const S_IFSOCK: u32 = 0o140000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFBLK: u32 = 0o060000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFIFO: u32 = 0o010000;

/// High-level file kind used by the public API (same as `btrfs::FileKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Regular,
    Dir,
    Symlink,
    CharDevice,
    BlockDevice,
    Fifo,
    Socket,
}

impl FileKind {
    pub fn from_mode(mode: u32) -> Self {
        match mode & S_IFMT {
            S_IFDIR => FileKind::Dir,
            S_IFLNK => FileKind::Symlink,
            S_IFCHR => FileKind::CharDevice,
            S_IFBLK => FileKind::BlockDevice,
            S_IFIFO => FileKind::Fifo,
            S_IFSOCK => FileKind::Socket,
            _ => FileKind::Regular,
        }
    }

    pub fn mode_bits(self) -> u32 {
        match self {
            FileKind::Regular => S_IFREG,
            FileKind::Dir => S_IFDIR,
            FileKind::Symlink => S_IFLNK,
            FileKind::CharDevice => S_IFCHR,
            FileKind::BlockDevice => S_IFBLK,
            FileKind::Fifo => S_IFIFO,
            FileKind::Socket => S_IFSOCK,
        }
    }

    /// On-disk kind byte. 1..3 are eclipsefs-lib's `NodeKind` values.
    pub fn to_disk(self) -> u8 {
        match self {
            FileKind::Regular => 1,
            FileKind::Dir => 2,
            FileKind::Symlink => 3,
            FileKind::CharDevice => 4,
            FileKind::BlockDevice => 5,
            FileKind::Fifo => 6,
            FileKind::Socket => 7,
        }
    }

    pub fn from_disk(b: u8) -> Option<Self> {
        Some(match b {
            1 => FileKind::Regular,
            2 => FileKind::Dir,
            3 => FileKind::Symlink,
            4 => FileKind::CharDevice,
            5 => FileKind::BlockDevice,
            6 => FileKind::Fifo,
            7 => FileKind::Socket,
            _ => return None,
        })
    }
}

fn le64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}
fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}
fn le16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().unwrap())
}
fn put64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn put16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

// ---------------------------------------------------------------------------
// Superblock
// ---------------------------------------------------------------------------

const SB_ITABLE: usize = 168;
const SB_CRC: usize = SB_ITABLE + INODE_SIZE;

/// The superblock, at block 0 (and copied to block 0 of every other group).
#[derive(Debug, Clone)]
pub struct Superblock {
    pub version: u32,
    pub block_size: u32,
    pub group_blocks: u32,
    pub total_blocks: u64,
    pub free_blocks: u64,
    /// Next inode number to hand out (numbers are never reused).
    pub next_ino: u64,
    /// Live inodes.
    pub inode_count: u64,
    pub root_ino: u64,
    /// Bumped on every superblock write.
    pub generation: u64,
    pub created: u64,
    pub flags: u32,
    pub inode_size: u32,
    pub uuid: [u8; 16],
    pub label: [u8; 64],
    /// Raw inode record of the inode table file.
    pub itable: [u8; INODE_SIZE],
}

impl Superblock {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = vec![0u8; BLOCK_SIZE as usize];
        b[0..9].copy_from_slice(MAGIC);
        put32(&mut b, 12, self.version);
        put32(&mut b, 16, self.block_size);
        put32(&mut b, 20, self.group_blocks);
        put64(&mut b, 24, self.total_blocks);
        put64(&mut b, 32, self.free_blocks);
        put64(&mut b, 40, self.next_ino);
        put64(&mut b, 48, self.inode_count);
        put64(&mut b, 56, self.root_ino);
        put64(&mut b, 64, self.generation);
        put64(&mut b, 72, self.created);
        put32(&mut b, 80, self.flags);
        put32(&mut b, 84, self.inode_size);
        b[88..104].copy_from_slice(&self.uuid);
        b[104..168].copy_from_slice(&self.label);
        b[SB_ITABLE..SB_CRC].copy_from_slice(&self.itable);
        let crc = crc32(&b[..SB_CRC]);
        put32(&mut b, SB_CRC, crc);
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() < SB_CRC + 4 || &b[0..9] != MAGIC {
            return Err(Error::BadSuperblock);
        }
        if crc32(&b[..SB_CRC]) != le32(b, SB_CRC) {
            return Err(Error::Corrupt("superblock checksum"));
        }
        let version = le32(b, 12);
        if version >> 16 != VERSION_MAJOR {
            return Err(Error::Unsupported("EclipseFS format version"));
        }
        let sb = Self {
            version,
            block_size: le32(b, 16),
            group_blocks: le32(b, 20),
            total_blocks: le64(b, 24),
            free_blocks: le64(b, 32),
            next_ino: le64(b, 40),
            inode_count: le64(b, 48),
            root_ino: le64(b, 56),
            generation: le64(b, 64),
            created: le64(b, 72),
            flags: le32(b, 80),
            inode_size: le32(b, 84),
            uuid: b[88..104].try_into().unwrap(),
            label: b[104..168].try_into().unwrap(),
            itable: b[SB_ITABLE..SB_CRC].try_into().unwrap(),
        };
        if sb.block_size as u64 != BLOCK_SIZE
            || sb.group_blocks as u64 != GROUP_BLOCKS
            || sb.inode_size as usize != INODE_SIZE
        {
            return Err(Error::Unsupported("EclipseFS geometry"));
        }
        if sb.total_blocks < MIN_GROUP_BLOCKS || sb.root_ino == 0 || sb.next_ino <= sb.root_ino {
            return Err(Error::Corrupt("superblock fields"));
        }
        Ok(sb)
    }

    pub fn label(&self) -> &[u8] {
        let end = self.label.iter().position(|&c| c == 0).unwrap_or(64);
        &self.label[..end]
    }

    /// Number of allocation groups, and the size of the last one.
    pub fn groups(&self) -> (usize, u64) {
        let n = self.total_blocks.div_ceil(GROUP_BLOCKS);
        let last = self.total_blocks - (n - 1) * GROUP_BLOCKS;
        (n as usize, last)
    }
}

// ---------------------------------------------------------------------------
// Inodes
// ---------------------------------------------------------------------------

/// In-memory inode. `extents` is the whole map, whatever part of it lives in
/// the overflow chain on disk.
#[derive(Debug, Clone)]
pub struct Inode {
    pub kind: FileKind,
    pub flags: u8,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub size: u64,
    /// Bytes of storage used.
    pub nbytes: u64,
    pub rdev: u64,
    pub atime: (u64, u32),
    pub mtime: (u64, u32),
    pub ctime: (u64, u32),
    /// Containing directory (for directories, the target of `..`).
    pub parent: u64,
    pub extents: ExtentTree,
    /// Inline content when `flags & INODE_FLAG_INLINE`.
    pub inline: Vec<u8>,
    /// Overflow extent blocks currently on disk, in chain order.
    pub chain: Vec<u64>,
    /// The extent map changed since the chain was last written.
    pub extents_dirty: bool,
}

impl Inode {
    pub fn new(kind: FileKind, mode: u32, now: (u64, u32)) -> Self {
        Self {
            kind,
            flags: 0,
            mode: kind.mode_bits() | (mode & !S_IFMT),
            uid: 0,
            gid: 0,
            nlink: 1,
            size: 0,
            nbytes: 0,
            rdev: 0,
            atime: now,
            mtime: now,
            ctime: now,
            parent: 0,
            extents: ExtentTree::new(),
            inline: Vec::new(),
            chain: Vec::new(),
            extents_dirty: false,
        }
    }

    pub fn is_inline(&self) -> bool {
        self.flags & INODE_FLAG_INLINE != 0
    }

    /// Encode the 256-byte record. Extents past the inline ones are expected to
    /// be in `self.chain` already.
    pub fn encode(&self) -> [u8; INODE_SIZE] {
        let mut b = [0u8; INODE_SIZE];
        b[0] = self.kind.to_disk();
        b[1] = self.flags;
        let ext = self.extents.extents();
        put32(&mut b, 4, self.mode);
        put32(&mut b, 8, self.uid);
        put32(&mut b, 12, self.gid);
        put32(&mut b, 16, self.nlink);
        put32(&mut b, 20, self.atime.1);
        put64(&mut b, 24, self.size);
        put64(&mut b, 32, self.nbytes);
        put64(&mut b, 40, self.rdev);
        put64(&mut b, 48, self.atime.0);
        put64(&mut b, 56, self.mtime.0);
        put64(&mut b, 64, self.ctime.0);
        put32(&mut b, 72, self.mtime.1);
        put32(&mut b, 76, self.ctime.1);
        put64(&mut b, 80, self.parent);
        put64(&mut b, 88, self.chain.first().copied().unwrap_or(0));
        if self.is_inline() {
            b[96..96 + self.inline.len()].copy_from_slice(&self.inline);
        } else {
            put32(&mut b, 248, ext.len() as u32);
            for (i, e) in ext.iter().take(INLINE_EXTENTS).enumerate() {
                let o = 96 + i * EXTENT_DISK_SIZE;
                e.encode(&mut b[o..o + EXTENT_DISK_SIZE]);
            }
        }
        let crc = crc32(&b[..INODE_SIZE - 4]);
        put32(&mut b, INODE_SIZE - 4, crc);
        b
    }

    /// Decode a record. `Ok(None)` for a free slot. The returned inode holds
    /// only the inline extents; the caller follows `overflow` for the rest.
    pub fn decode(b: &[u8]) -> Result<Option<(Self, u64, u32)>> {
        if b[0] == 0 {
            return Ok(None);
        }
        if crc32(&b[..INODE_SIZE - 4]) != le32(b, INODE_SIZE - 4) {
            return Err(Error::Corrupt("inode checksum"));
        }
        let kind = FileKind::from_disk(b[0]).ok_or(Error::Corrupt("inode kind"))?;
        let flags = b[1];
        let size = le64(b, 24);
        let mut inode = Self {
            kind,
            flags,
            mode: le32(b, 4),
            uid: le32(b, 8),
            gid: le32(b, 12),
            nlink: le32(b, 16),
            size,
            nbytes: le64(b, 32),
            rdev: le64(b, 40),
            atime: (le64(b, 48), le32(b, 20)),
            mtime: (le64(b, 56), le32(b, 72)),
            ctime: (le64(b, 64), le32(b, 76)),
            parent: le64(b, 80),
            extents: ExtentTree::new(),
            inline: Vec::new(),
            chain: Vec::new(),
            extents_dirty: false,
        };
        let overflow = le64(b, 88);
        let mut count = 0;
        if inode.is_inline() {
            if size as usize > INLINE_LEN {
                return Err(Error::Corrupt("inline inode too large"));
            }
            inode.inline = b[96..96 + size as usize].to_vec();
        } else {
            count = le32(b, 248);
            let n = (count as usize).min(INLINE_EXTENTS);
            let mut list = Vec::with_capacity(count as usize);
            for i in 0..n {
                let o = 96 + i * EXTENT_DISK_SIZE;
                list.push(Extent::decode(&b[o..o + EXTENT_DISK_SIZE]));
            }
            inode.extents = ExtentTree::from_extents(list)?;
        }
        Ok(Some((inode, overflow, count)))
    }
}

/// Encode one overflow extent block.
pub fn encode_extent_block(owner: u64, next: u64, extents: &[Extent]) -> Vec<u8> {
    debug_assert!(extents.len() <= EXTENTS_PER_BLOCK);
    let mut b = vec![0u8; BLOCK_SIZE as usize];
    put32(&mut b, 0, EXTENT_BLOCK_MAGIC);
    put32(&mut b, 4, extents.len() as u32);
    put64(&mut b, 8, next);
    put64(&mut b, 16, owner);
    for (i, e) in extents.iter().enumerate() {
        let o = EXTENT_BLOCK_HDR + i * EXTENT_DISK_SIZE;
        e.encode(&mut b[o..o + EXTENT_DISK_SIZE]);
    }
    let crc = crc32(&b);
    put32(&mut b, 24, crc);
    b
}

/// Decode an overflow extent block: `(next, extents)`.
pub fn decode_extent_block(owner: u64, b: &[u8]) -> Result<(u64, Vec<Extent>)> {
    if le32(b, 0) != EXTENT_BLOCK_MAGIC || le64(b, 16) != owner {
        return Err(Error::Corrupt("extent block header"));
    }
    let mut copy = b[..BLOCK_SIZE as usize].to_vec();
    put32(&mut copy, 24, 0);
    if crc32(&copy) != le32(b, 24) {
        return Err(Error::Corrupt("extent block checksum"));
    }
    let count = le32(b, 4) as usize;
    if count > EXTENTS_PER_BLOCK {
        return Err(Error::Corrupt("extent block count"));
    }
    let extents = (0..count)
        .map(|i| {
            let o = EXTENT_BLOCK_HDR + i * EXTENT_DISK_SIZE;
            Extent::decode(&b[o..o + EXTENT_DISK_SIZE])
        })
        .collect();
    Ok((le64(b, 8), extents))
}

// ---------------------------------------------------------------------------
// Directory records
// ---------------------------------------------------------------------------

/// Directory contents are a stream of records:
/// `ino u64 | rec_len u16 | kind u8 | name_len u8 | name`.
/// A removed entry keeps its space with `ino = 0` until the directory is
/// compacted.
pub const DIRENT_HDR: usize = 12;

pub fn encode_dirent(ino: u64, kind: FileKind, name: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; DIRENT_HDR + name.len()];
    put64(&mut b, 0, ino);
    put16(&mut b, 8, (DIRENT_HDR + name.len()) as u16);
    b[10] = kind.to_disk();
    b[11] = name.len() as u8;
    b[DIRENT_HDR..].copy_from_slice(name);
    b
}

/// One parsed directory record; `ino == 0` is a removed entry.
#[derive(Debug, Clone, Copy)]
pub struct RawDirent<'a> {
    pub off: u64,
    pub rec_len: usize,
    pub ino: u64,
    pub kind: Option<FileKind>,
    pub name: &'a [u8],
}

pub fn parse_dirents(data: &[u8]) -> Result<Vec<RawDirent<'_>>> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off < data.len() {
        if data.len() - off < DIRENT_HDR {
            return Err(Error::Corrupt("truncated directory record"));
        }
        let ino = le64(data, off);
        let rec_len = le16(data, off + 8) as usize;
        let name_len = data[off + 11] as usize;
        if rec_len < DIRENT_HDR + name_len || off + rec_len > data.len() {
            return Err(Error::Corrupt("directory record length"));
        }
        let kind = FileKind::from_disk(data[off + 10]);
        if ino != 0 && (kind.is_none() || name_len == 0) {
            return Err(Error::Corrupt("directory record"));
        }
        let name = &data[off + DIRENT_HDR..off + DIRENT_HDR + name_len];
        out.push(RawDirent {
            off: off as u64,
            rec_len,
            ino,
            kind,
            name,
        });
        off += rec_len;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sb() -> Superblock {
        Superblock {
            version: VERSION,
            block_size: BLOCK_SIZE as u32,
            group_blocks: GROUP_BLOCKS as u32,
            total_blocks: 1000,
            free_blocks: 900,
            next_ino: 2,
            inode_count: 1,
            root_ino: ROOT_INO,
            generation: 1,
            created: 5,
            flags: 0,
            inode_size: INODE_SIZE as u32,
            uuid: [7; 16],
            label: [0; 64],
            itable: [0; INODE_SIZE],
        }
    }

    #[test]
    fn superblock_roundtrip_and_checksum() {
        let mut b = sb().encode();
        let back = Superblock::decode(&b).unwrap();
        assert_eq!(back.total_blocks, 1000);
        assert_eq!(back.uuid, [7; 16]);
        b[30] ^= 1;
        assert_eq!(
            Superblock::decode(&b).unwrap_err(),
            Error::Corrupt("superblock checksum")
        );
        b[0] = b'X';
        assert_eq!(Superblock::decode(&b).unwrap_err(), Error::BadSuperblock);
    }

    #[test]
    fn inode_roundtrip() {
        let mut ino = Inode::new(FileKind::Regular, 0o644, (10, 20));
        ino.size = 12345;
        for i in 0..4 {
            ino.extents
                .add_extent(Extent::new(i * 10, 1000 + i * 100, 3))
                .unwrap();
        }
        let rec = ino.encode();
        let (back, overflow, count) = Inode::decode(&rec).unwrap().unwrap();
        assert_eq!(overflow, 0);
        assert_eq!(count, 4);
        assert_eq!(back.extents, ino.extents);
        assert_eq!(back.mode, S_IFREG | 0o644);
        assert_eq!(back.mtime, (10, 20));
        let mut bad = rec;
        bad[30] ^= 0x40;
        assert!(Inode::decode(&bad).is_err());
        assert!(Inode::decode(&[0u8; INODE_SIZE]).unwrap().is_none());
    }

    #[test]
    fn inline_inode_roundtrip() {
        let mut ino = Inode::new(FileKind::Symlink, 0o777, (0, 0));
        ino.flags = INODE_FLAG_INLINE;
        ino.inline = b"../lib/libc.so".to_vec();
        ino.size = ino.inline.len() as u64;
        let (back, _, _) = Inode::decode(&ino.encode()).unwrap().unwrap();
        assert_eq!(back.inline, b"../lib/libc.so");
    }

    #[test]
    fn extent_block_roundtrip() {
        let ext: Vec<Extent> = (0..EXTENTS_PER_BLOCK as u64)
            .map(|i| Extent::new(i * 2, 50 + i * 3, 1))
            .collect();
        let b = encode_extent_block(42, 9, &ext);
        let (next, back) = decode_extent_block(42, &b).unwrap();
        assert_eq!(next, 9);
        assert_eq!(back, ext);
        assert!(decode_extent_block(43, &b).is_err());
    }

    #[test]
    fn dirents() {
        let mut data = encode_dirent(5, FileKind::Dir, b"etc");
        data.extend(encode_dirent(0, FileKind::Regular, b"gone"));
        data.extend(encode_dirent(9, FileKind::Symlink, b"lib64"));
        let p = parse_dirents(&data).unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].name, b"etc");
        assert_eq!(p[1].ino, 0);
        assert_eq!(p[2].off, 15 + 16);
        assert!(parse_dirents(&data[..data.len() - 1]).is_err());
    }

    #[test]
    fn file_kinds_roundtrip_through_mode_and_disk() {
        use FileKind::*;
        for k in [Regular, Dir, Symlink, CharDevice, BlockDevice, Fifo, Socket] {
            assert_eq!(FileKind::from_mode(k.mode_bits() | 0o644), k);
            assert_eq!(FileKind::from_disk(k.to_disk()), Some(k));
        }
        assert_eq!(FileKind::from_disk(0), None);
        assert_eq!(FileKind::from_disk(8), None);
        assert_eq!(FileKind::from_mode(0o644), Regular);
        // eclipsefs-lib's NodeKind values.
        assert_eq!(
            (Regular.to_disk(), Dir.to_disk(), Symlink.to_disk()),
            (1, 2, 3)
        );
    }

    #[test]
    fn a_newer_major_version_is_refused_a_newer_minor_is_not() {
        let mut s = sb();
        s.version = 0x0004_0000;
        assert_eq!(
            Superblock::decode(&s.encode()).unwrap_err(),
            Error::Unsupported("EclipseFS format version")
        );
        s.version = 0x0003_0007;
        assert!(Superblock::decode(&s.encode()).is_ok());
        // The old in-memory v0.2 images are not this format.
        s.version = 0x0002_0000;
        assert!(Superblock::decode(&s.encode()).is_err());
    }

    #[test]
    fn foreign_geometry_is_refused() {
        for f in [
            |s: &mut Superblock| s.block_size = 512,
            |s: &mut Superblock| s.group_blocks = 1000,
            |s: &mut Superblock| s.inode_size = 128,
        ] {
            let mut s = sb();
            f(&mut s);
            assert!(matches!(
                Superblock::decode(&s.encode()),
                Err(Error::Unsupported(_))
            ));
        }
    }

    #[test]
    fn nonsense_counters_are_corruption() {
        for f in [
            |s: &mut Superblock| s.total_blocks = 3,
            |s: &mut Superblock| s.root_ino = 0,
            |s: &mut Superblock| s.next_ino = 1,
        ] {
            let mut s = sb();
            f(&mut s);
            assert!(matches!(
                Superblock::decode(&s.encode()),
                Err(Error::Corrupt(_))
            ));
        }
    }

    #[test]
    fn short_buffers_are_not_a_superblock() {
        assert_eq!(
            Superblock::decode(&[0u8; 10]).unwrap_err(),
            Error::BadSuperblock
        );
        assert_eq!(
            Superblock::decode(&sb().encode()[..400]).unwrap_err(),
            Error::BadSuperblock
        );
    }

    #[test]
    fn label_stops_at_the_first_nul() {
        let mut s = sb();
        s.label[..7].copy_from_slice(b"ECLIPSE");
        assert_eq!(s.label(), b"ECLIPSE");
        s.label = [b'x'; 64];
        assert_eq!(s.label().len(), 64);
    }

    #[test]
    fn group_geometry() {
        let mut s = sb();
        s.total_blocks = GROUP_BLOCKS;
        assert_eq!(s.groups(), (1, GROUP_BLOCKS));
        s.total_blocks = GROUP_BLOCKS + 100;
        assert_eq!(s.groups(), (2, 100));
        s.total_blocks = 3 * GROUP_BLOCKS;
        assert_eq!(s.groups(), (3, GROUP_BLOCKS));
    }

    #[test]
    fn every_inode_field_survives() {
        let mut i = Inode::new(FileKind::BlockDevice, 0o660, (1, 2));
        i.uid = 1000;
        i.gid = 6;
        i.nlink = 3;
        i.size = u64::MAX - 1;
        i.nbytes = 12345 * 4096;
        i.rdev = 0x0803;
        i.atime = (u64::MAX, 999_999_999);
        i.mtime = (5, 6);
        i.ctime = (7, 8);
        i.parent = 77;
        i.chain = vec![555];
        let (b, overflow, _) = Inode::decode(&i.encode()).unwrap().unwrap();
        assert_eq!(overflow, 555);
        assert_eq!(b.kind, FileKind::BlockDevice);
        assert_eq!(b.mode, S_IFBLK | 0o660);
        assert_eq!((b.uid, b.gid, b.nlink), (1000, 6, 3));
        assert_eq!(
            (b.size, b.nbytes, b.rdev),
            (u64::MAX - 1, 12345 * 4096, 0x0803)
        );
        assert_eq!(
            (b.atime, b.mtime, b.ctime),
            ((u64::MAX, 999_999_999), (5, 6), (7, 8))
        );
        assert_eq!(b.parent, 77);
    }

    #[test]
    fn only_six_extents_live_in_the_record() {
        let mut i = Inode::new(FileKind::Regular, 0o644, (0, 0));
        for n in 0..9u64 {
            i.extents
                .add_extent(Extent::new(n * 2, 100 + n * 10, 1))
                .unwrap();
        }
        let (b, _, count) = Inode::decode(&i.encode()).unwrap().unwrap();
        assert_eq!(count, 9);
        assert_eq!(b.extents.extent_count(), INLINE_EXTENTS);
        assert_eq!(b.extents.extents(), &i.extents.extents()[..INLINE_EXTENTS]);
    }

    #[test]
    fn the_inline_area_holds_152_bytes() {
        let mut i = Inode::new(FileKind::Symlink, 0o777, (0, 0));
        i.flags = INODE_FLAG_INLINE;
        i.inline = vec![b'z'; INLINE_LEN];
        i.size = INLINE_LEN as u64;
        let (b, _, _) = Inode::decode(&i.encode()).unwrap().unwrap();
        assert_eq!(b.inline.len(), INLINE_LEN);
        // A size claiming more than fits is corruption, not an overflow.
        let mut rec = i.encode();
        rec[24..32].copy_from_slice(&(INLINE_LEN as u64 + 1).to_le_bytes());
        let crc = crc32(&rec[..INODE_SIZE - 4]);
        rec[INODE_SIZE - 4..].copy_from_slice(&crc.to_le_bytes());
        assert!(Inode::decode(&rec).is_err());
    }

    #[test]
    fn unknown_inode_kind_is_corruption() {
        let mut rec = Inode::new(FileKind::Regular, 0o644, (0, 0)).encode();
        rec[0] = 42;
        let crc = crc32(&rec[..INODE_SIZE - 4]);
        rec[INODE_SIZE - 4..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(
            Inode::decode(&rec).unwrap_err(),
            Error::Corrupt("inode kind")
        );
    }

    #[test]
    fn overlapping_extents_in_a_record_are_corruption() {
        let mut i = Inode::new(FileKind::Regular, 0o644, (0, 0));
        i.extents.add_extent(Extent::new(0, 100, 5)).unwrap();
        i.extents.add_extent(Extent::new(10, 300, 5)).unwrap();
        let mut rec = i.encode();
        // Second extent's logical start: 96 + 24.
        rec[120..128].copy_from_slice(&2u64.to_le_bytes());
        let crc = crc32(&rec[..INODE_SIZE - 4]);
        rec[INODE_SIZE - 4..].copy_from_slice(&crc.to_le_bytes());
        assert!(Inode::decode(&rec).is_err());
    }

    #[test]
    fn extent_blocks_reject_tampering() {
        let ext = vec![Extent::new(0, 9, 1)];
        let mut b = encode_extent_block(5, 0, &ext);
        assert!(decode_extent_block(5, &b).is_ok());
        b[100] ^= 1;
        assert_eq!(
            decode_extent_block(5, &b).unwrap_err(),
            Error::Corrupt("extent block checksum")
        );
        let mut c = encode_extent_block(5, 0, &ext);
        c[0] = 0;
        assert!(decode_extent_block(5, &c).is_err());
        let mut d = encode_extent_block(5, 0, &ext);
        d[4..8].copy_from_slice(&(EXTENTS_PER_BLOCK as u32 + 1).to_le_bytes());
        assert!(decode_extent_block(5, &d).is_err());
        assert_eq!(EXTENTS_PER_BLOCK, 169);
    }

    #[test]
    fn empty_extent_block() {
        let b = encode_extent_block(3, 0, &[]);
        assert_eq!(decode_extent_block(3, &b).unwrap(), (0, vec![]));
    }

    #[test]
    fn dirent_with_the_longest_name() {
        let name = [b'n'; MAX_NAME_LEN];
        let d = encode_dirent(1 << 50, FileKind::Fifo, &name);
        assert_eq!(d.len(), DIRENT_HDR + MAX_NAME_LEN);
        let p = parse_dirents(&d).unwrap();
        assert_eq!(p[0].ino, 1 << 50);
        assert_eq!(p[0].kind, Some(FileKind::Fifo));
        assert_eq!(p[0].name.len(), MAX_NAME_LEN);
    }

    #[test]
    fn malformed_dirents_are_corruption() {
        // rec_len shorter than its own header.
        let mut d = encode_dirent(4, FileKind::Regular, b"a");
        d[8..10].copy_from_slice(&3u16.to_le_bytes());
        assert!(parse_dirents(&d).is_err());
        // A live entry with an unknown kind.
        let mut d = encode_dirent(4, FileKind::Regular, b"a");
        d[10] = 99;
        assert!(parse_dirents(&d).is_err());
        // A live entry with an empty name.
        let d = encode_dirent(4, FileKind::Regular, b"");
        assert!(parse_dirents(&d).is_err());
        // A trailing fragment.
        let mut d = encode_dirent(4, FileKind::Regular, b"a");
        d.extend_from_slice(&[0, 0, 0]);
        assert!(parse_dirents(&d).is_err());
        assert!(parse_dirents(&[]).unwrap().is_empty());
    }
}
