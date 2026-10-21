//! Reads through `&self`, so that several can run at once.
//!
//! Everything else in [`CowFs`] takes `&mut self`: lookups fill the node
//! cache, a subvolume switch swaps roots, a bad block read from one copy is
//! rewritten from another. The calls here touch none of that. They read the
//! node cache as it stands, keep the nodes they fetch in a side cache of
//! their own (`Store::get_shared`), and leave everything that would write or
//! needs state they cannot reach to the exclusive path by answering
//! `Ok(None)`:
//!
//! - an inode of a subvolume whose root is not at hand;
//! - compressed extents (they decompress into the one-entry `zcache`);
//! - encrypted files whose keys are not derived yet;
//! - a block that fails its checksum (the exclusive read repairs it from
//!   another copy, or reports it).
//!
//! The caller holds the filesystem shared (a read lock), so nothing changes
//! underneath; an `Ok(None)` means "ask again holding it exclusively", and
//! gives the same answer the exclusive call would have.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use super::{stat_of, CowFs, Ext, BS, WRITE_CHUNK};
use crate::btree::{Key, Root};
use crate::cowfmt::*;
use crate::crc::crc32c;
use crate::crypt::FileKeys;
use crate::format::FileKind;
use crate::fs::InodeStat;
use crate::{Error, Result};

impl CowFs {
    /// The tree root of subvolume `sv`, if this side of the lock has it.
    fn root_shared(&self, sv: u64) -> Option<(&Root, &RootItem)> {
        if sv == self.subvol {
            Some((&self.fs_root, &self.fs_item))
        } else {
            self.subvols.get(&sv).map(|(r, i)| (r, i))
        }
    }

    fn inode_shared(&self, root: &Root, item: &RootItem, ino: u64) -> Result<InodeItem> {
        if ino < FIRST_INO || ino >= item.next_ino {
            return Err(Error::NotFound);
        }
        let d = self
            .st
            .get_shared(root, &Key::new(ino, INODE_ITEM, 0))?
            .ok_or(Error::NotFound)?;
        InodeItem::decode(&d)
    }

    /// [`stat`](Self::stat) through `&self`; `None` when the inode's
    /// subvolume is not loaded.
    pub fn stat_shared(&self, h: u64) -> Result<Option<InodeStat>> {
        let (sv, ino) = split(h);
        let Some((root, item)) = self.root_shared(sv) else {
            return Ok(None);
        };
        let i = self.inode_shared(root, item, ino)?;
        let mut s = stat_of(ino, &i);
        s.ino = h;
        Ok(Some(s))
    }

    /// [`read`](Self::read) through `&self`; `None` when this read needs
    /// the exclusive path (see the module documentation).
    pub fn read_shared(&self, h: u64, offset: u64, buf: &mut [u8]) -> Result<Option<usize>> {
        let (sv, ino) = split(h);
        let Some((root, item)) = self.root_shared(sv) else {
            return Ok(None);
        };
        let i = self.inode_shared(root, item, ino)?;
        if i.kind() == FileKind::Dir {
            return Err(Error::IsDir);
        }
        if offset >= i.size || buf.is_empty() {
            return Ok(Some(0));
        }
        let keys = if i.flags & FL_ENCRYPT as u64 != 0 {
            match self.keyring.files.get(&(sv, ino)) {
                Some((k, _)) => Some(k.clone()),
                None => return Ok(None),
            }
        } else {
            None
        };
        let n = (buf.len() as u64).min(i.size - offset) as usize;
        let buf = &mut buf[..n];
        if i.size <= INLINE_MAX as u64 {
            if let Some(d) = self.st.get_shared(root, &Self::ext_key(ino, 0))? {
                if let FileExtent::Inline(v) = FileExtent::decode(&d)? {
                    let o = offset as usize;
                    let avail = v.len().saturating_sub(o).min(n);
                    buf[..avail].copy_from_slice(&v[o..o + avail]);
                    buf[avail..].fill(0);
                    return Ok(Some(n));
                }
            }
        }
        let sums = i.flags & FL_NOCOW as u64 == 0;
        let done = self.read_blocks_shared(root, ino, sums, keys, offset, buf)?;
        Ok(done.then_some(n))
    }

    /// The extents of `ino` overlapping file blocks `from..to`, at most
    /// `limit` past the first (see `extents_upto`); `None` if one is
    /// compressed.
    fn extents_shared(
        &self,
        root: &Root,
        ino: u64,
        from: u64,
        to: u64,
        limit: usize,
    ) -> Result<Option<Vec<Ext>>> {
        let mut out = Vec::new();
        let first = self.st.seek_le_shared(root, &Self::ext_key(ino, from))?;
        let first = first.filter(|(k, _)| k.objectid == ino && k.ty == EXTENT_DATA);
        let lo = Key::new(ino, EXTENT_DATA, from * BS + 1);
        let hi = Key::new(ino, EXTENT_DATA, to * BS - 1);
        let rest = self.st.range_shared(root, &lo, &hi, limit)?;
        for (k, d) in first.into_iter().chain(rest) {
            let (disk, blocks, prealloc) = match FileExtent::decode(&d)? {
                FileExtent::Reg { disk, blocks } => (disk, blocks, false),
                FileExtent::Prealloc { disk, blocks } => (disk, blocks, true),
                FileExtent::Comp(_) => return Ok(None),
                FileExtent::Inline(_) => continue,
            };
            if k.offset % BS != 0 {
                return Err(Error::Corrupt("unaligned file extent"));
            }
            let e = Ext {
                lblk: k.offset / BS,
                pblk: disk,
                blocks,
                prealloc,
                z: None,
            };
            if e.lblk + e.blocks > from && e.lblk < to {
                out.push(e);
            }
        }
        Ok(Some(out))
    }

    /// `read_blocks_with` through `&self`, a batch of extents at a time;
    /// `false` when the exclusive path has to do it.
    fn read_blocks_shared(
        &self,
        root: &Root,
        ino: u64,
        sums: bool,
        keys: Option<Arc<FileKeys>>,
        off: u64,
        buf: &mut [u8],
    ) -> Result<bool> {
        let end = off + buf.len() as u64;
        let last = end.div_ceil(BS);
        let mut zeroed_to = off;
        let mut cur = off / BS;
        while cur < last {
            let Some(batch) = self.extents_shared(root, ino, cur, last, super::EXTENT_BATCH)?
            else {
                return Ok(false);
            };
            let Some(tail) = batch.last() else {
                break;
            };
            cur = tail.lblk + tail.blocks;
            for e in batch {
                if e.prealloc {
                    continue;
                }
                let s = (e.lblk * BS).max(off);
                let t = ((e.lblk + e.blocks) * BS).min(end);
                // Extents in the batch overlap the blocks of `off..end`.
                debug_assert!(s < t);
                if s > zeroed_to {
                    buf[(zeroed_to - off) as usize..(s - off) as usize].fill(0);
                }
                zeroed_to = zeroed_to.max(t);
                let mut pos = s;
                while pos < t {
                    let b0 = pos / BS;
                    let b1 = t.div_ceil(BS).min(b0 + WRITE_CHUNK);
                    let upto = (b1 * BS).min(t);
                    let p0 = e.pblk + (b0 - e.lblk);
                    let dst = &mut buf[(pos - off) as usize..(upto - off) as usize];
                    if pos.is_multiple_of(BS) && upto.is_multiple_of(BS) {
                        if !self.read_data_shared(p0, dst, sums)? {
                            return Ok(false);
                        }
                        if let Some(k) = &keys {
                            k.decrypt_blocks(b0, dst, BS as usize);
                        }
                    } else {
                        let mut tmp = vec![0u8; ((b1 - b0) * BS) as usize];
                        if !self.read_data_shared(p0, &mut tmp, sums)? {
                            return Ok(false);
                        }
                        if let Some(k) = &keys {
                            k.decrypt_blocks(b0, &mut tmp, BS as usize);
                        }
                        let o = (pos - b0 * BS) as usize;
                        let len = dst.len();
                        dst.copy_from_slice(&tmp[o..o + len]);
                    }
                    pos = upto;
                }
            }
        }
        if zeroed_to < end {
            buf[(zeroed_to - off) as usize..].fill(0);
        }
        Ok(true)
    }

    /// Disk blocks from `pblk` on into `buf`, checked if `sums`; `false`
    /// if a block fails (the exclusive read repairs or reports it).
    fn read_data_shared(&self, pblk: u64, buf: &mut [u8], sums: bool) -> Result<bool> {
        self.raw.read_at(pblk * BS, buf)?;
        if !sums {
            return Ok(true);
        }
        let want = self.csums_shared(pblk, buf.len() as u64 / BS)?;
        Ok(want
            .into_iter()
            .zip(buf.chunks(BS as usize))
            .all(|(c, b)| c == Some(crc32c(b))))
    }

    /// `csums` through `&self`.
    fn csums_shared(&self, start: u64, n: u64) -> Result<Vec<Option<u32>>> {
        let mut out = vec![None; n as usize];
        let mut bucket = start / CSUM_BUCKET * CSUM_BUCKET;
        while bucket < start + n {
            if let Some(d) = self
                .st
                .get_shared(&self.csum_root, &Key::new(bucket, CSUM_ITEM, 0))?
            {
                let from = start.max(bucket);
                let to = (start + n).min(bucket + CSUM_BUCKET);
                for b in from..to {
                    out[(b - start) as usize] = CsumBucket::lookup(&d, (b - bucket) as usize)?;
                }
            }
            bucket += CSUM_BUCKET;
        }
        for (s, l) in self.sp.csums_freed.within(start, start + n) {
            for b in s..s + l {
                out[(b - start) as usize] = None;
            }
        }
        for (&b, &c) in self.sp.csums.range(start..start + n) {
            out[(b - start) as usize] = c;
        }
        Ok(out)
    }
}
