//! btrfs mount support via the in-tree `btrfs` crate.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::convert::{TryFrom, TryInto};
use core::sync::atomic::{AtomicUsize, Ordering};

use btrfs::{Btrfs, Error as BtrfsError, FileKind};
use lock::Mutex;
use rcore_fs::vfs::{
    FileSystem, FileType, FsError, FsInfo, INode, Metadata, PollStatus, Result, Timespec,
};
use zcore_drivers::scheme::BlockScheme;

use super::block_mount::{backend_size, device_from_backend, MountBackend};

/// Adapter: rcore-fs `Device` (+ explicit size) → `btrfs::BlockDevice`.
struct DevAdapter {
    inner: Arc<dyn rcore_fs::dev::Device>,
    size: u64,
    /// Adaptive cap on a single backend transfer. Starts at `IO_CHUNK_BYTES`
    /// and ratchets *down* (never up) the first time a larger transfer fails,
    /// so a device/controller that can't sustain big DMA requests settles to a
    /// working size instead of re-failing (and burning retries) on every chunk
    /// of a large file. See `chunked`.
    max_xfer: AtomicUsize,
}

/// How many times a single block transfer is retried before the error is
/// surfaced to btrfs. A large file (e.g. the ~130 MiB `libLLVM.so` pulled in by
/// `apk fix`) is written as *hundreds* of separate block commands, so even a
/// rare transient device error (an AHCI task-file error that the driver clears
/// with a port reset, a momentarily busy controller, …) becomes likely over the
/// whole file. Without a retry that single hiccup aborts the entire extraction
/// with EIO, which is exactly the "failed to extract …: I/O error" seen only on
/// the biggest package. Re-issuing the same offset/buffer is idempotent.
const IO_RETRIES: usize = 5;
/// Cap one backend transfer to a moderate size so block drivers that reject
/// very large requests don't fail a whole btrfs operation.
const IO_CHUNK_BYTES: usize = 128 * 1024;
/// Smallest transfer the shrink-on-error fallback drops to before giving up.
/// Some controllers / DMA paths reject (or intermittently fail) a large
/// request but accept smaller ones. The large requests only happen while
/// streaming a big file's data, so without this fallback a single ~130 MiB
/// package (`libLLVM.so`) fails extraction with EIO while every small package
/// installs fine. Shrinking the failing transfer lets the big file complete
/// (more, smaller commands) instead of aborting the whole operation.
const IO_MIN_BYTES: usize = 512;

impl DevAdapter {
    /// The `usize` start of `[offset, offset + len)`, or `Err` when that is not
    /// a range this device has. Both directions ask the same question:
    /// `write_at` used to ask it and go ahead anyway, and `read_at` never asked
    /// it at all, so an out-of-range transfer reached the driver and only came
    /// back as a failure — after the whole retry-and-shrink ladder below, which
    /// also left the transfer cap permanently lowered.
    fn check_span(&self, offset: u64, len: usize, what: &str) -> btrfs::Result<usize> {
        // `checked_add`, not `+`: in release a wrapped sum passes every
        // comparison and hands the chunk loop a `start` somewhere in the middle
        // of the device, which for `write_at` means writing over live metadata.
        let start = match offset.checked_add(len as u64) {
            Some(end) if end <= self.size => usize::try_from(offset).ok(),
            _ => None,
        };
        match start {
            Some(start) => Ok(start),
            None => {
                // btrfs asked for a range outside the device it was told about:
                // an allocation/geometry bug in the FS layer, not a device
                // fault. Surface it with the numbers needed to debug.
                warn!(
                    "btrfs: {} OUT OF BOUNDS off={:#x} len={} dev_size={:#x}",
                    what, offset, len, self.size,
                );
                Err(BtrfsError::Io)
            }
        }
    }

    /// Transfer `len` bytes in chunks via `op`. Each chunk is retried
    /// `IO_RETRIES` times; if it still fails the chunk is halved (down to
    /// `IO_MIN_BYTES`) and retried, so a size-sensitive device failure on a
    /// large request degrades to slower-but-working smaller requests instead of
    /// a hard EIO. `op(rel_off, this_len)` performs one transfer and returns
    /// `true` on full success.
    fn chunked<F: FnMut(usize, usize) -> bool>(
        &self,
        offset: u64,
        len: usize,
        what: &str,
        mut op: F,
    ) -> btrfs::Result<()> {
        let mut done = 0usize;
        while done < len {
            let cap = self.max_xfer.load(Ordering::Relaxed).max(IO_MIN_BYTES);
            let first_try = (len - done).min(cap);
            // Only a transfer that was actually as large as the cap can say
            // anything about the cap. A short tail, or a metadata read, failing
            // is evidence about that transfer and not about the largest request
            // the device sustains -- and since the cap only ever ratchets down,
            // one such failure used to pin the whole mount near `IO_MIN_BYTES`
            // until it was unmounted.
            let tests_the_cap = first_try == cap;
            let mut piece = first_try;
            loop {
                let mut ok = false;
                for _ in 0..IO_RETRIES {
                    if op(done, piece) {
                        ok = true;
                        break;
                    }
                }
                if ok {
                    if tests_the_cap && piece < first_try {
                        // Remember the size that WORKED, so the rest of this
                        // (large) file uses it directly instead of re-failing
                        // the big transfer on every chunk. Recording the sizes
                        // that failed instead meant a transfer failing at every
                        // size -- which is not a size problem at all -- left the
                        // cap at `IO_MIN_BYTES`.
                        self.max_xfer.fetch_min(piece, Ordering::Relaxed);
                    }
                    done += piece;
                    break;
                }
                if piece > IO_MIN_BYTES {
                    // Re-issuing the same offset/buffer is idempotent; try a
                    // smaller, more conservative transfer.
                    piece = (piece / 2).max(IO_MIN_BYTES);
                    warn!(
                        "btrfs: {} transfer shrunk to {} after failure (off={:#x}, +{})",
                        what, piece, offset, done,
                    );
                    continue;
                }
                warn!(
                    "btrfs: {}(off={:#x}, len={}) failed at +{} even at {}-byte transfers, dev_size={:#x}",
                    what, offset, len, done, IO_MIN_BYTES, self.size,
                );
                return Err(BtrfsError::Io);
            }
        }
        Ok(())
    }
}

impl btrfs::BlockDevice for DevAdapter {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> btrfs::Result<()> {
        let len = buf.len();
        let start = self.check_span(offset, len, "read_at")?;
        self.chunked(offset, len, "read_at", |rel, n| {
            matches!(
                self.inner.read_at(start + rel, &mut buf[rel..rel + n]),
                Ok(got) if got == n
            )
        })
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> btrfs::Result<()> {
        let len = buf.len();
        let start = self.check_span(offset, len, "write_at")?;
        self.chunked(offset, len, "write_at", |rel, n| {
            matches!(
                self.inner.write_at(start + rel, &buf[rel..rel + n]),
                Ok(got) if got == n
            )
        })
    }

    fn sync(&self) -> btrfs::Result<()> {
        self.inner.sync().map_err(|_| BtrfsError::Io)
    }

    fn size(&self) -> u64 {
        self.size
    }
}

fn map_err(e: BtrfsError) -> FsError {
    match e {
        BtrfsError::Io => FsError::DeviceError,
        BtrfsError::BadSuperblock => FsError::WrongFs,
        BtrfsError::Corrupt(msg) => {
            warn!("btrfs: corrupt filesystem: {}", msg);
            FsError::DeviceError
        }
        BtrfsError::Unsupported(msg) => {
            warn!("btrfs: unsupported: {}", msg);
            FsError::NotSupported
        }
        BtrfsError::NotFound => FsError::EntryNotFound,
        BtrfsError::Exists => FsError::EntryExist,
        BtrfsError::NotDir => FsError::NotDir,
        BtrfsError::IsDir => FsError::IsDir,
        BtrfsError::NotEmpty => FsError::DirNotEmpty,
        BtrfsError::NoSpace => FsError::NoDeviceSpace,
        BtrfsError::Invalid => FsError::InvalidParam,
    }
}

fn wall_clock() -> (u64, u32) {
    let now = kernel_hal::timer::wall_clock_now();
    (now.as_secs(), now.subsec_nanos())
}

pub struct BtrfsMountFs {
    inner: Mutex<Btrfs>,
    this: Mutex<Weak<Self>>,
    /// Cached directory listings keyed by inode number (cleared on any
    /// mutation of that directory).
    dir_cache: Mutex<BTreeMap<u64, Arc<Vec<CachedDirEntry>>>>,
    /// Kernel-side write-back coalescing buffer (page-cache-lite). Sequential
    /// small writes to one file are accumulated here and handed to the FS in
    /// large chunks, turning the ~32000 tiny synchronous writes of a big
    /// package extraction (which stalled `apk`) into a few hundred large ones.
    /// At most one file is buffered at a time; any access that isn't a
    /// contiguous append flushes it first, so reads always observe written
    /// data. See `flush_inode` / `flush_any`.
    write_buf: Mutex<Option<PendingWrite>>,
}

/// Pending tail of buffered, not-yet-committed writes for a single inode.
struct PendingWrite {
    ino: u64,
    start: u64,
    data: Vec<u8>,
}

/// Flush the accumulated buffer once it reaches this size.
const WRITE_BUF_FLUSH: usize = 1024 * 1024;

impl PendingWrite {
    /// The offset just past the last buffered byte: where a write has to start
    /// to be a contiguous append, and the end `stat` reports for the file while
    /// the tail is still in memory. Both callers spelled the sum out for
    /// themselves and neither had a test.
    fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }
}

/// Is a write of `ino` at `off` a contiguous append onto the buffered tail?
/// That is the only case the fast path of `write_at` may take, because it skips
/// the `stat` that would tell it anything else about the file.
fn appends_to(pw: &PendingWrite, ino: u64, off: u64) -> bool {
    pw.ino == ino && pw.end() == off
}

/// A write shorter than the flush size is worth accumulating; one at least that
/// long goes straight to the filesystem. Both the "keep buffering" and the
/// "straight through" decisions ask this one question.
fn worth_buffering(len: usize) -> bool {
    len < WRITE_BUF_FLUSH
}

/// Serve `buf` from a fixed run of bytes starting at `offset`, the way
/// `read(2)` pages through one: nothing at or past the end, never more than
/// what is left. Returns how many bytes were copied.
fn read_from(bytes: &[u8], offset: usize, buf: &mut [u8]) -> usize {
    if offset >= bytes.len() {
        return 0;
    }
    let take = buf.len().min(bytes.len() - offset);
    buf[..take].copy_from_slice(&bytes[offset..offset + take]);
    take
}

#[derive(Clone)]
struct CachedDirEntry {
    name: String,
    ino: u64,
}

impl BtrfsMountFs {
    pub fn open(backend: &MountBackend, read_only: bool) -> Result<Arc<Self>> {
        let size = backend_size(backend)?;
        let device = device_from_backend(backend)?;
        let adapter: Arc<dyn btrfs::BlockDevice> = Arc::new(DevAdapter {
            inner: device,
            size,
            max_xfer: AtomicUsize::new(IO_CHUNK_BYTES),
        });
        warn!(
            "btrfs: mounting, device size = {:#x} ({} MiB), read_only={}",
            size,
            size / (1024 * 1024),
            read_only
        );
        let mut fs = Btrfs::mount(adapter, read_only).map_err(map_err)?;
        fs.set_clock(wall_clock);
        // Auto-expand to the partition size (the installer writes a small
        // image onto a larger partition and relies on this).
        if !read_only {
            match fs.grow_to_device() {
                Ok(true) => warn!("btrfs: filesystem expanded to device size"),
                Ok(false) => warn!("btrfs: NOT expanded (FS dev size >= partition size)"),
                Err(e) => warn!("btrfs: grow_to_device failed: {:?}", e),
            }
        }
        let arc = Arc::new(Self {
            inner: Mutex::new(fs),
            this: Mutex::new(Weak::new()),
            dir_cache: Mutex::new(BTreeMap::new()),
            write_buf: Mutex::new(None),
        });
        *arc.this.lock() = Arc::downgrade(&arc);
        Ok(arc)
    }

    fn arc(&self) -> Arc<Self> {
        self.this.lock().upgrade().expect("BtrfsMountFs dropped")
    }

    fn inode(&self, ino: u64) -> Arc<BtrfsMountINode> {
        Arc::new(BtrfsMountINode {
            fs: self.arc(),
            ino,
            kind: Mutex::new(None),
        })
    }

    fn cached_readdir(&self, dir: u64) -> Result<Arc<Vec<CachedDirEntry>>> {
        if let Some(entries) = self.dir_cache.lock().get(&dir) {
            return Ok(entries.clone());
        }
        let entries = {
            let mut fs = self.inner.lock();
            let entries = fs.readdir(dir).map_err(map_err)?;
            let mut cached = Vec::with_capacity(entries.len());
            for entry in entries {
                cached.push(CachedDirEntry {
                    name: entry.name,
                    ino: entry.ino,
                });
            }
            Arc::new(cached)
        };
        self.dir_cache.lock().insert(dir, entries.clone());
        Ok(entries)
    }

    fn invalidate_dir(&self, dir: u64) {
        self.dir_cache.lock().remove(&dir);
    }

    /// Write a pending buffer out to the filesystem in full. `fs` is the
    /// already-locked inner FS (callers must hold it; lock order is always
    /// `inner` before `write_buf`, so this never deadlocks).
    fn flush_pending(fs: &mut Btrfs, pw: PendingWrite) -> Result<()> {
        let mut off = pw.start;
        let mut done = 0usize;
        while done < pw.data.len() {
            let n = fs.write(pw.ino, off, &pw.data[done..]).map_err(map_err)?;
            if n == 0 {
                return Err(FsError::DeviceError);
            }
            off += n as u64;
            done += n;
        }
        Ok(())
    }

    /// Flush the buffer iff it belongs to `ino`.
    fn flush_inode(&self, fs: &mut Btrfs, ino: u64) -> Result<()> {
        let taken = {
            let mut wb = self.write_buf.lock();
            match &*wb {
                Some(pw) if pw.ino == ino => wb.take(),
                _ => None,
            }
        };
        if let Some(pw) = taken {
            Self::flush_pending(fs, pw)?;
        }
        Ok(())
    }

    /// Flush any pending buffer regardless of inode.
    fn flush_any(&self, fs: &mut Btrfs) -> Result<()> {
        let taken = self.write_buf.lock().take();
        if let Some(pw) = taken {
            Self::flush_pending(fs, pw)?;
        }
        Ok(())
    }

    /// Size contributed by a buffered tail for `ino`, if any (so `stat` reflects
    /// not-yet-flushed writes without forcing a flush).
    fn buffered_end(&self, ino: u64) -> Option<u64> {
        let wb = self.write_buf.lock();
        match &*wb {
            Some(pw) if pw.ino == ino => Some(pw.end()),
            _ => None,
        }
    }
}

impl FileSystem for BtrfsMountFs {
    fn sync(&self) -> Result<()> {
        let mut fs = self.inner.lock();
        self.flush_any(&mut fs)?;
        fs.sync().map_err(map_err)
    }

    fn root_inode(&self) -> Arc<dyn INode> {
        let root = self.inner.lock().root_ino();
        self.inode(root)
    }

    fn info(&self) -> FsInfo {
        let stat = self.inner.lock().fsinfo();
        let bsize = stat.block_size.max(1);
        FsInfo {
            bsize: bsize as usize,
            frsize: bsize as usize,
            blocks: (stat.total_bytes / bsize) as usize,
            bfree: (stat.total_bytes.saturating_sub(stat.bytes_used) / bsize) as usize,
            bavail: (stat.total_bytes.saturating_sub(stat.bytes_used) / bsize) as usize,
            files: 0,
            ffree: 0,
            namemax: 255,
        }
    }
}

struct BtrfsMountINode {
    fs: Arc<BtrfsMountFs>,
    ino: u64,
    /// The inode's kind, resolved once on first use. A btrfs object never
    /// changes kind while it exists, and `read_at` runs once per 4 KiB on the
    /// demand-paging path — a full B-tree `stat` per call is pure overhead.
    kind: Mutex<Option<FileKind>>,
}

impl BtrfsMountINode {
    /// The inode's kind, from cache or via one `stat` on the locked fs.
    fn kind(&self, fs: &mut Btrfs) -> Result<FileKind> {
        let mut cached = self.kind.lock();
        if let Some(kind) = *cached {
            return Ok(kind);
        }
        let kind = fs.stat(self.ino).map_err(map_err)?.kind;
        *cached = Some(kind);
        Ok(kind)
    }
}

fn vfs_type(kind: FileKind) -> FileType {
    match kind {
        FileKind::Regular => FileType::File,
        FileKind::Dir => FileType::Dir,
        FileKind::Symlink => FileType::SymLink,
        FileKind::CharDevice => FileType::CharDevice,
        FileKind::BlockDevice => FileType::BlockDevice,
        FileKind::Fifo => FileType::NamedPipe,
        FileKind::Socket => FileType::Socket,
    }
}

fn btrfs_kind(type_: FileType) -> Result<FileKind> {
    Ok(match type_ {
        FileType::File => FileKind::Regular,
        FileType::Dir => FileKind::Dir,
        FileType::SymLink => FileKind::Symlink,
        FileType::CharDevice => FileKind::CharDevice,
        FileType::BlockDevice => FileKind::BlockDevice,
        FileType::NamedPipe => FileKind::Fifo,
        FileType::Socket => FileKind::Socket,
    })
}

fn stat_to_metadata(st: &btrfs::InodeStat) -> Metadata {
    Metadata {
        dev: 0,
        inode: st.ino as usize,
        size: st.size as usize,
        blk_size: 512,
        blocks: st.nbytes.div_ceil(512) as usize,
        atime: Timespec {
            sec: st.atime.0 as i64,
            nsec: st.atime.1 as i32,
        },
        mtime: Timespec {
            sec: st.mtime.0 as i64,
            nsec: st.mtime.1 as i32,
        },
        ctime: Timespec {
            sec: st.ctime.0 as i64,
            nsec: st.ctime.1 as i32,
        },
        type_: vfs_type(st.kind),
        mode: (st.mode & 0o7777) as u16,
        nlinks: st.nlink as usize,
        uid: st.uid as usize,
        gid: st.gid as usize,
        rdev: st.rdev as usize,
    }
}

impl INode for BtrfsMountINode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let mut fs = self.fs.inner.lock();
        // A read must observe everything written so far: flush this inode's
        // coalescing buffer before serving it.
        self.fs.flush_inode(&mut fs, self.ino)?;
        match self.kind(&mut fs)? {
            FileKind::Dir => Err(FsError::IsDir),
            FileKind::Symlink => {
                let target = fs.read_link(self.ino).map_err(map_err)?;
                Ok(read_from(&target, offset, buf))
            }
            _ => fs.read(self.ino, offset as u64, buf).map_err(|e| {
                // Surface the exact failing operation in dmesg (klog bypasses the
                // log-level filter), so an "I/O error" can be pinned to a btrfs
                // reason + offset instead of guessing.
                zcore_drivers::klog_err!(
                    "btrfs: read ino={} off={:#x} len={} -> {:?}",
                    self.ino,
                    offset,
                    buf.len(),
                    e,
                );
                map_err(e)
            }),
        }
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut fs = self.fs.inner.lock();
        let off = offset as u64;

        // Fast path: a contiguous append to the already-buffered file. The file
        // is known regular (only regular files are buffered, and unlink/rename
        // flush first), so we skip even the `stat` and just grow the buffer.
        {
            let mut wb = self.fs.write_buf.lock();
            let hit = matches!(&*wb, Some(pw) if appends_to(pw, self.ino, off));
            if hit {
                let pw = wb.as_mut().unwrap();
                pw.data.extend_from_slice(buf);
                if worth_buffering(pw.data.len()) {
                    return Ok(buf.len());
                }
                let full = wb.take().unwrap();
                drop(wb);
                BtrfsMountFs::flush_pending(&mut fs, full)?;
                return Ok(buf.len());
            }
        }

        // Slow path: new file / non-contiguous offset. Determine the kind and
        // flush any other inode's pending buffer first.
        let st = fs.stat(self.ino).map_err(map_err)?;
        match st.kind {
            FileKind::Dir => return Err(FsError::IsDir),
            FileKind::Symlink => {
                self.fs.flush_any(&mut fs)?;
                return fs.write_symlink(self.ino, off, buf).map_err(map_err);
            }
            _ => {}
        }
        self.fs.flush_any(&mut fs)?;
        // Small write: start a fresh buffer. Large write: straight through.
        if worth_buffering(buf.len()) {
            *self.fs.write_buf.lock() = Some(PendingWrite {
                ino: self.ino,
                start: off,
                data: buf.to_vec(),
            });
            return Ok(buf.len());
        }
        fs.write(self.ino, off, buf).map_err(|e| {
            zcore_drivers::klog_err!(
                "btrfs: write ino={} off={:#x} len={} -> {:?}",
                self.ino,
                offset,
                buf.len(),
                e,
            );
            map_err(e)
        })
    }

    fn poll(&self) -> Result<PollStatus> {
        let st = {
            let mut fs = self.fs.inner.lock();
            fs.stat(self.ino).map_err(map_err)?
        };
        Ok(PollStatus {
            read: true,
            write: st.kind != FileKind::Dir,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        let mut fs = self.fs.inner.lock();
        let mut st = fs.stat(self.ino).map_err(map_err)?;
        // Reflect not-yet-flushed buffered writes in the reported size without
        // forcing a flush (keeps buffering effective when callers `fstat`).
        if let Some(end) = self.fs.buffered_end(self.ino) {
            if end > st.size {
                st.size = end;
                st.nbytes = st.nbytes.max(end);
            }
        }
        Ok(stat_to_metadata(&st))
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<()> {
        let mut fs = self.fs.inner.lock();
        // Flush first: a later flush would otherwise overwrite mtime/ctime with
        // "now" and clobber the times being set here (apk sets archive mtimes
        // right after extracting a file).
        self.fs.flush_inode(&mut fs, self.ino)?;
        fs.set_attr(
            self.ino,
            Some(metadata.mode as u32),
            Some(metadata.uid as u32),
            Some(metadata.gid as u32),
            Some((metadata.atime.sec as u64, metadata.atime.nsec as u32)),
            Some((metadata.mtime.sec as u64, metadata.mtime.nsec as u32)),
        )
        .map_err(map_err)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." | "" => Ok(self.fs.inode(self.ino)),
            // A relative symlink (`/var/run -> ../run`) is resolved by
            // `lookup_follow` from its directory, so `..` must work on an
            // installed root: PulseAudio's `mkdir /var/run/pulse` got ENOENT
            // here and the daemon never started.
            ".." => {
                let parent = {
                    let mut fs = self.fs.inner.lock();
                    fs.parent(self.ino).map_err(map_err)?
                };
                Ok(self.fs.inode(parent))
            }
            name => {
                let ino = {
                    let mut fs = self.fs.inner.lock();
                    fs.lookup(self.ino, name).map_err(map_err)?
                };
                Ok(self.fs.inode(ino))
            }
        }
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        Ok(self.get_entry_with_metadata(id)?.1)
    }

    fn get_entry_with_metadata(&self, id: usize) -> Result<(Metadata, String)> {
        match id {
            0 => Ok((self.metadata()?, String::from("."))),
            1 => Ok((self.metadata()?, String::from(".."))),
            i => {
                let entries = self.fs.cached_readdir(self.ino)?;
                let entry = entries.get(i - 2).ok_or(FsError::EntryNotFound)?;
                let metadata = {
                    let mut fs = self.fs.inner.lock();
                    let st = fs.stat(entry.ino).map_err(map_err)?;
                    stat_to_metadata(&st)
                };
                Ok((metadata, entry.name.clone()))
            }
        }
    }

    fn create2(
        &self,
        name: &str,
        type_: FileType,
        mode: u32,
        data: usize,
    ) -> Result<Arc<dyn INode>> {
        let kind = btrfs_kind(type_)?;
        let ino = {
            let mut fs = self.fs.inner.lock();
            // Flush any buffered file before mutating the namespace, so its data
            // is durable before another file/operation depends on it.
            self.fs.flush_any(&mut fs)?;
            fs.create(self.ino, name, kind, mode, data as u64)
                .map_err(map_err)?
        };
        self.fs.invalidate_dir(self.ino);
        Ok(self.fs.inode(ino))
    }

    fn unlink(&self, name: &str) -> Result<()> {
        {
            let mut fs = self.fs.inner.lock();
            self.fs.flush_any(&mut fs)?;
            fs.unlink(self.ino, name).map_err(map_err)?;
        }
        self.fs.invalidate_dir(self.ino);
        Ok(())
    }

    fn link(&self, name: &str, other: &Arc<dyn INode>) -> Result<()> {
        let other = other
            .downcast_ref::<BtrfsMountINode>()
            .ok_or(FsError::NotSameFs)?;
        if !Arc::ptr_eq(&self.fs, &other.fs) {
            return Err(FsError::NotSameFs);
        }
        {
            let mut fs = self.fs.inner.lock();
            self.fs.flush_any(&mut fs)?;
            fs.link(self.ino, name, other.ino).map_err(map_err)?;
        }
        self.fs.invalidate_dir(self.ino);
        Ok(())
    }

    fn move_(&self, old_name: &str, target: &Arc<dyn INode>, new_name: &str) -> Result<()> {
        let target = target
            .downcast_ref::<BtrfsMountINode>()
            .ok_or(FsError::NotSameFs)?;
        if !Arc::ptr_eq(&self.fs, &target.fs) {
            return Err(FsError::NotSameFs);
        }
        {
            let mut fs = self.fs.inner.lock();
            self.fs.flush_any(&mut fs)?;
            fs.rename(self.ino, old_name, target.ino, new_name)
                .map_err(map_err)?;
        }
        self.fs.invalidate_dir(self.ino);
        self.fs.invalidate_dir(target.ino);
        Ok(())
    }

    fn resize(&self, len: usize) -> Result<()> {
        let mut fs = self.fs.inner.lock();
        // Pending writes must land before the truncate so the final size/extents
        // are correct.
        self.fs.flush_inode(&mut fs, self.ino)?;
        fs.truncate(self.ino, len as u64).map_err(map_err)
    }

    fn sync_all(&self) -> Result<()> {
        let mut fs = self.fs.inner.lock();
        self.fs.flush_any(&mut fs)?;
        fs.sync().map_err(map_err)
    }

    fn sync_data(&self) -> Result<()> {
        self.sync_all()
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.fs.clone()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// Open a mount backend as a btrfs filesystem.
pub fn open_btrfs(backend: &MountBackend, read_only: bool) -> Result<Arc<dyn FileSystem>> {
    BtrfsMountFs::open(backend, read_only).map(|fs| fs as Arc<dyn FileSystem>)
}

/// Cheap pre-mount probe: does the backing device look like btrfs?
pub(crate) fn probe_btrfs_superblock(block: &Arc<dyn BlockScheme>) -> bool {
    // Primary superblock lives at byte 0x10000; magic at +0x40.
    const SB_SECTOR: usize = 0x10000 / 512;
    #[repr(align(4096))]
    struct SectorBuf([u8; 512]);
    let mut sb = SectorBuf([0u8; 512]);
    if block.read_block(SB_SECTOR, &mut sb.0).is_err() {
        return false;
    }
    let magic = u64::from_le_bytes(sb.0[0x40..0x48].try_into().unwrap());
    if magic != 0x4D5F53665248425F {
        return false;
    }
    let num_devices = u64::from_le_bytes(sb.0[0x88..0x90].try_into().unwrap());
    let total_bytes = u64::from_le_bytes(sb.0[0x70..0x78].try_into().unwrap());
    let device_bytes = block.block_count() as u64 * 512;
    num_devices == 1 && total_bytes > 0 && total_bytes <= device_bytes.saturating_mul(2)
}

#[cfg(test)]
mod dev_adapter_tests {
    //! Host tests for `DevAdapter`, the layer that turns one btrfs transfer
    //! into the block commands the disk driver actually receives: chunking,
    //! retry, the shrink-on-failure fallback and the bounds of the device.
    //! btrfs itself is exonerated by the `btrfs` crate's own suites and the
    //! driver by its own; this is the glue in between, and on real hardware it
    //! is the only piece that sees a transient controller error.
    use super::*;
    use alloc::vec;
    use btrfs::BlockDevice as _;
    use rcore_fs::dev::{DevError, Device, Result as DevResult};

    /// In-memory disk that records every transfer the adapter asks for and can
    /// be told to fail: `hiccups` many transfers regardless of size (a
    /// transient controller error), or every transfer at or above `too_big` (a
    /// controller that rejects large DMA requests).
    struct FakeDisk {
        data: Mutex<Vec<u8>>,
        /// `(offset, len, served)` of every transfer attempted, in order.
        log: Mutex<Vec<(usize, usize, bool)>>,
        hiccups: AtomicUsize,
        too_big: AtomicUsize,
    }

    impl FakeDisk {
        fn new(len: usize) -> Arc<Self> {
            let mut data = vec![0u8; len];
            // A recognisable pattern, so a transfer landing at the wrong offset
            // or copied into the wrong part of the buffer shows up as wrong
            // bytes and not as a coincidence of zeros.
            for (i, b) in data.iter_mut().enumerate() {
                *b = (i % 251) as u8;
            }
            Arc::new(Self {
                data: Mutex::new(data),
                log: Mutex::new(Vec::new()),
                hiccups: AtomicUsize::new(0),
                too_big: AtomicUsize::new(usize::MAX),
            })
        }
        /// Fail the next `n` transfers whatever their size.
        fn hiccup(&self, n: usize) {
            self.hiccups.store(n, Ordering::Relaxed);
        }
        /// Fail every transfer of `n` bytes or more.
        fn reject_from(&self, n: usize) {
            self.too_big.store(n, Ordering::Relaxed);
        }
        fn heal(&self) {
            self.hiccups.store(0, Ordering::Relaxed);
            self.too_big.store(usize::MAX, Ordering::Relaxed);
        }
        fn forget(&self) {
            self.log.lock().clear();
        }
        /// `(offset, len)` of every transfer attempted, served or not.
        fn log(&self) -> Vec<(usize, usize)> {
            self.log.lock().iter().map(|&(o, n, _)| (o, n)).collect()
        }
        /// `(offset, len)` of the transfers the device actually served.
        fn served(&self) -> Vec<(usize, usize)> {
            self.log
                .lock()
                .iter()
                .filter(|&&(_, _, ok)| ok)
                .map(|&(o, n, _)| (o, n))
                .collect()
        }
        /// The size of each transfer attempted, in order.
        fn sizes(&self) -> Vec<usize> {
            self.log().into_iter().map(|(_, n)| n).collect()
        }
        /// Record the transfer and say whether the device serves it.
        fn accepts(&self, offset: usize, len: usize) -> bool {
            let ok = len < self.too_big.load(Ordering::Relaxed)
                && match self.hiccups.load(Ordering::Relaxed) {
                    0 => true,
                    n => {
                        self.hiccups.store(n - 1, Ordering::Relaxed);
                        false
                    }
                };
            self.log.lock().push((offset, len, ok));
            ok
        }
    }

    impl Device for FakeDisk {
        fn read_at(&self, offset: usize, buf: &mut [u8]) -> DevResult<usize> {
            if !self.accepts(offset, buf.len()) {
                return Err(DevError);
            }
            let d = self.data.lock();
            let end = offset.checked_add(buf.len()).ok_or(DevError)?;
            if end > d.len() {
                return Err(DevError);
            }
            buf.copy_from_slice(&d[offset..end]);
            Ok(buf.len())
        }
        fn write_at(&self, offset: usize, buf: &[u8]) -> DevResult<usize> {
            if !self.accepts(offset, buf.len()) {
                return Err(DevError);
            }
            let mut d = self.data.lock();
            let end = offset.checked_add(buf.len()).ok_or(DevError)?;
            if end > d.len() {
                return Err(DevError);
            }
            d[offset..end].copy_from_slice(buf);
            Ok(buf.len())
        }
        fn sync(&self) -> DevResult<()> {
            Ok(())
        }
    }

    /// A 1 MiB disk behind a fresh adapter.
    fn disk_and_adapter() -> (Arc<FakeDisk>, DevAdapter) {
        const LEN: usize = 1024 * 1024;
        let disk = FakeDisk::new(LEN);
        let adapter = DevAdapter {
            inner: disk.clone(),
            size: LEN as u64,
            max_xfer: AtomicUsize::new(IO_CHUNK_BYTES),
        };
        (disk, adapter)
    }

    /// What a healthy disk holds at `[offset, offset + len)`.
    fn expected(disk: &FakeDisk, offset: usize, len: usize) -> Vec<u8> {
        disk.data.lock()[offset..offset + len].to_vec()
    }

    #[test]
    /// A read longer than the cap is split into cap-sized commands that tile
    /// the range exactly, and what lands in the buffer is what is on the disk.
    fn a_long_read_is_split_into_commands_that_tile_the_range() {
        let (disk, adapter) = disk_and_adapter();
        let len = IO_CHUNK_BYTES * 2 + 4096;
        let mut buf = vec![0u8; len];
        adapter.read_at(8192, &mut buf).unwrap();
        assert_eq!(buf, expected(&disk, 8192, len));
        assert_eq!(
            disk.sizes(),
            vec![IO_CHUNK_BYTES, IO_CHUNK_BYTES, 4096],
            "the last piece is the remainder, not another whole chunk",
        );
        // Contiguous, starting where btrfs asked and nowhere else.
        let mut want = 8192;
        for (off, n) in disk.log() {
            assert_eq!(off, want);
            want += n;
        }
        assert_eq!(want, 8192 + len);
    }

    #[test]
    /// The same for the write direction, on the disk's own bytes.
    fn a_long_write_lands_where_it_was_asked_to() {
        let (disk, adapter) = disk_and_adapter();
        let len = IO_CHUNK_BYTES + 777;
        let payload: Vec<u8> = (0..len).map(|i| (i % 97) as u8 ^ 0x5a).collect();
        adapter.write_at(4096, &payload).unwrap();
        assert_eq!(expected(&disk, 4096, len), payload);
        assert_eq!(disk.sizes(), vec![IO_CHUNK_BYTES, 777]);
        // The byte before and the byte after are untouched.
        assert_eq!(disk.data.lock()[4095], (4095 % 251) as u8);
        assert_eq!(disk.data.lock()[4096 + len], ((4096 + len) % 251) as u8,);
    }

    #[test]
    /// A transient error is retried at the same offset and length, and the
    /// caller never sees it.
    fn a_transient_error_is_retried_in_place() {
        let (disk, adapter) = disk_and_adapter();
        disk.hiccup(3);
        let mut buf = vec![0u8; 4096];
        adapter.read_at(65536, &mut buf).unwrap();
        assert_eq!(buf, expected(&disk, 65536, 4096));
        assert_eq!(
            disk.log(),
            vec![(65536, 4096); 4],
            "three failures then the same transfer once more",
        );
    }

    #[test]
    /// A controller that rejects large requests is discovered once and the
    /// smaller size is remembered for the rest of the mount.
    fn a_size_the_controller_refuses_is_learnt_once() {
        let (disk, adapter) = disk_and_adapter();
        disk.reject_from(64 * 1024);
        let len = IO_CHUNK_BYTES * 2;
        let mut buf = vec![0u8; len];
        adapter.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, expected(&disk, 0, len));
        let sizes = disk.sizes();
        // 128 KiB refused `IO_RETRIES` times, then 64 KiB the same, then 32 KiB
        // works.
        let mut ladder = vec![IO_CHUNK_BYTES; IO_RETRIES];
        ladder.extend(vec![65536; IO_RETRIES]);
        ladder.push(32768);
        assert_eq!(&sizes[..ladder.len()], &ladder[..]);
        // ... and every command after that is 32 KiB: the big one is not tried
        // again on the next chunk, which is the whole point of the cap.
        assert!(
            sizes[ladder.len()..].iter().all(|&n| n == 32768),
            "cap not remembered: {:?}",
            &sizes[ladder.len()..],
        );
        assert_eq!(adapter.max_xfer.load(Ordering::Relaxed), 32768);
    }

    #[test]
    /// A short transfer failing says nothing about how large a request the
    /// device sustains, so it must not lower the cap: the cap only ever
    /// ratchets down, and one transient error on a file's 4 KiB tail used to
    /// pin every later transfer on the whole mount near `IO_MIN_BYTES`.
    fn a_short_transfer_failing_does_not_shrink_the_whole_mount() {
        let (disk, adapter) = disk_and_adapter();
        // Five failures: enough to exhaust the retries at 4 KiB and force one
        // shrink, which is exactly the moment the old cap was written.
        disk.hiccup(IO_RETRIES);
        let mut tail = vec![0u8; 4096];
        adapter.read_at(512 * 1024, &mut tail).unwrap();
        assert_eq!(tail, expected(&disk, 512 * 1024, 4096));
        assert_eq!(
            adapter.max_xfer.load(Ordering::Relaxed),
            IO_CHUNK_BYTES,
            "a 4 KiB failure is not evidence about a 128 KiB request",
        );

        disk.forget();
        let mut big = vec![0u8; IO_CHUNK_BYTES];
        adapter.read_at(0, &mut big).unwrap();
        assert_eq!(disk.sizes(), vec![IO_CHUNK_BYTES], "one command, not 256");
    }

    #[test]
    /// A transfer that fails at every size down to `IO_MIN_BYTES` is a bad
    /// range or a dead device, not a size the controller dislikes, so it must
    /// leave the cap alone too. Otherwise one unreadable spot makes the rest of
    /// the filesystem run 512 bytes at a time.
    fn failing_at_every_size_is_not_a_size_problem() {
        let (disk, adapter) = disk_and_adapter();
        disk.reject_from(1);
        let mut buf = vec![0u8; IO_CHUNK_BYTES];
        assert_eq!(adapter.read_at(0, &mut buf), Err(BtrfsError::Io));
        assert_eq!(adapter.max_xfer.load(Ordering::Relaxed), IO_CHUNK_BYTES);

        disk.heal();
        disk.forget();
        adapter.read_at(0, &mut buf).unwrap();
        assert_eq!(disk.sizes(), vec![IO_CHUNK_BYTES]);
    }

    #[test]
    /// The ladder halves, but it stops at `IO_MIN_BYTES` instead of stepping
    /// under it: 1000 bytes is the awkward case, because half of it is below
    /// the floor.
    fn the_ladder_stops_at_the_smallest_transfer_it_is_allowed() {
        let (disk, adapter) = disk_and_adapter();
        disk.reject_from(600);
        let mut buf = vec![0u8; 1000];
        adapter.read_at(2048, &mut buf).unwrap();
        assert_eq!(buf, expected(&disk, 2048, 1000));
        assert_eq!(
            disk.log(),
            vec![
                (2048, 1000),
                (2048, 1000),
                (2048, 1000),
                (2048, 1000),
                (2048, 1000),
                (2048, IO_MIN_BYTES),
                (2048 + IO_MIN_BYTES, 1000 - IO_MIN_BYTES),
            ],
        );
    }

    #[test]
    /// However the ladder ends up splitting a range, no command ever leaves it
    /// and the pieces tile it exactly once.
    fn no_command_ever_leaves_the_range_it_was_given() {
        let (disk, adapter) = disk_and_adapter();
        disk.reject_from(4096);
        let len = IO_CHUNK_BYTES + 1000;
        let mut buf = vec![0u8; len];
        adapter.read_at(1024, &mut buf).unwrap();
        assert_eq!(buf, expected(&disk, 1024, len));
        for (off, n) in disk.log() {
            assert!(off >= 1024 && off + n <= 1024 + len, "{:#x}+{}", off, n);
        }
        // The transfers that were served tile the range exactly once, in order.
        let mut want = 1024;
        for (off, n) in disk.served() {
            assert_eq!(off, want, "a gap or an overlap at {:#x}", want);
            want += n;
        }
        assert_eq!(want, 1024 + len);
    }

    #[test]
    /// A read past the end of the device is refused before the driver is
    /// touched. It used to walk the whole retry-and-shrink ladder first --
    /// forty-five commands that could not possibly succeed -- and lower the cap
    /// on the way.
    fn a_read_past_the_end_never_reaches_the_driver() {
        let (disk, adapter) = disk_and_adapter();
        let mut buf = vec![0u8; 8192];
        assert_eq!(
            adapter.read_at(1024 * 1024 - 4096, &mut buf),
            Err(BtrfsError::Io)
        );
        assert!(disk.log().is_empty(), "{:?}", disk.log());
        assert_eq!(adapter.max_xfer.load(Ordering::Relaxed), IO_CHUNK_BYTES);
        // The very last byte is still readable: the bound is the end, not a
        // guard band in front of it.
        let mut last = [0u8; 1];
        adapter.read_at(1024 * 1024 - 1, &mut last).unwrap();
        assert_eq!(last[0], ((1024 * 1024 - 1) % 251) as u8);
    }

    #[test]
    /// A write past the end changes nothing, on either side of the boundary.
    /// It used to warn and then write the part that fitted.
    fn a_write_past_the_end_writes_nothing_at_all() {
        let (disk, adapter) = disk_and_adapter();
        let before = disk.data.lock().clone();
        assert_eq!(
            adapter.write_at(1024 * 1024 - 4, &[0xaa; 8]),
            Err(BtrfsError::Io)
        );
        assert!(disk.log().is_empty());
        assert_eq!(*disk.data.lock(), before);
        // A write that ends exactly at the end is fine.
        adapter.write_at(1024 * 1024 - 8, &[0xaa; 8]).unwrap();
        assert_eq!(&disk.data.lock()[1024 * 1024 - 8..], &[0xaa; 8]);
    }

    #[test]
    /// An offset whose range wraps round is refused rather than folded back
    /// into the middle of the device. In release the sum does not trap, so the
    /// unchecked version turned a corrupt on-disk pointer into a write over
    /// live metadata.
    fn a_range_that_wraps_does_not_land_back_on_the_disk() {
        let (disk, adapter) = disk_and_adapter();
        let before = disk.data.lock().clone();
        let mut buf = vec![0u8; 8];
        assert_eq!(adapter.read_at(u64::MAX - 3, &mut buf), Err(BtrfsError::Io));
        assert_eq!(
            adapter.write_at(u64::MAX - 3, &[0xaa; 8]),
            Err(BtrfsError::Io)
        );
        assert_eq!(adapter.write_at(u64::MAX, &[0xaa; 1]), Err(BtrfsError::Io));
        assert!(disk.log().is_empty(), "{:?}", disk.log());
        assert_eq!(*disk.data.lock(), before);
    }

    #[test]
    /// An empty transfer is not an error, and does not become a command.
    fn an_empty_transfer_asks_the_device_for_nothing() {
        let (disk, adapter) = disk_and_adapter();
        adapter.read_at(4096, &mut []).unwrap();
        adapter.write_at(4096, &[]).unwrap();
        // Even right at the end, where `offset + 0` is the first byte the
        // device does not have.
        adapter.read_at(1024 * 1024, &mut []).unwrap();
        assert!(disk.log().is_empty());
        // But one byte past it is still out of bounds.
        let mut one = [0u8; 1];
        assert_eq!(adapter.read_at(1024 * 1024, &mut one), Err(BtrfsError::Io));
    }

    #[test]
    /// The adapter reports the size it was mounted with, which is what btrfs
    /// uses to place its superblock mirrors and to grow onto the partition.
    fn the_adapter_reports_the_size_it_was_given() {
        let (_disk, adapter) = disk_and_adapter();
        assert_eq!(adapter.size(), 1024 * 1024);
        adapter.sync().unwrap();
    }
}

#[cfg(test)]
mod mount_glue_tests {
    //! Host tests for everything this mount decides *without* a filesystem
    //! under it: the two translation tables, the `stat` conversion and the
    //! pre-mount probe. `dev_adapter_tests` above covers the transfer layer;
    //! between them they are the whole of this file a host can reach, and
    //! until now every one of these decisions could be changed without a
    //! single test noticing.
    use super::*;
    use alloc::vec;
    use zcore_drivers::scheme::Scheme;
    use zcore_drivers::{DeviceError, DeviceResult};

    /// btrfs' own magic, `_BHRfS_M` read little-endian, as it sits at +0x40 of
    /// the superblock.
    const BTRFS_MAGIC: u64 = 0x4D5F53665248425F;
    /// Byte offset of the primary superblock.
    const SB_AT: usize = 0x10000;
    /// Sectors of the disks these tests build: 128 KiB, enough to hold the
    /// primary superblock and a little beyond it.
    const SECTORS: usize = 256;

    /// A block device whose bytes the test writes by hand, so a superblock can
    /// be built one field at a time.
    struct Platter {
        bytes: Mutex<Vec<u8>>,
        readable: bool,
    }

    impl Platter {
        fn of(sectors: usize) -> Arc<Self> {
            Arc::new(Self {
                bytes: Mutex::new(vec![0u8; sectors * 512]),
                readable: true,
            })
        }

        fn unreadable() -> Arc<Self> {
            Arc::new(Self {
                bytes: Mutex::new(vec![0u8; 512]),
                readable: false,
            })
        }

        /// Write a little-endian `u64` at an absolute byte offset.
        fn put(&self, at: usize, v: u64) {
            self.bytes.lock()[at..at + 8].copy_from_slice(&v.to_le_bytes());
        }
    }

    impl Scheme for Platter {
        fn name(&self) -> &str {
            "platter"
        }
    }

    impl BlockScheme for Platter {
        fn read_block(&self, block_id: usize, buf: &mut [u8]) -> DeviceResult {
            if !self.readable {
                return Err(DeviceError::IoError);
            }
            let start = block_id * 512;
            let d = self.bytes.lock();
            if start + buf.len() > d.len() {
                return Err(DeviceError::InvalidParam);
            }
            buf.copy_from_slice(&d[start..start + buf.len()]);
            Ok(())
        }
        fn write_block(&self, _block_id: usize, _buf: &[u8]) -> DeviceResult {
            Err(DeviceError::NotSupported)
        }
        fn flush(&self) -> DeviceResult {
            Ok(())
        }
        fn block_count(&self) -> usize {
            self.bytes.lock().len() / 512
        }
    }

    /// A disk carrying a superblock that passes every test of the probe, so
    /// each test can spoil exactly one field of it.
    fn a_btrfs_disk() -> Arc<Platter> {
        let p = Platter::of(SECTORS);
        p.put(SB_AT + 0x40, BTRFS_MAGIC);
        p.put(SB_AT + 0x70, (SECTORS as u64) * 512);
        p.put(SB_AT + 0x88, 1);
        p
    }

    fn probe(p: &Arc<Platter>) -> bool {
        let dev: Arc<dyn BlockScheme> = p.clone();
        probe_btrfs_superblock(&dev)
    }

    #[test]
    fn an_untouched_superblock_is_recognised() {
        assert!(probe(&a_btrfs_disk()), "a superblock with nothing wrong");
    }

    /// The primary superblock lives at byte 0x10000, which on a 512-byte
    /// device is sector 128. Looking for it anywhere else finds zeros and
    /// refuses to mount a perfectly good volume.
    #[test]
    fn the_superblock_is_read_from_byte_64_kib() {
        let elsewhere = Platter::of(SECTORS);
        elsewhere.put(0x2000 + 0x40, BTRFS_MAGIC);
        elsewhere.put(0x2000 + 0x70, (SECTORS as u64) * 512);
        elsewhere.put(0x2000 + 0x88, 1);
        assert!(
            !probe(&elsewhere),
            "a superblock at the wrong offset is not a superblock"
        );
        assert!(probe(&a_btrfs_disk()), "and the right offset still is");
    }

    /// The magic is at +0x40 and nowhere else, and it is a veto: a volume of
    /// some other filesystem whose remaining numbers happen to look plausible
    /// is still not btrfs.
    #[test]
    fn without_the_magic_at_its_own_offset_nothing_else_matters() {
        let blank = a_btrfs_disk();
        blank.put(SB_AT + 0x40, 0);
        assert!(!probe(&blank), "plausible numbers but no magic");

        let shifted = a_btrfs_disk();
        shifted.put(SB_AT + 0x40, 0);
        shifted.put(SB_AT + 0x48, BTRFS_MAGIC);
        assert!(
            !probe(&shifted),
            "the magic one field further on is somebody else's field"
        );
    }

    /// A volume spread over several disks is one this mount cannot read: half
    /// its chunks live on a device nobody handed us.
    #[test]
    fn a_volume_spread_over_several_disks_is_refused() {
        let p = a_btrfs_disk();
        p.put(SB_AT + 0x88, 2);
        assert!(!probe(&p));
    }

    /// A superblock claiming no bytes at all is a zeroed or half-written one.
    #[test]
    fn a_superblock_that_claims_no_bytes_is_refused() {
        let p = a_btrfs_disk();
        p.put(SB_AT + 0x70, 0);
        assert!(!probe(&p));
    }

    /// The volume may be at most twice the disk it was found on -- slack for a
    /// device whose size we count differently than btrfs did. Beyond that the
    /// superblock describes some other, bigger disk.
    #[test]
    fn a_volume_far_bigger_than_its_disk_is_refused() {
        let disk = SECTORS as u64 * 512;
        let edge = a_btrfs_disk();
        edge.put(SB_AT + 0x70, disk * 2);
        assert!(probe(&edge), "exactly twice the disk is still allowed");

        let over = a_btrfs_disk();
        over.put(SB_AT + 0x70, disk * 2 + 1);
        assert!(!probe(&over), "one byte past the allowance is not");
    }

    /// A device that cannot even be read is not btrfs. Saying yes here sends
    /// the mount on to open a filesystem it has no way of reading.
    #[test]
    fn a_disk_that_cannot_be_read_is_not_btrfs() {
        let dev: Arc<dyn BlockScheme> = Platter::unreadable();
        assert!(!probe_btrfs_superblock(&dev));
    }

    /// Every btrfs error reaches userspace as a specific errno, and the ones
    /// easiest to confuse are the ones that matter: `NoSpace` is ENOSPC and
    /// not EINVAL (a package manager retries one and gives up on the other),
    /// `BadSuperblock` is "wrong filesystem" so `mount` goes on to try the
    /// next type instead of reporting a broken disk, and `NotEmpty` is
    /// ENOTEMPTY, which is what `rmdir` tells apart from ENOTDIR.
    #[test]
    fn each_btrfs_error_keeps_its_own_errno() {
        let pairs = [
            (BtrfsError::Io, FsError::DeviceError),
            (BtrfsError::BadSuperblock, FsError::WrongFs),
            (BtrfsError::Corrupt("t"), FsError::DeviceError),
            (BtrfsError::Unsupported("t"), FsError::NotSupported),
            (BtrfsError::NotFound, FsError::EntryNotFound),
            (BtrfsError::Exists, FsError::EntryExist),
            (BtrfsError::NotDir, FsError::NotDir),
            (BtrfsError::IsDir, FsError::IsDir),
            (BtrfsError::NotEmpty, FsError::DirNotEmpty),
            (BtrfsError::NoSpace, FsError::NoDeviceSpace),
            (BtrfsError::Invalid, FsError::InvalidParam),
        ];
        for (from, want) in pairs {
            assert_eq!(map_err(from), want, "{:?}", from);
        }
    }

    /// What `stat` answers for a file's type. Getting one wrong is not
    /// cosmetic: a FIFO reported as a socket sends `open` into the wrong
    /// layer, and a symlink reported as a regular file makes every path that
    /// crosses it resolve to the link itself.
    #[test]
    fn each_btrfs_kind_has_its_own_vfs_type() {
        let pairs = [
            (FileKind::Regular, FileType::File),
            (FileKind::Dir, FileType::Dir),
            (FileKind::Symlink, FileType::SymLink),
            (FileKind::CharDevice, FileType::CharDevice),
            (FileKind::BlockDevice, FileType::BlockDevice),
            (FileKind::Fifo, FileType::NamedPipe),
            (FileKind::Socket, FileType::Socket),
        ];
        for (kind, want) in pairs {
            assert_eq!(vfs_type(kind), want, "{:?}", kind);
        }
    }

    /// And the way back, which is what `mknod` writes into the inode: a node
    /// created as the wrong kind is wrong on disk, not just in one answer.
    #[test]
    fn each_vfs_type_has_its_own_btrfs_kind() {
        let pairs = [
            (FileType::File, FileKind::Regular),
            (FileType::Dir, FileKind::Dir),
            (FileType::SymLink, FileKind::Symlink),
            (FileType::CharDevice, FileKind::CharDevice),
            (FileType::BlockDevice, FileKind::BlockDevice),
            (FileType::NamedPipe, FileKind::Fifo),
            (FileType::Socket, FileKind::Socket),
        ];
        for (type_, want) in pairs {
            assert_eq!(btrfs_kind(type_).unwrap(), want, "{:?}", type_);
        }
    }

    fn a_stat() -> btrfs::InodeStat {
        btrfs::InodeStat {
            ino: 257,
            kind: FileKind::Regular,
            mode: 0o100_644,
            nlink: 3,
            uid: 1000,
            gid: 100,
            size: 5000,
            nbytes: 8192,
            rdev: 0,
            atime: (11, 111),
            mtime: (22, 222),
            ctime: (33, 333),
        }
    }

    /// `st_blocks` is counted in 512-byte units and rounds *up*: a one-byte
    /// file still occupies a block, and `du` adds these up.
    #[test]
    fn the_block_count_rounds_up_in_512_byte_units() {
        let mut st = a_stat();
        st.nbytes = 8192;
        let m = stat_to_metadata(&st);
        assert_eq!(m.blk_size, 512, "the unit blocks are counted in");
        assert_eq!(m.blocks, 16);

        st.nbytes = 1;
        assert_eq!(stat_to_metadata(&st).blocks, 1, "one byte is one block");

        st.nbytes = 513;
        assert_eq!(stat_to_metadata(&st).blocks, 2, "a part block is a block");
    }

    /// `st_size` is how long the file is; `nbytes` is what it costs on disk.
    /// A sparse or compressed file makes the two differ, and answering the
    /// second cuts short every read that trusts `stat`.
    #[test]
    fn the_size_is_the_length_and_not_what_it_occupies() {
        let mut st = a_stat();
        st.size = 5000;
        st.nbytes = 8192;
        assert_eq!(stat_to_metadata(&st).size, 5000);
    }

    /// The top bits of `st_mode` are the file type, which `Metadata` carries
    /// separately in `type_`. Letting them through reports a directory as mode
    /// 40755, and every tool that prints or compares permissions says so.
    #[test]
    fn the_file_type_bits_never_leak_into_the_mode() {
        let mut st = a_stat();
        st.kind = FileKind::Dir;
        st.mode = 0o40_755;
        let m = stat_to_metadata(&st);
        assert_eq!(m.mode, 0o755, "permissions only");
        assert_eq!(m.type_, FileType::Dir, "the type is carried apart");
    }

    /// Each of the three timestamps keeps its own seconds *and* its own
    /// nanoseconds. Copying a field from its neighbour is invisible on a whole
    /// second and shows up as a file that `make` thinks is older than itself.
    #[test]
    fn every_timestamp_keeps_its_own_seconds_and_nanoseconds() {
        let m = stat_to_metadata(&a_stat());
        assert_eq!((m.atime.sec, m.atime.nsec), (11, 111));
        assert_eq!((m.mtime.sec, m.mtime.nsec), (22, 222));
        assert_eq!((m.ctime.sec, m.ctime.nsec), (33, 333));
        assert_eq!(m.inode, 257);
        assert_eq!((m.nlinks, m.uid, m.gid), (3, 1000, 100));
    }

    fn a_tail(ino: u64, start: u64, len: usize) -> PendingWrite {
        PendingWrite {
            ino,
            start,
            data: vec![0xEE; len],
        }
    }

    /// Where the buffered tail ends is one number with two readers: `stat`
    /// reports it as the file's size while the tail is still in memory, and
    /// `write_at` compares it against the incoming offset to decide whether a
    /// write continues it. Dropping the start of the buffer makes the first
    /// answer a file that shrank and the second accept a write that belongs
    /// somewhere else.
    #[test]
    fn the_buffered_tail_ends_where_it_started_plus_what_it_holds() {
        assert_eq!(a_tail(257, 4096, 300).end(), 4396);
        assert_eq!(
            a_tail(257, 0, 300).end(),
            300,
            "a tail that starts at the start of the file"
        );
    }

    /// The fast path skips the `stat` that would say anything at all about the
    /// file, so it may only be taken for a write that continues the buffered
    /// tail of the *same* inode, exactly where it left off. Accepting any
    /// offset for that inode splices the bytes into the buffer at the wrong
    /// place; accepting another inode's write puts one file's bytes into
    /// another file.
    #[test]
    fn only_a_contiguous_append_of_the_same_inode_takes_the_fast_path() {
        let pw = a_tail(257, 4096, 300);
        assert!(appends_to(&pw, 257, 4396), "right where it left off");
        assert!(!appends_to(&pw, 257, 4096), "back at the start of the tail");
        assert!(!appends_to(&pw, 257, 4395), "one byte back is an overwrite");
        assert!(!appends_to(&pw, 257, 4397), "one byte on leaves a hole");
        assert!(!appends_to(&pw, 258, 4396), "another file entirely");
    }

    /// The buffer exists to turn the ~32000 tiny writes of a big package
    /// extraction into a few hundred large ones, so the threshold is a
    /// megabyte, and the boundary belongs to the filesystem: a write *of* the
    /// flush size is already large enough to go straight through.
    #[test]
    fn the_flush_size_is_the_first_length_not_worth_buffering() {
        assert!(worth_buffering(0));
        assert!(worth_buffering(WRITE_BUF_FLUSH - 1));
        assert!(!worth_buffering(WRITE_BUF_FLUSH));
        assert!(!worth_buffering(WRITE_BUF_FLUSH + 1));
        assert!(
            WRITE_BUF_FLUSH >= 1024 * 1024,
            "a buffer under a megabyte gives the coalescing back"
        );
    }

    /// Reading a symlink pages like any other read: `read(2)` is what advances
    /// the file position, so a second call asks for the rest. Ignoring what is
    /// left fills whatever the caller's buffer can hold and reports having
    /// read it, which for an 18-byte target and a 4 KiB buffer is 4 KiB of
    /// somebody else's memory handed back as the link.
    #[test]
    fn reading_a_run_of_bytes_never_goes_past_its_end() {
        let target = b"/usr/lib/libc.so.6";
        let mut buf = [0u8; 64];
        assert_eq!(read_from(target, 0, &mut buf), target.len());
        assert_eq!(&buf[..target.len()], target);
        assert!(
            buf[target.len()..].iter().all(|&b| b == 0),
            "and writes nothing past what it read"
        );

        let mut small = [0u8; 4];
        assert_eq!(read_from(target, 0, &mut small), 4);
        assert_eq!(&small, b"/usr");
        assert_eq!(read_from(target, 4, &mut small), 4, "and on from there");
        assert_eq!(&small, b"/lib");

        let mut tail = [0u8; 64];
        assert_eq!(read_from(target, 15, &mut tail), 3, "only what is left");
        assert_eq!(&tail[..3], b"o.6");
        assert_eq!(read_from(target, target.len(), &mut tail), 0, "at the end");
        assert_eq!(
            read_from(target, target.len() + 99, &mut tail),
            0,
            "past it"
        );
    }
}
