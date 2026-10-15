//! `eclipsefs-fuse`: mount an EclipseFS image or block device on Linux
//! through FUSE.
//!
//! It exists so EclipseFS can be benchmarked against the filesystems of the
//! Linux kernel (ext4, xfs, btrfs) with the same tool (fio) and through the
//! same VFS: `scripts/eclipsefs-linux-bench.sh` uses it. It is the same
//! driver the Eclipse kernel uses (`linux-object/src/fs/eclipsefs_mount.rs`),
//! with the same commit points: `fsync` and unmount commit the transaction,
//! and a timer commits every `--commit` seconds (5, as ext4).
//!
//! ```text
//! eclipsefs-fuse [--mkfs] [--commit SECS] [--no-writeback-cache] DEVICE MOUNTPOINT
//! ```
//!
//! DEVICE can be an image file or a block device (`/dev/loopN`), opened
//! with `pread`/`pwrite` and no cache of its own.

use eclipsefs::{BlockDevice, EclipseFS, Error, FileKind, InodeStat};
use fuser::{
    FileAttr, FileType, Filesystem, KernelConfig, MountOption, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The kernel may cache attributes and names this long. Only this process
/// changes the filesystem, so every change goes through the kernel and its
/// caches stay right; ext4 has no such timeout at all.
const TTL: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Device: pread/pwrite on a file or block device
// ---------------------------------------------------------------------------

struct Device {
    file: File,
    size: u64,
}

impl Device {
    fn open(path: &Path) -> std::io::Result<Self> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        // `metadata().len()` is 0 for a block device; seeking to the end is
        // right for both.
        let size = file.seek(SeekFrom::End(0))?;
        Ok(Self { file, size })
    }
}

impl BlockDevice for Device {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> eclipsefs::Result<()> {
        self.file.read_exact_at(buf, offset).map_err(|_| Error::Io)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> eclipsefs::Result<()> {
        self.file.write_all_at(buf, offset).map_err(|_| Error::Io)
    }
    fn sync(&self) -> eclipsefs::Result<()> {
        self.file.sync_data().map_err(|_| Error::Io)
    }
    fn size(&self) -> u64 {
        self.size
    }
}

fn now() -> (u64, u32) {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (d.as_secs(), d.subsec_nanos())
}

// ---------------------------------------------------------------------------
// Translation between EclipseFS and FUSE
// ---------------------------------------------------------------------------

fn errno(e: Error) -> i32 {
    match e {
        Error::Io | Error::Corrupt(_) => libc::EIO,
        Error::BadSuperblock => libc::EINVAL,
        Error::Unsupported(_) => libc::EOPNOTSUPP,
        Error::NotFound => libc::ENOENT,
        Error::Exists => libc::EEXIST,
        Error::NotDir => libc::ENOTDIR,
        Error::IsDir => libc::EISDIR,
        Error::NotEmpty => libc::ENOTEMPTY,
        Error::NoSpace => libc::ENOSPC,
        Error::Invalid => libc::EINVAL,
        Error::NoKey => libc::ENOKEY,
    }
}

fn file_type(kind: FileKind) -> FileType {
    match kind {
        FileKind::Regular => FileType::RegularFile,
        FileKind::Dir => FileType::Directory,
        FileKind::Symlink => FileType::Symlink,
        FileKind::CharDevice => FileType::CharDevice,
        FileKind::BlockDevice => FileType::BlockDevice,
        FileKind::Fifo => FileType::NamedPipe,
        FileKind::Socket => FileType::Socket,
    }
}

fn time((secs, nsecs): (u64, u32)) -> SystemTime {
    UNIX_EPOCH + Duration::new(secs, nsecs)
}

fn to_time(t: TimeOrNow) -> (u64, u32) {
    match t {
        TimeOrNow::Now => now(),
        TimeOrNow::SpecificTime(t) => {
            let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
            (d.as_secs(), d.subsec_nanos())
        }
    }
}

struct Fuse {
    fs: Arc<Mutex<EclipseFS>>,
    root: u64,
    writeback_cache: bool,
    /// Inodes unlinked while the kernel still knows them (until `forget`),
    /// with their last attributes. EclipseFS frees an inode with its last
    /// link, but the kernel may still flush timestamps of it (GNU tar
    /// replaces its placeholders this way); ext4 would keep it as an orphan.
    gone: HashMap<u64, FileAttr>,
}

impl Fuse {
    /// FUSE calls the root inode 1; EclipseFS has its own number for it.
    fn ino(&self, fuse_ino: u64) -> u64 {
        if fuse_ino == fuser::FUSE_ROOT_ID {
            self.root
        } else {
            fuse_ino
        }
    }

    fn fuse_ino(&self, ino: u64) -> u64 {
        if ino == self.root {
            fuser::FUSE_ROOT_ID
        } else {
            ino
        }
    }

    fn attr(&self, st: &InodeStat) -> FileAttr {
        FileAttr {
            ino: self.fuse_ino(st.ino),
            size: st.size,
            blocks: st.nbytes.div_ceil(512),
            atime: time(st.atime),
            mtime: time(st.mtime),
            ctime: time(st.ctime),
            crtime: time(st.ctime),
            kind: file_type(st.kind),
            perm: (st.mode & 0o7777) as u16,
            nlink: st.nlink,
            uid: st.uid,
            gid: st.gid,
            rdev: st.rdev as u32,
            blksize: 4096,
            flags: 0,
        }
    }

    fn stat(&self, fs: &mut EclipseFS, ino: u64) -> Result<FileAttr, i32> {
        fs.stat(ino).map(|st| self.attr(&st)).map_err(errno)
    }

    /// Create `name` in `parent` owned by the caller, and return its
    /// attributes.
    fn make(
        &self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        kind: FileKind,
        mode: u32,
        rdev: u64,
    ) -> Result<FileAttr, i32> {
        let name = name.to_str().ok_or(libc::EINVAL)?;
        let mut fs = self.fs.lock().unwrap();
        let ino = fs
            .create(self.ino(parent), name, kind, mode & 0o7777, rdev)
            .map_err(errno)?;
        fs.set_attr(ino, None, Some(req.uid()), Some(req.gid()), None, None)
            .map_err(errno)?;
        self.stat(&mut fs, ino)
    }
}

impl Filesystem for Fuse {
    fn init(&mut self, _req: &Request<'_>, config: &mut KernelConfig) -> Result<(), i32> {
        // Same request sizes a block filesystem sees from the page cache.
        let _ = config.set_max_write(1 << 20);
        let _ = config.set_max_readahead(1 << 20);
        if self.writeback_cache {
            // Buffered writes stay in the page cache and reach the driver
            // in large pieces, as with ext4/xfs/btrfs.
            let _ = config.add_capabilities(fuser::consts::FUSE_WRITEBACK_CACHE);
        }
        Ok(())
    }

    fn destroy(&mut self) {
        if let Err(e) = self.fs.lock().unwrap().sync() {
            eprintln!("eclipsefs-fuse: final commit failed: {:?}", e);
        }
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            return reply.error(libc::ENOENT);
        };
        let mut fs = self.fs.lock().unwrap();
        match fs
            .lookup(self.ino(parent), name)
            .map_err(errno)
            .and_then(|ino| self.stat(&mut fs, ino))
        {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(e) => reply.error(e),
        }
    }

    fn forget(&mut self, _req: &Request<'_>, ino: u64, _nlookup: u64) {
        self.gone.remove(&ino);
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let mut fs = self.fs.lock().unwrap();
        match self.stat(&mut fs, self.ino(ino)) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(libc::ENOENT) if self.gone.contains_key(&ino) => reply.attr(&TTL, &self.gone[&ino]),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        if let Some(attr) = self.gone.get_mut(&ino) {
            // Only the kernel's view of it is left.
            if let Some(m) = mode {
                attr.perm = (m & 0o7777) as u16;
            }
            if let Some(t) = mtime {
                attr.mtime = time(to_time(t));
            }
            if let Some(t) = atime {
                attr.atime = time(to_time(t));
            }
            return reply.attr(&TTL, attr);
        }
        let ino = self.ino(ino);
        let mut fs = self.fs.lock().unwrap();
        let r = (|| {
            if let Some(size) = size {
                fs.truncate(ino, size)?;
            }
            if mode.is_some()
                || uid.is_some()
                || gid.is_some()
                || atime.is_some()
                || mtime.is_some()
            {
                fs.set_attr(
                    ino,
                    mode.map(|m| m & 0o7777),
                    uid,
                    gid,
                    atime.map(to_time),
                    mtime.map(to_time),
                )?;
            }
            Ok(())
        })()
        .map_err(errno)
        .and_then(|()| self.stat(&mut fs, ino));
        match r {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        match self.fs.lock().unwrap().read_link(self.ino(ino)) {
            Ok(target) => reply.data(&target),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn mknod(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        let kind = FileKind::from_mode(mode);
        match self.make(req, parent, name, kind, mode & !umask, rdev as u64) {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        match self.make(req, parent, name, FileKind::Dir, mode & !umask, 0) {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = name.to_str() else {
            return reply.error(libc::ENOENT);
        };
        let dir = self.ino(parent);
        let mut fs = self.fs.lock().unwrap();
        let before = fs.lookup(dir, name).and_then(|ino| fs.stat(ino));
        match fs.unlink(dir, name) {
            Ok(()) => {
                if let Ok(st) = before {
                    if st.nlink <= 1 || st.kind == FileKind::Dir {
                        let mut attr = self.attr(&st);
                        attr.nlink = 0;
                        self.gone.insert(attr.ino, attr);
                    }
                }
                reply.ok()
            }
            Err(e) => reply.error(errno(e)),
        }
    }

    fn rmdir(&mut self, req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        // EclipseFS removes empty directories with `unlink`, as the kernel
        // adapter does.
        self.unlink(req, parent, name, reply)
    }

    fn symlink(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        use std::os::unix::ffi::OsStrExt;
        let Some(name) = link_name.to_str() else {
            return reply.error(libc::EINVAL);
        };
        let mut fs = self.fs.lock().unwrap();
        let r = fs
            .symlink(self.ino(parent), name, target.as_os_str().as_bytes())
            .and_then(|ino| {
                fs.set_attr(ino, None, Some(req.uid()), Some(req.gid()), None, None)?;
                Ok(ino)
            })
            .map_err(errno)
            .and_then(|ino| self.stat(&mut fs, ino));
        match r {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(e) => reply.error(e),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        if flags != 0 {
            return reply.error(libc::EINVAL);
        }
        let (Some(name), Some(newname)) = (name.to_str(), newname.to_str()) else {
            return reply.error(libc::EINVAL);
        };
        let (from, to) = (self.ino(parent), self.ino(newparent));
        match self.fs.lock().unwrap().rename(from, name, to, newname) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn link(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        newparent: u64,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let Some(newname) = newname.to_str() else {
            return reply.error(libc::EINVAL);
        };
        let ino = self.ino(ino);
        let mut fs = self.fs.lock().unwrap();
        let r = fs
            .link(self.ino(newparent), newname, ino)
            .map_err(errno)
            .and_then(|()| self.stat(&mut fs, ino));
        match r {
            Ok(attr) => reply.entry(&TTL, &attr, 0),
            Err(e) => reply.error(e),
        }
    }

    fn open(&mut self, _req: &Request<'_>, _ino: u64, _flags: i32, reply: ReplyOpen) {
        // Keep the page cache across opens, as a kernel filesystem does.
        reply.opened(0, fuser::consts::FOPEN_KEEP_CACHE);
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let mut buf = vec![0u8; size as usize];
        match self
            .fs
            .lock()
            .unwrap()
            .read(self.ino(ino), offset as u64, &mut buf)
        {
            Ok(n) => reply.data(&buf[..n]),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        match self
            .fs
            .lock()
            .unwrap()
            .write(self.ino(ino), offset as u64, data)
        {
            Ok(n) => reply.written(n as u32),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn flush(&mut self, _req: &Request<'_>, _ino: u64, _fh: u64, _lock: u64, reply: ReplyEmpty) {
        // close() does not commit on ext4/xfs/btrfs either.
        reply.ok();
    }

    fn fsync(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        // EclipseFS commits the whole transaction, as btrfs without a log
        // tree would.
        match self.fs.lock().unwrap().sync() {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn fsyncdir(
        &mut self,
        req: &Request<'_>,
        ino: u64,
        fh: u64,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.fsync(req, ino, fh, datasync, reply)
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let dir = self.ino(ino);
        let mut fs = self.fs.lock().unwrap();
        let entries = match fs.readdir(dir) {
            Ok(entries) => entries,
            Err(e) => return reply.error(errno(e)),
        };
        let parent = fs.parent(dir).unwrap_or(dir);
        let dots = [
            (dir, FileType::Directory, ".".to_string()),
            (parent, FileType::Directory, "..".to_string()),
        ];
        let all = dots.into_iter().chain(
            entries
                .into_iter()
                .filter(|e| e.name != "." && e.name != "..")
                .map(|e| (e.ino, file_type(e.kind), e.name)),
        );
        for (i, (ino, kind, name)) in all.enumerate().skip(offset as usize) {
            if reply.add(self.fuse_ino(ino), i as i64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        let st = self.fs.lock().unwrap().fsinfo();
        let bs = st.block_size.max(1);
        let blocks = st.total_bytes / bs;
        let free = st.total_bytes.saturating_sub(st.bytes_used) / bs;
        reply.statfs(blocks, free, free, 0, 0, bs as u32, 255, bs as u32);
    }

    fn create(
        &mut self,
        req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        match self.make(req, parent, name, FileKind::Regular, mode & !umask, 0) {
            Ok(attr) => reply.created(&TTL, &attr, 0, 0, fuser::consts::FOPEN_KEEP_CACHE),
            Err(e) => reply.error(e),
        }
    }

    fn fallocate(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        length: i64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        match self.fs.lock().unwrap().fallocate(
            self.ino(ino),
            mode as u32,
            offset as u64,
            length as u64,
        ) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(e)),
        }
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn usage() -> ! {
    eprintln!(
        "usage: eclipsefs-fuse [--mkfs] [--commit SECS] [--no-writeback-cache] DEVICE MOUNTPOINT"
    );
    std::process::exit(2);
}

fn main() {
    let mut mkfs = false;
    let mut commit = 5u64;
    let mut writeback_cache = true;
    let mut paths = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--mkfs" => mkfs = true,
            "--no-writeback-cache" => writeback_cache = false,
            "--commit" => {
                commit = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| usage())
            }
            _ if a.starts_with('-') => usage(),
            _ => paths.push(a),
        }
    }
    let [device, mountpoint] = <[String; 2]>::try_from(paths).unwrap_or_else(|_| usage());

    let dev: Arc<dyn BlockDevice> =
        Arc::new(Device::open(Path::new(&device)).unwrap_or_else(|e| {
            eprintln!("eclipsefs-fuse: {}: {}", device, e);
            std::process::exit(1);
        }));
    if mkfs {
        let mut id = [0u8; 16];
        let (s, n) = now();
        id[..8].copy_from_slice(&s.to_le_bytes());
        id[8..12].copy_from_slice(&n.to_le_bytes());
        id[12..].copy_from_slice(&std::process::id().to_le_bytes());
        let uuid = |k: u8| {
            let mut u = id;
            u[15] ^= k;
            u
        };
        eclipsefs::mkfs::format(
            &dev,
            &eclipsefs::mkfs::MkfsOptions {
                label: "eclipsefs".into(),
                fsid: uuid(1),
                chunk_uuid: uuid(2),
                dev_uuid: uuid(3),
                subvol_uuid: uuid(4),
                now: now(),
            },
        )
        .unwrap_or_else(|e| {
            eprintln!("eclipsefs-fuse: mkfs: {:?}", e);
            std::process::exit(1);
        });
    }

    let mut fs = EclipseFS::mount(dev, false).unwrap_or_else(|e| {
        eprintln!("eclipsefs-fuse: mount {}: {:?}", device, e);
        std::process::exit(1);
    });
    fs.set_clock(now);
    let root = fs.root_ino();
    let fs = Arc::new(Mutex::new(fs));

    if commit > 0 {
        let fs = Arc::downgrade(&fs);
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(commit));
            let Some(fs) = fs.upgrade() else { break };
            let r = fs.lock().unwrap().sync();
            if let Err(e) = r {
                eprintln!("eclipsefs-fuse: periodic commit failed: {:?}", e);
            }
        });
    }

    let options = [
        MountOption::FSName(device.clone()),
        MountOption::Subtype("eclipsefs".into()),
        MountOption::AllowOther,
        MountOption::DefaultPermissions,
        MountOption::RW,
    ];
    let fuse = Fuse {
        fs,
        root,
        writeback_cache,
        gone: HashMap::new(),
    };
    if let Err(e) = fuser::mount2(fuse, &mountpoint, &options) {
        eprintln!("eclipsefs-fuse: {}: {}", mountpoint, e);
        std::process::exit(1);
    }
}
