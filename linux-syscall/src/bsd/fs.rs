//! FreeBSD filesystem ABI structures: `struct stat` (as `fstat`/`fstatat`
//! write it) and `struct dirent` (as `getdirentries` writes it). Both have a
//! completely different layout from their Linux equivalents, so the bytes must
//! be produced field by field rather than reusing the Linux `Stat`/dirent
//! writers.

use super::consts::stat as c;
use alloc::vec::Vec;
use linux_object::error::{LxError, LxResult};
use linux_object::fs::vfs::{FileType, Metadata};

/// FreeBSD `struct stat` for amd64 (`sys/sys/stat.h`, default — i.e. without
/// `__STAT_TIME_T_EXT`). Total size is 224 bytes; a FreeBSD libc `fstat`
/// stub hands the kernel a buffer of exactly this size and reads `st_mode`,
/// `st_size` and the timestamps back out of it.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct BsdStat {
    /// inode's device
    pub st_dev: u64,
    /// inode's number
    pub st_ino: u64,
    /// number of hard links
    pub st_nlink: u64,
    /// inode protection mode
    pub st_mode: u16,
    /// misc system flags
    pub st_bsdflags: i16,
    /// user ID of the file's owner
    pub st_uid: u32,
    /// group ID of the file's group
    pub st_gid: u32,
    /// padding
    pub st_padding1: i32,
    /// device type
    pub st_rdev: u64,
    /// time of last access (tv_sec, tv_nsec)
    pub st_atim: [i64; 2],
    /// time of last data modification
    pub st_mtim: [i64; 2],
    /// time of last file status change
    pub st_ctim: [i64; 2],
    /// time of file creation
    pub st_birthtim: [i64; 2],
    /// file size, in bytes
    pub st_size: i64,
    /// blocks allocated for file
    pub st_blocks: i64,
    /// optimal blocksize for I/O
    pub st_blksize: i32,
    /// user defined flags for file
    pub st_flags: u32,
    /// file generation number
    pub st_gen: u64,
    /// file revision
    pub st_filerev: u64,
    /// reserved (pads the struct to its ABI size of 224 bytes)
    pub st_spare: [u64; 9],
}

/// Map an rcore-fs [`FileType`] to the FreeBSD `S_IF*` type bits. FreeBSD and
/// Linux happen to agree on these octal values, but the mapping is written out
/// so the layer does not silently depend on that coincidence.
fn type_bits(t: FileType) -> u32 {
    match t {
        FileType::File => c::S_IFREG,
        FileType::Dir => c::S_IFDIR,
        FileType::SymLink => c::S_IFLNK,
        FileType::CharDevice => c::S_IFCHR,
        FileType::BlockDevice => c::S_IFBLK,
        FileType::Socket => c::S_IFSOCK,
        FileType::NamedPipe => c::S_IFIFO,
    }
}

impl BsdStat {
    /// Build a FreeBSD `stat` from filesystem metadata.
    pub fn from_metadata(info: &Metadata) -> Self {
        let mode = (type_bits(info.type_) | (info.mode as u32 & 0o7777)) as u16;
        BsdStat {
            st_dev: info.dev as u64,
            st_ino: info.inode as u64,
            st_nlink: info.nlinks as u64,
            st_mode: mode,
            st_uid: info.uid as u32,
            st_gid: info.gid as u32,
            st_rdev: info.rdev as u64,
            st_atim: [info.atime.sec, info.atime.nsec as i64],
            st_mtim: [info.mtime.sec, info.mtime.nsec as i64],
            st_ctim: [info.ctime.sec, info.ctime.nsec as i64],
            // rcore-fs has no birth time; reuse ctime as FreeBSD tools do when a
            // filesystem cannot supply one.
            st_birthtim: [info.ctime.sec, info.ctime.nsec as i64],
            st_size: info.size as i64,
            st_blocks: info.blocks as i64,
            st_blksize: info.blk_size as i32,
            ..Default::default()
        }
    }

    /// The little-endian bytes of this structure, for writing to user memory.
    ///
    /// Serialised field by field in declaration order rather than by
    /// reinterpreting the struct's memory: the crate forbids `unsafe_code`, and
    /// the `#[repr(C)]` layout has no implicit padding (every field is
    /// naturally aligned after the previous one), so an in-order emission
    /// reproduces the exact 224-byte ABI image. The offset test in this module
    /// guards that invariant.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(core::mem::size_of::<BsdStat>());
        b.extend_from_slice(&self.st_dev.to_le_bytes());
        b.extend_from_slice(&self.st_ino.to_le_bytes());
        b.extend_from_slice(&self.st_nlink.to_le_bytes());
        b.extend_from_slice(&self.st_mode.to_le_bytes());
        b.extend_from_slice(&self.st_bsdflags.to_le_bytes());
        b.extend_from_slice(&self.st_uid.to_le_bytes());
        b.extend_from_slice(&self.st_gid.to_le_bytes());
        b.extend_from_slice(&self.st_padding1.to_le_bytes());
        b.extend_from_slice(&self.st_rdev.to_le_bytes());
        for ts in [
            &self.st_atim,
            &self.st_mtim,
            &self.st_ctim,
            &self.st_birthtim,
        ] {
            b.extend_from_slice(&ts[0].to_le_bytes());
            b.extend_from_slice(&ts[1].to_le_bytes());
        }
        b.extend_from_slice(&self.st_size.to_le_bytes());
        b.extend_from_slice(&self.st_blocks.to_le_bytes());
        b.extend_from_slice(&self.st_blksize.to_le_bytes());
        b.extend_from_slice(&self.st_flags.to_le_bytes());
        b.extend_from_slice(&self.st_gen.to_le_bytes());
        b.extend_from_slice(&self.st_filerev.to_le_bytes());
        for w in &self.st_spare {
            b.extend_from_slice(&w.to_le_bytes());
        }
        debug_assert_eq!(b.len(), core::mem::size_of::<BsdStat>());
        b
    }
}

/// Map an rcore-fs [`FileType`] to a FreeBSD `d_type` value.
pub fn dirent_type(t: FileType) -> u8 {
    match t {
        FileType::File => c::DT_REG,
        FileType::Dir => c::DT_DIR,
        FileType::SymLink => c::DT_LNK,
        FileType::CharDevice => c::DT_CHR,
        FileType::BlockDevice => c::DT_BLK,
        FileType::Socket => c::DT_SOCK,
        FileType::NamedPipe => c::DT_FIFO,
    }
}

/// Size of the fixed part of FreeBSD's `struct dirent` (everything before
/// `d_name`): `d_fileno`(8) + `d_off`(8) + `d_reclen`(2) + `d_type`(1) +
/// `d_pad0`(1) + `d_namlen`(2) + `d_pad1`(2) = 24 bytes.
pub const DIRENT_HDR: usize = 24;

/// Round a record length up to the 8-byte boundary FreeBSD's `_GENERIC_DIRSIZ`
/// enforces, so consecutive entries stay naturally aligned.
pub(crate) fn dirsiz(namlen: usize) -> usize {
    let raw = DIRENT_HDR + namlen + 1; // +1 for the terminating NUL.
    raw.div_ceil(8) * 8
}

/// Accumulates FreeBSD `dirent` records into a byte buffer, honouring the
/// caller's byte budget so a record is never split across the end of the
/// user buffer (which `getdirentries` must not do).
pub struct BsdDirentWriter {
    buf: Vec<u8>,
    cap: usize,
}

impl BsdDirentWriter {
    /// A writer that will emit at most `cap` bytes, or `ENOMEM`.
    ///
    /// `Vec::with_capacity` is an infallible allocation, and `cap` is the
    /// caller's own `nbytes`: `getdirentries(fd, buf, 262144, &base)` asked
    /// the kernel for a quarter of a megabyte of its fixed heap up front, and
    /// when the heap could not give it `alloc_error` panicked the machine
    /// instead of the call answering `ENOMEM`. It is the same rule as
    /// [`crate::try_zeroed_buf`], which every other user-sized buffer in this
    /// crate goes through; this one could not, because it was built behind a
    /// constructor that had no way to fail.
    pub fn try_new(cap: usize) -> LxResult<Self> {
        let mut buf = Vec::new();
        buf.try_reserve_exact(cap.min(256 * 1024))
            .map_err(|_| LxError::ENOMEM)?;
        Ok(BsdDirentWriter { buf, cap })
    }

    /// Try to append one directory entry. Returns `false` (and appends
    /// nothing) when the record would not fit in the remaining budget — the
    /// caller stops and reports what was written so far. `off` is the entry's
    /// `d_off`: the directory position that follows it, where an `lseek`
    /// resumes.
    pub fn try_push(&mut self, ino: u64, off: u64, dtype: u8, name: &str) -> bool {
        let namlen = name.len();
        let reclen = dirsiz(namlen);
        if self.buf.len() + reclen > self.cap {
            return false;
        }
        let start = self.buf.len();
        self.buf.extend_from_slice(&ino.to_le_bytes()); // d_fileno
        self.buf.extend_from_slice(&off.to_le_bytes()); // d_off
        self.buf.extend_from_slice(&(reclen as u16).to_le_bytes()); // d_reclen
        self.buf.push(dtype); // d_type
        self.buf.push(0); // d_pad0
        self.buf.extend_from_slice(&(namlen as u16).to_le_bytes()); // d_namlen
        self.buf.extend_from_slice(&0u16.to_le_bytes()); // d_pad1
        self.buf.extend_from_slice(name.as_bytes());
        // Pad (including the NUL terminator) out to reclen.
        self.buf.resize(start + reclen, 0);
        true
    }

    /// The accumulated records.
    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    /// Number of bytes written.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been written yet.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::convert::TryInto;
    use core::mem::{align_of, size_of};

    #[test]
    fn bsd_stat_is_224_bytes() {
        // A FreeBSD amd64 libc fstat stub passes a buffer of exactly this size.
        assert_eq!(size_of::<BsdStat>(), 224);
        assert_eq!(align_of::<BsdStat>(), 8);
    }

    #[test]
    fn stat_field_offsets_match_freebsd() {
        // Spot-check the offsets a real binary actually reads.
        let s = BsdStat::default();
        let base = &s as *const _ as usize;
        assert_eq!(&s.st_mode as *const _ as usize - base, 24);
        assert_eq!(&s.st_uid as *const _ as usize - base, 28);
        assert_eq!(&s.st_size as *const _ as usize - base, 112);
        assert_eq!(&s.st_blksize as *const _ as usize - base, 128);
    }

    #[test]
    fn dirent_records_are_8_byte_aligned() {
        assert_eq!(dirsiz(0), 32); // 24 + 0 + 1 = 25 -> 32.
        assert_eq!(dirsiz(1), 32); // 24 + 1 + 1 = 26 -> 32.
        assert_eq!(dirsiz(7), 32); // 24 + 7 + 1 = 32 -> 32.
        assert_eq!(dirsiz(8), 40); // 24 + 8 + 1 = 33 -> 40.
    }

    #[test]
    fn writer_respects_budget_and_lays_out_fields() {
        let mut w = BsdDirentWriter::try_new(64).unwrap();
        assert!(w.try_push(0x1122, 1, c::DT_REG, "hi"));
        // "hi" -> reclen 32; a second 32-byte record still fits in 64.
        assert!(w.try_push(0x3344, 2, c::DT_DIR, "yo"));
        // Third would overflow 64 bytes.
        assert!(!w.try_push(0x5566, 3, c::DT_REG, "no"));
        assert_eq!(w.len(), 64);

        let b = w.as_slice();
        // First record: d_fileno little-endian.
        assert_eq!(u64::from_le_bytes(b[0..8].try_into().unwrap()), 0x1122);
        // d_off at offset 8: the position that follows the entry.
        assert_eq!(u64::from_le_bytes(b[8..16].try_into().unwrap()), 1);
        assert_eq!(u64::from_le_bytes(b[40..48].try_into().unwrap()), 2);
        // d_reclen at offset 16.
        assert_eq!(u16::from_le_bytes(b[16..18].try_into().unwrap()), 32);
        // d_type at offset 18.
        assert_eq!(b[18], c::DT_REG);
        // d_namlen at offset 20.
        assert_eq!(u16::from_le_bytes(b[20..22].try_into().unwrap()), 2);
        // name bytes at offset 24.
        assert_eq!(&b[24..26], b"hi");
    }
}

#[cfg(test)]
mod dirent_writer_alloc_tests {
    use super::*;

    #[test]
    fn a_hostile_nbytes_is_not_asked_of_the_allocator() {
        // `getdirentries` hands `nbytes` straight through, so this is the
        // caller's own number. The reservation is clamped; `cap` is not,
        // because it is the byte budget `try_push` measures against.
        let w = BsdDirentWriter::try_new(usize::MAX).expect("a clamped reservation fits");
        assert_eq!(w.len(), 0);
        assert!(w.buf.capacity() <= 256 * 1024, "{}", w.buf.capacity());
        assert_eq!(w.cap, usize::MAX);
    }

    #[test]
    fn the_reservation_is_exact_for_a_request_under_the_clamp() {
        // `try_reserve_exact`, so the buffer a 4 KiB `getdirentries` gets is
        // 4 KiB and not the next power of two, and the `try_push`es that
        // follow never reallocate -- a reallocation is the infallible path
        // again, which is what this constructor exists to avoid.
        let w = BsdDirentWriter::try_new(4096).expect("4 KiB fits");
        assert_eq!(w.buf.capacity(), 4096);
        assert_eq!(w.cap, 4096);
    }
}

#[cfg(test)]
mod benches {
    //! What `getdirentries` pays per name, and the `stat` field it fills for
    //! each one. `dirsiz` runs once per entry and the type map once per
    //! entry, so a FreeBSD `readdir` over a large directory pays both as
    //! many times as there are names -- on top of the filesystem walk that
    //! `file::dir::benches` showed is quadratic.
    //!
    //! Compare against `crate::benches::the_second_floor_control` before
    //! reading a cost into any of them.

    use super::*;
    use test::{black_box, Bencher};

    #[bench]
    fn dirent_type_of_a_file(b: &mut Bencher) {
        b.iter(|| black_box(dirent_type(black_box(FileType::File))));
    }

    /// The last arm of the match, which costs the same as the first if the
    /// compiler made a table of it.
    #[bench]
    fn dirent_type_of_a_named_pipe(b: &mut Bencher) {
        b.iter(|| black_box(dirent_type(black_box(FileType::NamedPipe))));
    }

    /// The record length for a name of the length a shared library has: the
    /// header, the name, its NUL, rounded up to eight.
    #[bench]
    fn dirsiz_of_a_library_name(b: &mut Bencher) {
        b.iter(|| black_box(dirsiz(black_box(22))));
    }

    /// A name whose length lands exactly on the boundary, so the round-up
    /// adds nothing: the same work either way, which is the point.
    #[bench]
    fn dirsiz_already_aligned(b: &mut Bencher) {
        b.iter(|| black_box(dirsiz(black_box(7))));
    }
}
