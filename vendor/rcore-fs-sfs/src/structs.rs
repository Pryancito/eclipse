//! On-disk structures in SFS

use crate::vfs;
use alloc::str;

use core::fmt::{Debug, Error, Formatter};
use core::mem::{size_of, size_of_val};
use core::slice;
use rcore_fs::vfs::Timespec;
use static_assertions::const_assert;

/// On-disk superblock
#[repr(C)]
#[derive(Debug)]
pub struct SuperBlock {
    /// magic number, should be SFS_MAGIC
    pub magic: u32,
    /// number of blocks in fs
    pub blocks: u32,
    /// number of unused blocks in fs
    pub unused_blocks: u32,
    /// information for sfs
    pub info: Str32,
    /// number of freemap blocks
    pub freemap_blocks: u32,
}

/// inode (on disk)
#[repr(C)]
#[derive(Debug)]
pub struct DiskINode {
    /// size of the file (in bytes)
    /// undefined in dir (256 * #entries ?)
    pub size: u32,
    /// one of SYS_TYPE_* above
    pub type_: FileType,
    /// number of hard links to this file
    /// Note: "." and ".." is counted in this nlinks
    pub nlinks: u16,
    /// number of blocks
    pub blocks: u32,
    /// direct blocks
    pub direct: [u32; NDIRECT],
    /// indirect blocks
    pub indirect: u32,
    /// double indirect blocks
    pub db_indirect: u32,
    /// device inode id for char/block device (major, minor)
    pub device_inode_id: usize,
    /// Time of last access
    pub atime: Timespec,
    /// Time of last modification
    pub mtime: Timespec,
    /// Time of last change
    pub ctime: Timespec,
}

/*
pub trait DeviceINode : Any + Sync + Send{
    fn read_at(&self, _offset: usize, buf: &mut [u8]) -> vfs::Result<usize>;
    fn write_at(&self, _offset: usize, buf: &[u8]) -> vfs::Result<usize>;
}
*/

pub type DeviceINode = dyn vfs::INode;

#[repr(C)]
pub struct IndirectBlock {
    pub entries: [u32; BLK_NENTRY],
}

/// file entry (on disk)
#[repr(C)]
#[derive(Debug)]
pub struct DiskEntry {
    /// inode number
    pub id: u32,
    /// file name
    pub name: Str256,
}

#[repr(C)]
pub struct Str256(pub [u8; 256]);

#[repr(C)]
pub struct Str32(pub [u8; 32]);

impl AsRef<str> for Str256 {
    fn as_ref(&self) -> &str {
        let len = self.0.iter().enumerate().find(|(_, &b)| b == 0).unwrap().0;
        str::from_utf8(&self.0[0..len]).unwrap()
    }
}

impl AsRef<str> for Str32 {
    fn as_ref(&self) -> &str {
        let len = self.0.iter().enumerate().find(|(_, &b)| b == 0).unwrap().0;
        str::from_utf8(&self.0[0..len]).unwrap()
    }
}

impl Debug for Str256 {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        write!(f, "{}", self.as_ref())
    }
}

impl Debug for Str32 {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        write!(f, "{}", self.as_ref())
    }
}

impl<'a> From<&'a str> for Str256 {
    fn from(s: &'a str) -> Self {
        let mut ret = [0u8; 256];
        ret[0..s.len()].copy_from_slice(s.as_ref());
        Str256(ret)
    }
}

impl<'a> From<&'a str> for Str32 {
    fn from(s: &'a str) -> Self {
        let mut ret = [0u8; 32];
        ret[0..s.len()].copy_from_slice(s.as_ref());
        Str32(ret)
    }
}

impl SuperBlock {
    pub fn check(&self) -> bool {
        self.magic == MAGIC
    }
}

impl DiskINode {
    pub const fn new_file() -> Self {
        DiskINode {
            size: 0,
            type_: FileType::File,
            nlinks: 0,
            blocks: 0,
            direct: [0; NDIRECT],
            indirect: 0,
            db_indirect: 0,
            device_inode_id: NODEVICE,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
        }
    }
    pub const fn new_symlink() -> Self {
        DiskINode {
            size: 0,
            type_: FileType::SymLink,
            nlinks: 0,
            blocks: 0,
            direct: [0; NDIRECT],
            indirect: 0,
            db_indirect: 0,
            device_inode_id: NODEVICE,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
        }
    }
    pub const fn new_dir() -> Self {
        DiskINode {
            size: 0,
            type_: FileType::Dir,
            nlinks: 0,
            blocks: 0,
            direct: [0; NDIRECT],
            indirect: 0,
            db_indirect: 0,
            device_inode_id: NODEVICE,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
        }
    }
    pub const fn new_chardevice(device_inode_id: usize) -> Self {
        DiskINode {
            size: 0,
            type_: FileType::CharDevice,
            nlinks: 0,
            blocks: 0,
            direct: [0; NDIRECT],
            indirect: 0,
            db_indirect: 0,
            device_inode_id,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
        }
    }
}

/// Convert structs to [u8] slice
pub trait AsBuf {
    fn as_buf(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self as *const _ as *const u8, size_of_val(self)) }
    }
    fn as_buf_mut(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self as *mut _ as *mut u8, size_of_val(self)) }
    }
}

impl AsBuf for SuperBlock {}

impl AsBuf for DiskINode {}

impl AsBuf for DiskEntry {}

impl AsBuf for u32 {}

/*
 * Simple FS (SFS) definitions visible to ucore. This covers the on-disk format
 * and is used by tools that work on SFS volumes, such as mksfs.
 */
pub type BlockId = usize;
pub type INodeId = BlockId;

pub const NODEVICE: usize = 100;

/// magic number for sfs
pub const MAGIC: u32 = 0x2f8dbe2b;
/// size of block
pub const BLKSIZE: usize = 1usize << BLKSIZE_LOG2;
/// log2( size of block )
pub const BLKSIZE_LOG2: u8 = 12;
/// number of direct blocks in inode
pub const NDIRECT: usize = 12;
/// default sfs infomation string
pub const DEFAULT_INFO: &str = "simple file system";
/// max length of infomation
pub const MAX_INFO_LEN: usize = 31;
/// max length of filename
pub const MAX_FNAME_LEN: usize = 255;
/// max file size in theory (48KB + 4MB + 4GB)
/// however, the file size is stored in u32
pub const MAX_FILE_SIZE: usize = 0xffffffff;
/// block the superblock lives in
pub const BLKN_SUPER: BlockId = 0;
/// location of the root dir inode
pub const BLKN_ROOT: BlockId = 1;
/// 1st block of the freemap
pub const BLKN_FREEMAP: BlockId = 2;
/// number of bits in a block
pub const BLKBITS: usize = BLKSIZE * 8;
/// size of one entry
pub const ENTRY_SIZE: usize = 4;
/// number of entries in a block
pub const BLK_NENTRY: usize = BLKSIZE / ENTRY_SIZE;
/// size of a dirent used in the size field
pub const DIRENT_SIZE: usize = MAX_FNAME_LEN + 1 + ENTRY_SIZE;
/// max number of blocks with direct blocks
pub const MAX_NBLOCK_DIRECT: usize = NDIRECT;
/// max number of blocks with indirect blocks
pub const MAX_NBLOCK_INDIRECT: usize = NDIRECT + BLK_NENTRY;
/// max number of blocks with double indirect blocks
pub const MAX_NBLOCK_DOUBLE_INDIRECT: usize = NDIRECT + BLK_NENTRY + BLK_NENTRY * BLK_NENTRY;

/// file types
#[repr(u16)]
#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub enum FileType {
    Invalid = 0,
    File = 1,
    Dir = 2,
    SymLink = 3,
    CharDevice = 4,
    BlockDevice = 5,
}

const_assert!(size_of::<SuperBlock>() <= BLKSIZE);
const_assert!(size_of::<DiskINode>() <= BLKSIZE);
const_assert!(size_of::<DiskEntry>() <= BLKSIZE);
const_assert!(size_of::<IndirectBlock>() == BLKSIZE);
const_assert!(DEFAULT_INFO.len() <= MAX_INFO_LEN);
#[cfg(test)]
mod layout_tests {
    //! The on-disk format, written out byte by byte.
    //!
    //! `as_buf` hands a `SuperBlock` or a `DiskINode` to the block device as
    //! a raw byte slice, so the order of the fields *is* the format: an image
    //! written by one build has to stay readable by the next. What guarded
    //! that was these two lines at the bottom of the file:
    //!
    //! ```text
    //! const_assert!(size_of::<SuperBlock>() <= BLKSIZE);
    //! const_assert!(size_of::<DiskINode>() <= BLKSIZE);
    //! ```
    //!
    //! An inequality against 4096, which every conceivable layout of these
    //! two satisfies. Measured over the crate's suite: swapping `magic` with
    //! `blocks`, or `atime` with `mtime`, passes all eighteen tests.
    //!
    //! And the suite is not weak, it is structurally unable to see it. Every
    //! test that runs creates its own filesystem and reads it back with the
    //! same code, which agrees with itself whatever the order is. The five
    //! that open an image made elsewhere -- `open_sample_file` and the four
    //! `kernel_image_file_*` -- are the ones that would notice, and all five
    //! are `#[ignore]`d because `sfs.img` is not in the tree. So an
    //! assertion on the bytes is the only oracle there is.
    //!
    //! The numbers are the 64-bit ones. `device_inode_id` is a `usize` inside
    //! an on-disk struct, so a 32-bit build writes a different format
    //! altogether; that is worth knowing and is not something a test fixes.

    use super::*;
    use core::mem::{align_of, offset_of};

    #[test]
    fn the_superblock_is_forty_eight_bytes_in_a_fixed_order() {
        assert_eq!(size_of::<SuperBlock>(), 48);
        assert_eq!(align_of::<SuperBlock>(), 4);
        assert_eq!(offset_of!(SuperBlock, magic), 0);
        assert_eq!(offset_of!(SuperBlock, blocks), 4);
        assert_eq!(offset_of!(SuperBlock, unused_blocks), 8);
        assert_eq!(offset_of!(SuperBlock, info), 12);
        // The info string is thirty-two bytes of it, and the freemap count
        // follows on from the end of it.
        assert_eq!(offset_of!(SuperBlock, freemap_blocks), 44);
        assert_eq!(offset_of!(SuperBlock, freemap_blocks) + 4, 48);
    }

    #[test]
    fn the_inode_is_a_hundred_and_twenty_eight_bytes_in_a_fixed_order() {
        assert_eq!(size_of::<DiskINode>(), 128);
        assert_eq!(align_of::<DiskINode>(), 8);
        assert_eq!(offset_of!(DiskINode, size), 0);
        assert_eq!(offset_of!(DiskINode, type_), 4);
        assert_eq!(offset_of!(DiskINode, nlinks), 6);
        assert_eq!(offset_of!(DiskINode, blocks), 8);
        assert_eq!(offset_of!(DiskINode, direct), 12);
        // Twelve direct block numbers, four bytes each, so the first
        // indirect one lands at 12 + 48.
        assert_eq!(offset_of!(DiskINode, indirect), 60);
        assert_eq!(offset_of!(DiskINode, db_indirect), 64);
        // Bytes 68..72 are padding: `device_inode_id` is a usize and has to
        // be eight-aligned.
        assert_eq!(offset_of!(DiskINode, device_inode_id), 72);
        assert_eq!(offset_of!(DiskINode, atime), 80);
        assert_eq!(offset_of!(DiskINode, mtime), 96);
        assert_eq!(offset_of!(DiskINode, ctime), 112);
        // Which accounts for all 128: the last timestamp is sixteen wide and
        // ends exactly at the end of the struct.
        assert_eq!(offset_of!(DiskINode, ctime) + 16, 128);
    }

    /// The three timestamps are `rcore_fs::vfs::Timespec`, which is **not**
    /// `#[repr(C)]`: `repr(Rust)` may order its two fields as it likes, and
    /// it happens to put the seconds first. Three of them go straight to
    /// disk inside every inode, so this is the only thing between the format
    /// and that freedom.
    #[test]
    fn the_timestamps_are_a_signed_second_and_then_a_nanosecond() {
        assert_eq!(size_of::<Timespec>(), 16);
        assert_eq!(align_of::<Timespec>(), 8);
        assert_eq!(offset_of!(Timespec, sec), 0);
        assert_eq!(offset_of!(Timespec, nsec), 8);
    }

    /// `FileType` is the one field of an inode that is not an integer, and
    /// its `#[repr(u16)]` is what makes it two bytes in the right place.
    /// The numbers are on disk too, so they are written out here.
    #[test]
    fn the_file_type_is_two_bytes_and_its_numbers_are_the_on_disk_ones() {
        assert_eq!(size_of::<FileType>(), 2);
        assert_eq!(FileType::Invalid as u16, 0);
        assert_eq!(FileType::File as u16, 1);
        assert_eq!(FileType::Dir as u16, 2);
        assert_eq!(FileType::SymLink as u16, 3);
        assert_eq!(FileType::CharDevice as u16, 4);
        assert_eq!(FileType::BlockDevice as u16, 5);
    }

    /// And the two string newtypes, which are what keeps a name or an info
    /// string a fixed-width field on disk instead of a pointer.
    #[test]
    fn the_name_and_info_strings_are_fixed_width_byte_arrays() {
        assert_eq!(size_of::<Str32>(), 32);
        assert_eq!(align_of::<Str32>(), 1);
        assert_eq!(size_of::<Str256>(), 256);
        assert_eq!(align_of::<Str256>(), 1);
        // Which is what makes a directory entry 260 bytes: an inode number
        // and a name, with nothing between them.
        assert_eq!(size_of::<DiskEntry>(), 260);
        assert_eq!(offset_of!(DiskEntry, id), 0);
        assert_eq!(offset_of!(DiskEntry, name), 4);
    }
}
