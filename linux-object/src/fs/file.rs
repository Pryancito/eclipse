//! File handle for process

use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};

use async_trait::async_trait;
use lock::{Mutex, RwLock};

use rcore_fs::vfs::{FileType, FsError, INode, Metadata, PollStatus, Timespec};
use zircon_object::object::*;
use zircon_object::vm::{pages, VmObject};

use super::FileLike;
use crate::error::{LxError, LxResult};

bitflags::bitflags! {
    /// File open flags
    pub struct OpenFlags: usize {
        /// read only
        const RDONLY = 0;
        /// write only
        const WRONLY = 1;
        /// read write
        const RDWR = 2;
        /// create file if it does not exist
        const CREATE = 1 << 6;
        /// error if CREATE and the file exists
        const EXCLUSIVE = 1 << 7;
        /// truncate file upon open
        const TRUNCATE = 1 << 9;
        /// append on each write
        const APPEND = 1 << 10;
        /// non block open
        const NON_BLOCK = 1 << 11;
        /// close on exec
        const CLOEXEC = 1 << 19;
    }
}

impl OpenFlags {
    /// check if the OpenFlags is readable
    pub fn readable(self) -> bool {
        let b = self.bits() & 0b11;
        b == Self::RDONLY.bits() || b == Self::RDWR.bits()
    }
    /// check if the OpenFlags is writable
    pub fn writable(self) -> bool {
        let b = self.bits() & 0b11;
        b == Self::WRONLY.bits() || b == Self::RDWR.bits()
    }
    /// check if the OpenFlags caontains append
    pub fn is_append(self) -> bool {
        self.contains(Self::APPEND)
    }
    /// check if the OpenFlags caontains non-block
    pub fn non_block(self) -> bool {
        self.contains(Self::NON_BLOCK)
    }
    /// close on exec
    pub fn close_on_exec(self) -> bool {
        self.contains(Self::CLOEXEC)
    }
}

bitflags::bitflags! {
    pub struct PollEvents: u16 {
        /// There is data to read.
        const IN = 0x0001;
        /// Writing is now possible.
        const OUT = 0x0004;
        /// Error condition (return only)
        const ERR = 0x0008;
        /// Hang up (return only)
        const HUP = 0x0010;
        /// Invalid request: fd not open (return only)
        const INVAL = 0x0020;
    }
}

/// file seek type
#[derive(Debug)]
pub enum SeekFrom {
    /// seek from start point
    Start(u64),
    /// seek from end
    End(i64),
    /// seek from current
    Current(i64),
}

/// file inner mut data struct
#[derive(Clone)]
struct FileInner {
    /// content offset on read/write
    offset: u64,
    /// file open options
    flags: OpenFlags,
    /// file INode
    inode: Arc<dyn INode>,
}

/// file implement struct
pub struct File {
    /// object base
    base: KObjectBase,
    /// file path
    path: String,
    /// file inner mut data
    inner: RwLock<FileInner>,
}

impl_kobject!(File);

/// Readahead window ceiling, in pages. A purely-sequential scan reads at most
/// this many pages in one backing read before serving from memory.
const READAHEAD_MAX_PAGES: usize = 16; // 64 KiB at a 4 KiB page

/// Per-mapping sequential-readahead buffer.
///
/// A file-backed mapping is demand-paged one page per fault, i.e. one inode
/// read per page for a sequential scan. This buffer collapses such a scan into
/// a few large reads: on a sequential miss it reads a window of several pages
/// at once and serves the following faults from memory, ramping the window up
/// while access stays sequential and snapping back to a single page on a random
/// jump (so random access never reads more than it needs).
///
/// It is purely an I/O optimisation — the bytes placed into a page are exactly
/// those a single-page read would have produced (see the equivalence tests).
struct Readahead {
    /// Cached source bytes and the mapping offset `buf[0]` corresponds to.
    buf: Vec<u8>,
    buf_off: usize,
    /// Valid bytes in `buf`; a short backing read leaves the tail invalid.
    buf_len: usize,
    /// Mapping offset the next *sequential* fault is expected at.
    next_off: usize,
    /// Current readahead window in pages, ramped 1 -> MAX while sequential.
    window_pages: usize,
}

impl Readahead {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            buf_off: 0,
            buf_len: 0,
            next_off: usize::MAX, // first access counts as non-sequential
            window_pages: 1,
        }
    }

    /// Fills one pre-zeroed `page` with source bytes at mapping `offset`.
    ///
    /// `source_len` bounds the readable region (bytes at or past it stay zero,
    /// the BSS tail). `read(off, dst) -> valid` fetches source bytes starting at
    /// mapping offset `off` into `dst`, returning how many are valid (0 at
    /// EOF/error); the caller bounds `dst` to the source so `read` never runs
    /// past end-of-file.
    fn fill_page<F>(&mut self, offset: usize, page: &mut [u8], source_len: usize, mut read: F)
    where
        F: FnMut(usize, &mut [u8]) -> usize,
    {
        if offset >= source_len {
            return; // wholly past end-of-file: leave the page zero
        }
        let want = (source_len - offset).min(page.len());

        // 1. Serve from the buffer when the whole requested span is cached.
        if offset >= self.buf_off && offset + want <= self.buf_off + self.buf_len {
            let s = offset - self.buf_off;
            page[..want].copy_from_slice(&self.buf[s..s + want]);
            self.next_off = offset + page.len();
            return;
        }

        // 2. Miss: read a window ahead when sequential, else just this page.
        self.window_pages = if offset == self.next_off {
            (self.window_pages * 2).min(READAHEAD_MAX_PAGES)
        } else {
            1
        };
        let chunk = (self.window_pages * page.len()).min(source_len - offset);
        self.buf.clear();
        self.buf.resize(chunk, 0);
        let valid = read(offset, &mut self.buf).min(chunk);
        self.buf_off = offset;
        self.buf_len = valid;

        // 3. Serve this page from the freshly read window (short read -> the
        // unread tail stays zero, exactly as a single-page read would leave it).
        let avail = want.min(valid);
        page[..avail].copy_from_slice(&self.buf[..avail]);
        self.next_off = offset + page.len();
    }
}

/// Demand-paging source for a file-backed `mmap` (see [`get_vmo`]).
///
/// Reads from the backing inode the first time a page is touched, so a large
/// mapping (e.g. `libLLVM.so`) is paged in lazily instead of being read into
/// memory in full at map time. A [`Readahead`] window batches the inode reads
/// of a sequential scan without changing the bytes any page receives.
///
/// [`get_vmo`]: File::get_vmo
struct FileFrameFiller {
    inode: Arc<dyn INode>,
    /// File offset that VMO offset 0 maps to.
    file_offset: usize,
    /// Number of readable bytes from `file_offset` within the mapping. Pages
    /// past this are left zero (the BSS tail of the mapping).
    source_len: usize,
    /// Sequential-readahead state. Guarded by a mutex for soundness; in practice
    /// the VMO serialises `fill_page` under its own lock, so it never contends.
    readahead: Mutex<Readahead>,
}

impl zircon_object::vm::FrameFiller for FileFrameFiller {
    fn source_len(&self) -> usize {
        self.source_len
    }

    fn fill_page(&self, offset: usize, buf: &mut [u8]) {
        let inode = &self.inode;
        let file_offset = self.file_offset;
        self.readahead
            .lock()
            .fill_page(offset, buf, self.source_len, |off, dst| {
                // Read `dst` worth of source bytes at mapping offset `off`,
                // looping over short reads. A read error mid-mapping leaves the
                // rest zero-filled; the faulting access proceeds rather than
                // wedging the kernel.
                let file_pos = file_offset + off;
                let mut done = 0;
                while done < dst.len() {
                    match inode.read_at(file_pos + done, &mut dst[done..]) {
                        Ok(0) => break,
                        Ok(n) => done += n,
                        Err(_) => break,
                    }
                }
                done
            });
    }
}

impl FileInner {
    /// write to file
    fn write(&mut self, buf: &[u8]) -> LxResult<usize> {
        let offset = if self.flags.is_append() {
            self.inode.metadata()?.size as u64
        } else {
            self.offset
        };
        let len = self.write_at(offset, buf)?;
        self.offset = offset + len as u64;
        Ok(len)
    }

    /// write to file at given offset
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> LxResult<usize> {
        if !self.flags.writable() {
            return Err(LxError::EBADF);
        }
        let len = self.inode.write_at(offset as usize, buf)?;
        Ok(len)
    }
}

impl File {
    /// create a file struct
    pub fn new(inode: Arc<dyn INode>, flags: OpenFlags, path: String) -> Arc<Self> {
        Arc::new(File {
            base: KObjectBase::new(),
            path,
            inner: RwLock::new(FileInner {
                offset: 0,
                flags,
                inode,
            }),
        })
    }

    /// Returns the file path.
    pub fn path(&self) -> &String {
        &self.path
    }

    /// seek from given type and offset
    pub fn seek(&self, pos: SeekFrom) -> LxResult<u64> {
        let mut inner = self.inner.write();
        // Compute the new offset with checked arithmetic and reject results
        // that would be negative; otherwise a negative relative seek would wrap
        // to a huge `u64` and let later reads/writes use an out-of-range offset.
        let new_offset: i64 = match pos {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::End(offset) => (inner.inode.metadata()?.size as i64)
                .checked_add(offset)
                .ok_or(LxError::EINVAL)?,
            SeekFrom::Current(offset) => (inner.offset as i64)
                .checked_add(offset)
                .ok_or(LxError::EINVAL)?,
        };
        if new_offset < 0 {
            return Err(LxError::EINVAL);
        }
        inner.offset = new_offset as u64;
        Ok(inner.offset)
    }

    /// resize the file
    pub fn set_len(&self, len: u64) -> LxResult {
        let inner = self.inner.write();
        if !inner.flags.writable() {
            return Err(LxError::EBADF);
        }
        inner.inode.resize(len as usize)?;
        Ok(())
    }

    /// Sync all data and metadata
    pub fn sync_all(&self) -> LxResult {
        self.inner.read().inode.sync_all()?;
        Ok(())
    }

    /// Sync data (not include metadata)
    pub fn sync_data(&self) -> LxResult {
        self.inner.read().inode.sync_data()?;
        Ok(())
    }

    /// get metadata of file
    /// fstat
    pub fn metadata(&self) -> LxResult<Metadata> {
        Ok(self.inner.read().inode.metadata()?)
    }

    /// lookup the file following the link
    pub fn lookup_follow(&self, path: &str, max_follow: usize) -> LxResult<Arc<dyn INode>> {
        Ok(self.inner.read().inode.lookup_follow(path, max_follow)?)
    }

    /// get the name of dir entry
    pub fn read_entry(&self) -> LxResult<String> {
        Ok(self.read_entry_with_metadata()?.1)
    }

    /// get the next directory entry and its metadata
    pub fn read_entry_with_metadata(&self) -> LxResult<(Metadata, String)> {
        let mut inner = self.inner.write();
        if !inner.flags.readable() {
            return Err(LxError::EBADF);
        }
        let offset = inner.offset as usize;
        match inner.inode.get_entry_with_metadata(offset) {
            Ok(entry) => {
                inner.offset += 1;
                Ok(entry)
            }
            Err(e) => {
                // `get_entry_with_metadata`'s default implementation resolves
                // the entry's metadata via `find(name)`, which can fail even
                // though the entry exists — e.g. the devfs root's ".." has no
                // parent, so `find("..")` returns EntryNotFound. Treating that
                // as end-of-directory truncates the listing (this made
                // `ls /dev` appear empty). Distinguish the two: if the name
                // still resolves, emit it with a synthetic directory metadata;
                // only a missing name means we reached the end.
                let name = inner.inode.get_entry(offset).map_err(|_| e)?;
                inner.offset += 1;
                let meta = Metadata {
                    dev: 0,
                    inode: 0,
                    size: 0,
                    blk_size: 0,
                    blocks: 0,
                    atime: Timespec { sec: 0, nsec: 0 },
                    mtime: Timespec { sec: 0, nsec: 0 },
                    ctime: Timespec { sec: 0, nsec: 0 },
                    type_: FileType::Dir,
                    mode: 0,
                    nlinks: 1,
                    uid: 0,
                    gid: 0,
                    rdev: 0,
                };
                Ok((meta, name))
            }
        }
    }

    /// get INode of this file
    pub fn inode(&self) -> Arc<dyn INode> {
        self.inner.read().inode.clone()
    }
}

#[async_trait]
impl FileLike for File {
    fn flags(&self) -> OpenFlags {
        self.inner.read().flags
    }

    fn set_flags(&self, f: OpenFlags) -> LxResult {
        let flags = &mut self.inner.write().flags;
        flags.set(OpenFlags::APPEND, f.contains(OpenFlags::APPEND));
        flags.set(OpenFlags::NON_BLOCK, f.contains(OpenFlags::NON_BLOCK));
        flags.set(OpenFlags::CLOEXEC, f.contains(OpenFlags::CLOEXEC));
        Ok(())
    }

    fn dup(&self) -> Arc<dyn FileLike> {
        Arc::new(Self {
            base: KObjectBase::new(),
            path: self.path.clone(),
            inner: RwLock::new(self.inner.read().clone()),
        })
    }

    async fn read(&self, buf: &mut [u8]) -> LxResult<usize> {
        let (offset, flags, inode) = {
            let inner = self.inner.read();
            (inner.offset, inner.flags, inner.inode.clone())
        };

        if !flags.readable() {
            return Err(LxError::EBADF);
        }

        let len = if !flags.non_block() {
            // block
            loop {
                match inode.read_at(offset as usize, buf) {
                    Ok(read_len) => break read_len,
                    Err(FsError::Again) => {
                        inode.async_poll().await?;
                    }
                    Err(err) => return Err(err.into()),
                }
            }
        } else {
            inode.read_at(offset as usize, buf)?
        };

        let mut inner = self.inner.write();
        inner.offset += len as u64;
        Ok(len)
    }

    fn write(&self, buf: &[u8]) -> LxResult<usize> {
        self.inner.write().write(buf)
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> LxResult<usize> {
        let (flags, inode) = {
            let inner = self.inner.read();
            (inner.flags, inner.inode.clone())
        };

        if !flags.readable() {
            return Err(LxError::EBADF);
        }

        if !flags.non_block() {
            // block
            loop {
                match inode.read_at(offset as usize, buf) {
                    Ok(read_len) => return Ok(read_len),
                    Err(FsError::Again) => {
                        inode.async_poll().await?;
                    }
                    Err(err) => return Err(err.into()),
                }
            }
        }
        let len = inode.read_at(offset as usize, buf)?;
        Ok(len)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> LxResult<usize> {
        self.inner.write().write_at(offset, buf)
    }

    fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        let inner = self.inner.read();
        // A FIFO node opened from the fs has no pipe-buffer / writer tracking
        // here, so a reader polling an empty FIFO (e.g. openrc-init's control
        // FIFO, which never gets a writer) would spin: the node reads as an
        // empty regular file (0 bytes = EOF) yet polls "readable". Treat it as
        // readable only when it actually holds bytes, so the reader blocks
        // instead of busy-looping on repeated 0-byte reads.
        //
        // Use metadata() best-effort: some fds (sockets, special devices) don't
        // implement it and return ENOSYS — those must fall through to the inode's
        // own poll(), NOT propagate the error (that regressed `poll()` on packet
        // sockets, e.g. udhcpc, to "Function not implemented").
        if let Ok(meta) = inner.inode.metadata() {
            if meta.type_ == FileType::NamedPipe {
                return Ok(PollStatus {
                    read: meta.size > 0,
                    write: true,
                    error: false,
                });
            }
        }
        Ok(inner.inode.poll()?)
    }

    async fn async_poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        let inode = self.inner.read().inode.clone();
        // See `poll`: special-case an empty FIFO so the reader blocks, but only
        // when metadata() is available — sockets/special devices return ENOSYS
        // and must fall through to the inode's own async_poll() rather than
        // failing the whole poll() syscall.
        if let Ok(meta) = inode.metadata() {
            if meta.type_ == FileType::NamedPipe {
                return Ok(PollStatus {
                    read: meta.size > 0,
                    write: true,
                    error: false,
                });
            }
        }
        Ok(inode.async_poll().await?)
    }

    fn ioctl(&self, request: usize, arg1: usize, _arg2: usize, _arg3: usize) -> LxResult<usize> {
        // ioctl syscall
        self.inner.read().inode.io_control(request as u32, arg1)?;
        Ok(0)
    }

    fn is_input_device(&self) -> bool {
        use super::devfs::{EventDev, MiceDev};
        let inode = self.inner.read().inode.clone();
        inode.downcast_ref::<MiceDev>().is_some() || inode.downcast_ref::<EventDev>().is_some()
    }

    /// Returns the [`VmObject`] representing the file with given `offset` and `len`.
    fn get_vmo(&self, offset: usize, len: usize) -> LxResult<Arc<VmObject>> {
        let inner = self.inner.read();
        match inner.inode.metadata()?.type_ {
            FileType::File => {
                // Back the file mapping with a *paged* (non-contiguous) VMO that
                // is demand-paged from the file: each page is read in on the
                // page fault that first touches it, instead of reading the whole
                // mapping up front.
                //
                // Eagerly reading the whole mapping used to stall the machine:
                // the dynamic linker maps a library's entire LOAD span in one
                // `mmap`, and the ~150 MiB `libLLVM.so` pulled in by `perf`
                // forced ~9.6k synchronous 16 KiB reads plus a full commit of
                // every page before the syscall returned — on real hardware that
                // looked like a hard freeze (couldn't even switch VT). A non-PIE
                // program only touches a fraction of such a library, so paging it
                // in on demand reads (and commits) only what is actually used.
                //
                // The source captures the file inode and the file offset; bytes
                // past end-of-file stay zero (the BSS tail of a file mapping).
                let file_size = inner.inode.metadata()?.size;
                let source_len = file_size.saturating_sub(offset).min(len);
                if len >= 16 * 1024 * 1024 {
                    info!(
                        "get_vmo: demand-paged file map len={} MiB offset={:#x} source={} MiB",
                        len / (1024 * 1024),
                        offset,
                        source_len / (1024 * 1024),
                    );
                }
                let source: Arc<dyn zircon_object::vm::FrameFiller> = Arc::new(FileFrameFiller {
                    inode: inner.inode.clone(),
                    file_offset: offset,
                    source_len,
                    readahead: Mutex::new(Readahead::new()),
                });
                Ok(VmObject::new_paged_with_source(pages(len), source))
            }
            FileType::CharDevice => {
                use super::devfs::{DrmDev, FbDev};
                if let Some(fbdev) = inner.inode.downcast_ref::<FbDev>() {
                    fbdev.get_vmo(offset, len)
                } else if let Some(drmdev) = inner.inode.downcast_ref::<DrmDev>() {
                    drmdev.get_vmo(offset, len).map_err(Into::into)
                } else {
                    Err(LxError::ENOSYS)
                }
            }
            _ => Err(LxError::ENOSYS),
        }
    }
}

#[cfg(test)]
mod readahead_tests {
    use super::{Readahead, READAHEAD_MAX_PAGES};
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cell::Cell;

    const PAGE: usize = 16;

    /// Deterministic, non-aligned source pattern.
    fn data(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// What a single-page (non-readahead) fill must produce for `off`.
    fn reference(src: &[u8], source_len: usize, off: usize) -> Vec<u8> {
        let mut page = vec![0u8; PAGE];
        if off < source_len {
            let want = (source_len - off).min(PAGE);
            page[..want].copy_from_slice(&src[off..off + want]);
        }
        page
    }

    /// Drives `Readahead` over `offsets`, asserting every page matches the
    /// single-page reference; returns how many backing reads it issued.
    fn run(src: &[u8], source_len: usize, offsets: &[usize]) -> usize {
        let reads = Cell::new(0usize);
        let mut ra = Readahead::new();
        for &off in offsets {
            let mut page = vec![0u8; PAGE];
            ra.fill_page(off, &mut page, source_len, |o, dst| {
                reads.set(reads.get() + 1);
                let n = dst.len().min(source_len.saturating_sub(o));
                dst[..n].copy_from_slice(&src[o..o + n]);
                n
            });
            assert_eq!(page, reference(src, source_len, off), "offset {}", off);
        }
        reads.get()
    }

    #[test]
    fn sequential_scan_is_equivalent_and_batches_reads() {
        let len = PAGE * 40;
        let src = data(len);
        let offsets: Vec<usize> = (0..40).map(|p| p * PAGE).collect();
        let reads = run(&src, len, &offsets);
        // Equivalence held (asserted inside `run`); and readahead actually
        // collapsed the 40 per-page reads into far fewer windowed ones.
        assert!(reads < 40, "expected batching, got {} reads", reads);
        assert!(reads >= len.div_ceil(READAHEAD_MAX_PAGES * PAGE));
    }

    #[test]
    fn random_and_strided_access_stay_equivalent() {
        let len = PAGE * 64;
        let src = data(len);
        // Strided (every other page) then a deterministic pseudo-random walk.
        let mut offsets: Vec<usize> = (0..32).map(|p| (p * 2) * PAGE).collect();
        let mut x: usize = 12345;
        for _ in 0..200 {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            offsets.push((x % 64) * PAGE);
        }
        run(&src, len, &offsets);
    }

    #[test]
    fn partial_eof_page_and_beyond_are_equivalent() {
        // source_len ends mid-page: the straddling page is half data/half zero,
        // and any page wholly past the end is all zero.
        let len = PAGE * 8 + 7;
        let src = data(len + PAGE); // backing store longer than source_len
        let offsets: Vec<usize> = (0..12).map(|p| p * PAGE).collect();
        run(&src, len, &offsets);
        // Re-touching the partial and the past-end pages out of order still matches.
        run(&src, len, &[PAGE * 8, PAGE * 10, PAGE * 8, 0]);
    }

    #[test]
    fn repeated_same_offset_is_equivalent() {
        let len = PAGE * 4;
        let src = data(len);
        run(&src, len, &[0, 0, PAGE, PAGE, 0, PAGE * 3]);
    }
}
