//! btrfs mount support via the in-tree `btrfs` crate.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::convert::TryInto;

use btrfs::{Btrfs, Error as BtrfsError, FileKind};
use lock::Mutex;
use rcore_fs::vfs::{
    FileSystem, FileType, FsError, FsInfo, INode, Metadata, PollStatus, Result, Timespec,
};
use zcore_drivers::scheme::BlockScheme;

use super::block_mount::{device_from_backend, MountBackend};

/// Adapter: rcore-fs `Device` (+ explicit size) → `btrfs::BlockDevice`.
struct DevAdapter {
    inner: Arc<dyn rcore_fs::dev::Device>,
    size: u64,
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

impl btrfs::BlockDevice for DevAdapter {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> btrfs::Result<()> {
        for attempt in 0..IO_RETRIES {
            match self.inner.read_at(offset as usize, buf) {
                Ok(n) if n == buf.len() => return Ok(()),
                other => {
                    if attempt + 1 == IO_RETRIES {
                        warn!(
                            "btrfs: read_at({:#x}, {}) failed after {} attempts: {:?}",
                            offset,
                            buf.len(),
                            IO_RETRIES,
                            other.err()
                        );
                    }
                }
            }
        }
        Err(BtrfsError::Io)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> btrfs::Result<()> {
        for attempt in 0..IO_RETRIES {
            match self.inner.write_at(offset as usize, buf) {
                Ok(n) if n == buf.len() => return Ok(()),
                other => {
                    if attempt + 1 == IO_RETRIES {
                        warn!(
                            "btrfs: write_at({:#x}, {}) failed after {} attempts: {:?}",
                            offset,
                            buf.len(),
                            IO_RETRIES,
                            other.err()
                        );
                    }
                }
            }
        }
        Err(BtrfsError::Io)
    }

    fn sync(&self) -> btrfs::Result<()> {
        self.inner.sync().map_err(|_| BtrfsError::Io)
    }

    fn size(&self) -> u64 {
        self.size
    }
}

fn backend_size(backend: &MountBackend) -> Result<u64> {
    match backend {
        MountBackend::Block(block) => Ok(block.block_count() as u64 * 512),
        MountBackend::File(file) => Ok(file.metadata()?.size as u64),
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
}

#[derive(Clone)]
struct CachedDirEntry {
    name: String,
    metadata: Metadata,
}

impl BtrfsMountFs {
    pub fn open(backend: &MountBackend, read_only: bool) -> Result<Arc<Self>> {
        let size = backend_size(backend)?;
        let device = device_from_backend(backend)?;
        let adapter: Arc<dyn btrfs::BlockDevice> = Arc::new(DevAdapter {
            inner: device,
            size,
        });
        let mut fs = Btrfs::mount(adapter, read_only).map_err(map_err)?;
        fs.set_clock(wall_clock);
        // Auto-expand to the partition size (the installer writes a small
        // image onto a larger partition and relies on this).
        if !read_only {
            match fs.grow_to_device() {
                Ok(true) => info!("btrfs: filesystem expanded to device size"),
                Ok(false) => {}
                Err(e) => warn!("btrfs: grow_to_device failed: {:?}", e),
            }
        }
        let arc = Arc::new(Self {
            inner: Mutex::new(fs),
            this: Mutex::new(Weak::new()),
            dir_cache: Mutex::new(BTreeMap::new()),
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
                let st = fs.stat(entry.ino).map_err(map_err)?;
                cached.push(CachedDirEntry {
                    name: entry.name,
                    metadata: stat_to_metadata(&st),
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
}

impl FileSystem for BtrfsMountFs {
    fn sync(&self) -> Result<()> {
        self.inner.lock().sync().map_err(map_err)
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
        blocks: ((st.nbytes + 511) / 512) as usize,
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
        let st = fs.stat(self.ino).map_err(map_err)?;
        match st.kind {
            FileKind::Dir => Err(FsError::IsDir),
            FileKind::Symlink => {
                let target = fs.read_link(self.ino).map_err(map_err)?;
                if offset >= target.len() {
                    return Ok(0);
                }
                let take = buf.len().min(target.len() - offset);
                buf[..take].copy_from_slice(&target[offset..offset + take]);
                Ok(take)
            }
            _ => fs.read(self.ino, offset as u64, buf).map_err(map_err),
        }
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        let mut fs = self.fs.inner.lock();
        let st = fs.stat(self.ino).map_err(map_err)?;
        match st.kind {
            FileKind::Dir => Err(FsError::IsDir),
            FileKind::Symlink => fs
                .write_symlink(self.ino, offset as u64, buf)
                .map_err(map_err),
            _ => fs.write(self.ino, offset as u64, buf).map_err(map_err),
        }
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
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        let mut fs = self.fs.inner.lock();
        let st = fs.stat(self.ino).map_err(map_err)?;
        Ok(stat_to_metadata(&st))
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<()> {
        let mut fs = self.fs.inner.lock();
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
            ".." => Err(FsError::EntryNotFound),
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
                Ok((entry.metadata.clone(), entry.name.clone()))
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
            fs.create(self.ino, name, kind, mode, data as u64)
                .map_err(map_err)?
        };
        self.fs.invalidate_dir(self.ino);
        Ok(self.fs.inode(ino))
    }

    fn unlink(&self, name: &str) -> Result<()> {
        {
            let mut fs = self.fs.inner.lock();
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
            fs.rename(self.ino, old_name, target.ino, new_name)
                .map_err(map_err)?;
        }
        self.fs.invalidate_dir(self.ino);
        self.fs.invalidate_dir(target.ino);
        Ok(())
    }

    fn resize(&self, len: usize) -> Result<()> {
        let mut fs = self.fs.inner.lock();
        fs.truncate(self.ino, len as u64).map_err(map_err)
    }

    fn sync_all(&self) -> Result<()> {
        self.fs.inner.lock().sync().map_err(map_err)
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
