use core::mem;

use alloc::vec::Vec;

use error::Error;
use sector::{Address, SectorSize};
use sys::block_group::BlockGroupDescriptor;
use sys::inode::Inode as RawInode;
use sys::superblock::Superblock;
use volume::Volume;

pub mod sync;

pub(crate) struct Struct<T, S: SectorSize> {
    pub inner: T,
    pub _offset: Address<S>,
}

impl<T, S: SectorSize> From<(T, Address<S>)> for Struct<T, S> {
    #[inline]
    fn from((inner, offset): (T, Address<S>)) -> Struct<T, S> {
        Struct {
            inner,
            _offset: offset,
        }
    }
}

/// Safe wrapper for raw sys structs
pub struct Ext2<S: SectorSize, V: Volume<u8, S>> {
    pub(crate) volume: V,
    pub(crate) superblock: Struct<Superblock, S>,
    pub(crate) block_groups: Struct<Vec<BlockGroupDescriptor>, S>,
}

impl<S: SectorSize, V: Volume<u8, S>> Ext2<S, V> {
    pub fn new(volume: V) -> Result<Ext2<S, V>, Error> {
        let superblock = unsafe { Struct::from(Superblock::find(&volume)?) };
        let block_groups_offset = Address::with_block_size(
            superblock.inner.first_data_block + 1,
            0,
            superblock.inner.log_block_size + 10,
        );
        let block_groups_count = superblock
            .inner
            .block_group_count()
            .map(|count| count as usize)
            .map_err(|(a, b)| Error::BadBlockGroupCount {
                by_blocks: a,
                by_inodes: b,
            })?;
        const MAX_BLOCK_GROUPS: usize = 8192;
        if block_groups_count > MAX_BLOCK_GROUPS {
            return Err(Error::Other(alloc::string::String::from(
                "block group count exceeds sanity limit",
            )));
        }
        let block_groups = unsafe {
            BlockGroupDescriptor::find_descriptor_table(
                &volume,
                block_groups_offset,
                block_groups_count,
            )?
        };
        let block_groups = Struct::from(block_groups);
        Ok(Ext2 {
            volume,
            superblock,
            block_groups,
        })
    }

    pub fn version(&self) -> (u32, u16) {
        (
            self.superblock.inner.rev_major,
            self.superblock.inner.rev_minor,
        )
    }

    pub fn inode_size(&self) -> usize {
        if self.version().0 == 0 {
            mem::size_of::<RawInode>()
        } else {
            self.superblock.inner.inode_size as usize
        }
    }

    pub fn inodes_count(&self) -> usize {
        self.superblock.inner.inodes_per_group as _
    }

    pub fn total_inodes_count(&self) -> usize {
        self.superblock.inner.inodes_count as _
    }

    pub fn block_group_count(&self) -> Result<usize, Error> {
        self.superblock
            .inner
            .block_group_count()
            .map(|count| count as usize)
            .map_err(|(a, b)| Error::BadBlockGroupCount {
                by_blocks: a,
                by_inodes: b,
            })
    }

    pub fn total_block_count(&self) -> usize {
        self.superblock.inner.blocks_count as _
    }

    pub fn free_block_count(&self) -> usize {
        self.superblock.inner.free_blocks_count as _
    }

    pub fn free_blocks_count_raw(&self) -> u32 {
        self.superblock.inner.free_blocks_count
    }

    pub fn free_inodes_count_raw(&self) -> u32 {
        self.superblock.inner.free_inodes_count
    }

    pub fn block_size(&self) -> usize {
        self.superblock.inner.block_size()
    }

    pub fn log_block_size(&self) -> u32 {
        self.superblock.inner.log_block_size + 10
    }

    pub fn first_data_block(&self) -> u32 {
        self.superblock.inner.first_data_block
    }

    /// First inode usable for regular files/dirs. Inodes below this are
    /// reserved (root is 2). For old-revision filesystems the on-disk field
    /// may be 0, so callers should floor it at `EXT2_GOOD_OLD_FIRST_INO` (11).
    pub fn first_inode(&self) -> u32 {
        self.superblock.inner.first_inode
    }

    pub fn blocks_per_group(&self) -> u32 {
        self.superblock.inner.blocks_per_group
    }

    pub fn block_groups_len(&self) -> usize {
        self.block_groups.inner.len()
    }

    pub fn block_group(&self, index: usize) -> BlockGroupDescriptor {
        self.block_groups.inner[index]
    }

    pub fn block_group_mut(
        &mut self,
        index: usize,
    ) -> &mut BlockGroupDescriptor {
        &mut self.block_groups.inner[index]
    }

    pub fn superblock_mut(&mut self) -> &mut Superblock {
        &mut self.superblock.inner
    }

    pub fn sector_size(&self) -> usize {
        S::SIZE
    }

    pub fn log_sector_size(&self) -> u32 {
        S::LOG_SIZE
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs::File;

    use sector::{Address, Size512};
    use volume::Volume;

    use super::Ext2;

    #[test]
    fn file_len() {
        let file = RefCell::new(File::open("ext2.img").unwrap());
        assert_eq!(
            Address::<Size512>::from(2048_u64)
                - Address::<Size512>::from(1024_u64),
            Address::<Size512>::new(2, 0)
        );
        assert_eq!(
            unsafe {
                file.slice_unchecked(
                    Address::<Size512>::from(1024_u64)
                        ..Address::<Size512>::from(2048_u64),
                )
                .len()
            },
            1024
        );
    }

    #[test]
    fn file() {
        let file = RefCell::new(File::open("ext2.img").unwrap());
        let fs = Ext2::<Size512, _>::new(file);

        assert!(
            fs.is_ok(),
            "Err({:?})",
            fs.err().unwrap_or_else(|| unreachable!()),
        );

        let fs = fs.unwrap();

        let vers = fs.version();
        println!("version: {}.{}", vers.0, vers.1);
        assert_eq!(128, fs.inode_size());
    }
}
