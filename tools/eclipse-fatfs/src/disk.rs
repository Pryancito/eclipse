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

struct Chunk {
    bytes: Box<[u8]>,
    /// The sectors whose bytes are here: the others are still only on the
    /// device.
    held: Sectors,
    /// The sectors written here that the device does not have yet.
    dirty: Sectors,
    /// When it was last used: the least recent goes first.
    used: u64,
}

pub struct Disk<B> {
    blocks: B,
    len: u64,
    pos: u64,
    chunks: HashMap<u64, Chunk, BuildHasherDefault<ByIndex>>,
    clock: u64,
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
    /// `blocks`, which holds `len` bytes.
    pub fn new(blocks: B, len: u64) -> Self {
        Disk {
            blocks,
            len,
            pos: 0,
            chunks: HashMap::default(),
            clock: 0,
            device_reads: 0,
            device_writes: 0,
        }
    }

    /// The device, as it is: without what [`flush`](Self::flush) has not
    /// written yet.
    pub fn blocks(&self) -> &B {
        &self.blocks
    }

    /// Whether something written has not reached the device.
    pub fn is_dirty(&self) -> bool {
        self.chunks.values().any(|c| c.dirty != 0)
    }

    /// The bytes of the chunk `index`: the last one may be short.
    fn chunk_len(&self, index: u64) -> usize {
        (self.len - index * CHUNK as u64).min(CHUNK as u64) as usize
    }

    /// Write the sectors of the chunk `index` the device does not have, a
    /// run of them at a time.
    fn write_out(&mut self, index: u64) -> io::Result<()> {
        let Some(chunk) = self.chunks.get_mut(&index) else {
            return Ok(());
        };
        let mut left = chunk.dirty;
        while left != 0 {
            let from = left.trailing_zeros() as usize;
            let run = (left >> from).trailing_ones() as usize;
            let (start, end) = (
                from * SECTOR,
                ((from + run) * SECTOR).min(chunk.bytes.len()),
            );
            self.device_writes += 1;
            write_fully(
                &mut self.blocks,
                index * CHUNK as u64 + start as u64,
                &chunk.bytes[start..end],
            )?;
            left &= !sectors(from, from + run);
            chunk.dirty = left;
        }
        Ok(())
    }

    /// Everything written reaches the device, and the device its medium.
    pub fn flush(&mut self) -> io::Result<()> {
        let mut dirty: Vec<u64> = self
            .chunks
            .iter()
            .filter(|(_, c)| c.dirty != 0)
            .map(|(i, _)| *i)
            .collect();
        dirty.sort_unstable();
        for index in dirty {
            self.write_out(index)?;
        }
        self.blocks.sync()
    }

    /// The chunk `index` with the sectors `needed` of it held, from the
    /// device those that are not.
    fn chunk(&mut self, index: u64, needed: Sectors) -> io::Result<&mut Chunk> {
        self.clock += 1;
        if !self.chunks.contains_key(&index) {
            if self.chunks.len() >= CHUNKS_KEPT {
                let oldest = self
                    .chunks
                    .iter()
                    .min_by_key(|(_, c)| c.used)
                    .map(|(i, _)| *i);
                if let Some(oldest) = oldest {
                    self.write_out(oldest)?;
                    self.chunks.remove(&oldest);
                }
            }
            let bytes = vec![0u8; self.chunk_len(index)].into_boxed_slice();
            self.chunks.insert(
                index,
                Chunk {
                    bytes,
                    held: 0,
                    dirty: 0,
                    used: 0,
                },
            );
        }
        let missing = needed & !self.chunks[&index].held;
        if missing != 0 {
            // One read of the chunk, of which the sectors not held are
            // taken: those held may be newer than the device.
            let len = self.chunk_len(index);
            let mut read = vec![0u8; len];
            self.device_reads += 1;
            read_fully(&self.blocks, index * CHUNK as u64, &mut read)?;
            let chunk = self.chunks.get_mut(&index).expect("just put there");
            for sector in 0..len.div_ceil(SECTOR) {
                if chunk.held & (1 << sector) == 0 {
                    let (from, to) = (sector * SECTOR, ((sector + 1) * SECTOR).min(len));
                    chunk.bytes[from..to].copy_from_slice(&read[from..to]);
                }
            }
            chunk.held = Sectors::MAX;
        }
        let chunk = self.chunks.get_mut(&index).expect("just put there");
        chunk.used = self.clock;
        Ok(chunk)
    }

    /// Read from the chunk the offset `at` is in, up to its end.
    fn read_chunk(&mut self, at: u64, buf: &mut [u8]) -> io::Result<usize> {
        let (index, within) = (at / CHUNK as u64, (at % CHUNK as u64) as usize);
        let n = buf.len().min(self.chunk_len(index) - within);
        let needed = sectors(within / SECTOR, (within + n).div_ceil(SECTOR));
        // The usual read, of a chunk that is kept and holds it: one lookup.
        self.clock += 1;
        let chunk = match self.chunks.get_mut(&index) {
            Some(chunk) if needed & !chunk.held == 0 => {
                chunk.used = self.clock;
                chunk
            }
            _ => self.chunk(index, needed)?,
        };
        buf[..n].copy_from_slice(&chunk.bytes[within..within + n]);
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
        let chunk = self.chunk(index, touched & !whole)?;
        chunk.bytes[within..end].copy_from_slice(&buf[..n]);
        chunk.held |= touched;
        chunk.dirty |= touched;
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
    /// `(chunk, first sector, sectors, where in the range)`: the range is
    /// whole sectors.
    fn held_in(&self, offset: u64, len: usize) -> Vec<(u64, usize, usize, usize)> {
        let mut parts = Vec::new();
        let mut at = offset;
        let end = offset + len as u64;
        while at < end {
            let (index, within) = (at / CHUNK as u64, (at % CHUNK as u64) as usize);
            let n = ((end - at) as usize).min(CHUNK - within);
            if self.chunks.contains_key(&index) {
                parts.push((index, within / SECTOR, n / SECTOR, (at - offset) as usize));
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
        if direct(offset, buf.len(), DIRECT_MIN) {
            self.device_reads += 1;
            read_fully(&self.blocks, offset, buf)?;
            // What a kept chunk holds of it is as new as the device or
            // newer.
            for (index, first, count, at) in self.held_in(offset, buf.len()) {
                let chunk = &self.chunks[&index];
                for sector in (first..first + count).filter(|s| chunk.held & (1 << s) != 0) {
                    let to = at + (sector - first) * SECTOR;
                    buf[to..to + SECTOR]
                        .copy_from_slice(&chunk.bytes[sector * SECTOR..(sector + 1) * SECTOR]);
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
        if direct(offset, buf.len(), DIRECT_WRITE_MIN) {
            self.device_writes += 1;
            write_fully(&mut self.blocks, offset, buf)?;
            // The kept chunks have it too, and no older bytes to write
            // over it later.
            for (index, first, count, at) in self.held_in(offset, buf.len()) {
                let chunk = self.chunks.get_mut(&index).expect("it was there");
                let (from, n) = (first * SECTOR, count * SECTOR);
                chunk.bytes[from..from + n].copy_from_slice(&buf[at..at + n]);
                chunk.held |= sectors(first, first + count);
                chunk.dirty &= !sectors(first, first + count);
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
}
