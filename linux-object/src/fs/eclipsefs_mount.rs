//! EclipseFS mount support via the in-tree `eclipsefs` crate.
//!
//! Same shape as `btrfs_mount`: the crate is a `&mut self` filesystem behind
//! one lock, the VFS inodes are `(fs, ino)` pairs, and the block device is the
//! very `DevAdapter` btrfs uses, so both get the same chunking and retries.
//!
//! Mount options (`-o`): `compress=zlib|lz4|zstd|no` (and `compress-force=`),
//! `subvol=<path>` / `subvolid=<id>` for the subvolume to show as `/`,
//! `degraded` to mount a volume with devices missing, and `device=<dev>` to
//! name a member of a volume, and `commit=<secs>` for how often the open
//! transaction commits (5 by default, as ext4; a kernel task does it, and
//! `fsync` writes the file to the fsync log in between). A volume's other
//! members are also found by
//! scanning every block device the kernel registered, as `btrfs device
//! scan` does. The filesystem ioctls (chattr, reflink, snapshots, quotas,
//! scrub, devices) are in `eclipsefs_ioctl`.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;

use eclipsefs::{Compression, EclipseFS, Error as EfsError, FileKind};
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
        EfsError::NoKey => FsError::NoKey,
    }
}

fn wall_clock() -> (u64, u32) {
    let now = kernel_hal::timer::wall_clock_now();
    (now.as_secs(), now.subsec_nanos())
}

/// A directory's entries as `(name, ino)`, in on-disk order.
type Listing = Arc<Vec<(String, u64)>>;

pub struct EclipseMountFs {
    pub(crate) inner: Mutex<EclipseFS>,
    /// Mounted read-only: the ioctls that change anything refuse (EROFS).
    pub(crate) read_only: bool,
    /// What `/` of this mount is: the default subvolume's root directory,
    /// or the one `subvol=`/`subvolid=` named.
    root: u64,
    this: Mutex<Weak<Self>>,
    /// Directory listings by inode, so `getdents` over a big directory is not
    /// one full listing per entry. Dropped on any change to that directory.
    dir_cache: Mutex<BTreeMap<u64, Listing>>,
}

/// Block devices the kernel found at boot, by name (`sda2`), for the
/// device scan that puts a volume together and for `device=`.
static BLOCK_DEVICES: Mutex<Vec<(String, Arc<dyn BlockScheme>)>> = Mutex::new(Vec::new());

/// Remember a block device for the volume scan (see [`BLOCK_DEVICES`]).
pub(crate) fn register_block_device(name: &str, block: Arc<dyn BlockScheme>) {
    let mut devs = BLOCK_DEVICES.lock();
    devs.retain(|(n, _)| n != name);
    devs.push((String::from(name), block));
}

/// Forget a block device that went away.
///
/// A hot-plugged disk leaves on its own, and the name it had goes back into
/// circulation: leaving its entry here would hand the next disk to carry that
/// name a driver for the one that is gone, and `volume_members` searches only
/// this registry.
pub(crate) fn forget_block_device(name: &str) {
    BLOCK_DEVICES.lock().retain(|(n, _)| n != name);
}

/// The EclipseFS `-o` options this adapter understands.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct EfsOptions {
    /// `compress=`: `Some(None)` is `compress=no`.
    pub compress: Option<Option<Compression>>,
    pub degraded: bool,
    /// `device=` names, basename only (`/dev/sdb1` -> `sdb1`).
    pub devices: Vec<String>,
    pub subvol: Option<String>,
    pub subvolid: Option<u64>,
    /// `commit=`: seconds between commits.
    pub commit: Option<u64>,
}

/// Seconds between commits when `commit=` does not say (ext4's default).
pub(crate) const DEFAULT_COMMIT_SECS: u64 = 5;

impl EfsOptions {
    /// Read the comma-separated `data` of mount(2). Options of the generic
    /// layer (`ro`, `noatime`, ...) are left to it; an EclipseFS option with
    /// a bad value is an error, as on btrfs.
    pub(crate) fn parse(data: &str) -> Result<Self> {
        let mut o = Self::default();
        for opt in data.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (key, value) = match opt.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (opt, None),
            };
            match (key, value) {
                ("compress" | "compress-force", None) => {
                    o.compress = Some(Some(Compression::Zlib));
                }
                ("compress" | "compress-force", Some(v)) => {
                    // btrfs accepts a level after a colon (`zstd:3`).
                    let algo = v.split(':').next().unwrap_or("");
                    o.compress = Some(match algo {
                        "no" | "none" => None,
                        name => Some(Compression::from_name(name).ok_or(FsError::InvalidParam)?),
                    });
                }
                ("degraded", None) => o.degraded = true,
                ("device", Some(v)) if !v.is_empty() => {
                    let name = v.rsplit('/').next().unwrap_or(v);
                    o.devices.push(String::from(name));
                }
                ("subvol", Some(v)) => o.subvol = Some(String::from(v)),
                ("subvolid", Some(v)) => {
                    o.subvolid = Some(v.parse().map_err(|_| FsError::InvalidParam)?);
                }
                ("commit", Some(v)) => {
                    // btrfs: 0 means the default.
                    let secs: u64 = v.parse().map_err(|_| FsError::InvalidParam)?;
                    o.commit = Some(if secs == 0 { DEFAULT_COMMIT_SECS } else { secs });
                }
                ("device" | "subvol" | "subvolid" | "commit", _) => {
                    return Err(FsError::InvalidParam)
                }
                _ => {}
            }
        }
        Ok(o)
    }
}

fn adapter(backend: &MountBackend) -> Result<Arc<dyn eclipsefs::BlockDevice>> {
    let size = backend_size(backend)?;
    let device = device_from_backend(backend)?;
    Ok(Arc::new(DevAdapter::new(device, size)))
}

/// The other members of the volume `main` belongs to: the `device=` ones
/// first, then every registered block device whose superblock carries the
/// same filesystem uuid, one per device id.
fn volume_members(
    main: &Arc<dyn eclipsefs::BlockDevice>,
    probe: &eclipsefs::DeviceProbe,
    opts: &EfsOptions,
) -> Vec<Arc<dyn eclipsefs::BlockDevice>> {
    let registered: Vec<(String, Arc<dyn BlockScheme>)> = BLOCK_DEVICES.lock().clone();
    let named = opts
        .devices
        .iter()
        .filter_map(|want| registered.iter().find(|(n, _)| n == want));
    let rest = registered.iter().filter(|(n, _)| !opts.devices.contains(n));
    let mut ids = alloc::vec![probe.devid];
    let mut devs = alloc::vec![main.clone()];
    for (name, block) in named.chain(rest) {
        if !probe_eclipsefs_superblock(block) {
            continue;
        }
        let Ok(dev) = adapter(&MountBackend::Block(block.clone())) else {
            continue;
        };
        match eclipsefs::probe(&*dev) {
            Ok(p) if p.uuid == probe.uuid && !ids.contains(&p.devid) => {
                warn!(
                    "eclipsefs: device {} of the volume is /dev/{}",
                    p.devid, name
                );
                ids.push(p.devid);
                devs.push(dev);
            }
            _ => {}
        }
    }
    devs
}

impl EclipseMountFs {
    #[cfg(test)]
    pub fn open(backend: &MountBackend, read_only: bool) -> Result<Arc<Self>> {
        Self::open_with(backend, read_only, &EfsOptions::default())
    }

    pub(crate) fn open_with(
        backend: &MountBackend,
        read_only: bool,
        opts: &EfsOptions,
    ) -> Result<Arc<Self>> {
        let size = backend_size(backend)?;
        let adapter = adapter(backend)?;
        warn!(
            "eclipsefs: mounting, device size = {:#x} ({} MiB), read_only={}",
            size,
            size / (1024 * 1024),
            read_only
        );
        let probe = eclipsefs::probe(&*adapter).map_err(map_err)?;
        let mut fs = if probe.num_devices > 1 || opts.degraded {
            let devs = volume_members(&adapter, &probe, opts);
            warn!(
                "eclipsefs: volume of {} devices, {} found{}",
                probe.num_devices,
                devs.len(),
                if opts.degraded { " (degraded)" } else { "" }
            );
            if devs.len() < probe.num_devices as usize && !opts.degraded {
                warn!("eclipsefs: devices of the volume are missing; mount -o degraded to use it");
                return Err(FsError::InvalidParam);
            }
            if opts.degraded {
                EclipseFS::mount_degraded(devs, read_only)
            } else {
                EclipseFS::mount_devices(devs, read_only)
            }
        } else {
            EclipseFS::mount(adapter, read_only)
        }
        .map_err(map_err)?;
        fs.set_clock(wall_clock);
        // Nonces of encrypted inodes.
        fs.set_random(kernel_hal::rand::fill_random);
        if let Some(algo) = opts.compress {
            fs.set_mount_compression(algo).map_err(map_err)?;
        }
        let root = mount_root(&mut fs, opts)?;
        if !read_only {
            fs.set_commit_interval(Some(opts.commit.unwrap_or(DEFAULT_COMMIT_SECS)));
            match fs.grow_to_device() {
                Ok(true) => warn!("eclipsefs: filesystem expanded to device size"),
                Ok(false) => {}
                Err(e) => warn!("eclipsefs: grow_to_device failed: {:?}", e),
            }
        }
        let arc = Arc::new(Self {
            inner: Mutex::new(fs),
            read_only,
            root,
            this: Mutex::new(Weak::new()),
            dir_cache: Mutex::new(BTreeMap::new()),
        });
        *arc.this.lock() = Arc::downgrade(&arc);
        #[cfg(not(test))]
        if !read_only {
            Self::start_committer(Arc::downgrade(&arc));
        }
        Ok(arc)
    }

    /// The kernel task that commits the open transaction every `commit=`
    /// seconds, so that operations do not (and an idle filesystem still
    /// does). It ends with the mount.
    #[cfg(not(test))]
    fn start_committer(fs: Weak<Self>) {
        kernel_hal::thread::spawn(async move {
            loop {
                let tick = kernel_hal::timer::timer_now() + core::time::Duration::from_secs(1);
                kernel_hal::thread::sleep_until(tick).await;
                let Some(fs) = fs.upgrade() else { break };
                let r = fs.inner.lock().commit_if_due();
                if let Err(e) = r {
                    warn!("eclipsefs: periodic commit failed: {:?}", e);
                }
            }
        });
    }

    pub(crate) fn inode(&self, ino: u64) -> Arc<EclipseMountINode> {
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

    pub(crate) fn invalidate_all_dirs(&self) {
        self.dir_cache.lock().clear();
    }

    pub(crate) fn invalidate_dir(&self, dir: u64) {
        self.dir_cache.lock().remove(&dir);
    }
}

/// The directory `subvol=`/`subvolid=` name, or the default subvolume's
/// root. `subvol=` is a path from the top subvolume (id 5), as on btrfs.
fn mount_root(fs: &mut EclipseFS, opts: &EfsOptions) -> Result<u64> {
    use eclipsefs::cowfmt::{handle, FIRST_INO, FS_TREE};
    let mut root = match opts.subvolid {
        Some(id) => handle(id, FIRST_INO),
        None if opts.subvol.is_some() => handle(FS_TREE, FIRST_INO),
        None => return Ok(fs.root_ino()),
    };
    if fs.format_version() < 4 {
        return Err(FsError::NotSupported);
    }
    if let Some(path) = &opts.subvol {
        for name in path.split('/').filter(|c| !c.is_empty() && *c != ".") {
            root = fs.lookup(root, name).map_err(map_err)?;
        }
    }
    let st = fs.stat(root).map_err(map_err)?;
    if st.kind != FileKind::Dir {
        return Err(FsError::NotDir);
    }
    Ok(root)
}

impl FileSystem for EclipseMountFs {
    fn sync(&self) -> Result<()> {
        self.inner.lock().sync().map_err(map_err)
    }

    fn root_inode(&self) -> Arc<dyn INode> {
        self.inode(self.root)
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

pub(crate) struct EclipseMountINode {
    pub(crate) fs: Arc<EclipseMountFs>,
    pub(crate) ino: u64,
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

    /// `fsync`: the file alone through the fsync log when it can, a commit
    /// otherwise (directories, new files, renames...).
    fn sync_all(&self) -> Result<()> {
        self.fs.inner.lock().fsync(self.ino).map_err(map_err)
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

/// Whether `inode` is one of a mounted EclipseFS. Its reads hold
/// `EclipseMountFs::inner`, an interrupts-off spinlock, across the device
/// command, which is why `File` queues them first (`disk_read_gate`).
pub(super) fn owns(inode: &dyn INode) -> bool {
    inode.downcast_ref::<EclipseMountINode>().is_some()
}

/// Open a mount backend as an EclipseFS filesystem, with the `-o` options
/// in `data` (see the module documentation).
pub fn open_eclipsefs(
    backend: &MountBackend,
    read_only: bool,
    data: &str,
) -> Result<Arc<dyn FileSystem>> {
    let opts = EfsOptions::parse(data)?;
    EclipseMountFs::open_with(backend, read_only, &opts).map(|fs| fs as Arc<dyn FileSystem>)
}

/// Cheap pre-mount probe: does the backing device start with an EclipseFS
/// superblock of a version this kernel reads, that fits on the device? A
/// member of a volume only needs room for its own superblock: the chunk map
/// places the rest.
pub(crate) fn probe_eclipsefs_superblock(block: &Arc<dyn BlockScheme>) -> bool {
    #[repr(align(4096))]
    struct SuperBuf([u8; 4096]);
    let mut sb = SuperBuf([0u8; 4096]);
    for (i, sector) in sb.0.chunks_mut(512).enumerate() {
        if block.read_block(i, sector).is_err() {
            return false;
        }
        if i == 0 && &sector[0..9] != eclipsefs::format::MAGIC {
            return false;
        }
    }
    let device_bytes = block.block_count() as u64 * 512;
    let dev = SuperblockDev {
        block: &sb.0,
        size: device_bytes,
    };
    match eclipsefs::probe(&dev) {
        Ok(p) => p.min_device_bytes > 0 && p.min_device_bytes <= device_bytes,
        Err(_) => false,
    }
}

/// The first block of a device, as a `BlockDevice` for `eclipsefs::probe`.
struct SuperblockDev<'a> {
    block: &'a [u8],
    size: u64,
}

impl eclipsefs::BlockDevice for SuperblockDev<'_> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> eclipsefs::Result<()> {
        let start = offset as usize;
        let src = self
            .block
            .get(start..start + buf.len())
            .ok_or(EfsError::Io)?;
        buf.copy_from_slice(src);
        Ok(())
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> eclipsefs::Result<()> {
        Err(EfsError::Io)
    }

    fn sync(&self) -> eclipsefs::Result<()> {
        Ok(())
    }

    fn size(&self) -> u64 {
        self.size
    }
}

#[cfg(test)]
pub(crate) mod testdisk {
    //! RAM disks with an EclipseFS on them, for the tests of this adapter
    //! and of `eclipsefs_ioctl`.

    use super::*;
    use alloc::vec;

    pub(crate) struct RamDisk {
        pub(crate) bytes: Mutex<Vec<u8>>,
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
    pub(crate) struct Formatter(pub(crate) Arc<RamDisk>);

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

    pub(crate) fn formatted(bytes: usize) -> Arc<RamDisk> {
        formatted_with(bytes, eclipsefs::mkfs::format)
    }

    pub(crate) fn formatted_with(
        bytes: usize,
        format: fn(
            &dyn eclipsefs::BlockDevice,
            &eclipsefs::mkfs::MkfsOptions,
        ) -> eclipsefs::Result<()>,
    ) -> Arc<RamDisk> {
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
        format(&Formatter(disk.clone()), &opts).unwrap();
        disk
    }

    pub(crate) fn blank(bytes: usize) -> Arc<RamDisk> {
        Arc::new(RamDisk {
            bytes: Mutex::new(vec![0u8; bytes]),
        })
    }
}

#[cfg(test)]
mod tests {
    //! The adapter end to end on a RAM disk: format, mount through the same
    //! `MountBackend` the kernel uses, and drive it through `INode`.

    use super::testdisk::*;
    use super::*;

    #[test]
    fn older_v3_images_are_recognised_and_mount() {
        let disk = formatted_with(8 * 1024 * 1024, eclipsefs::mkfs::format_v3);
        let dev: Arc<dyn BlockScheme> = disk.clone();
        assert!(probe_eclipsefs_superblock(&dev));
        {
            let fs = EclipseMountFs::open(&MountBackend::Block(disk.clone()), false).unwrap();
            let f = fs
                .root_inode()
                .create("old", FileType::File, 0o644)
                .unwrap();
            assert_eq!(f.write_at(0, b"v0.3").unwrap(), 4);
            fs.sync().unwrap();
        }
        let fs = EclipseMountFs::open(&MountBackend::Block(disk), false).unwrap();
        let mut back = [0u8; 8];
        let n = fs
            .root_inode()
            .find("old")
            .unwrap()
            .read_at(0, &mut back)
            .unwrap();
        assert_eq!(&back[..n], b"v0.3");
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
