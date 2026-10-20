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
//! `fsync` writes the file to the fsync log in between), and `repair` to
//! put right what fsck finds before the filesystem is used (as `btrfs check
//! --repair`; refused read-only, damage it cannot fix is logged and the
//! mount goes on). A volume's other
//! members are also found by
//! scanning every block device the kernel registered, as `btrfs device
//! scan` does. The filesystem ioctls (chattr, reflink, snapshots, quotas,
//! scrub, devices) are in `eclipsefs_ioctl`.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;

use core::sync::atomic::{AtomicUsize, Ordering};
use eclipsefs::{Compression, EclipseFS, Error as EfsError, FileKind};
use lock::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
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

/// The filesystem's lock: exclusive for everything ([`lock`](Self::lock)),
/// shared for the reads that can run side by side
/// ([`EclipseFS::read_shared`] through [`shared`](Self::shared)).
///
/// The `RwLock` underneath lets readers in while any reader is inside, so
/// a steady stream of them would keep a writer spinning for ever. A writer
/// that wants in says so first, and while one does, `shared` answers `None`
/// and the reader queues for the exclusive lock behind it instead.
pub(crate) struct FsLock {
    fs: RwLock<EclipseFS>,
    writers: AtomicUsize,
}

impl FsLock {
    fn new(fs: EclipseFS) -> Self {
        Self {
            fs: RwLock::new(fs),
            writers: AtomicUsize::new(0),
        }
    }

    /// The filesystem, exclusively.
    #[track_caller]
    pub(crate) fn lock(&self) -> RwLockWriteGuard<'_, EclipseFS> {
        self.writers.fetch_add(1, Ordering::SeqCst);
        let g = self.fs.write();
        self.writers.fetch_sub(1, Ordering::SeqCst);
        g
    }

    /// The filesystem shared with other readers, unless a writer is waiting.
    #[track_caller]
    pub(crate) fn shared(&self) -> Option<RwLockReadGuard<'_, EclipseFS>> {
        if self.writers.load(Ordering::SeqCst) != 0 {
            return None;
        }
        Some(self.fs.read())
    }
}

/// A directory's entries as `(name, ino)`, in on-disk order.
type Listing = Arc<Vec<(String, u64)>>;

pub struct EclipseMountFs {
    pub(crate) inner: FsLock,
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
    /// `repair`: run the offline repair at mount.
    pub repair: bool,
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
                ("repair", None) => o.repair = true,
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
                ("device" | "subvol" | "subvolid" | "commit" | "repair", _) => {
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
        if opts.repair {
            if read_only {
                warn!("eclipsefs: repair needs a read-write mount");
                return Err(FsError::InvalidParam);
            }
            let r = fs.repair().map_err(map_err)?;
            warn!("eclipsefs: repair: {:?}", r);
            if let Some(why) = r.still_damaged {
                warn!("eclipsefs: repair could not fix everything: {}", why);
            }
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
            inner: FsLock::new(fs),
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
        // Shared first: a regular file whose read needs nothing the
        // exclusive path has (see `EclipseFS::read_shared`).
        if let Some(fs) = self.fs.inner.shared() {
            if let Some(st) = fs.stat_shared(self.ino).map_err(map_err)? {
                if st.kind == FileKind::Regular {
                    if let Some(n) = fs
                        .read_shared(self.ino, offset as u64, buf)
                        .map_err(map_err)?
                    {
                        return Ok(n);
                    }
                }
            }
        }
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
        if let Some(fs) = self.fs.inner.shared() {
            if let Some(st) = fs.stat_shared(self.ino).map_err(map_err)? {
                return Ok(stat_to_metadata(&st));
            }
        }
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
/// `EclipseMountFs::inner`, an interrupts-off spinlock (shared when they
/// can), across the device command, which is why `File` queues them first
/// (`disk_read_gate`, with a slot per CPU but one).
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

    extern crate std;

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

    /// Reads take the lock shared and see what exclusive reads see; while a
    /// writer waits they take it exclusively, behind the writer.
    #[test]
    fn reads_share_the_lock_and_give_way_to_a_waiting_writer() {
        let disk = formatted(16 * 1024 * 1024);
        let fs = EclipseMountFs::open(&MountBackend::Block(disk), false).unwrap();
        let root = fs.root_inode();
        let f = root.create("f", FileType::File, 0o644).unwrap();
        let data: Vec<u8> = (0..70_000).map(|i| (i * 7 % 253) as u8).collect();
        assert_eq!(f.write_at(0, &data).unwrap(), data.len());
        let ino = f.downcast_ref::<EclipseMountINode>().unwrap().ino;
        {
            // Two readers at once.
            let a = fs.inner.shared().unwrap();
            let b = fs.inner.shared().unwrap();
            let mut x = vec![0u8; 1000];
            let mut y = vec![0u8; 1000];
            assert_eq!(a.read_shared(ino, 5000, &mut x).unwrap(), Some(1000));
            assert_eq!(b.read_shared(ino, 5000, &mut y).unwrap(), Some(1000));
            assert_eq!((&x[..], &y[..]), (&data[5000..6000], &data[5000..6000]));
        }
        let mut back = vec![0u8; data.len() + 10];
        assert_eq!(f.read_at(0, &mut back).unwrap(), data.len());
        assert_eq!(&back[..data.len()], &data[..]);
        assert_eq!(f.metadata().unwrap().size, data.len());
        // A writer waiting: readers do not get in ahead of it.
        fs.inner.writers.fetch_add(1, Ordering::SeqCst);
        assert!(fs.inner.shared().is_none());
        assert_eq!(f.read_at(100, &mut back[..10]).unwrap(), 10);
        assert_eq!(&back[..10], &data[100..110]);
        fs.inner.writers.fetch_sub(1, Ordering::SeqCst);
        assert!(fs.inner.shared().is_some());
        // Directories and symlinks still read through the exclusive path.
        let l = root.create("l", FileType::SymLink, 0o777).unwrap();
        l.write_at(0, b"target").unwrap();
        let mut t = [0u8; 16];
        let n = l.read_at(0, &mut t).unwrap();
        assert_eq!(&t[..n], b"target");
        assert!(matches!(root.read_at(0, &mut t), Err(FsError::IsDir)));
    }

    /// The two tables that translate between the on-disk file kinds and the
    /// VFS ones, walked entry by entry in both directions.
    #[test]
    fn the_file_kind_tables_name_the_same_seven_things_both_ways() {
        // Written out rather than derived from one of the tables, because
        // what matters is the PAIRING: a round trip through both of them
        // also comes back right when the same two entries are swapped in
        // each, and `getdents` would then call a socket a fifo for ever
        // without one test noticing.
        let pairs = [
            (FileKind::Regular, FileType::File),
            (FileKind::Dir, FileType::Dir),
            (FileKind::Symlink, FileType::SymLink),
            (FileKind::CharDevice, FileType::CharDevice),
            (FileKind::BlockDevice, FileType::BlockDevice),
            (FileKind::Fifo, FileType::NamedPipe),
            (FileKind::Socket, FileType::Socket),
        ];
        for &(kind, type_) in pairs.iter() {
            assert_eq!(vfs_type(kind), type_, "{:?}", kind);
            assert_eq!(efs_kind(type_), kind, "{:?}", type_);
        }
    }

    /// Every error the filesystem crate can return, and the `FsError` the
    /// VFS gets for it.
    #[test]
    fn every_error_of_the_crate_keeps_its_meaning_at_the_vfs_boundary() {
        // One line per variant, because a wrong line here is a wrong errno
        // that a program acts on: ENOTDIR where EISDIR belongs sends
        // `mkdir -p` down its "make the parent" path on a file.
        let table = [
            (EfsError::Io, FsError::DeviceError),
            (EfsError::BadSuperblock, FsError::WrongFs),
            (EfsError::Corrupt("tree"), FsError::DeviceError),
            (EfsError::Unsupported("feature"), FsError::NotSupported),
            (EfsError::NotFound, FsError::EntryNotFound),
            (EfsError::Exists, FsError::EntryExist),
            (EfsError::NotDir, FsError::NotDir),
            (EfsError::IsDir, FsError::IsDir),
            (EfsError::NotEmpty, FsError::DirNotEmpty),
            (EfsError::NoSpace, FsError::NoDeviceSpace),
            (EfsError::Invalid, FsError::InvalidParam),
            (EfsError::NoKey, FsError::NoKey),
        ];
        for (from, want) in table {
            assert_eq!(map_err(from), want, "{:?}", from);
        }
    }

    /// The numbers `stat(2)` hands a program about one file.
    #[test]
    fn the_metadata_of_a_file_reads_back_field_by_field() {
        let disk = formatted(8 * 1024 * 1024);
        let fs = EclipseMountFs::open(&MountBackend::Block(disk), false).unwrap();
        let root = fs.root_inode();
        let stat_of = |node: &Arc<dyn INode>| {
            let ino = node.downcast_ref::<EclipseMountINode>().unwrap().ino;
            fs.inner.lock().stat(ino).unwrap()
        };

        // A small file lives inside its own inode, so what it takes on disk
        // is its exact length, which is not a whole number of blocks: a
        // count that rounds DOWN says the file occupies nothing at all, and
        // `du` adds up zeros.
        let small = root.create("small", FileType::File, 0o644).unwrap();
        assert_eq!(small.write_at(0, &[1u8; 100]).unwrap(), 100);
        let md = small.metadata().unwrap();
        let st = stat_of(&small);
        assert_eq!(st.nbytes, 100, "a file that small is stored in its inode");
        assert_eq!(md.size, 100);
        assert!(md.blocks >= 1, "100 bytes occupy a block, not none");
        assert_eq!(md.blocks as u64, st.nbytes.div_ceil(md.blk_size as u64));

        // A big one takes whole filesystem blocks, and that is where the
        // unit the count is in shows: `blocks` is a number of `blk_size`
        // bytes, and `linux-syscall` hands both to the program as they are
        // (`st_blocks` and `st_blksize` in `file/stat.rs`), so a count in
        // some other unit is a size wrong by a factor.
        let big = root.create("big", FileType::File, 0o644).unwrap();
        let payload = [2u8; 10_000];
        assert_eq!(big.write_at(0, &payload).unwrap(), payload.len());
        let st = stat_of(&big);
        let md = big.metadata().unwrap();
        assert_eq!(md.size, payload.len(), "the file's length, not its extents");
        assert!(st.nbytes >= payload.len() as u64, "{}", st.nbytes);
        assert_eq!(md.blk_size, 512, "the unit POSIX counts st_blocks in");
        assert_eq!(
            md.blocks as u64,
            st.nbytes.div_ceil(md.blk_size as u64),
            "{} bytes in blocks of {}",
            st.nbytes,
            md.blk_size
        );

        // setuid, setgid and the sticky bit sit above the nine permission
        // bits: masking them off is silent, and `ls -l` would show a plain
        // -rwxr-xr-x on a setuid binary.
        let mut md = small.metadata().unwrap();
        md.mode = 0o6755;
        // Two different times, each with its own nanoseconds, so that
        // neither a swap of the two nor a dropped `nsec` reads back equal.
        md.atime = Timespec {
            sec: 1_700_000_111,
            nsec: 222,
        };
        md.mtime = Timespec {
            sec: 1_700_000_333,
            nsec: 444,
        };
        small.set_metadata(&md).unwrap();
        let back = small.metadata().unwrap();
        assert_eq!(back.mode, 0o6755, "{:#o}", back.mode);
        assert_eq!(back.atime, md.atime);
        assert_eq!(back.mtime, md.mtime);
        assert_eq!(back.type_, FileType::File);

        // A hard link is a second name for one inode and the count says so:
        // a fixed 1 would have `rm` of either name free the file.
        assert_eq!(back.nlinks, 1);
        root.link("small2", &small).unwrap();
        assert_eq!(small.metadata().unwrap().nlinks, 2);
    }

    /// What `statfs(2)` says about the mount as a whole.
    #[test]
    fn statfs_counts_blocks_and_takes_its_name_limit_from_the_format() {
        let bytes = 8 * 1024 * 1024;
        let disk = formatted(bytes);
        let fs = EclipseMountFs::open(&MountBackend::Block(disk), false).unwrap();
        let info = fs.info();
        assert!(info.bsize >= 512, "{}", info.bsize);
        assert_eq!(info.frsize, info.bsize);
        // `blocks` is a count of `bsize` units, so the whole filesystem has
        // to fit on the device it is on. Bytes there tell `df` the disk is
        // `bsize` times bigger than it is.
        assert!(info.blocks > 0);
        assert!(
            info.blocks <= bytes / info.bsize,
            "{} blocks of {} on {} bytes",
            info.blocks,
            info.bsize,
            bytes
        );
        assert!(info.bfree <= info.blocks);
        assert_eq!(info.bavail, info.bfree);
        // The longest name is the on-disk format's, not a number of this
        // adapter's own: what `statfs` promises is what `create` honours.
        assert_eq!(info.namemax, eclipsefs::format::MAX_NAME_LEN);
        let root = fs.root_inode();
        let longest = "n".repeat(info.namemax);
        root.create(&longest, FileType::File, 0o644).unwrap();
        let longer = "n".repeat(info.namemax + 1);
        assert_eq!(
            root.create(&longer, FileType::File, 0o644).err(),
            Some(FsError::InvalidParam)
        );
    }

    /// The `-o` string as `mount(8)` hands it over.
    #[test]
    fn the_mount_options_are_read_the_way_they_are_typed() {
        // A user types spaces after the commas, and trimming is what makes
        // " degraded" the option instead of an unknown one. An unknown one
        // is dropped in silence, so the mount would quietly not be degraded.
        let o = EfsOptions::parse(" degraded , compress=lz4 , subvolid=7 ").unwrap();
        assert!(o.degraded, "a spaced option is still the option");
        assert_eq!(o.compress, Some(Some(Compression::Lz4)));
        assert_eq!(o.subvolid, Some(7));
        // `compress=none` is btrfs's own spelling of "no compression", and
        // it is not the same as an algorithm nobody knows, which is an
        // error: taking it for one refuses the mount outright.
        assert_eq!(
            EfsOptions::parse("compress=none").unwrap().compress,
            Some(None)
        );
        assert_eq!(
            EfsOptions::parse("compress=no").unwrap().compress,
            Some(None)
        );
        assert_eq!(
            EfsOptions::parse("compress=nope"),
            Err(FsError::InvalidParam)
        );
        // The level after a colon is btrfs's too: the algorithm is what is
        // in front of it.
        assert_eq!(
            EfsOptions::parse("compress=zstd:3").unwrap().compress,
            Some(Some(Compression::Zstd))
        );
        // `commit=0` means the default, not "never".
        assert_eq!(
            EfsOptions::parse("commit=0").unwrap().commit,
            Some(DEFAULT_COMMIT_SECS)
        );
        // The generic layer's options, and the empty segments a trailing
        // comma leaves, are not ours and not errors either.
        assert_eq!(EfsOptions::parse("").unwrap(), EfsOptions::default());
        assert_eq!(
            EfsOptions::parse("ro,,noatime,").unwrap(),
            EfsOptions::default()
        );
        // `device=` keeps the basename, because that is how the registry
        // names what the kernel found; an empty one is an error.
        assert_eq!(
            EfsOptions::parse("device=/dev/sdb1").unwrap().devices,
            ["sdb1"]
        );
        assert_eq!(EfsOptions::parse("device="), Err(FsError::InvalidParam));
    }

    /// Which directory the mount shows as `/`.
    #[test]
    fn the_subvolume_shown_as_root_is_the_one_the_options_asked_for() {
        // v0.3 has no subvolumes at all, so asking for one is refused and
        // not ignored: a mount that quietly showed the whole filesystem
        // where a subvolume was asked for would hand a container the lot.
        let v3 = formatted_with(8 * 1024 * 1024, eclipsefs::mkfs::format_v3);
        let byid = EfsOptions {
            subvolid: Some(256),
            ..EfsOptions::default()
        };
        assert_eq!(
            EclipseMountFs::open_with(&MountBackend::Block(v3.clone()), false, &byid).err(),
            Some(FsError::NotSupported)
        );
        let bypath = EfsOptions {
            subvol: Some(String::from("@home")),
            ..EfsOptions::default()
        };
        assert_eq!(
            EclipseMountFs::open_with(&MountBackend::Block(v3), false, &bypath).err(),
            Some(FsError::NotSupported)
        );
        // On v0.4 an id no subvolume has is an error too, and not the top of
        // the filesystem: mounting something else than what was asked for is
        // worse than not mounting.
        let disk = formatted(8 * 1024 * 1024);
        let bogus = EfsOptions {
            subvolid: Some(4096),
            ..EfsOptions::default()
        };
        assert!(
            EclipseMountFs::open_with(&MountBackend::Block(disk.clone()), false, &bogus).is_err()
        );
        // `subvol=` is a path from the top subvolume, so what is walked are
        // its own components: an empty one and a `.` name nothing to look
        // up, and `/./` is the top itself.
        let dot = EfsOptions {
            subvol: Some(String::from("/./")),
            ..EfsOptions::default()
        };
        let fs = EclipseMountFs::open_with(&MountBackend::Block(disk), false, &dot).unwrap();
        let root = fs.root_inode();
        assert_eq!(root.metadata().unwrap().type_, FileType::Dir);
        root.create("here", FileType::Dir, 0o755).unwrap();
        assert_eq!(root.get_entry(2).unwrap(), "here");
    }

    /// A mount that cannot do what it was asked says so.
    #[test]
    fn a_mount_refuses_what_it_cannot_do_instead_of_pretending() {
        let disk = formatted(8 * 1024 * 1024);
        // `repair` writes. On a read-only mount it is refused outright,
        // because a repair that silently did not run leaves the filesystem
        // damaged and the mount looking fine.
        let repair = EfsOptions {
            repair: true,
            ..EfsOptions::default()
        };
        assert_eq!(
            EclipseMountFs::open_with(&MountBackend::Block(disk.clone()), true, &repair).err(),
            Some(FsError::InvalidParam)
        );
        // Read-write it runs, and a filesystem it has nothing to fix mounts.
        EclipseMountFs::open_with(&MountBackend::Block(disk.clone()), false, &repair).unwrap();
        // `degraded` sends the mount down the volume path, and a whole
        // filesystem on one device goes through it fine ...
        let degraded = EfsOptions {
            degraded: true,
            ..EfsOptions::default()
        };
        EclipseMountFs::open_with(&MountBackend::Block(disk), false, &degraded).unwrap();
        // ... but v0.3 has no volumes at all, so that path cannot read it
        // and says so instead of quietly mounting it whole, which is what
        // taking the single-device path would do: `-o degraded` is a user
        // saying a device is missing, and a mount that answers it with the
        // filesystem as if nothing were missing is the answer to another
        // question.
        let v3 = formatted_with(8 * 1024 * 1024, eclipsefs::mkfs::format_v3);
        assert_eq!(
            EclipseMountFs::open_with(&MountBackend::Block(v3), false, &degraded).err(),
            Some(FsError::WrongFs)
        );
    }

    /// One name in the registry is one device.
    #[test]
    fn a_block_device_registered_twice_is_remembered_once() {
        // `volume_members` searches this registry by name, and a name comes
        // back into circulation when a disk is unplugged. Two entries under
        // one name hand the scan the disk that is gone, and forgetting the
        // name takes out only one of them.
        let old = formatted(1024 * 1024);
        let new = formatted(2 * 1024 * 1024);
        assert_ne!(old.block_count(), new.block_count());
        register_block_device("t33disk", old);
        register_block_device("t33disk", new.clone());
        let found: Vec<usize> = BLOCK_DEVICES
            .lock()
            .iter()
            .filter(|(n, _)| n == "t33disk")
            .map(|(_, b)| b.block_count())
            .collect();
        assert_eq!(found, [new.block_count()], "the newest one, once");
        forget_block_device("t33disk");
        assert!(!BLOCK_DEVICES.lock().iter().any(|(n, _)| n == "t33disk"));
    }

    /// A writer says it wants the lock before it waits for it.
    #[test]
    fn a_waiting_writer_is_counted_before_it_blocks_and_not_after() {
        extern crate std;
        let disk = formatted(4 * 1024 * 1024);
        let fs = EclipseMountFs::open(&MountBackend::Block(disk), false).unwrap();
        // Held shared, so the writer below has to wait for it.
        let held = fs.inner.shared().unwrap();
        let writer = fs.clone();
        let t = std::thread::spawn(move || drop(writer.inner.lock()));
        // Counting itself only after the lock is in hand would leave the
        // counter at zero for the whole wait, which is exactly when it is
        // read: readers would keep going in ahead of the writer and it
        // would spin for as long as they kept coming.
        let mut turned_away = false;
        for _ in 0..500 {
            if fs.inner.shared().is_none() {
                turned_away = true;
                break;
            }
            std::thread::sleep(core::time::Duration::from_millis(1));
        }
        assert!(turned_away, "a reader got in while a writer waited");
        drop(held);
        t.join().unwrap();
        assert!(fs.inner.shared().is_some(), "and gives way once it is done");
    }
}
