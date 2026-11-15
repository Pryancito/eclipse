//! The disk as the FAT code reads it: a stream over the device, with the
//! parts of it read so far kept.
//!
//! `fatfs` reads a few bytes at a time -- an entry of the allocation table,
//! a directory entry, a cluster of a file -- and each read of the device is
//! a system call. So the device is read a chunk at a time, and the chunks
//! last read are kept: the table and the directories stay in them, and a
//! file read in order costs one read of the device per chunk.
//!
//! Nothing is written: the mount is read-only.

use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom, Write};

/// What the device is read in.
pub const CHUNK: usize = 64 * 1024;
/// The chunks kept: 4 MiB. The file data the kernel has read is in its page
/// cache, not here; these are the table, the directories, and what was
/// read ahead.
pub const CHUNKS_KEPT: usize = 64;

/// Something bytes are read from at an offset: the device, or in a test a
/// volume in memory.
pub trait Blocks {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
}

impl Blocks for std::fs::File {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        std::os::unix::fs::FileExt::read_at(self, buf, offset)
    }
}

impl Blocks for Vec<u8> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let from = (offset as usize).min(self.len());
        let n = buf.len().min(self.len() - from);
        buf[..n].copy_from_slice(&self[from..from + n]);
        Ok(n)
    }
}

struct Chunk {
    bytes: Box<[u8]>,
    /// When it was last read from: the least recent goes first.
    used: u64,
}

pub struct Disk<B> {
    blocks: B,
    len: u64,
    pos: u64,
    chunks: HashMap<u64, Chunk>,
    clock: u64,
    /// How many times the device was read.
    pub device_reads: u64,
}

impl<B: Blocks> Disk<B> {
    /// `blocks`, which holds `len` bytes.
    pub fn new(blocks: B, len: u64) -> Self {
        Disk {
            blocks,
            len,
            pos: 0,
            chunks: HashMap::new(),
            clock: 0,
            device_reads: 0,
        }
    }

    /// The chunk `index`, from the device if it is not kept.
    fn chunk(&mut self, index: u64) -> io::Result<&[u8]> {
        self.clock += 1;
        if !self.chunks.contains_key(&index) {
            let start = index * CHUNK as u64;
            let len = (self.len - start).min(CHUNK as u64) as usize;
            let mut bytes = vec![0u8; len].into_boxed_slice();
            let mut done = 0;
            while done < len {
                self.device_reads += 1;
                match self.blocks.read_at(start + done as u64, &mut bytes[done..]) {
                    Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                    Ok(n) => done += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            if self.chunks.len() >= CHUNKS_KEPT {
                let oldest = self
                    .chunks
                    .iter()
                    .min_by_key(|(_, c)| c.used)
                    .map(|(i, _)| *i);
                if let Some(oldest) = oldest {
                    self.chunks.remove(&oldest);
                }
            }
            self.chunks.insert(index, Chunk { bytes, used: 0 });
        }
        let chunk = self.chunks.get_mut(&index).expect("just put there");
        chunk.used = self.clock;
        Ok(&chunk.bytes)
    }
}

/// A read this long, of whole sectors from a sector's start, goes straight
/// from the device to whoever asked: a file's data, which the kernel keeps
/// once it has it and this would only hold a second time.
const DIRECT_MIN: usize = 4096;
const SECTOR: u64 = 512;

impl<B: Blocks> Disk<B> {
    /// Fill `buf` from `offset`, wherever the stream's position is: an
    /// error when the device ends first.
    pub fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let direct = buf.len() >= DIRECT_MIN
            && offset.is_multiple_of(SECTOR)
            && (buf.len() as u64).is_multiple_of(SECTOR);
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            if at >= self.len {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            if direct {
                self.device_reads += 1;
                match self.blocks.read_at(at, &mut buf[done..]) {
                    Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                    Ok(n) => done += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            } else {
                let (index, within) = (at / CHUNK as u64, (at % CHUNK as u64) as usize);
                let chunk = self.chunk(index)?;
                let n = (buf.len() - done).min(chunk.len() - within);
                buf[done..done + n].copy_from_slice(&chunk[within..within + n]);
                done += n;
            }
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
        let (index, at) = (self.pos / CHUNK as u64, (self.pos % CHUNK as u64) as usize);
        let chunk = self.chunk(index)?;
        let n = buf.len().min(chunk.len() - at);
        buf[..n].copy_from_slice(&chunk[at..at + n]);
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
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::ErrorKind::ReadOnlyFilesystem.into())
    }

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
    fn nothing_is_written() {
        let mut disk = Disk::new(device(CHUNK), CHUNK as u64);
        assert!(disk.write(b"x").is_err());
    }
}
