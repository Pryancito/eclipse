//! The fsync log: `fsync` of one file without committing the transaction,
//! as btrfs's tree-log or ext4's fast commits do.
//!
//! A commit writes every tree node the transaction changed and goes through
//! the journal, with three device flushes; `fsync` of a file that only had
//! its data or attributes changed needs much less. It writes one record to
//! the log area with what changed in that file since the last commit or
//! record (its inode item, and the extents of the file ranges that were
//! rewritten, with the CRC32C of every data block), and flushes once.
//!
//! ```text
//! record header (one block)
//! 0   "EFSL"            4   version             8   filesystem uuid [16]
//! 24  base generation   32  sequence number     40  payload blocks
//! 44  flags             48  payload bytes       56  CRC32C of the payload
//! 60  CRC32C of bytes 0..60
//! payload (the blocks after the header)
//!     subvolume u64, inode u64, inode item (len u32 + bytes), then tagged
//!     entries: 1 = range (first, end file block), 2 = extent (file block,
//!     disk block, blocks, prealloc u8, with sums u8, then a u32 CRC32C per
//!     block when with sums), 0 = end.
//! ```
//!
//! A record only counts on top of the committed state it was written
//! against (base generation) and in sequence. At mount the valid records
//! are read in order, each checked against its own CRC and the CRC32C of
//! every data block it points at: a record torn by a power cut fails one of
//! them and ends the log, so data and record need not be flushed apart. The
//! surviving records are merged per inode (the later one wins on each file
//! block), their blocks are claimed, the ranges are unmapped and remapped as
//! a write would, the inode items put back, and the result is committed.
//!
//! What may go to the log, and what keeps it safe:
//! - Only regular files whose changes since the last commit are their
//!   attributes and extents set up by writes, truncates and `fallocate`
//!   ([`CowFs::map`]/[`CowFs::unmap`]), with blocks this transaction
//!   allocated and nobody else shares. Anything else (a new inode, a name
//!   change, xattrs, flags, inline data, compression, reflinks...) marks the
//!   inode "full": its `fsync` commits. Directories always commit.
//! - Blocks a record points at stop being "fresh": they are never rewritten
//!   in place afterwards and a free only takes effect at commit, so their
//!   contents stay what the record's checksums describe until the next
//!   commit makes the log obsolete.
//! - The log area is a preallocated extent owned by the root tree (object
//!   [`LOG_TREE`]): fsck, the bitmap rebuild and relocation see it as any
//!   other owned block run. Resizing and balancing drop it first; the next
//!   `fsync` makes a new one.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use super::{CowFs, BS, EXTENT_BATCH};
use crate::btree::{Key, Space};
use crate::cowfmt::*;
use crate::crc::{crc32c, crc32c_update};
use crate::device::BlockDevice;
use crate::format::{le32, le64, put32, put64, FileKind, GROUP_BLOCKS};
use crate::space::RangeSet;
use crate::volume::ChunkKind;
use crate::{Error, Result};

const LOG_MAGIC: u32 = u32::from_le_bytes(*b"EFSL");
const LOG_VERSION: u32 = 1;
const HDR_CRC: usize = 60;
/// The record holds blocks without checksums (no-COW files): data was
/// flushed before the header was written.
const FLAG_UNSUMMED: u32 = 1;

const TAG_END: u8 = 0;
const TAG_RANGE: u8 = 1;
const TAG_EXTENT: u8 = 2;

/// Blocks of the log area a filesystem of `total` blocks gets.
pub(super) fn area_blocks(total: u64) -> u64 {
    (total / 256).clamp(64, 4096)
}

/// What changed in one inode since the last commit or log record.
#[derive(Debug, Default)]
pub(crate) struct InoLog {
    /// File blocks whose mapping changed.
    ranges: RangeSet,
    /// Anything changed (attributes or extents) not yet in a record.
    dirty: bool,
    /// A change the log cannot carry: `fsync` commits.
    full: bool,
}

/// The open transaction's view of the fsync log.
#[derive(Debug, Default)]
pub(crate) struct LogState {
    inodes: BTreeMap<(u64, u64), InoLog>,
    /// Blocks of the area the records of this transaction take.
    pos: u64,
    seq: u64,
    /// Data blocks the records point at (no longer fresh).
    blocks: RangeSet,
    /// Inside [`CowFs::map`]/[`CowFs::unmap`]: their extent item changes
    /// are tracked by range.
    pub(super) in_extents: bool,
    /// No records until the next commit (a change the log cannot follow).
    off: bool,
    /// Records written since mount (statistics).
    pub(super) records: u64,
}

/// A record's extent as the log stores it.
#[derive(Debug, Clone)]
struct LogExt {
    pblk: u64,
    blocks: u64,
    prealloc: bool,
    sums: Option<Vec<u32>>,
}

/// One inode's surviving records, merged.
#[derive(Debug, Default)]
struct Replay {
    item: Vec<u8>,
    touched: RangeSet,
    exts: BTreeMap<u64, LogExt>,
}

impl Replay {
    /// Forget the mapping of file blocks `s..e`.
    fn cut(&mut self, s: u64, e: u64) {
        let hit: Vec<u64> = self
            .exts
            .range(..e)
            .filter(|(&l, x)| l + x.blocks > s)
            .map(|(&l, _)| l)
            .collect();
        for l in hit {
            let x = self.exts.remove(&l).unwrap();
            let end = l + x.blocks;
            let piece = |from: u64, to: u64| LogExt {
                pblk: x.pblk + (from - l),
                blocks: to - from,
                prealloc: x.prealloc,
                sums: x
                    .sums
                    .as_ref()
                    .map(|v| v[(from - l) as usize..(to - l) as usize].to_vec()),
            };
            if l < s {
                self.exts.insert(l, piece(l, s));
            }
            if end > e {
                self.exts.insert(e, piece(e, end));
            }
        }
    }
}

/// Writes a payload block by block, with its running CRC.
struct LogWriter {
    raw: Arc<dyn BlockDevice>,
    next: u64,
    end: u64,
    buf: Vec<u8>,
    len: u64,
    crc: u32,
}

impl LogWriter {
    fn put(&mut self, mut b: &[u8]) -> Result<bool> {
        while !b.is_empty() {
            let used = self.buf.len();
            let n = b.len().min(BS as usize - used);
            self.buf.extend_from_slice(&b[..n]);
            b = &b[n..];
            if self.buf.len() == BS as usize && !self.emit()? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn emit(&mut self) -> Result<bool> {
        if self.next >= self.end {
            return Ok(false);
        }
        self.crc = crc32c_update(self.crc, &self.buf);
        self.len += self.buf.len() as u64;
        self.buf.resize(BS as usize, 0);
        self.raw.write_at(self.next * BS, &self.buf)?;
        self.next += 1;
        self.buf.clear();
        Ok(true)
    }

    fn finish(&mut self) -> Result<bool> {
        if self.buf.is_empty() {
            return Ok(true);
        }
        // The CRC covers what was put, not the padding.
        let n = self.buf.len();
        if self.next >= self.end {
            return Ok(false);
        }
        self.crc = crc32c_update(self.crc, &self.buf);
        self.len += n as u64;
        self.buf.resize(BS as usize, 0);
        self.raw.write_at(self.next * BS, &self.buf)?;
        self.next += 1;
        self.buf.clear();
        Ok(true)
    }
}

/// Reads a payload back.
struct LogReader {
    raw: Arc<dyn BlockDevice>,
    next: u64,
    end: u64,
    left: u64,
    buf: Vec<u8>,
    at: usize,
}

impl LogReader {
    fn take(&mut self, out: &mut [u8]) -> Result<()> {
        let mut done = 0;
        while done < out.len() {
            if self.at == self.buf.len() {
                if self.next >= self.end || self.left == 0 {
                    return Err(Error::Corrupt("fsync log record"));
                }
                self.buf.resize(BS as usize, 0);
                self.raw.read_at(self.next * BS, &mut self.buf)?;
                let n = (self.left as usize).min(BS as usize);
                self.buf.truncate(n);
                self.left -= n as u64;
                self.next += 1;
                self.at = 0;
            }
            let n = (out.len() - done).min(self.buf.len() - self.at);
            out[done..done + n].copy_from_slice(&self.buf[self.at..self.at + n]);
            self.at += n;
            done += n;
        }
        Ok(())
    }

    fn u8(&mut self) -> Result<u8> {
        let mut b = [0u8; 1];
        self.take(&mut b)?;
        Ok(b[0])
    }

    fn u32(&mut self) -> Result<u32> {
        let mut b = [0u8; 4];
        self.take(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    fn u64(&mut self) -> Result<u64> {
        let mut b = [0u8; 8];
        self.take(&mut b)?;
        Ok(u64::from_le_bytes(b))
    }
}

/// A record as read: subvolume, inode, inode item, ranges with extents.
type Record = (u64, u64, Vec<u8>, Vec<(u64, u64, Vec<(u64, LogExt)>)>);

impl CowFs {
    // -----------------------------------------------------------------------
    // Tracking what changed
    // -----------------------------------------------------------------------

    fn log_entry(&mut self, ino: u64) -> &mut InoLog {
        self.log.inodes.entry((self.subvol, ino)).or_default()
    }

    /// An item of the current subvolume changed.
    pub(super) fn log_item(&mut self, key: &Key) {
        let extents = key.ty == EXTENT_DATA && self.log.in_extents;
        let e = self.log_entry(key.objectid);
        if extents {
            e.dirty = true;
        } else {
            e.full = true;
        }
    }

    /// An inode item was rewritten.
    pub(super) fn log_attr(&mut self, ino: u64) {
        self.log_entry(ino).dirty = true;
    }

    /// File blocks `from..to` of `ino` were unmapped (and maybe mapped).
    pub(super) fn log_range(&mut self, ino: u64, from: u64, to: u64) {
        let e = self.log_entry(ino);
        e.dirty = true;
        e.ranges.insert(from, to.saturating_sub(from));
    }

    /// The log cannot carry the changes of `ino`.
    pub(super) fn log_full(&mut self, ino: u64) {
        self.log_entry(ino).full = true;
    }

    /// No records until the next commit.
    pub(crate) fn log_off(&mut self) {
        self.log.off = true;
    }

    /// The transaction committed: every record is obsolete.
    pub(super) fn log_committed(&mut self) {
        let records = self.log.records;
        self.log = LogState {
            records,
            ..Default::default()
        };
    }

    /// Records written since mount.
    pub fn log_records(&self) -> u64 {
        self.log.records
    }

    // -----------------------------------------------------------------------
    // The area
    // -----------------------------------------------------------------------

    fn log_key() -> Key {
        Key::new(LOG_TREE, EXTENT_DATA, 0)
    }

    /// Where the log area is (first block, blocks), if there is one.
    pub(super) fn log_area(&mut self) -> Result<Option<(u64, u64)>> {
        match self.st.get(&self.root_tree, &Self::log_key())? {
            Some(d) => match FileExtent::decode(&d)? {
                FileExtent::Prealloc { disk, blocks } if blocks >= 2 => Ok(Some((disk, blocks))),
                _ => Err(Error::Corrupt("fsync log area")),
            },
            None => Ok(None),
        }
    }

    /// Make a log area (in the open transaction). False if there is no
    /// room for one.
    fn make_log_area(&mut self) -> Result<bool> {
        let kind = self.vol.as_ref().map(|_| ChunkKind::Metadata);
        let mut n = area_blocks(self.sb.total_blocks);
        let at = loop {
            let room = self
                .sp
                .alloc
                .free_blocks()
                .saturating_sub(self.sp.reserve * 2);
            if n <= room {
                if let Some(at) = self.sp.alloc.allocate_run_for(None, n, kind) {
                    break at;
                }
                if kind.is_some() {
                    if let Some(at) = self.sp.alloc.allocate_run_for(None, n, None) {
                        break at;
                    }
                }
            }
            if n <= 16 {
                return Ok(false);
            }
            n /= 4;
        };
        self.sp.fresh.insert(at, n);
        let item = FileExtent::Prealloc {
            disk: at,
            blocks: n,
        }
        .encode();
        self.st
            .insert(&mut self.sp, &mut self.root_tree, Self::log_key(), item)?;
        Ok(true)
    }

    /// Give the log area back (in the open transaction): before anything
    /// that moves blocks around.
    pub(crate) fn drop_log_area(&mut self) -> Result<()> {
        self.log_off();
        if let Some((at, n)) = self.log_area()? {
            self.st
                .delete(&mut self.sp, &mut self.root_tree, Self::log_key())?;
            Space::decref_data(&mut self.sp, at, n)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // fsync
    // -----------------------------------------------------------------------

    /// Make what was written to file `h` durable: one log record and one
    /// device flush when the log can carry its changes, a commit otherwise.
    /// `fdatasync` is the same call.
    pub fn fsync(&mut self, h: u64) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        let (sv, ino) = split(h);
        let state = match self.log.inodes.get(&(sv, ino)) {
            // Nothing since the last commit or record, both flushed.
            None => return Ok(()),
            Some(s) => (s.full, s.dirty),
        };
        let area = self.log_area()?.is_some();
        match state {
            (false, false) => return Ok(()),
            (false, true) if !self.log.off && area => {}
            _ => {
                // A commit anyway: it takes a log area along for the next
                // ones if there is none.
                if !area && !self.log.off && !self.make_log_area()? {
                    self.log_off();
                }
                return self.sync();
            }
        }
        let cur = self.subvol;
        let r = self.write_record(sv, ino);
        self.enter(cur)?;
        match r {
            Ok(true) => Ok(()),
            Ok(false) => self.sync(),
            Err(e) => {
                // The record is not valid; nothing refers to it yet.
                warn!("eclipsefs: fsync log record failed ({:?}), committing", e);
                self.sync()
            }
        }
    }

    /// Write the record of `(sv, ino)`. False when it does not fit or the
    /// inode turns out not to be one the log can carry.
    fn write_record(&mut self, sv: u64, ino: u64) -> Result<bool> {
        self.enter(sv)?;
        let inode = self.inode(ino)?;
        if inode.kind() != FileKind::Regular {
            return Ok(false);
        }
        let Some((area, area_n)) = self.log_area()? else {
            return Ok(false);
        };
        let ranges = match self.log.inodes.get(&(sv, ino)) {
            Some(s) => s.ranges.clone(),
            None => return Ok(true),
        };
        let start = area + self.log.pos;
        if self.log.pos + 2 > area_n {
            return Ok(false);
        }
        // Payload first, so a record is never seen before it is whole.
        let mut taken = RangeSet::default();
        let mut unsummed = false;
        let mut w = LogWriter {
            raw: self.raw.clone(),
            next: start + 1,
            end: area + area_n,
            buf: Vec::with_capacity(BS as usize),
            len: 0,
            crc: 0,
        };
        let mut head = Vec::with_capacity(20 + INODE_ITEM_SIZE);
        head.extend_from_slice(&sv.to_le_bytes());
        head.extend_from_slice(&ino.to_le_bytes());
        let item = inode.encode();
        head.extend_from_slice(&(item.len() as u32).to_le_bytes());
        head.extend_from_slice(&item);
        if !w.put(&head)? {
            return Ok(false);
        }
        for (s, l) in ranges.iter() {
            let e = s.saturating_add(l);
            let mut tag = vec![TAG_RANGE];
            tag.extend_from_slice(&s.to_le_bytes());
            tag.extend_from_slice(&e.to_le_bytes());
            if !w.put(&tag)? {
                return Ok(false);
            }
            let mut cur = s;
            while cur < e {
                let batch = self.extents_upto(ino, cur, e, EXTENT_BATCH)?;
                let Some(last) = batch.last() else {
                    break;
                };
                cur = last.lblk + last.blocks;
                for x in batch {
                    if x.z.is_some() {
                        return Ok(false);
                    }
                    let from = x.lblk.max(s);
                    let to = (x.lblk + x.blocks).min(e);
                    let p = x.pblk + (from - x.lblk);
                    let n = to - from;
                    let private = (p..p + n).all(|b| self.sp.refs.get(b) == 1);
                    let ours = self.sp.fresh.covers(p, n) || self.log.blocks.covers(p, n);
                    if !private || !ours {
                        return Ok(false);
                    }
                    let mut b = Vec::with_capacity(34 + 4 * n as usize);
                    b.push(TAG_EXTENT);
                    b.extend_from_slice(&from.to_le_bytes());
                    b.extend_from_slice(&p.to_le_bytes());
                    b.extend_from_slice(&n.to_le_bytes());
                    b.push(x.prealloc as u8);
                    let mut sums = Vec::new();
                    if !x.prealloc {
                        for blk in p..p + n {
                            match self.sp.csums.get(&blk) {
                                Some(Some(c)) => sums.extend_from_slice(&c.to_le_bytes()),
                                Some(None) => {
                                    unsummed = true;
                                    sums.clear();
                                    break;
                                }
                                None => return Ok(false),
                            }
                        }
                    }
                    b.push(!sums.is_empty() as u8);
                    b.extend_from_slice(&sums);
                    if !w.put(&b)? {
                        return Ok(false);
                    }
                    taken.insert(p, n);
                }
            }
        }
        if !w.put(&[TAG_END])? || !w.finish()? {
            return Ok(false);
        }
        let blocks = w.next - (start + 1);
        let mut hdr = vec![0u8; BS as usize];
        put32(&mut hdr, 0, LOG_MAGIC);
        put32(&mut hdr, 4, LOG_VERSION);
        hdr[8..24].copy_from_slice(&self.sb.uuid);
        put64(&mut hdr, 24, self.sb.generation);
        put64(&mut hdr, 32, self.log.seq);
        put32(&mut hdr, 40, blocks as u32);
        put32(&mut hdr, 44, if unsummed { FLAG_UNSUMMED } else { 0 });
        put64(&mut hdr, 48, w.len);
        put32(&mut hdr, 56, w.crc);
        let c = crc32c(&hdr[..HDR_CRC]);
        put32(&mut hdr, HDR_CRC, c);
        if unsummed {
            // Data without checksums cannot prove itself at replay.
            self.raw.sync()?;
        }
        self.raw.write_at(start * BS, &hdr)?;
        self.raw.sync()?;
        self.log.pos += 1 + blocks;
        self.log.seq += 1;
        self.log.records += 1;
        for (s, l) in taken.iter() {
            self.sp.fresh.remove(s, l);
            self.log.blocks.insert(s, l);
        }
        if let Some(st) = self.log.inodes.get_mut(&(sv, ino)) {
            st.ranges.clear();
            st.dirty = false;
        }
        Ok(true)
    }

    // -----------------------------------------------------------------------
    // Replay
    // -----------------------------------------------------------------------

    /// Read the record at `at` (of an area ending at `end`) if it is the
    /// valid record number `seq`: (blocks it takes, the record).
    fn read_record(&mut self, at: u64, end: u64, seq: u64) -> Result<Option<(u64, Record)>> {
        let mut hdr = vec![0u8; BS as usize];
        self.raw.read_at(at * BS, &mut hdr)?;
        if le32(&hdr, 0) != LOG_MAGIC
            || le32(&hdr, 4) != LOG_VERSION
            || hdr[8..24] != self.sb.uuid
            || le64(&hdr, 24) != self.sb.generation
            || le64(&hdr, 32) != seq
            || crc32c(&hdr[..HDR_CRC]) != le32(&hdr, HDR_CRC)
        {
            return Ok(None);
        }
        let blocks = le32(&hdr, 40) as u64;
        let len = le64(&hdr, 48);
        if at + 1 + blocks > end || len > blocks * BS || len + BS <= blocks * BS {
            return Ok(None);
        }
        // The payload as written.
        let mut crc = 0;
        let mut buf = vec![0u8; BS as usize];
        let mut left = len;
        for b in 0..blocks {
            self.raw.read_at((at + 1 + b) * BS, &mut buf)?;
            let n = (left as usize).min(BS as usize);
            crc = crc32c_update(crc, &buf[..n]);
            left -= n as u64;
        }
        if crc != le32(&hdr, 56) {
            return Ok(None);
        }
        let mut r = LogReader {
            raw: self.raw.clone(),
            next: at + 1,
            end: at + 1 + blocks,
            left: len,
            buf: Vec::new(),
            at: 0,
        };
        let sv = r.u64()?;
        let ino = r.u64()?;
        let n = r.u32()? as usize;
        if n != INODE_ITEM_SIZE {
            return Err(Error::Corrupt("fsync log inode item"));
        }
        let mut item = vec![0u8; n];
        r.take(&mut item)?;
        InodeItem::decode(&item)?;
        let mut ranges: Vec<(u64, u64, Vec<(u64, LogExt)>)> = Vec::new();
        loop {
            match r.u8()? {
                TAG_END => break,
                TAG_RANGE => {
                    let s = r.u64()?;
                    let e = r.u64()?;
                    if s >= e {
                        return Err(Error::Corrupt("fsync log range"));
                    }
                    ranges.push((s, e, Vec::new()));
                }
                TAG_EXTENT => {
                    let lb = r.u64()?;
                    let pblk = r.u64()?;
                    let n = r.u64()?;
                    let prealloc = r.u8()? != 0;
                    let summed = r.u8()? != 0;
                    let Some((s, e, exts)) = ranges.last_mut() else {
                        return Err(Error::Corrupt("fsync log extent"));
                    };
                    if n == 0
                        || n > MAX_EXTENT_BLOCKS
                        || lb < *s
                        || lb + n > *e
                        || pblk.checked_add(n).is_none_or(|t| t > self.sb.total_blocks)
                    {
                        return Err(Error::Corrupt("fsync log extent"));
                    }
                    let sums = if summed {
                        let mut v = Vec::with_capacity(n as usize);
                        for _ in 0..n {
                            v.push(r.u32()?);
                        }
                        Some(v)
                    } else {
                        None
                    };
                    exts.push((
                        lb,
                        LogExt {
                            pblk,
                            blocks: n,
                            prealloc,
                            sums,
                        },
                    ));
                }
                _ => return Err(Error::Corrupt("fsync log tag")),
            }
        }
        // The data must be what the record says: a record whose data did
        // not all reach the disk is torn.
        let mut data = vec![0u8; (64 * BS) as usize];
        for (_, _, exts) in &ranges {
            for (_, x) in exts {
                let Some(sums) = &x.sums else {
                    continue;
                };
                let mut done = 0;
                while done < x.blocks {
                    let k = (x.blocks - done).min(64);
                    let buf = &mut data[..(k * BS) as usize];
                    self.raw.read_at((x.pblk + done) * BS, buf)?;
                    for (i, blk) in buf.chunks(BS as usize).enumerate() {
                        if crc32c(blk) != sums[(done as usize) + i] {
                            return Ok(None);
                        }
                    }
                    done += k;
                }
            }
        }
        Ok(Some((1 + blocks, (sv, ino, item, ranges))))
    }

    /// Apply what the fsync log holds on top of the committed state (at
    /// mount). Returns the number of records replayed.
    pub(super) fn replay_log(&mut self) -> Result<u64> {
        let Some((area, n)) = self.log_area()? else {
            return Ok(0);
        };
        let end = area + n;
        let mut merged: BTreeMap<(u64, u64), Replay> = BTreeMap::new();
        let (mut pos, mut seq) = (0, 0);
        while pos + 2 <= n {
            let rec = match self.read_record(area + pos, end, seq) {
                Ok(Some(r)) => r,
                Ok(None) => break,
                Err(e) => {
                    warn!("eclipsefs: fsync log record {} unreadable: {:?}", seq, e);
                    break;
                }
            };
            let (taken, (sv, ino, item, ranges)) = rec;
            let m = merged.entry((sv, ino)).or_default();
            m.item = item;
            for (s, e, exts) in ranges {
                m.touched.insert(s, e - s);
                m.cut(s, e);
                for (lb, x) in exts {
                    m.exts.insert(lb, x);
                }
            }
            pos += taken;
            seq += 1;
        }
        if seq == 0 {
            return Ok(0);
        }
        // Claim every block the result points at: all of them were free
        // in the committed state.
        let mut claim = RangeSet::default();
        for m in merged.values() {
            for x in m.exts.values() {
                claim.insert(x.pblk, x.blocks);
            }
        }
        let cur = self.subvol;
        for &(sv, ino) in merged.keys() {
            self.enter(sv)?;
            if self.inode(ino).is_err() {
                warn!("eclipsefs: fsync log names a missing inode, not replayed");
                self.enter(cur)?;
                return Ok(0);
            }
        }
        if claim
            .iter()
            .any(|(s, l)| (s..s + l).any(|b| self.sp.alloc.is_used(b)))
        {
            warn!("eclipsefs: fsync log points at blocks in use, not replayed");
            self.enter(cur)?;
            return Ok(0);
        }
        for (s, l) in claim.iter() {
            let mut b = s;
            while b < s + l {
                let g = &mut self.sp.alloc.groups_mut()[(b / GROUP_BLOCKS) as usize];
                let off = b % GROUP_BLOCKS;
                let k = (s + l - b).min(g.block_count - off);
                g.mark_used(off, k);
                b += k;
            }
            self.sp.fresh.insert(s, l);
        }
        for ((sv, ino), m) in merged {
            self.enter(sv)?;
            let mut i = self.inode(ino)?;
            for (s, l) in m.touched.iter() {
                self.unmap(ino, &mut i, s, s.saturating_add(l))?;
            }
            for (lb, x) in m.exts {
                self.map(ino, &mut i, lb, x.blocks, x.pblk, x.prealloc)?;
                for b in 0..x.blocks {
                    let c = x.sums.as_ref().map(|v| v[b as usize]);
                    self.sp.csums.insert(x.pblk + b, c);
                }
            }
            self.put_inode(ino, InodeItem::decode(&m.item)?)?;
        }
        self.enter(cur)?;
        Ok(seq)
    }
}
