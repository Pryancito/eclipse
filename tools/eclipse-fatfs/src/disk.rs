//! The disk as the FAT code reads and writes it: a stream over the device,
//! with the parts of it used last kept.
//!
//! `fatfs` reads and writes a few bytes at a time -- an entry of the
//! allocation table, a directory entry, a cluster of a file -- and each
//! read or write of the device is a system call. So the device is read a
//! chunk at a time, and the chunks used last are kept: the table and the
//! directories stay in them, and a file read in order costs one read of
//! the device per chunk.
//!
//! What is written stays in its chunk until the chunk is let go or
//! [`Disk::flush`] is called, and then goes to the device as runs of
//! sectors. A chunk knows which of its sectors it holds: whole sectors
//! written into a chunk that was never read (a new file's clusters) do not
//! read the device first.
//!
//! The chunks are in one stretch of memory, [`Kept`], with nothing in it
//! that points anywhere: the server gives it to the kernel with the mount,
//! and when the server dies the next one is given it back and goes on from
//! what was written and had not reached the device. So each change to it
//! is made in an order that leaves it true whenever the process stops: a
//! sector is marked as not on the device before its bytes change, and as
//! held only once its bytes are there.
//!
//! A change of several writes that is only true whole -- a rename, which
//! writes the new name and then takes the old one off -- is made as a
//! *group*: between [`Disk::begin`] and [`Disk::commit`] each sector is
//! put in a journal in the kept memory, as it was, before it is first
//! written, and `commit` empties the journal. A server that dies inside
//! the group leaves the journal, and the next `Disk` over the memory
//! writes the sectors back as they were: the change was not begun.
//! Reading costs a group nothing, which is why it is the old sectors that
//! are kept and not the new: the FAT code reads a directory entry in a
//! dozen pieces, and looked each of them up among the group's.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{self, Read, Seek, SeekFrom, Write};

/// What the device is read in.
pub const CHUNK: usize = 64 * 1024;
/// The chunks kept: 4 MiB. The file data the kernel has read is in its page
/// cache, not here; these are the table, the directories, what was read
/// ahead, and what was written and has not reached the device.
pub const CHUNKS_KEPT: usize = 64;

const SECTOR: usize = 512;
/// One bit for each sector of a chunk.
type Sectors = u128;
const _: () = assert!(CHUNK / SECTOR == Sectors::BITS as usize);

/// The bits of the sectors `[from, to)`.
fn sectors(from: usize, to: usize) -> Sectors {
    if from >= to {
        return 0;
    }
    let upto = |n: usize| match n {
        128 => Sectors::MAX,
        n => (1 << n) - 1,
    };
    upto(to) & !upto(from)
}

/// Something bytes are read from and written to at an offset: the device,
/// or in a test a volume in memory.
pub trait Blocks {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<usize>;
    /// What was written is on the medium.
    fn sync(&mut self) -> io::Result<()>;
}

impl Blocks for std::fs::File {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        std::os::unix::fs::FileExt::read_at(self, buf, offset)
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        std::os::unix::fs::FileExt::write_at(self, buf, offset)
    }

    fn sync(&mut self) -> io::Result<()> {
        // A device node may have nothing to flush, and say so as an error
        // of its own: only the device failing is one here.
        match self.sync_all() {
            Err(e) if e.raw_os_error() == Some(libc::EIO) => Err(e),
            _ => Ok(()),
        }
    }
}

impl Blocks for Vec<u8> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let from = (offset as usize).min(self.len());
        let n = buf.len().min(self.len() - from);
        buf[..n].copy_from_slice(&self[from..from + n]);
        Ok(n)
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<usize> {
        let from = (offset as usize).min(self.len());
        let n = buf.len().min(self.len() - from);
        self[from..from + n].copy_from_slice(&buf[..n]);
        Ok(n)
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The hash of a chunk's index: the index, spread. `fatfs` reads a
/// directory entry in a dozen pieces, each a lookup of its chunk, and the
/// default hash was most of what a lookup cost.
#[derive(Default)]
struct ByIndex(u64);

impl Hasher for ByIndex {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 << 8) | u64::from(*b);
        }
    }

    fn write_u64(&mut self, index: u64) {
        self.0 = index.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

/// What [`Kept`] starts with.
#[repr(C)]
struct Head {
    /// [`MAGIC`] once the memory is laid out.
    magic: u64,
    /// The bytes of the device the chunks are of.
    device_len: u64,
    /// What whoever uses the disk keeps here ([`Disk::word`]).
    word: u64,
    /// The sectors in the journal: those the group being made has
    /// written, as they were before it. 0 when no group is being made.
    journal: u64,
}

/// "eclfatk2": the layout below.
const MAGIC: u64 = 0x326b_7461_666c_6365;

/// The sectors of a group the journal has room for: 128 KiB, which is a
/// directory's new cluster of the largest size and the entries and table
/// sectors around it. A group that writes more is, from there on, written
/// as it would be without one.
pub const GROUP_MAX: usize = 256;

/// What is known of the chunk in each place of [`Kept`].
#[repr(C)]
#[derive(Clone, Copy)]
struct Meta {
    /// Which chunk of the device, or [`NO_CHUNK`].
    index: u64,
    /// When it was last used: the least recent goes first.
    used: u64,
    /// The sectors whose bytes are here: the others are still only on the
    /// device.
    held: Sectors,
    /// The sectors written here that the device does not have yet.
    dirty: Sectors,
}

const NO_CHUNK: u64 = u64::MAX;
/// Where the [`Meta`]s start, and the chunks' bytes.
const METAS_AT: usize = 4096;
const BYTES_AT: usize = 8192;
const _: () = assert!(METAS_AT + CHUNKS_KEPT * core::mem::size_of::<Meta>() <= BYTES_AT);
/// The journal, after the chunks: which sector each of its places is of,
/// then the places' bytes.
const JOURNAL_AT: usize = BYTES_AT + CHUNKS_KEPT * CHUNK;
const JOURNAL_BYTES_AT: usize = JOURNAL_AT + 4096;
const _: () = assert!(GROUP_MAX * 8 <= JOURNAL_BYTES_AT - JOURNAL_AT);

/// The memory the chunks are kept in: a [`Head`], a [`Meta`] for each
/// place, the places' bytes, and the journal.
pub struct Kept {
    base: *mut u8,
    /// The memory, when it is this process's own.
    _own: Option<Vec<u128>>,
}

impl Kept {
    /// The bytes of it.
    pub const LEN: usize = JOURNAL_BYTES_AT + GROUP_MAX * SECTOR;

    /// Memory of this process's own, which goes when it does.
    pub fn private() -> Self {
        let mut own = vec![0u128; Self::LEN / 16];
        Kept {
            base: own.as_mut_ptr().cast(),
            _own: Some(own),
        }
    }

    /// The memory at `base`: zeros, or what a [`Disk`] left there.
    ///
    /// # Safety
    /// `base` is the start of [`Kept::LEN`] bytes, at a page's start, that
    /// are readable and writable for as long as this is used, and nothing
    /// else reads or writes them meanwhile.
    pub unsafe fn at(base: *mut u8) -> Self {
        Kept { base, _own: None }
    }

    fn head(&mut self) -> &mut Head {
        // SAFETY: inside the memory, aligned (the base is), and borrowed
        // as `self` is.
        unsafe { &mut *self.base.cast::<Head>() }
    }

    fn meta(&self, place: usize) -> &Meta {
        assert!(place < CHUNKS_KEPT);
        // SAFETY: as `head`; the place is one of them.
        unsafe { &*self.base.add(METAS_AT).cast::<Meta>().add(place) }
    }

    fn meta_mut(&mut self, place: usize) -> &mut Meta {
        assert!(place < CHUNKS_KEPT);
        // SAFETY: as `meta`.
        unsafe { &mut *self.base.add(METAS_AT).cast::<Meta>().add(place) }
    }

    fn bytes(&self, place: usize) -> &[u8] {
        assert!(place < CHUNKS_KEPT);
        // SAFETY: the place's own stretch of the memory.
        unsafe { core::slice::from_raw_parts(self.base.add(BYTES_AT + place * CHUNK), CHUNK) }
    }

    /// The sector the journal's place `i` is of, and its bytes.
    fn journal(&self, i: usize) -> (u64, &[u8]) {
        assert!(i < GROUP_MAX);
        // SAFETY: the journal's own stretch of the memory, aligned as the
        // base is.
        unsafe {
            (
                *self.base.add(JOURNAL_AT).cast::<u64>().add(i),
                core::slice::from_raw_parts(self.base.add(JOURNAL_BYTES_AT + i * SECTOR), SECTOR),
            )
        }
    }

    fn set_journal(&mut self, i: usize, sector: u64, bytes: &[u8; SECTOR]) {
        assert!(i < GROUP_MAX);
        // SAFETY: as `journal`.
        unsafe {
            *self.base.add(JOURNAL_AT).cast::<u64>().add(i) = sector;
            core::slice::from_raw_parts_mut(self.base.add(JOURNAL_BYTES_AT + i * SECTOR), SECTOR)
                .copy_from_slice(bytes);
        }
    }

    /// Say how many sectors of the journal stand: the one store a sector
    /// is in it by, or the group is done with, so it is made where it is
    /// written and not moved before what it follows.
    fn set_journaled(&mut self, sectors: usize) {
        use core::sync::atomic::{compiler_fence, Ordering};
        compiler_fence(Ordering::SeqCst);
        // SAFETY: the head's own field.
        unsafe {
            core::ptr::write_volatile(&raw mut (*self.base.cast::<Head>()).journal, sectors as u64)
        };
        compiler_fence(Ordering::SeqCst);
    }

    /// The place's metadata and its bytes, to change both.
    fn place_mut(&mut self, place: usize) -> (&mut Meta, &mut [u8]) {
        assert!(place < CHUNKS_KEPT);
        // SAFETY: as `meta` and `bytes`, which do not overlap.
        unsafe {
            (
                &mut *self.base.add(METAS_AT).cast::<Meta>().add(place),
                core::slice::from_raw_parts_mut(self.base.add(BYTES_AT + place * CHUNK), CHUNK),
            )
        }
    }
}

pub struct Disk<B> {
    blocks: B,
    len: u64,
    pos: u64,
    kept: Kept,
    /// The place each kept chunk is in: what `kept` says, to look up.
    place_of: HashMap<u64, usize, BuildHasherDefault<ByIndex>>,
    clock: u64,
    /// The memory was another `Disk`'s before.
    adopted: bool,
    /// The group being made: the sectors in the journal, which are those
    /// written since [`Disk::begin`]. `None` when there is no group, and
    /// once a group has written more than the journal holds.
    group: Option<Vec<u64>>,
    /// In a test, the changes this may still make to the memory and the
    /// device before it is as a process that stopped: every one after is
    /// not made.
    #[cfg(test)]
    pub changes_left: Option<u64>,
    /// How many times the device was read, and written.
    pub device_reads: u64,
    pub device_writes: u64,
}

fn read_fully<B: Blocks>(blocks: &B, mut at: u64, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match blocks.read_at(at, buf) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                at += n as u64;
                buf = &mut buf[n..];
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn write_fully<B: Blocks>(blocks: &mut B, mut at: u64, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match blocks.write_at(at, buf) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                at += n as u64;
                buf = &buf[n..];
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

impl<B: Blocks> Disk<B> {
    /// `blocks`, which holds `len` bytes, with chunks that go when this
    /// does.
    pub fn new(blocks: B, len: u64) -> Self {
        Self::over(blocks, len, Kept::private()).expect("new memory is no other disk's")
    }

    /// `blocks`, which holds `len` bytes, with its chunks in `kept`: empty
    /// when `kept` is new, and what the last `Disk` over it left when it is
    /// not. `None` when what is there is not this device's.
    pub fn over(blocks: B, len: u64, mut kept: Kept) -> Option<Self> {
        let mut place_of = HashMap::default();
        let mut clock = 0;
        let adopted = kept.head().magic == MAGIC;
        if adopted {
            if kept.head().device_len != len {
                return None;
            }
            for place in 0..CHUNKS_KEPT {
                let meta = *kept.meta(place);
                if meta.index == NO_CHUNK {
                    continue;
                }
                // A chunk past the device, or one in two places, is not
                // what a `Disk` leaves.
                if meta.index >= len.div_ceil(CHUNK as u64)
                    || place_of.insert(meta.index, place).is_some()
                {
                    return None;
                }
                clock = clock.max(meta.used);
            }
        } else {
            for place in 0..CHUNKS_KEPT {
                *kept.meta_mut(place) = Meta {
                    index: NO_CHUNK,
                    used: 0,
                    held: 0,
                    dirty: 0,
                };
            }
            kept.head().device_len = len;
            kept.head().word = 0;
            kept.head().journal = 0;
            // Last: until it is there the memory is new.
            kept.head().magic = MAGIC;
        }
        let mut disk = Disk {
            blocks,
            len,
            pos: 0,
            kept,
            place_of,
            clock,
            adopted,
            group: None,
            #[cfg(test)]
            changes_left: None,
            device_reads: 0,
            device_writes: 0,
        };
        // A group the last `Disk` was making: its sectors go back to what
        // they were, which changes nothing where it was done before.
        let journaled = disk.kept.head().journal as usize;
        if journaled > GROUP_MAX {
            return None;
        }
        for i in 0..journaled {
            let (sector, bytes) = disk.kept.journal(i);
            let at = sector.checked_mul(SECTOR as u64).filter(|at| *at < len)?;
            let bytes: [u8; SECTOR] = bytes.try_into().expect("a sector");
            let n = SECTOR.min((len - at) as usize);
            disk.write_base(at, &bytes[..n]).ok()?;
        }
        disk.kept.set_journaled(0);
        Some(disk)
    }

    /// Whether this may still change the memory and the device: always,
    /// but in a test that stops it at a change.
    #[inline]
    fn alive(&mut self) -> bool {
        #[cfg(test)]
        if let Some(left) = &mut self.changes_left {
            if *left == 0 {
                return false;
            }
            *left -= 1;
        }
        true
    }

    /// What is written from here to [`commit`](Self::commit) is one
    /// change: none of it is there after a server's death in between.
    pub fn begin(&mut self) {
        self.group.get_or_insert_with(Vec::new);
    }

    /// The group is made: nothing is left to take it back with.
    pub fn commit(&mut self) {
        if self.group.take().is_some_and(|group| !group.is_empty()) && self.alive() {
            self.kept.set_journaled(0);
        }
    }

    /// Before `[offset, offset + len)` is written in a group: each of its
    /// sectors that the journal does not have goes in it, as it is.
    fn journal(&mut self, offset: u64, len: usize) -> io::Result<()> {
        let end = offset + len as u64;
        for sector in offset / SECTOR as u64..end.div_ceil(SECTOR as u64) {
            let Some(group) = &self.group else {
                return Ok(());
            };
            if group.contains(&sector) {
                continue;
            }
            let i = group.len();
            if i == GROUP_MAX {
                // More than the journal holds: the group is written, from
                // here, as without one, and what it wrote stays.
                self.group = None;
                if self.alive() {
                    self.kept.set_journaled(0);
                }
                return Ok(());
            }
            let at = sector * SECTOR as u64;
            let mut was = [0u8; SECTOR];
            let n = SECTOR.min((self.len - at) as usize);
            self.read_exact_at(at, &mut was[..n])?;
            // The sector, and then that it is there: a sector the word
            // does not count is not written back.
            if self.alive() {
                self.kept.set_journal(i, sector, &was);
            }
            if self.alive() {
                self.kept.set_journaled(i + 1);
            }
            self.group.as_mut().expect("a group").push(sector);
        }
        Ok(())
    }

    /// Whether the chunks were in the memory already: another `Disk`'s,
    /// that is gone.
    pub fn adopted(&self) -> bool {
        self.adopted
    }

    /// A word kept with the chunks, for whoever uses the disk: 0 in new
    /// memory.
    pub fn word(&mut self) -> u64 {
        self.kept.head().word
    }

    pub fn set_word(&mut self, word: u64) {
        self.kept.head().word = word;
    }

    /// The device, as it is: without what [`flush`](Self::flush) has not
    /// written yet.
    pub fn blocks(&self) -> &B {
        &self.blocks
    }

    /// Whether something written has not reached the device.
    pub fn is_dirty(&self) -> bool {
        (0..CHUNKS_KEPT).any(|place| self.kept.meta(place).dirty != 0)
    }

    /// The bytes of the chunk `index`: the last one may be short.
    fn chunk_len(&self, index: u64) -> usize {
        (self.len - index * CHUNK as u64).min(CHUNK as u64) as usize
    }

    /// Write the sectors of the chunk in `place` the device does not have,
    /// a run of them at a time.
    fn write_out(&mut self, place: usize) -> io::Result<()> {
        let index = self.kept.meta(place).index;
        if index == NO_CHUNK {
            return Ok(());
        }
        let len = self.chunk_len(index);
        let mut left = self.kept.meta(place).dirty;
        while left != 0 {
            let from = left.trailing_zeros() as usize;
            let run = (left >> from).trailing_ones() as usize;
            let (start, end) = (from * SECTOR, ((from + run) * SECTOR).min(len));
            if !self.alive() {
                return Ok(());
            }
            self.device_writes += 1;
            write_fully(
                &mut self.blocks,
                index * CHUNK as u64 + start as u64,
                &self.kept.bytes(place)[start..end],
            )?;
            left &= !sectors(from, from + run);
            self.kept.meta_mut(place).dirty = left;
        }
        Ok(())
    }

    /// Everything written reaches the device, and the device its medium.
    pub fn flush(&mut self) -> io::Result<()> {
        let mut dirty: Vec<(u64, usize)> = (0..CHUNKS_KEPT)
            .map(|place| (self.kept.meta(place), place))
            .filter(|(meta, _)| meta.index != NO_CHUNK && meta.dirty != 0)
            .map(|(meta, place)| (meta.index, place))
            .collect();
        dirty.sort_unstable();
        for (_, place) in dirty {
            self.write_out(place)?;
        }
        self.blocks.sync()
    }

    /// The place of the chunk `index` with the sectors `needed` of it held,
    /// from the device those that are not.
    fn chunk(&mut self, index: u64, needed: Sectors) -> io::Result<usize> {
        self.clock += 1;
        let place = match self.place_of.get(&index) {
            Some(place) => *place,
            None => {
                // A place nothing is in, or the one used least lately.
                let place = (0..CHUNKS_KEPT)
                    .min_by_key(|place| match self.kept.meta(*place) {
                        meta if meta.index == NO_CHUNK => 0,
                        meta => meta.used + 1,
                    })
                    .expect("there are places");
                self.write_out(place)?;
                if !self.alive() {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                let meta = self.kept.meta_mut(place);
                let was = meta.index;
                // Nothing held first: the bytes there are the old chunk's.
                meta.held = 0;
                meta.dirty = 0;
                meta.index = index;
                self.place_of.remove(&was);
                self.place_of.insert(index, place);
                place
            }
        };
        let missing = needed & !self.kept.meta(place).held;
        if missing != 0 {
            // One read of the chunk, of which the sectors not held are
            // taken: those held may be newer than the device.
            let len = self.chunk_len(index);
            let mut read = vec![0u8; len];
            self.device_reads += 1;
            read_fully(&self.blocks, index * CHUNK as u64, &mut read)?;
            let (meta, bytes) = self.kept.place_mut(place);
            for sector in 0..len.div_ceil(SECTOR) {
                if meta.held & (1 << sector) == 0 {
                    let (from, to) = (sector * SECTOR, ((sector + 1) * SECTOR).min(len));
                    bytes[from..to].copy_from_slice(&read[from..to]);
                }
            }
            meta.held = Sectors::MAX;
        }
        self.kept.meta_mut(place).used = self.clock;
        Ok(place)
    }

    /// Read from the chunk the offset `at` is in, up to its end.
    fn read_chunk(&mut self, at: u64, buf: &mut [u8]) -> io::Result<usize> {
        let (index, within) = (at / CHUNK as u64, (at % CHUNK as u64) as usize);
        let n = buf.len().min(self.chunk_len(index) - within);
        let needed = sectors(within / SECTOR, (within + n).div_ceil(SECTOR));
        // The usual read, of a chunk that is kept and holds it: one lookup.
        let place = match self.place_of.get(&index) {
            Some(place) if needed & !self.kept.meta(*place).held == 0 => {
                self.clock += 1;
                self.kept.meta_mut(*place).used = self.clock;
                *place
            }
            _ => self.chunk(index, needed)?,
        };
        buf[..n].copy_from_slice(&self.kept.bytes(place)[within..within + n]);
        Ok(n)
    }

    /// Write into the chunk the offset `at` is in, up to its end.
    fn write_chunk(&mut self, at: u64, buf: &[u8]) -> io::Result<usize> {
        let (index, within) = (at / CHUNK as u64, (at % CHUNK as u64) as usize);
        let len = self.chunk_len(index);
        let n = buf.len().min(len - within);
        let end = within + n;
        let touched = sectors(within / SECTOR, end.div_ceil(SECTOR));
        // The sectors written whole need not be read; the chunk's last
        // sector is whole when the write reaches the end of the device.
        let whole_to = if end == len {
            end.div_ceil(SECTOR)
        } else {
            end / SECTOR
        };
        let whole = sectors(within.div_ceil(SECTOR), whole_to);
        let place = self.chunk(index, touched & !whole)?;
        if !self.alive() {
            return Ok(n);
        }
        let (meta, bytes) = self.kept.place_mut(place);
        // In this order, for a process that stops in the middle: a sector
        // that was held is not the device's from before its bytes change,
        // and one that was not is held only once its bytes are whole.
        meta.dirty |= touched & meta.held;
        bytes[within..end].copy_from_slice(&buf[..n]);
        meta.dirty |= touched;
        meta.held |= touched;
        Ok(n)
    }
}

/// A read this long, of whole sectors from a sector's start, goes straight
/// from the device to whoever asked: a file's data, which the kernel keeps
/// once it has it and this would only hold a second time.
const DIRECT_MIN: usize = 4096;
/// A write this long goes straight to the device: it is a chunk's worth,
/// which would be one write of the device later all the same. A shorter
/// one waits in its chunk for its neighbours.
const DIRECT_WRITE_MIN: usize = CHUNK;

fn direct(offset: u64, len: usize, min: usize) -> bool {
    len >= min && offset.is_multiple_of(SECTOR as u64) && len.is_multiple_of(SECTOR)
}

impl<B: Blocks> Disk<B> {
    /// The parts of `[offset, offset + len)` that kept chunks hold, as
    /// `(place, first sector, sectors, where in the range)`: the range is
    /// whole sectors.
    fn held_in(&self, offset: u64, len: usize) -> Vec<(usize, usize, usize, usize)> {
        let mut parts = Vec::new();
        let mut at = offset;
        let end = offset + len as u64;
        while at < end {
            let (index, within) = (at / CHUNK as u64, (at % CHUNK as u64) as usize);
            let n = ((end - at) as usize).min(CHUNK - within);
            if let Some(place) = self.place_of.get(&index) {
                parts.push((*place, within / SECTOR, n / SECTOR, (at - offset) as usize));
            }
            at += n as u64;
        }
        parts
    }

    /// Fill `buf` from `offset`, wherever the stream's position is: an
    /// error when the device ends first.
    pub fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        if offset + buf.len() as u64 > self.len {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.read_base(offset, buf)
    }

    /// [`read_exact_at`](Self::read_exact_at), the range being inside the
    /// device.
    fn read_base(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        if direct(offset, buf.len(), DIRECT_MIN) {
            self.device_reads += 1;
            read_fully(&self.blocks, offset, buf)?;
            // What a kept chunk holds of it is as new as the device or
            // newer.
            for (place, first, count, at) in self.held_in(offset, buf.len()) {
                let (held, bytes) = (self.kept.meta(place).held, self.kept.bytes(place));
                for sector in (first..first + count).filter(|s| held & (1 << s) != 0) {
                    let to = at + (sector - first) * SECTOR;
                    buf[to..to + SECTOR]
                        .copy_from_slice(&bytes[sector * SECTOR..(sector + 1) * SECTOR]);
                }
            }
            return Ok(());
        }
        let mut done = 0;
        while done < buf.len() {
            done += self.read_chunk(offset + done as u64, &mut buf[done..])?;
        }
        Ok(())
    }

    /// Write all of `buf` at `offset`, wherever the stream's position is:
    /// an error when the device ends first.
    pub fn write_all_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if offset + buf.len() as u64 > self.len {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if self.group.is_some() {
            self.journal(offset, buf.len())?;
        }
        self.write_base(offset, buf)
    }

    /// [`write_all_at`](Self::write_all_at), the range being inside the
    /// device and in the journal if it has to be.
    fn write_base(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if direct(offset, buf.len(), DIRECT_WRITE_MIN) {
            if !self.alive() {
                return Ok(());
            }
            self.device_writes += 1;
            write_fully(&mut self.blocks, offset, buf)?;
            // The kept chunks have it too, and no older bytes to write
            // over it later.
            for (place, first, count, at) in self.held_in(offset, buf.len()) {
                let (meta, bytes) = self.kept.place_mut(place);
                let (from, n) = (first * SECTOR, count * SECTOR);
                bytes[from..from + n].copy_from_slice(&buf[at..at + n]);
                meta.held |= sectors(first, first + count);
                meta.dirty &= !sectors(first, first + count);
            }
            return Ok(());
        }
        let mut done = 0;
        while done < buf.len() {
            done += self.write_chunk(offset + done as u64, &buf[done..])?;
        }
        Ok(())
    }
}

impl<B: Blocks> Read for Disk<B> {
    /// Up to the end of the chunk the position is in: `fatfs` asks again
    /// for the rest.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let n = self.read_chunk(self.pos, buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<B: Blocks> Seek for Disk<B> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let to = match pos {
            SeekFrom::Start(to) => Some(to),
            SeekFrom::Current(by) => self.pos.checked_add_signed(by),
            SeekFrom::End(by) => self.len.checked_add_signed(by),
        };
        self.pos = to.ok_or(io::ErrorKind::InvalidInput)?;
        Ok(self.pos)
    }
}

impl<B: Blocks> Write for Disk<B> {
    /// Up to the end of the chunk the position is in, into the chunk.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.pos >= self.len || buf.is_empty() {
            return Ok(0);
        }
        if self.group.is_some() {
            // What `write_chunk` will write: up to the chunk's end.
            let within = (self.pos % CHUNK as u64) as usize;
            let n = buf
                .len()
                .min(self.chunk_len(self.pos / CHUNK as u64) - within);
            self.journal(self.pos, n)?;
        }
        let n = self.write_chunk(self.pos, buf)?;
        self.pos += n as u64;
        Ok(n)
    }

    /// Nothing: `fatfs` asks for this after every file it closes, and the
    /// device is written when [`Disk::flush`] is called.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device whose byte at `i` is `i` folded into a byte, twice over so
    /// that neighbouring chunks differ.
    fn device(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i ^ (i >> 8) ^ (i >> 16)) as u8).collect()
    }

    fn read_all(disk: &mut Disk<Vec<u8>>, at: u64, len: usize) -> Vec<u8> {
        disk.seek(SeekFrom::Start(at)).unwrap();
        let mut out = vec![0u8; len];
        let mut done = 0;
        while done < len {
            match disk.read(&mut out[done..]).unwrap() {
                0 => break,
                n => done += n,
            }
        }
        out.truncate(done);
        out
    }

    #[test]
    fn what_is_read_is_what_the_device_holds_across_chunks() {
        let dev = device(3 * CHUNK + 1000);
        let mut disk = Disk::new(dev.clone(), dev.len() as u64);
        let at = CHUNK as u64 - 10;
        assert_eq!(read_all(&mut disk, at, 30), dev[CHUNK - 10..CHUNK + 20]);
        // The last chunk is short, and the end is the end.
        let tail = read_all(&mut disk, 3 * CHUNK as u64 + 900, 500);
        assert_eq!(tail, dev[3 * CHUNK + 900..]);
        assert_eq!(read_all(&mut disk, dev.len() as u64, 10), []);
    }

    #[test]
    fn a_chunk_read_once_is_not_read_from_the_device_again() {
        let dev = device(4 * CHUNK);
        let mut disk = Disk::new(dev.clone(), dev.len() as u64);
        for at in [0u64, 100, 4000, CHUNK as u64 - 1] {
            read_all(&mut disk, at, 1);
        }
        assert_eq!(disk.device_reads, 1);
        read_all(&mut disk, CHUNK as u64, 1);
        assert_eq!(disk.device_reads, 2);
    }

    #[test]
    fn the_chunk_least_recently_read_is_the_one_let_go() {
        let dev = device((CHUNKS_KEPT + 2) * CHUNK);
        let mut disk = Disk::new(dev.clone(), dev.len() as u64);
        for i in 0..CHUNKS_KEPT as u64 {
            read_all(&mut disk, i * CHUNK as u64, 1);
        }
        // Chunk 0 is read again, so chunk 1 is now the oldest.
        read_all(&mut disk, 0, 1);
        read_all(&mut disk, CHUNKS_KEPT as u64 * CHUNK as u64, 1);
        let reads = disk.device_reads;
        read_all(&mut disk, 0, 1);
        assert_eq!(disk.device_reads, reads, "chunk 0 was kept");
        read_all(&mut disk, CHUNK as u64, 1);
        assert_eq!(disk.device_reads, reads + 1, "chunk 1 was let go");
    }

    #[test]
    fn a_long_read_of_whole_sectors_goes_past_the_kept_chunks() {
        let dev = device(4 * CHUNK);
        let mut disk = Disk::new(dev.clone(), dev.len() as u64);
        let mut out = vec![0u8; CHUNK];
        disk.read_exact_at(512 * 3, &mut out).unwrap();
        assert_eq!(out, dev[512 * 3..512 * 3 + CHUNK]);
        assert_eq!(disk.device_reads, 1);
        // Nothing was kept of it: a short read there reads its chunk.
        let mut few = [0u8; 100];
        disk.read_exact_at(512 * 3 + 7, &mut few).unwrap();
        assert_eq!(few[..], dev[512 * 3 + 7..512 * 3 + 107]);
        assert_eq!(disk.device_reads, 2);
        disk.read_exact_at(9000, &mut few).unwrap();
        assert_eq!(disk.device_reads, 2);
        // A read that is not whole sectors goes through the chunks, across
        // their edges, and one past the end of the device is an error.
        let mut odd = vec![0u8; 5000];
        disk.read_exact_at(CHUNK as u64 - 1000, &mut odd).unwrap();
        assert_eq!(odd, dev[CHUNK - 1000..CHUNK + 4000]);
        assert!(disk
            .read_exact_at(4 * CHUNK as u64 - 100, &mut odd)
            .is_err());
    }

    #[test]
    fn what_is_written_is_read_back_and_reaches_the_device_when_flushed() {
        let dev = device(3 * CHUNK);
        let mut disk = Disk::new(dev.clone(), dev.len() as u64);
        let at = CHUNK as u64 - 700;
        disk.write_all_at(at, &[0xAB; 1500]).unwrap();
        let mut back = vec![0u8; 1700];
        disk.read_exact_at(at - 100, &mut back).unwrap();
        assert_eq!(back[..100], dev[CHUNK - 800..CHUNK - 700]);
        assert_eq!(back[100..1600], [0xAB; 1500]);
        assert_eq!(back[1600..], dev[CHUNK + 800..CHUNK + 900]);
        // The device has none of it yet.
        assert_eq!(disk.blocks()[..], dev[..]);
        assert!(disk.is_dirty());
        disk.flush().unwrap();
        assert!(!disk.is_dirty());
        let mut expect = dev.clone();
        expect[CHUNK - 700..CHUNK + 800].fill(0xAB);
        assert_eq!(disk.blocks()[..], expect[..]);
        // Nothing left to write the second time.
        let writes = disk.device_writes;
        disk.flush().unwrap();
        assert_eq!(disk.device_writes, writes);
    }

    #[test]
    fn whole_sectors_are_written_without_reading_the_device() {
        let dev = device(4 * CHUNK);
        let mut disk = Disk::new(dev.clone(), dev.len() as u64);
        disk.write_all_at(1024, &[1; 1024]).unwrap();
        disk.write_all_at(8192, &[2; 512]).unwrap();
        assert_eq!(disk.device_reads, 0);
        // Only what was written goes out: the sectors between are the
        // device's own, and are not written over with what was never read.
        disk.flush().unwrap();
        assert_eq!(disk.device_writes, 2);
        let mut expect = dev.clone();
        expect[1024..2048].fill(1);
        expect[8192..8704].fill(2);
        assert_eq!(disk.blocks()[..], expect[..]);
        // A sector written in part is read first, once, and the sectors
        // written before keep what was written.
        disk.write_all_at(1000, &[3; 100]).unwrap();
        assert_eq!(disk.device_reads, 1);
        let mut back = vec![0u8; 2048];
        disk.read_exact_at(0, &mut back).unwrap();
        expect[1000..1100].fill(3);
        assert_eq!(back[..], expect[..2048]);
        assert_eq!(disk.device_reads, 1);
    }

    #[test]
    fn a_chunk_let_go_is_written_first() {
        let dev = device((CHUNKS_KEPT + 2) * CHUNK);
        let mut disk = Disk::new(dev.clone(), dev.len() as u64);
        disk.write_all_at(10, b"kept until it must go").unwrap();
        for i in 1..=CHUNKS_KEPT as u64 {
            read_all(&mut disk, i * CHUNK as u64, 1);
        }
        assert_eq!(&disk.blocks()[10..31], b"kept until it must go");
        assert!(!disk.is_dirty());
    }

    #[test]
    fn a_long_write_goes_straight_to_the_device_and_the_chunks_agree() {
        let dev = device(4 * CHUNK);
        let mut disk = Disk::new(dev.clone(), dev.len() as u64);
        // A chunk held, with something in it not yet written.
        disk.write_all_at(CHUNK as u64 + 100, &[7; 50]).unwrap();
        let long = vec![9u8; CHUNK];
        disk.write_all_at(CHUNK as u64 - 4096, &long).unwrap();
        assert_eq!(disk.blocks()[CHUNK - 4096..2 * CHUNK - 4096], long[..]);
        // The older bytes do not come back over it, from the chunk or at
        // the flush.
        let mut few = [0u8; 200];
        disk.read_exact_at(CHUNK as u64 + 50, &mut few).unwrap();
        assert_eq!(few, [9; 200]);
        disk.flush().unwrap();
        assert_eq!(disk.blocks()[CHUNK - 4096..2 * CHUNK - 4096], long[..]);
        // And a long read sees what only a chunk has.
        disk.write_all_at(2 * CHUNK as u64 + 512, &[5; 512])
            .unwrap();
        let mut back = vec![0u8; 8192];
        disk.read_exact_at(2 * CHUNK as u64, &mut back).unwrap();
        assert_eq!(back[..512], dev[2 * CHUNK..2 * CHUNK + 512]);
        assert_eq!(back[512..1024], [5; 512]);
        assert_eq!(back[1024..], dev[2 * CHUNK + 1024..2 * CHUNK + 8192]);
    }

    #[test]
    fn the_last_chunk_may_be_short_and_not_whole_sectors() {
        let dev = device(CHUNK + 1000);
        let mut disk = Disk::new(dev.clone(), dev.len() as u64);
        disk.write_all_at(CHUNK as u64 + 600, &[4; 400]).unwrap();
        assert!(disk.write_all_at(CHUNK as u64 + 900, &[4; 200]).is_err());
        disk.flush().unwrap();
        let mut expect = dev.clone();
        expect[CHUNK + 600..].fill(4);
        assert_eq!(disk.blocks()[..], expect[..]);
    }

    /// Memory for [`Kept::at`], as a mapping the kernel holds would be.
    fn memory() -> Vec<u128> {
        vec![0u128; Kept::LEN / 16]
    }

    #[test]
    fn what_was_written_and_not_flushed_is_the_next_disks_over_the_same_memory() {
        let dev = device(4 * CHUNK);
        let mut memory = memory();
        let base = memory.as_mut_ptr().cast::<u8>();
        // SAFETY: the test's own memory, one `Disk` over it at a time.
        let mut disk = Disk::over(dev.clone(), dev.len() as u64, unsafe { Kept::at(base) })
            .expect("new memory");
        disk.write_all_at(1000, &[7; 3000]).unwrap();
        disk.write_all_at(2 * CHUNK as u64 + 512, &[8; 512])
            .unwrap();
        // The process dies: the device has nothing of it.
        let left = disk.blocks().clone();
        core::mem::forget(disk);
        assert_eq!(left, dev);

        // SAFETY: as above; the first is gone.
        let mut next =
            Disk::over(left, dev.len() as u64, unsafe { Kept::at(base) }).expect("the same device");
        assert!(next.is_dirty());
        let mut expect = dev.clone();
        expect[1000..4000].fill(7);
        expect[2 * CHUNK + 512..2 * CHUNK + 1024].fill(8);
        assert_eq!(read_all(&mut next, 0, 4 * CHUNK), expect);
        next.flush().unwrap();
        assert!(!next.is_dirty());
        assert_eq!(next.blocks()[..], expect[..]);
    }

    #[test]
    fn memory_kept_for_another_device_is_not_taken() {
        let dev = device(4 * CHUNK);
        let mut memory = memory();
        let base = memory.as_mut_ptr().cast::<u8>();
        // SAFETY: the test's own memory, one `Disk` over it at a time.
        drop(Disk::over(dev.clone(), dev.len() as u64, unsafe { Kept::at(base) }).unwrap());
        // SAFETY: as above.
        assert!(Disk::over(dev[..CHUNK].to_vec(), CHUNK as u64, unsafe {
            Kept::at(base)
        })
        .is_none());
        // What is not a `Disk`'s: a chunk in two places.
        // SAFETY: as above.
        let mut kept = unsafe { Kept::at(base) };
        kept.meta_mut(0).index = 1;
        kept.meta_mut(1).index = 1;
        assert!(Disk::over(dev.clone(), dev.len() as u64, kept).is_none());
    }
    #[test]
    fn a_group_a_dead_disk_was_making_is_taken_back_and_one_it_made_is_not() {
        let dev = device(4 * CHUNK);
        let len = dev.len() as u64;
        let mut mem = memory();
        let base = mem.as_mut_ptr().cast::<u8>();
        let (a, b) = (CHUNK as u64 + 700, 3 * CHUNK as u64 - 3);
        let mut want = dev.clone();
        want[a as usize..a as usize + 5].copy_from_slice(b"first");
        want[b as usize..b as usize + 6].copy_from_slice(b"second");
        // The disk stops after each of the changes the group is, and
        // then is let finish: the one after it has all of the group or,
        // while there was a change left to make, none.
        let mut dev_now = dev.clone();
        for cut in 0.. {
            // SAFETY: the test's own memory, one disk over it at a time.
            let mut disk = Disk::over(dev_now, len, unsafe { Kept::at(base) }).unwrap();
            let has = read_all(&mut disk, 0, dev.len());
            if has == want {
                assert!(cut > 8, "{cut} changes");
                break;
            }
            assert!(has == dev, "cut at {cut}");
            disk.changes_left = Some(cut);
            disk.begin();
            disk.write_all_at(a, b"first").unwrap();
            disk.seek(SeekFrom::Start(b)).unwrap();
            // Across two sectors, and two chunks.
            disk.write_all(b"second").unwrap();
            // Some of it reaches the device inside the group.
            disk.flush().unwrap();
            disk.commit();
            dev_now = disk.blocks().clone();
            core::mem::forget(disk);
        }
    }

    #[test]
    fn a_group_longer_than_the_journal_is_written_and_stays() {
        let dev = device(8 * CHUNK);
        let len = dev.len() as u64;
        let mut mem = memory();
        let base = mem.as_mut_ptr().cast::<u8>();
        // SAFETY: the test's own memory, one disk over it at a time.
        let mut disk = Disk::over(dev.clone(), len, unsafe { Kept::at(base) }).unwrap();
        disk.begin();
        let wrote = vec![0xa5u8; (GROUP_MAX + 9) * SECTOR - 20];
        disk.write_all_at(CHUNK as u64 + 10, &wrote).unwrap();
        // Not taken back by the disk after this one, the group made or
        // not: there is no knowing all of what it was.
        let left = disk.blocks().clone();
        core::mem::forget(disk);
        // SAFETY: as above; the first is gone.
        let mut disk = Disk::over(left, len, unsafe { Kept::at(base) }).unwrap();
        let mut want = dev;
        want[CHUNK + 10..CHUNK + 10 + wrote.len()].copy_from_slice(&wrote);
        assert_eq!(read_all(&mut disk, 0, want.len()), want);
        disk.flush().unwrap();
        assert_eq!(*disk.blocks(), want);
    }
}
