//! EclipseFS mount support via the in-tree `eclipsefs` crate.
//!
//! Same shape as `btrfs_mount`: the crate is a `&mut self` filesystem behind
//! one lock, the VFS inodes are `(fs, ino)` pairs, and the block device is the
//! very `DevAdapter` btrfs uses, so both get the same chunking and retries.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::convert::TryInto;

use eclipsefs::{EclipseFS, Error as EfsError, FileKind};
use lock::Mutex;
use rcore_fs::vfs::{
    FileSystem, FileType, FsError, FsInfo, INode, Metadata, PollStatus, Result, Timespec,
};
use zcore_drivers::scheme::BlockScheme;

use super::block_mount::{backend_size, device_from_backend, MountBackend};
use super::btrfs_mount::DevAdapter;

fn map_err(e: EfsError) -> FsError {
    match e {
        EfsError::Io => FsError::DeviceError,
        EfsError::BadSuperblock => FsError::WrongFs,
        EfsError::Corrupt(msg) => {
            warn!("eclipsefs: corrupt filesystem: {}", msg);
            FsError::DeviceError
        }
        EfsError::Unsupported(msg) => {
            warn!("eclipsefs: unsupported: {}", msg);
            FsError::NotSupported
        }
        EfsError::NotFound => FsError::EntryNotFound,
        EfsError::Exists => FsError::EntryExist,
        EfsError::NotDir => FsError::NotDir,
        EfsError::IsDir => FsError::IsDir,
        EfsError::NotEmpty => FsError::DirNotEmpty,
        EfsError::NoSpace => FsError::NoDeviceSpace,
        EfsError::Invalid => FsError::InvalidParam,
    }
}

fn wall_clock() -> (u64, u32) {
    let now = kernel_hal::timer::wall_clock_now();
    (now.as_secs(), now.subsec_nanos())
}

/// A directory's entries as `(name, ino)`, in on-disk order.
type Listing = Arc<Vec<(String, u64)>>;

pub struct EclipseMountFs {
    inner: Mutex<EclipseFS>,
    this: Mutex<Weak<Self>>,
    /// Directory listings by inode, so `getdents` over a big directory is not
    /// one full listing per entry. Dropped on any change to that directory.
    dir_cache: Mutex<BTreeMap<u64, Listing>>,
}

impl EclipseMountFs {
    pub fn open(backend: &MountBackend, read_only: bool) -> Result<Arc<Self>> {
        let size = backend_size(backend)?;
        let device = device_from_backend(backend)?;
        let adapter: Arc<dyn eclipsefs::BlockDevice> = Arc::new(DevAdapter::new(device, size));
        warn!(
            "eclipsefs: mounting, device size = {:#x} ({} MiB), read_only={}",
            size,
            size / (1024 * 1024),
            read_only
        );
        let mut fs = EclipseFS::mount(adapter, read_only).map_err(map_err)?;
        fs.set_clock(wall_clock);
        if !read_only {
            match fs.grow_to_device() {
                Ok(true) => warn!("eclipsefs: filesystem expanded to device size"),
                Ok(false) => {}
                Err(e) => warn!("eclipsefs: grow_to_device failed: {:?}", e),
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

    fn inode(&self, ino: u64) -> Arc<EclipseMountINode> {
        Arc::new(EclipseMountINode {
            fs: self.this.lock().upgrade().expect("EclipseMountFs dropped"),
            ino,
        })
    }

    fn listing(&self, dir: u64) -> Result<Listing> {
        if let Some(entries) = self.dir_cache.lock().get(&dir) {
            return Ok(entries.clone());
        }
        let entries = self.inner.lock().readdir(dir).map_err(map_err)?;
        let entries: Listing = Arc::new(entries.into_iter().map(|e| (e.name, e.ino)).collect());
        self.dir_cache.lock().insert(dir, entries.clone());
        Ok(entries)
    }

    fn invalidate_dir(&self, dir: u64) {
        self.dir_cache.lock().remove(&dir);
    }
}

impl FileSystem for EclipseMountFs {
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
        let free = stat.total_bytes.saturating_sub(stat.bytes_used) / bsize;
        FsInfo {
            bsize: bsize as usize,
            frsize: bsize as usize,
            blocks: (stat.total_bytes / bsize) as usize,
            bfree: free as usize,
            bavail: free as usize,
            files: 0,
            ffree: 0,
            namemax: 255,
        }
    }
}

struct EclipseMountINode {
    fs: Arc<EclipseMountFs>,
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

fn efs_kind(type_: FileType) -> FileKind {
    match type_ {
        FileType::File => FileKind::Regular,
        FileType::Dir => FileKind::Dir,
        FileType::SymLink => FileKind::Symlink,
        FileType::CharDevice => FileKind::CharDevice,
        FileType::BlockDevice => FileKind::BlockDevice,
        FileType::NamedPipe => FileKind::Fifo,
        FileType::Socket => FileKind::Socket,
    }
}

fn ts((sec, nsec): (u64, u32)) -> Timespec {
    Timespec {
        sec: sec as i64,
        nsec: nsec as i32,
    }
}

fn stat_to_metadata(st: &eclipsefs::InodeStat) -> Metadata {
    Metadata {
        dev: 0,
        inode: st.ino as usize,
        size: st.size as usize,
        blk_size: 512,
        blocks: st.nbytes.div_ceil(512) as usize,
        atime: ts(st.atime),
        mtime: ts(st.mtime),
        ctime: ts(st.ctime),
        type_: vfs_type(st.kind),
        mode: (st.mode & 0o7777) as u16,
        nlinks: st.nlink as usize,
        uid: st.uid as usize,
        gid: st.gid as usize,
        rdev: st.rdev as usize,
    }
}

impl EclipseMountINode {
    fn same_fs<'a>(&self, other: &'a Arc<dyn INode>) -> Result<&'a EclipseMountINode> {
        let other = other
            .downcast_ref::<EclipseMountINode>()
            .ok_or(FsError::NotSameFs)?;
        if !Arc::ptr_eq(&self.fs, &other.fs) {
            return Err(FsError::NotSameFs);
        }
        Ok(other)
    }
}

impl INode for EclipseMountINode {
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
                let n = buf.len().min(target.len() - offset);
                buf[..n].copy_from_slice(&target[offset..offset + n]);
                Ok(n)
            }
            _ => fs.read(self.ino, offset as u64, buf).map_err(map_err),
        }
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        let mut fs = self.fs.inner.lock();
        match fs.stat(self.ino).map_err(map_err)?.kind {
            FileKind::Dir => Err(FsError::IsDir),
            FileKind::Symlink => fs
                .write_symlink(self.ino, offset as u64, buf)
                .map_err(map_err),
            _ => fs.write(self.ino, offset as u64, buf).map_err(map_err),
        }
    }

    fn poll(&self) -> Result<PollStatus> {
        let st = self.fs.inner.lock().stat(self.ino).map_err(map_err)?;
        Ok(PollStatus {
            read: true,
            write: st.kind != FileKind::Dir,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        let st = self.fs.inner.lock().stat(self.ino).map_err(map_err)?;
        Ok(stat_to_metadata(&st))
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<()> {
        self.fs
            .inner
            .lock()
            .set_attr(
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
        let ino = match name {
            "." | "" => self.ino,
            ".." => self.fs.inner.lock().parent(self.ino).map_err(map_err)?,
            name => self
                .fs
                .inner
                .lock()
                .lookup(self.ino, name)
                .map_err(map_err)?,
        };
        Ok(self.fs.inode(ino))
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        Ok(self.get_entry_with_metadata(id)?.1)
    }

    fn get_entry_with_metadata(&self, id: usize) -> Result<(Metadata, String)> {
        match id {
            0 => Ok((self.metadata()?, String::from("."))),
            1 => Ok((self.find("..")?.metadata()?, String::from(".."))),
            i => {
                let entries = self.fs.listing(self.ino)?;
                let (name, ino) = entries.get(i - 2).ok_or(FsError::EntryNotFound)?;
                let st = self.fs.inner.lock().stat(*ino).map_err(map_err)?;
                Ok((stat_to_metadata(&st), name.clone()))
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
        let ino = self
            .fs
            .inner
            .lock()
            .create(self.ino, name, efs_kind(type_), mode, data as u64)
            .map_err(map_err)?;
        self.fs.invalidate_dir(self.ino);
        Ok(self.fs.inode(ino))
    }

    fn unlink(&self, name: &str) -> Result<()> {
        self.fs
            .inner
            .lock()
            .unlink(self.ino, name)
            .map_err(map_err)?;
        self.fs.invalidate_dir(self.ino);
        Ok(())
    }

    fn link(&self, name: &str, other: &Arc<dyn INode>) -> Result<()> {
        let other = self.same_fs(other)?;
        self.fs
            .inner
            .lock()
            .link(self.ino, name, other.ino)
            .map_err(map_err)?;
        self.fs.invalidate_dir(self.ino);
        Ok(())
    }

    fn move_(&self, old_name: &str, target: &Arc<dyn INode>, new_name: &str) -> Result<()> {
        let target = self.same_fs(target)?;
        self.fs
            .inner
            .lock()
            .rename(self.ino, old_name, target.ino, new_name)
            .map_err(map_err)?;
        self.fs.invalidate_dir(self.ino);
        self.fs.invalidate_dir(target.ino);
        Ok(())
    }

    fn resize(&self, len: usize) -> Result<()> {
        self.fs
            .inner
            .lock()
            .truncate(self.ino, len as u64)
            .map_err(map_err)
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

/// Open a mount backend as an EclipseFS filesystem.
pub fn open_eclipsefs(backend: &MountBackend, read_only: bool) -> Result<Arc<dyn FileSystem>> {
    EclipseMountFs::open(backend, read_only).map(|fs| fs as Arc<dyn FileSystem>)
}

/// Cheap pre-mount probe: does the backing device start with an EclipseFS
/// superblock of a version this kernel reads, no larger than the device?
pub(crate) fn probe_eclipsefs_superblock(block: &Arc<dyn BlockScheme>) -> bool {
    #[repr(align(4096))]
    struct SectorBuf([u8; 512]);
    let mut sb = SectorBuf([0u8; 512]);
    if block.read_block(0, &mut sb.0).is_err() {
        return false;
    }
    if &sb.0[0..9] != eclipsefs::format::MAGIC {
        return false;
    }
    let version = u32::from_le_bytes(sb.0[12..16].try_into().unwrap());
    let total_blocks = u64::from_le_bytes(sb.0[24..32].try_into().unwrap());
    let device_bytes = block.block_count() as u64 * 512;
    version >> 16 == eclipsefs::format::VERSION_MAJOR
        && total_blocks > 0
        && total_blocks.saturating_mul(eclipsefs::format::BLOCK_SIZE) <= device_bytes
}

#[cfg(test)]
mod tests {
    //! The adapter end to end on a RAM disk: format, mount through the same
    //! `MountBackend` the kernel uses, and drive it through `INode`.

    use super::*;
    use alloc::vec;

    struct RamDisk {
        bytes: Mutex<Vec<u8>>,
    }

    impl zcore_drivers::scheme::Scheme for RamDisk {
        fn name(&self) -> &str {
            "ramdisk"
        }
    }

    impl BlockScheme for RamDisk {
        fn read_block(&self, block_id: usize, buf: &mut [u8]) -> zcore_drivers::DeviceResult {
            let d = self.bytes.lock();
            let start = block_id * 512;
            if !buf.len().is_multiple_of(512) || start + buf.len() > d.len() {
                return Err(zcore_drivers::DeviceError::InvalidParam);
            }
            buf.copy_from_slice(&d[start..start + buf.len()]);
            Ok(())
        }
        fn write_block(&self, block_id: usize, buf: &[u8]) -> zcore_drivers::DeviceResult {
            let mut d = self.bytes.lock();
            let start = block_id * 512;
            if !buf.len().is_multiple_of(512) || start + buf.len() > d.len() {
                return Err(zcore_drivers::DeviceError::InvalidParam);
            }
            d[start..start + buf.len()].copy_from_slice(buf);
            Ok(())
        }
        fn flush(&self) -> zcore_drivers::DeviceResult {
            Ok(())
        }
        fn block_count(&self) -> usize {
            self.bytes.lock().len() / 512
        }
    }

    /// `mkfs` straight into the disk's bytes.
    struct Formatter(Arc<RamDisk>);

    impl eclipsefs::BlockDevice for Formatter {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> eclipsefs::Result<()> {
            let d = self.0.bytes.lock();
            let o = offset as usize;
            buf.copy_from_slice(&d[o..o + buf.len()]);
            Ok(())
        }
        fn write_at(&self, offset: u64, buf: &[u8]) -> eclipsefs::Result<()> {
            let mut d = self.0.bytes.lock();
            let o = offset as usize;
            d[o..o + buf.len()].copy_from_slice(buf);
            Ok(())
        }
        fn sync(&self) -> eclipsefs::Result<()> {
            Ok(())
        }
        fn size(&self) -> u64 {
            self.0.bytes.lock().len() as u64
        }
    }

    fn formatted(bytes: usize) -> Arc<RamDisk> {
        let disk = Arc::new(RamDisk {
            bytes: Mutex::new(vec![0u8; bytes]),
        });
        let opts = eclipsefs::mkfs::MkfsOptions {
            label: "ECLIPSE".into(),
            fsid: [9; 16],
            chunk_uuid: [0; 16],
            dev_uuid: [0; 16],
            subvol_uuid: [0; 16],
            now: (1_700_000_000, 0),
        };
        eclipsefs::mkfs::format(&Formatter(disk.clone()), &opts).unwrap();
        disk
    }

    #[test]
    fn the_probe_recognises_only_eclipsefs() {
        let disk = formatted(8 * 1024 * 1024);
        let dev: Arc<dyn BlockScheme> = disk.clone();
        assert!(probe_eclipsefs_superblock(&dev));
        disk.bytes.lock()[3] = b'X';
        assert!(!probe_eclipsefs_superblock(&dev));
        let blank: Arc<dyn BlockScheme> = Arc::new(RamDisk {
            bytes: Mutex::new(vec![0u8; 1024 * 1024]),
        });
        assert!(!probe_eclipsefs_superblock(&blank));
    }

    #[test]
    fn a_tree_written_through_the_vfs_reads_back_after_remount() {
        let disk = formatted(16 * 1024 * 1024);
        let payload: Vec<u8> = (0..300_000).map(|i| (i % 251) as u8).collect();
        {
            let fs = EclipseMountFs::open(&MountBackend::Block(disk.clone()), false).unwrap();
            let root = fs.root_inode();
            let etc = root.create("etc", FileType::Dir, 0o755).unwrap();
            let f = etc.create("data", FileType::File, 0o644).unwrap();
            let mut off = 0;
            for chunk in payload.chunks(4000) {
                assert_eq!(f.write_at(off, chunk).unwrap(), chunk.len());
                off += chunk.len();
            }
            let l = root.create("lib64", FileType::SymLink, 0o777).unwrap();
            l.write_at(0, b"usr/lib").unwrap();
            etc.move_("data", &root, "moved").unwrap();
            fs.sync().unwrap();
        }
        let fs = EclipseMountFs::open(&MountBackend::Block(disk), false).unwrap();
        let root = fs.root_inode();
        let names: Vec<String> = (0..4).map(|i| root.get_entry(i).unwrap()).collect();
        assert_eq!(
            names,
            [".", "..", "etc", "lib64"].map(String::from).to_vec()
        );
        assert_eq!(root.get_entry(5).unwrap_err(), FsError::EntryNotFound);
        let f = root.find("moved").unwrap();
        let md = f.metadata().unwrap();
        assert_eq!(md.size, payload.len());
        assert_eq!(md.mode, 0o644);
        let mut back = vec![0u8; payload.len()];
        assert_eq!(f.read_at(0, &mut back).unwrap(), payload.len());
        assert_eq!(back, payload);
        let mut target = [0u8; 16];
        let n = root.find("lib64").unwrap().read_at(0, &mut target).unwrap();
        assert_eq!(&target[..n], b"usr/lib");
        let etc = root.find("etc").unwrap();
        assert_eq!(
            etc.find("..").unwrap().metadata().unwrap().inode,
            root.metadata().unwrap().inode
        );
        assert!(matches!(etc.find("data"), Err(FsError::EntryNotFound)));
        root.unlink("etc").unwrap();
    }
}
