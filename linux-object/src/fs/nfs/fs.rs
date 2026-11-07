//! `NfsFileSystem` and `NfsInode`: the session seen as a [`FileSystem`] and
//! its [`INode`]s.
//!
//! Everything below this is NFS. This is where NFS stops and the kernel's own
//! filesystem interface starts, and the two do not line up in four places —
//! each of which is a bug that a working-looking mount would hide.
//!
//! # A directory is a chain of cookies, and `get_entry` asks for the Nth
//!
//! [`INode::get_entry`] takes an index, and [`INode::list`] calls it with 0,
//! 1, 2, … until it errors. NFSv3 has no index: `READDIR` hands back entries
//! each carrying an opaque **cookie**, and the only way to the entry after
//! one is to ask with its cookie. There is no way to ask for the fourth.
//!
//! Listing again from the start for every index would be O(n²) calls, which
//! is merely slow. The part that is *wrong* is that the server's directory
//! moves underneath: between the call for index 3 and the call for index 4 a
//! file can appear or go, and then the entry at index 4 is one that was at 3
//! — so a `readdir` loop **skips or repeats names**, and nothing anywhere
//! reports an error. So a listing is read **once, whole**, and kept; the
//! indices a caller then asks for are indices into that one answer.
//!
//! # The attribute cache, and why it is not optional
//!
//! Every `stat` is a `GETATTR` round trip. `ls -l` of a hundred files is a
//! hundred `LOOKUP`s and a hundred `GETATTR`s, and the walk that resolves
//! each path asks for the metadata of every directory on the way — so a
//! mount without a cache is not slow, it is unusable. With one, the question
//! becomes how long an answer is allowed to be wrong, which is a mount
//! option in Linux (`acregmin`/`acregmax`, `acdirmin`/`acdirmax`) and a
//! single timeout here until there is a reason for four.
//!
//! What makes a short timeout honest rather than a guess: NFSv3 sends
//! attributes back **with the reply to the call that changed them**
//! (`post_op_attr`, `wcc_data`), which is what the write path collected them
//! for. So this client's own changes never wait for a timeout — they refresh
//! the cache as they happen — and the timeout only covers changes made by
//! *somebody else*, which is the one case where no protocol can do better.
//!
//! # A filehandle is the identity, and the inode number is not
//!
//! An inode here is a handle, and two handles are the same file only if the
//! server says so. The `fileid` the server reports goes into
//! [`Metadata::inode`] because that is what userspace compares, but it is
//! **the server's** number: it is unique within one filesystem on one server
//! and nothing more. Inventing a local number instead would break every
//! program that compares `st_ino` across a `stat` and a `readdir`.
//!
//! # Reading a symlink is a `READLINK`, not a read
//!
//! The VFS resolves a symlink by *reading* it (`read_symlink`, which calls
//! [`INode::read_at`]). A symlink has no data on an NFS server — its target
//! comes from a `READLINK` call — so `read_at` on one answers with the target
//! rather than with an error or with nothing, or every symlink on the mount
//! resolves to the empty path.

use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;

use rcore_fs::vfs::{
    FileSystem, FileType, FsError, FsInfo, INode, Metadata, PollStatus, Result as VfsResult,
    Timespec,
};
use spin::Mutex;

use crate::error::LxError;

use super::client::RpcTransport;
use super::mount3::FileHandle;
use super::nfs3::{Fattr3, Ftype3, PostOpAttr};
use super::nfs3_dir::DirEntry;
use super::nfs3_write::{CreateHow, NodeKind, SetAttr, StableHow};
use super::session::NfsSession;

/// How long an attribute answer is trusted, in milliseconds.
///
/// Linux's `acregmin` is 3 s and `acdirmin` 30 s, with the attribute cache
/// extended up to `acregmax`/`acdirmax` for a file that is not changing. One
/// number here, at the short end of that range: a long timeout is only a win
/// for a mount nobody else writes to, and being wrong about somebody else's
/// change is the failure that is hard to understand from the outside.
pub const ATTR_TTL_MS: u64 = 3_000;

/// How long a directory listing is trusted, in milliseconds.
///
/// Longer than [`ATTR_TTL_MS`] for the reason Linux's `acdirmin` is longer:
/// a listing costs far more to fetch than one attribute, and a directory
/// changing under a reader is less often what a program depends on. Still
/// seconds and not minutes, because `ls` after somebody else's `touch` has
/// to show the file.
pub const DIR_TTL_MS: u64 = 10_000;

/// Linux's `statfs.f_type` for NFS.
///
/// Userspace reads this to decide what it is standing on: `df -T` prints the
/// type from it, and the "is this a network filesystem" checks in build
/// systems and lock libraries key off exactly this value. A wrong magic makes
/// a real NFS mount look like something it is not.
pub const NFS_SUPER_MAGIC: u64 = 0x6969;

/// The errno for an `LxError` the kernel's filesystem interface has to
/// understand.
///
/// The one that has to be right is **`ESTALE`**: a handle that no longer
/// names anything is an ordinary outcome on a network filesystem, and the
/// caller's answer to it is to look the name up again. Reported as a device
/// error it reads as a broken disk, and a program that would have re-opened
/// gives up on the mount instead. See [`FsError::StaleHandle`].
pub fn fs_error_from_lx(e: LxError) -> FsError {
    match e {
        LxError::ESTALE => FsError::StaleHandle,
        LxError::ENOENT => FsError::EntryNotFound,
        LxError::EEXIST => FsError::EntryExist,
        LxError::ENOTDIR => FsError::NotDir,
        LxError::EISDIR => FsError::IsDir,
        LxError::ENOTEMPTY => FsError::DirNotEmpty,
        LxError::EACCES => FsError::NoPermission,
        LxError::EPERM => FsError::NotPermitted,
        LxError::EROFS => FsError::ReadOnly,
        LxError::ENOSPC | LxError::EDQUOT => FsError::NoDeviceSpace,
        LxError::ENOMEM => FsError::NoMemory,
        LxError::ENAMETOOLONG => FsError::NameTooLong,
        LxError::EINVAL => FsError::InvalidParam,
        LxError::EOPNOTSUPP => FsError::OpNotSupported,
        LxError::ENOSYS => FsError::NotSupported,
        LxError::EAGAIN => FsError::Again,
        LxError::ETIMEDOUT => FsError::TimedOut,
        LxError::EXDEV => FsError::NotSameFs,
        LxError::ELOOP => FsError::SymLoop,
        LxError::EINTR => FsError::Interrupted,
        LxError::EBUSY => FsError::Busy,
        // `ENOTCONN` and the rest of the transport's errors: the call did not
        // happen and the caller cannot do anything more specific about it.
        _ => FsError::DeviceError,
    }
}

/// Milliseconds from some fixed point, for the caches' timeouts.
fn now_ms() -> u64 {
    kernel_hal::timer::timer_now().as_millis() as u64
}

/// Something read from the server, with when it was read.
#[derive(Debug, Clone)]
struct Cached<V> {
    value: V,
    at: u64,
}

impl<V> Cached<V> {
    fn new(value: V) -> Self {
        Self {
            value,
            at: now_ms(),
        }
    }

    /// Whether this is still inside `ttl`.
    ///
    /// A clock that goes backwards makes this stale rather than eternal:
    /// `saturating_sub` of a later `at` is zero, so the entry is refetched.
    /// The other way round -- treating it as fresh -- is a cache that never
    /// expires once the clock steps back, which it does here, on several
    /// CPUs.
    fn fresh(&self, ttl: u64) -> bool {
        let now = now_ms();
        now >= self.at && now - self.at < ttl
    }
}

/// One mounted export.
pub struct NfsFileSystem<T: RpcTransport + Send + 'static> {
    /// The conversation with the server, behind one lock.
    ///
    /// One call at a time per mount. Linux has a slot table and several
    /// calls in flight; this is one connection and one `RpcClient`, and the
    /// client's transaction ids only make sense against one caller at a
    /// time. It is the honest limit of this batch, not a design: the lock is
    /// held across a round trip, so a slow server serialises the mount's
    /// readers. What it is *not* is wrong: every call completes or fails on
    /// its own.
    session: Mutex<NfsSession<T>>,
    /// The export's root handle.
    root: FileHandle,
    /// Himself, for [`INode::fs`], which has to hand back an `Arc`.
    me: Mutex<Weak<Self>>,
    /// How long attributes are trusted.
    attr_ttl: u64,
    /// How long listings are trusted.
    dir_ttl: u64,
}

impl<T: RpcTransport + Send + 'static> NfsFileSystem<T> {
    /// Mount `session`, whose root handle is `root`.
    ///
    /// An `Arc` and not a value: the inodes hand their filesystem back out,
    /// so it has to be shared from the start.
    pub fn new(session: NfsSession<T>, root: FileHandle) -> Arc<Self> {
        Self::with_ttls(session, root, ATTR_TTL_MS, DIR_TTL_MS)
    }

    /// The same, with the two cache timeouts given. For a test that has to
    /// see a refetch without waiting seconds for it, and for a mount that
    /// asks for other numbers.
    pub fn with_ttls(
        session: NfsSession<T>,
        root: FileHandle,
        attr_ttl: u64,
        dir_ttl: u64,
    ) -> Arc<Self> {
        let fs = Arc::new(Self {
            session: Mutex::new(session),
            root,
            me: Mutex::new(Weak::new()),
            attr_ttl,
            dir_ttl,
        });
        *fs.me.lock() = Arc::downgrade(&fs);
        fs
    }

    fn arc(&self) -> Arc<Self> {
        self.me
            .lock()
            .upgrade()
            .expect("the filesystem outlives its inodes")
    }

    /// An inode for `handle`, with `attr` if it came with it.
    fn inode(self: &Arc<Self>, handle: FileHandle, attr: PostOpAttr) -> Arc<NfsInode<T>> {
        Arc::new(NfsInode {
            fs: Arc::clone(self),
            handle,
            attr: Mutex::new(attr.map(Cached::new)),
            listing: Mutex::new(None),
        })
    }

    /// The session, for a caller that has to ask it something directly.
    pub fn session(&self) -> &Mutex<NfsSession<T>> {
        &self.session
    }
}

impl<T: RpcTransport + Send + 'static> FileSystem for NfsFileSystem<T> {
    /// `COMMIT` has nothing to commit without a file to commit it on, and
    /// NFSv3 has no whole-filesystem sync: `sync(2)` on a mount is a no-op
    /// on the wire, exactly as it is in Linux's NFS client, where the work
    /// is done per file by `nfs_file_fsync`.
    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }

    fn root_inode(&self) -> Arc<dyn INode> {
        let fs = self.arc();
        let root = self.root.clone();
        fs.inode(root, None)
    }

    /// What `statfs(2)` reports, from `FSSTAT`.
    ///
    /// The server answers in **bytes**, and `statfs` wants blocks, so the
    /// block size is picked first and the counts divided by it. Reporting
    /// bytes as blocks is a filesystem that claims to be four thousand times
    /// its size, and `df` prints that.
    fn info(&self) -> FsInfo {
        let bsize = {
            let session = self.session.lock();
            session
                .fsinfo()
                .map(|i| i.rtpref.max(1) as usize)
                .unwrap_or(4096)
        };
        let stat = {
            let mut session = self.session.lock();
            let root = self.root.clone();
            session.fsstat(&root)
        };
        match stat {
            Ok(s) => FsInfo {
                bsize,
                frsize: bsize,
                blocks: (s.tbytes / bsize as u64) as usize,
                bfree: (s.fbytes / bsize as u64) as usize,
                bavail: (s.abytes / bsize as u64) as usize,
                files: s.tfiles as usize,
                ffree: s.ffiles as usize,
                namemax: 255,
                ftype: NFS_SUPER_MAGIC,
            },
            // A server that will not answer `FSSTAT` is not a reason to fail
            // `statfs`: the mount works, and zeros are what Linux reports
            // for a filesystem that does not know.
            Err(_) => FsInfo {
                bsize,
                frsize: bsize,
                blocks: 0,
                bfree: 0,
                bavail: 0,
                files: 0,
                ffree: 0,
                namemax: 255,
                ftype: NFS_SUPER_MAGIC,
            },
        }
    }
}

/// One file, directory or link on the server: a handle and what is known
/// about it.
pub struct NfsInode<T: RpcTransport + Send + 'static> {
    fs: Arc<NfsFileSystem<T>>,
    /// The handle, which is this inode's identity. Never changes: a handle
    /// that stops naming anything becomes `ESTALE` and the caller looks the
    /// name up again, which makes a *new* inode.
    handle: FileHandle,
    /// The last attributes seen, and when.
    attr: Mutex<Option<Cached<Fattr3>>>,
    /// The last whole listing seen, and when. See the module docs for why it
    /// is whole.
    listing: Mutex<Option<Cached<Vec<DirEntry>>>>,
}

impl<T: RpcTransport + Send + 'static> NfsInode<T> {
    /// This inode's handle.
    pub fn handle(&self) -> &FileHandle {
        &self.handle
    }

    /// The attributes, from the cache while they are fresh.
    fn attrs(&self) -> VfsResult<Fattr3> {
        if let Some(cached) = self.attr.lock().as_ref() {
            if cached.fresh(self.fs.attr_ttl) {
                return Ok(cached.value);
            }
        }
        let fresh = {
            let mut session = self.fs.session.lock();
            session.getattr(&self.handle).map_err(fs_error_from_lx)?
        };
        *self.attr.lock() = Some(Cached::new(fresh));
        Ok(fresh)
    }

    /// Note attributes that came back with a call of this client's own.
    ///
    /// This is what keeps the timeout from covering changes *this* client
    /// made: a `WRITE` answers with the file's new attributes, so the size a
    /// `stat` reports right after a write is the size the write produced,
    /// not the one the cache was holding.
    fn note_attrs(&self, attr: &PostOpAttr) {
        if let Some(a) = attr {
            *self.attr.lock() = Some(Cached::new(*a));
        }
    }

    /// Throw the cached attributes away, for a change whose result the server
    /// did not send back.
    fn forget_attrs(&self) {
        *self.attr.lock() = None;
    }

    /// Throw the cached listing away: something was created or removed in
    /// this directory.
    fn forget_listing(&self) {
        *self.listing.lock() = None;
    }

    /// The whole listing, from the cache while it is fresh.
    fn entries(&self) -> VfsResult<Vec<DirEntry>> {
        if let Some(cached) = self.listing.lock().as_ref() {
            if cached.fresh(self.fs.dir_ttl) {
                return Ok(cached.value.clone());
            }
        }
        let fresh = {
            let mut session = self.fs.session.lock();
            // `READDIRPLUS`: one call brings the names *and* the attributes
            // and handles, which is what a `ls -l` needs next. Asking for
            // names alone turns one call into one `LOOKUP` and one `GETATTR`
            // per entry.
            session
                .readdir_all(&self.handle, true)
                .map_err(fs_error_from_lx)?
        };
        *self.listing.lock() = Some(Cached::new(fresh.clone()));
        Ok(fresh)
    }

    /// Whether this is a directory, from the cache if it can be.
    fn is_dir(&self) -> VfsResult<bool> {
        Ok(self.attrs()?.ftype == Ftype3::Dir)
    }

    fn refuse_if_not_dir(&self) -> VfsResult<()> {
        if self.is_dir()? {
            Ok(())
        } else {
            Err(FsError::NotDir)
        }
    }
}

/// The kernel's file type for an NFSv3 one.
pub fn file_type_of(ftype: Ftype3) -> FileType {
    match ftype {
        Ftype3::Reg => FileType::File,
        Ftype3::Dir => FileType::Dir,
        Ftype3::Lnk => FileType::SymLink,
        Ftype3::Chr => FileType::CharDevice,
        Ftype3::Blk => FileType::BlockDevice,
        Ftype3::Sock => FileType::Socket,
        Ftype3::Fifo => FileType::NamedPipe,
    }
}

/// The NFSv3 type for a kernel file type, for the calls that create one.
fn ftype_of(type_: FileType) -> Option<Ftype3> {
    match type_ {
        FileType::File => Some(Ftype3::Reg),
        FileType::Dir => Some(Ftype3::Dir),
        FileType::SymLink => Some(Ftype3::Lnk),
        FileType::CharDevice => Some(Ftype3::Chr),
        FileType::BlockDevice => Some(Ftype3::Blk),
        FileType::Socket => Some(Ftype3::Sock),
        FileType::NamedPipe => Some(Ftype3::Fifo),
    }
}

/// The metadata an `fattr3` describes.
///
/// `mode` is the permission bits **only**: NFSv3 keeps the type in its own
/// field, and the kernel's `Metadata` does too, so the two never have to be
/// packed together here. Putting the type bits into `mode` as well is how a
/// directory comes out of `stat` as a file with a strange mode.
pub fn metadata_of(attr: &Fattr3, dev: usize) -> Metadata {
    Metadata {
        dev,
        inode: attr.fileid as usize,
        size: attr.size as usize,
        blk_size: 4096,
        // Rounded **up**: `du` of a one-byte file reports one block, and
        // `used / 512` rounded down reports zero blocks for every file
        // smaller than a sector.
        blocks: attr.used.div_ceil(512) as usize,
        atime: Timespec {
            sec: attr.atime.sec,
            nsec: attr.atime.nsec,
        },
        mtime: Timespec {
            sec: attr.mtime.sec,
            nsec: attr.mtime.nsec,
        },
        ctime: Timespec {
            sec: attr.ctime.sec,
            nsec: attr.ctime.nsec,
        },
        type_: file_type_of(attr.ftype),
        mode: attr.mode as u16,
        nlinks: attr.nlink as usize,
        uid: attr.uid as usize,
        gid: attr.gid as usize,
        rdev: ((attr.rdev.0 as usize) << 8) | (attr.rdev.1 as usize),
    }
}

impl<T: RpcTransport + Send + 'static> INode for NfsInode<T> {
    /// Read at an offset.
    ///
    /// A **symlink** answers with its target, because that is how the VFS
    /// reads one (see the module docs). Anything else goes to `READ`, whose
    /// loop -- a short read is not the end of the file -- belongs to the
    /// session.
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> VfsResult<usize> {
        if self.attrs()?.ftype == Ftype3::Lnk {
            let target = {
                let mut session = self.fs.session.lock();
                session.readlink(&self.handle).map_err(fs_error_from_lx)?
            };
            let bytes = target.as_bytes();
            // From the offset, so a caller reading a link in pieces is not
            // handed the whole target twice.
            if offset >= bytes.len() {
                return Ok(0);
            }
            let n = (bytes.len() - offset).min(buf.len());
            buf[..n].copy_from_slice(&bytes[offset..offset + n]);
            return Ok(n);
        }
        let mut session = self.fs.session.lock();
        session
            .read_into(&self.handle, offset as u64, buf)
            .map_err(fs_error_from_lx)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> VfsResult<usize> {
        {
            let mut session = self.fs.session.lock();
            session
                .write_all(&self.handle, offset as u64, buf, StableHow::Unstable)
                .map_err(fs_error_from_lx)?;
        }
        // The cached attributes are wrong now, and dropping them is not
        // merely tidy: `write_all` answers with the commitment level and the
        // verifier, not with the file's new attributes, so the size this
        // write produced is not known here. Keeping the old one would make a
        // program that writes and then checks the size -- every archiver --
        // read the file short for the whole timeout.
        self.forget_attrs();
        Ok(buf.len())
    }

    /// A file on a server is always ready: there is nothing to wait for and
    /// nothing to be notified about. The same answer Linux gives for a
    /// regular file, which is what `poll` on one means.
    fn poll(&self) -> VfsResult<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: !self.fs.session.lock().is_read_only(),
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        let attr = self.attrs()?;
        // The `dev` is the server's `fsid`, which is what makes two exports
        // from one server two filesystems to userspace -- and what `find
        // -xdev` and `du -x` stop at.
        Ok(metadata_of(&attr, attr.fsid as usize))
    }

    fn set_metadata(&self, metadata: &Metadata) -> VfsResult<()> {
        let attr = SetAttr {
            mode: Some(u32::from(metadata.mode) & 0o7777),
            uid: Some(metadata.uid as u32),
            gid: Some(metadata.gid as u32),
            // Not the size: a `SETATTR` with a size is a truncate, and
            // `set_metadata` is called with the metadata a caller read and
            // changed one field of. Truncating because somebody set the mode
            // would destroy the file. `resize` is the operation for that.
            size: None,
            atime: Some(super::nfs3_write::SetTime::Client(metadata.atime)),
            mtime: Some(super::nfs3_write::SetTime::Client(metadata.mtime)),
        };
        let reply = {
            let mut session = self.fs.session.lock();
            session
                .setattr(&self.handle, &attr, None)
                .map_err(fs_error_from_lx)?
        };
        match &reply.after {
            Some(_) => self.note_attrs(&reply.after),
            None => self.forget_attrs(),
        }
        Ok(())
    }

    /// `COMMIT` of the whole file: the write path sends `UNSTABLE` writes, so
    /// what makes them durable is this.
    ///
    /// Offset zero and count zero is "everything", which is what RFC 1813
    /// says that pair means -- not "nothing".
    fn sync_all(&self) -> VfsResult<()> {
        if self.is_dir()? {
            // There is nothing to commit for a directory: the server made
            // its changes durable when it answered the call that made them.
            return Ok(());
        }
        let reply = {
            let mut session = self.fs.session.lock();
            session
                .commit(&self.handle, 0, 0)
                .map_err(fs_error_from_lx)?
        };
        self.note_attrs(&reply.wcc.after);
        Ok(())
    }

    fn sync_data(&self) -> VfsResult<()> {
        self.sync_all()
    }

    fn resize(&self, len: usize) -> VfsResult<()> {
        let attr = SetAttr {
            mode: None,
            uid: None,
            gid: None,
            size: Some(len as u64),
            atime: None,
            mtime: None,
        };
        let reply = {
            let mut session = self.fs.session.lock();
            session
                .setattr(&self.handle, &attr, None)
                .map_err(fs_error_from_lx)?
        };
        match &reply.after {
            Some(_) => self.note_attrs(&reply.after),
            None => self.forget_attrs(),
        }
        Ok(())
    }

    fn create2(
        &self,
        name: &str,
        type_: FileType,
        mode: u32,
        data: usize,
    ) -> VfsResult<Arc<dyn INode>> {
        self.refuse_if_not_dir()?;
        let ftype = ftype_of(type_).ok_or(FsError::InvalidParam)?;
        let attr = SetAttr {
            mode: Some(mode & 0o7777),
            uid: None,
            gid: None,
            size: None,
            atime: None,
            mtime: None,
        };
        let reply = {
            let mut session = self.fs.session.lock();
            match ftype {
                Ftype3::Dir => session.mkdir(&self.handle, name, &attr),
                // `UNCHECKED` and not `GUARDED`: the exclusive modes are
                // `open(O_EXCL)`'s business, and this seam has no flags to
                // tell one from the other. `create2` is reached with the
                // name already looked up, which is where Linux refuses an
                // `O_EXCL` that would collide.
                Ftype3::Reg => session.create(&self.handle, name, CreateHow::Unchecked(attr)),
                // A device node's major and minor arrive as `data`, packed
                // the way `mknod(2)` packs them.
                Ftype3::Chr => session.mknod(
                    &self.handle,
                    name,
                    NodeKind::Char((data >> 8) as u32, (data & 0xff) as u32),
                    &attr,
                ),
                Ftype3::Blk => session.mknod(
                    &self.handle,
                    name,
                    NodeKind::Block((data >> 8) as u32, (data & 0xff) as u32),
                    &attr,
                ),
                Ftype3::Sock => session.mknod(&self.handle, name, NodeKind::Socket, &attr),
                Ftype3::Fifo => session.mknod(&self.handle, name, NodeKind::Fifo, &attr),
                // A symlink needs a target, and `create` has nowhere to put
                // one: `symlink(2)` comes through `create2`'s caller with the
                // target as the name of a separate call in Linux too.
                Ftype3::Lnk => return Err(FsError::NotSupported),
            }
            .map_err(fs_error_from_lx)?
        };
        // Whatever happened, this directory's listing no longer matches the
        // server's. Thrown away rather than appended to: the server decides
        // where a new entry sits in the cookie order, and guessing puts it
        // in the wrong place for every reader until the timeout.
        self.forget_listing();
        self.note_attrs(&reply.dir_wcc.after);
        // A `CREATE` may answer without a handle, and that is legal: the
        // name has to be looked up to get one. A client that unwrapped it
        // would panic on a reply the protocol allows.
        let fh = match reply.fh {
            Some(fh) => fh,
            None => {
                let mut session = self.fs.session.lock();
                session
                    .lookup(&self.handle, name)
                    .map_err(fs_error_from_lx)?
                    .fh
            }
        };
        Ok(self.fs.arc().inode(fh, reply.attr))
    }

    fn create(&self, name: &str, type_: FileType, mode: u32) -> VfsResult<Arc<dyn INode>> {
        self.create2(name, type_, mode, 0)
    }

    fn link(&self, name: &str, other: &Arc<dyn INode>) -> VfsResult<()> {
        self.refuse_if_not_dir()?;
        let other = other
            .as_any_ref()
            .downcast_ref::<NfsInode<T>>()
            // A hard link to something on another filesystem is `EXDEV`, not
            // a type error: that is what the protocol and Linux both say,
            // and what `mv` across a mount point branches on.
            .ok_or(FsError::NotSameFs)?;
        if !Arc::ptr_eq(&self.fs, &other.fs) {
            return Err(FsError::NotSameFs);
        }
        let reply = {
            let mut session = self.fs.session.lock();
            session
                .link(&other.handle, &self.handle, name)
                .map_err(fs_error_from_lx)?
        };
        self.forget_listing();
        self.note_attrs(&reply.dir_wcc.after);
        // The link count changed on the file, and the server sent the new
        // attributes with the reply.
        other.note_attrs(&reply.attr);
        Ok(())
    }

    fn unlink(&self, name: &str) -> VfsResult<()> {
        self.refuse_if_not_dir()?;
        // `REMOVE` and `RMDIR` are the same arguments and different
        // procedures, and a server will not remove a directory with
        // `REMOVE`. Which one this is depends on what the name *is*, so it
        // is looked up -- from the cached listing when that is fresh, which
        // is the common case because the caller just listed the directory.
        let is_dir = self.child_is_dir(name)?;
        let wcc = {
            let mut session = self.fs.session.lock();
            if is_dir {
                session.rmdir(&self.handle, name)
            } else {
                session.remove(&self.handle, name)
            }
            .map_err(fs_error_from_lx)?
        };
        self.forget_listing();
        self.note_attrs(&wcc.after);
        Ok(())
    }

    fn move_(&self, old_name: &str, target: &Arc<dyn INode>, new_name: &str) -> VfsResult<()> {
        self.refuse_if_not_dir()?;
        let target = target
            .as_any_ref()
            .downcast_ref::<NfsInode<T>>()
            .ok_or(FsError::NotSameFs)?;
        if !Arc::ptr_eq(&self.fs, &target.fs) {
            return Err(FsError::NotSameFs);
        }
        {
            let mut session = self.fs.session.lock();
            session
                .rename(&self.handle, old_name, &target.handle, new_name)
                .map_err(fs_error_from_lx)?;
        }
        // Both directories changed, and they may be the same one.
        self.forget_listing();
        self.forget_attrs();
        target.forget_listing();
        target.forget_attrs();
        Ok(())
    }

    fn find(&self, name: &str) -> VfsResult<Arc<dyn INode>> {
        self.refuse_if_not_dir()?;
        // `.` is this inode. Asking the server for it costs a round trip to
        // be told what the caller already has.
        //
        // `..` is **not** short-circuited, and that is the deliberate half:
        // on a server the parent of a directory is whatever the server says,
        // and an export's root has a parent that is outside the export. Only
        // `LOOKUP` knows.
        if name == "." {
            return Ok(self.fs.arc().inode(self.handle.clone(), None));
        }
        let reply = {
            let mut session = self.fs.session.lock();
            session
                .lookup(&self.handle, name)
                .map_err(fs_error_from_lx)?
        };
        Ok(self.fs.arc().inode(reply.fh, reply.attr))
    }

    /// The name of the `id`th entry.
    ///
    /// An index into **one** listing, read whole and kept: see the module
    /// docs for why asking the server again per index is not merely slow but
    /// wrong.
    ///
    /// `.` and `..` come from the server, which sends them, rather than being
    /// put at the front here. A server that does not send them -- some
    /// exports of the root of a filesystem do not -- then has a directory
    /// that lists without them, which is what Linux shows for the same
    /// export.
    fn get_entry(&self, id: usize) -> VfsResult<String> {
        self.refuse_if_not_dir()?;
        let entries = self.entries()?;
        entries
            .get(id)
            .map(|e| e.name.clone())
            // Past the end: `INode::list` walks indices until this, so it is
            // the end of the listing and not a missing file.
            .ok_or(FsError::EntryNotFound)
    }

    /// The `id`th entry's metadata and name in one answer.
    ///
    /// This is what `READDIRPLUS` is for: the attributes came with the
    /// listing, so a `ls -l` costs one call rather than one per file. An
    /// entry the server sent without attributes falls back to a `LOOKUP` and
    /// a `GETATTR` for that one entry.
    fn get_entry_with_metadata(&self, id: usize) -> VfsResult<(Metadata, String)> {
        self.refuse_if_not_dir()?;
        let entries = self.entries()?;
        let entry = entries.get(id).ok_or(FsError::EntryNotFound)?;
        if let Some(attr) = &entry.attr {
            return Ok((metadata_of(attr, attr.fsid as usize), entry.name.clone()));
        }
        let found = self.find(&entry.name)?;
        Ok((found.metadata()?, entry.name.clone()))
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::clone(&self.fs) as Arc<dyn FileSystem>
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

impl<T: RpcTransport + Send + 'static> NfsInode<T> {
    /// Whether the entry `name` in this directory is a directory, which is
    /// what decides between `REMOVE` and `RMDIR`.
    ///
    /// From the cached listing when it is fresh and carries attributes --
    /// `READDIRPLUS` brings them -- and from a `LOOKUP` otherwise. The cache
    /// is not *trusted* for the decision when it is stale, because removing
    /// a directory with `REMOVE` fails and removing a file with `RMDIR`
    /// fails too: getting it wrong is an error either way, not a silent
    /// mistake.
    fn child_is_dir(&self, name: &str) -> VfsResult<bool> {
        if let Some(cached) = self.listing.lock().as_ref() {
            if cached.fresh(self.fs.dir_ttl) {
                if let Some(entry) = cached.value.iter().find(|e| e.name == name) {
                    if let Some(attr) = &entry.attr {
                        return Ok(attr.ftype == Ftype3::Dir);
                    }
                }
            }
        }
        let reply = {
            let mut session = self.fs.session.lock();
            session
                .lookup(&self.handle, name)
                .map_err(fs_error_from_lx)?
        };
        match reply.attr {
            Some(attr) => Ok(attr.ftype == Ftype3::Dir),
            // The server answered the `LOOKUP` without attributes, which is
            // legal. One `GETATTR` on the handle it did send.
            None => {
                let mut session = self.fs.session.lock();
                Ok(session.getattr(&reply.fh).map_err(fs_error_from_lx)?.ftype == Ftype3::Dir)
            }
        }
    }
}

#[cfg(test)]
#[path = "fs_tests.rs"]
mod fs_tests;
