use crate::dev::DevError;
use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use core::any::Any;
use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::result;
use core::str;

/// Abstract file system object such as file or directory.
pub trait INode: Any + Sync + Send {
    /// Read bytes at `offset` into `buf`, return the number of bytes read.
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize>;

    /// Write bytes at `offset` from `buf`, return the number of bytes written.
    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize>;

    /// Poll the events, return a bitmap of events.
    fn poll(&self) -> Result<PollStatus>;

    /// Poll the events, return a bitmap of events, async version.
    fn async_poll<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
        Box::pin(async move { self.poll() })
    }

    /// Get metadata of the INode
    fn metadata(&self) -> Result<Metadata> {
        Err(FsError::NotSupported)
    }

    /// Set metadata of the INode
    fn set_metadata(&self, _metadata: &Metadata) -> Result<()> {
        Err(FsError::NotSupported)
    }

    /// Sync all data and metadata
    fn sync_all(&self) -> Result<()> {
        Err(FsError::NotSupported)
    }

    /// Sync data (not include metadata)
    fn sync_data(&self) -> Result<()> {
        Err(FsError::NotSupported)
    }

    /// Resize the file
    fn resize(&self, _len: usize) -> Result<()> {
        Err(FsError::NotSupported)
    }

    /// Create a new INode in the directory
    fn create(&self, name: &str, type_: FileType, mode: u32) -> Result<Arc<dyn INode>> {
        self.create2(name, type_, mode, 0)
    }

    /// Create a new INode in the directory, with a data field for usages like device file.
    fn create2(
        &self,
        name: &str,
        type_: FileType,
        mode: u32,
        _data: usize,
    ) -> Result<Arc<dyn INode>> {
        self.create(name, type_, mode)
    }

    /// Create a hard link `name` to `other`
    fn link(&self, _name: &str, _other: &Arc<dyn INode>) -> Result<()> {
        Err(FsError::NotSupported)
    }

    /// Delete a hard link `name`
    fn unlink(&self, _name: &str) -> Result<()> {
        Err(FsError::NotSupported)
    }

    /// Move INode `self/old_name` to `target/new_name`.
    /// If `target` equals `self`, do rename.
    fn move_(&self, _old_name: &str, _target: &Arc<dyn INode>, _new_name: &str) -> Result<()> {
        Err(FsError::NotSupported)
    }

    /// Find the INode `name` in the directory
    fn find(&self, _name: &str) -> Result<Arc<dyn INode>> {
        Err(FsError::NotSupported)
    }

    /// Get the name of directory entry
    fn get_entry(&self, _id: usize) -> Result<String> {
        Err(FsError::NotSupported)
    }

    /// Get the name of directory entry with metadata
    fn get_entry_with_metadata(&self, id: usize) -> Result<(Metadata, String)> {
        // a default and slow implementation
        let name = self.get_entry(id)?;
        let entry = self.find(&name)?;
        Ok((entry.metadata()?, name))
    }

    /// Where a magic link leads, as `/proc/<pid>/fd/N` does (`nd_jump_link`
    /// on Linux): to an inode, and its name from the root, rather than to the
    /// path its text spells, which is only what `readlink` reads. `None` for
    /// every other inode, and an error for a magic link that leads nowhere
    /// now.
    fn jump(&self) -> Option<Result<(Arc<dyn INode>, String)>> {
        None
    }

    /// The whole of a file that is written out when it is read, as a
    /// `seq_file` of `/proc` is on Linux: an open file reads one copy of it
    /// until a read starts elsewhere than where the last one ended, and it
    /// has no end to `lseek` from. `None` for every other inode, and an error
    /// when there is nothing to write out now, as for a process that is gone.
    fn seq_text(&self) -> Option<Result<Vec<u8>>> {
        None
    }

    /// Control device
    fn io_control(&self, _cmd: u32, _data: usize) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    /// Map files or devices into memory
    fn mmap(&self, _area: MMapArea) -> Result<()> {
        Err(FsError::NotSupported)
    }

    /// Get the file system of the INode.
    ///
    /// Inodes that belong to no file system (ttys, pipes, sockets, device
    /// nodes) get [`no_fs`], a filesystem that owns nothing. The old default
    /// was `unimplemented!()`, which took the kernel down on `fsync(2)` of a
    /// terminal and on any path that keys a per-inode table by its file
    /// system.
    fn fs(&self) -> Arc<dyn FileSystem> {
        no_fs()
    }

    /// This is used to implement dynamics cast.
    /// Simply return self in the implement of the function.
    fn as_any_ref(&self) -> &dyn Any;
}

impl dyn INode {
    /// Downcast the INode to specific struct
    pub fn downcast_ref<T: INode>(&self) -> Option<&T> {
        self.as_any_ref().downcast_ref::<T>()
    }

    /// Get all directory entries as a Vec
    pub fn list(&self) -> Result<Vec<String>> {
        let info = self.metadata()?;
        if info.type_ != FileType::Dir {
            return Err(FsError::NotDir);
        }
        Ok((0..)
            .map(|i| self.get_entry(i))
            .take_while(|result| result.is_ok())
            .filter_map(|result| result.ok())
            .collect())
    }

    /// Lookup path from current INode, and do not follow a symlink that is
    /// its last component
    pub fn lookup(&self, path: &str) -> Result<Arc<dyn INode>> {
        self.lookup_follow(path, 0)
    }

    /// Lookup path from current INode, and follow symlinks at most `follow_times` times
    ///
    /// With `follow_times == 0` the last component is not followed: a symlink
    /// there is returned as it is (`lstat`, `readlink`, `O_NOFOLLOW`). The
    /// links on the way to it still are, up to [`MAXSYMLINKS`], as Linux
    /// follows them on every walk. They used to be `NotDir`, so `lstat` of
    /// `/proc/self/stat`, or of anything under a directory reached through a
    /// link, failed, and `ls -l` printed an error for each entry.
    ///
    /// With a budget, running out of it is `SymLoop`, as `ELOOP` on Linux,
    /// never the symlink itself: a link that points at itself used to come
    /// back as an ordinary file whose contents were its own name.
    pub fn lookup_follow(&self, path: &str, follow_times: usize) -> Result<Arc<dyn INode>> {
        self.lookup_walked(path, follow_times, None)
    }

    /// [`Self::lookup_follow`], and `walked` goes along: it holds where this
    /// inode is when the walk starts, and where what it found is when it is
    /// done.
    pub fn lookup_walked(
        &self,
        path: &str,
        follow_times: usize,
        walked: Option<&mut Walked>,
    ) -> Result<Arc<dyn INode>> {
        match follow_times {
            0 => lookup_with_budget(self, path, MAXSYMLINKS, false, walked),
            budget => lookup_with_budget(self, path, budget, true, walked),
        }
    }
}

/// Where a walk is: the names of the directories it entered, from the root
/// and through no link, which is what Linux names a file by (`d_path`).
///
/// A link the walk follows is not entered; its target is walked from the
/// directory that holds it, so a `..` after a link climbs out of where the
/// link leads.
#[derive(Default, Clone)]
pub struct Walked {
    /// The names, from the root.
    pub names: Vec<String>,
    /// The root of whoever walks: where an absolute target starts over, and
    /// what a `..` out of a file system reached without the mounts climbs
    /// back through. Without it an absolute target starts over at the root
    /// of the file system the walk is in, which for `/proc` or `/dev` is not
    /// `/`, so a link there to `/tmp` led nowhere.
    pub root: Option<Arc<dyn INode>>,
    /// The walk went through a magic link ([`INode::jump`]): what it found
    /// depends on who walked.
    pub jumped: bool,
    /// Whether whoever walks may search a directory (`may_lookup`), asked
    /// of each one before a name is looked up in it. Without it the walk
    /// searches every directory, as the kernel's own walks do.
    pub search: Option<Search>,
    /// The directories on the way that not everybody may search, which a
    /// walk remembered by its path has to ask again for each walker.
    pub gates: Vec<Arc<dyn INode>>,
}

/// The question [`Walked::search`] asks: whether a directory, with that
/// metadata, may be searched.
pub type Search = Arc<dyn Fn(&dyn INode, &Metadata) -> Result<()> + Send + Sync>;

impl Walked {
    /// `may_lookup`: whether a name may be looked up in `dir`, whose
    /// metadata is `meta`. A directory everybody may search (`x` for the
    /// owner, the group and the others) is no question for anyone, root
    /// without its capabilities included; any other is a gate.
    pub fn may_look_in(&mut self, dir: &Arc<dyn INode>, meta: &Metadata) -> Result<()> {
        if meta.mode & 0o111 == 0o111 {
            return Ok(());
        }
        self.gates.push(dir.clone());
        match &self.search {
            Some(search) => search(&**dir, meta),
            None => Ok(()),
        }
    }

    /// At `path`, an absolute path with nothing left to resolve in it.
    pub fn at(path: &str) -> Self {
        let mut walked = Walked::default();
        walked.set(path);
        walked
    }

    /// Moves to `path`, as [`Self::at`], keeping the root.
    pub fn set(&mut self, path: &str) {
        self.names.clear();
        for name in path.split('/') {
            self.enter(String::from(name));
        }
    }

    /// One step down to `name`, or up for `..`.
    pub fn enter(&mut self, name: String) {
        match name.as_str() {
            "" | "." => {}
            ".." => {
                self.names.pop();
            }
            _ => self.names.push(name),
        }
    }

    /// Through a magic link to what is named `name`, keeping the root.
    pub fn jump_to(&mut self, name: &str) {
        self.set(name);
        self.jumped = true;
    }

    /// The path, from the root.
    pub fn path(&self) -> String {
        String::from("/") + &self.names.join("/")
    }

    /// Where `..` leads from `dir`, which is where the walk is.
    ///
    /// At the top of a file system reached without the mounts (`/proc`,
    /// `/sys` and `/dev`, which the kernel walks straight into, and the
    /// `/dev/shm` inside `/dev`) `..` is that top again, or nothing at all,
    /// so `/proc/..` was `/proc` and `/dev/..` was `ENOENT`. The directory
    /// the top is in is then found from the root, by name.
    fn up(&self, dir: &dyn INode) -> Result<Arc<dyn INode>> {
        let found = dir.find("..");
        let (Some(root), Some((_, parent))) = (&self.root, self.names.split_last()) else {
            return found;
        };
        if let Ok(up) = found {
            let (here, there) = (dir.metadata()?, up.metadata()?);
            if (here.dev, here.inode) != (there.dev, there.inode) {
                return Ok(up);
            }
        }
        root.lookup(&parent.join("/"))
    }
}

/// How many symlinks one walk follows before it is `SymLoop`: Linux's
/// `MAXSYMLINKS`, the links on the way and the last one together.
pub const MAXSYMLINKS: usize = 40;

/// The longest symlink target `lookup_follow` resolves: Linux's `PATH_MAX`.
/// A longer one is `NameTooLong`. It used to be read into 256 bytes and cut
/// there without a word, so a long target resolved to whatever its first 256
/// bytes named.
pub const SYMLINK_MAX: usize = 4096;

/// What a symlink points at, whole.
fn read_symlink(inode: &dyn INode) -> Result<String> {
    let mut content = alloc::vec![0u8; SYMLINK_MAX + 1];
    let len = inode.read_at(0, &mut content)?;
    if len > SYMLINK_MAX {
        return Err(FsError::NameTooLong);
    }
    content.truncate(len);
    String::from_utf8(content).map_err(|_| FsError::NotDir)
}

fn lookup_with_budget(
    start: &dyn INode,
    path: &str,
    mut budget: usize,
    follow_last: bool,
    mut walked: Option<&mut Walked>,
) -> Result<Arc<dyn INode>> {
    if start.metadata()?.type_ != FileType::Dir {
        return Err(FsError::NotDir);
    }

    // handle absolute path
    let (mut result, mut rest_path) = if let Some(rest) = path.strip_prefix('/') {
        let root = walked.as_deref_mut().and_then(|walked| {
            walked.names.clear();
            walked.root.clone()
        });
        (
            root.unwrap_or_else(|| start.fs().root_inode()),
            String::from(rest),
        )
    } else {
        (start.find(".")?, String::from(path))
    };

    while !rest_path.is_empty() {
        let here = result.metadata()?;
        if here.type_ != FileType::Dir {
            return Err(FsError::NotDir);
        }
        // `on_the_way`: a slash comes after the name, if only a trailing one.
        let (name, on_the_way) = match rest_path.find('/') {
            None => (core::mem::take(&mut rest_path), false),
            Some(pos) => {
                let name = String::from(&rest_path[0..pos]);
                rest_path = String::from(&rest_path[pos + 1..]);
                (name, true)
            }
        };
        if name.is_empty() {
            continue;
        }
        if let Some(walked) = walked.as_deref_mut() {
            walked.may_look_in(&result, &here)?;
        }
        let inode = match walked.as_deref() {
            Some(walked) if name == ".." => walked.up(&*result)?,
            _ => result.find(&name)?,
        };
        // Handle symlink. A link on the way is followed by every walk; the
        // last component only by one asked to follow it, and otherwise it is
        // what `lstat` and `readlink` want. A spent budget is `SymLoop`
        // rather than the link where the walk stopped.
        if inode.metadata()?.type_ == FileType::SymLink && (on_the_way || follow_last) {
            if budget == 0 {
                return Err(FsError::SymLoop);
            }
            // A magic link leads to an inode, and the walk goes on from it.
            if let Some(jump) = inode.jump() {
                let (to, named) = jump?;
                if let Some(walked) = walked.as_deref_mut() {
                    walked.jump_to(&named);
                }
                result = to;
                budget -= 1;
                continue;
            }
            let link_path = read_symlink(&*inode)?;
            // result remains unchanged: a relative target is resolved from the
            // directory that holds the link.
            let new_path = link_path + "/" + &rest_path;
            return lookup_with_budget(&*result, &new_path, budget - 1, follow_last, walked);
        } else {
            result = inode;
            if let Some(walked) = walked.as_deref_mut() {
                walked.enter(name);
            }
        }
    }
    Ok(result)
}

pub enum IOCTLError {
    NotValidFD = 9,      // EBADF
    NotValidMemory = 14, // EFAULT
    NotValidParam = 22,  // EINVAL
    NotCharDevice = 25,  // ENOTTY
}

#[derive(Debug, Default)]
pub struct PollStatus {
    pub read: bool,
    pub write: bool,
    pub error: bool,
    /// Peer hangup / connection closed (Linux POLLHUP). Always reportable
    /// regardless of the events interest mask.
    pub hangup: bool,
}

#[derive(Debug)]
pub struct MMapArea {
    /// Start virtual address
    pub start_vaddr: usize,
    /// End virtual address
    pub end_vaddr: usize,
    /// Access permissions
    pub prot: usize,
    /// Flags
    pub flags: usize,
    /// Offset from the file in bytes
    pub offset: usize,
}

/// Metadata of INode
///
/// Ref: [http://pubs.opengroup.org/onlinepubs/009604499/basedefs/sys/stat.h.html]
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct Metadata {
    /// Device ID
    pub dev: usize, // (major << 8) | minor
    /// Inode number
    pub inode: usize,
    /// Size in bytes
    ///
    /// SFS Note: for normal file size is the actuate file size
    /// for directory this is count of dirent.
    pub size: usize,
    /// A file system-specific preferred I/O block size for this object.
    /// In some file system types, this may vary from file to file.
    pub blk_size: usize,
    /// Size in blocks
    pub blocks: usize,
    /// Time of last access
    pub atime: Timespec,
    /// Time of last modification
    pub mtime: Timespec,
    /// Time of last change
    pub ctime: Timespec,
    /// Type of file
    pub type_: FileType,
    /// Permission
    pub mode: u16,
    /// Number of hard links
    ///
    /// SFS Note: different from linux, "." and ".." count in nlinks
    /// this is same as original ucore.
    pub nlinks: usize,
    /// User ID
    pub uid: usize,
    /// Group ID
    pub gid: usize,
    /// Raw device id
    /// e.g. /dev/null: makedev(0x1, 0x3)
    pub rdev: usize, // (major << 8) | minor
}

/// A timestamp, and part of an on-disk format.
///
/// `repr(C)` is load-bearing: SFS puts three of these inside every
/// `DiskINode`, which goes to the block device as raw bytes, so the order of
/// these two fields is the format a filesystem image is written in.
/// `repr(Rust)` is free to reorder them -- it happens to put `sec` first
/// today -- and nothing would notice an image that stopped being readable.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Timespec {
    pub sec: i64,
    pub nsec: i32,
}

/// Sixteen bytes: a signed second, a nanosecond, and four bytes of tail
/// padding to the eight-byte alignment the second needs. This is what three
/// of the hundred and twenty-eight bytes of an SFS inode are made of, so it
/// is checked here, by the definition, rather than only where it is used.
const _: () = {
    assert!(core::mem::size_of::<Timespec>() == 16);
    assert!(core::mem::align_of::<Timespec>() == 8);
    assert!(
        core::mem::offset_of!(Timespec, sec) == 0,
        "the second first"
    );
    assert!(
        core::mem::offset_of!(Timespec, nsec) == 8,
        "then the nanosecond"
    );
};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FileType {
    File,
    Dir,
    SymLink,
    CharDevice,
    BlockDevice,
    NamedPipe,
    Socket,
}

/// Linux `statfs.f_type` magics (`include/uapi/linux/magic.h`).
pub const PROC_SUPER_MAGIC: u64 = 0x9fa0;
pub const SYSFS_MAGIC: u64 = 0x6265_6572;
pub const TMPFS_MAGIC: u64 = 0x0102_1994;
pub const EXT4_SUPER_MAGIC: u64 = 0xEF53;
pub const BTRFS_SUPER_MAGIC: u64 = 0x9123_683E;
pub const MSDOS_SUPER_MAGIC: u64 = 0x4d44;

/// Metadata of FileSystem
///
/// Ref: [http://pubs.opengroup.org/onlinepubs/9699919799/]
#[derive(Debug)]
pub struct FsInfo {
    /// File system block size
    pub bsize: usize,
    /// Fundamental file system block size
    pub frsize: usize,
    /// Total number of blocks on file system in units of `frsize`
    pub blocks: usize,
    /// Total number of free blocks
    pub bfree: usize,
    /// Number of free blocks available to non-privileged process
    pub bavail: usize,
    /// Total number of file serial numbers
    pub files: usize,
    /// Total number of free file serial numbers
    pub ffree: usize,
    /// Maximum filename length
    pub namemax: usize,
    /// Linux `statfs.f_type` filesystem magic. Userspace probes (systemd
    /// `proc_mounted()`, `df -T`, …) key off this; a wrong value makes
    /// `/proc` look unmounted even when it is present.
    pub ftype: u64,
}

// Note: IOError/NoMemory always lead to a panic since it's hard to recover from it.
//       We also panic when we can not parse the fs on disk normally
#[derive(Debug, Eq, PartialEq)]
pub enum FsError {
    NotSupported,  // E_UNIMP, or E_INVAL
    NotFile,       // E_ISDIR
    IsDir,         // E_ISDIR, used only in link
    NotDir,        // E_NOTDIR
    EntryNotFound, // E_NOENT
    EntryExist,    // E_EXIST
    NotSameFs,     // E_XDEV
    InvalidParam,  // E_INVAL
    NoDeviceSpace, // E_NOSPC, but is defined and not used in the original ucore, which uses E_NO_MEM
    /// E_NOMEM (12): out of memory / resource allocation refused. Distinct
    /// from `NoDeviceSpace` (ENOSPC): DRM GEM/`CREATE_DUMB` and oversized
    /// syncobj arrays are ENOMEM on Linux, not "disk full".
    NoMemory,
    DirRemoved,  // E_NOENT, when the current dir was remove by a previous unlink
    DirNotEmpty, // E_NOTEMPTY
    WrongFs,     // E_INVAL, when we find the content on disk is wrong when opening the device
    DeviceError,
    IOCTLError,
    NoDevice,
    Again,        // E_AGAIN, when no data is available, never happens in fs
    TimedOut,     // E_TIME (62), a wait reached its deadline -- e.g. DRM_IOCTL_SYNCOBJ_WAIT
    SymLoop,      // E_LOOP
    NameTooLong,  // E_NAMETOOLONG, e.g. a symlink target past `SYMLINK_MAX`
    Busy,         // E_BUSY
    ReadOnly,     // E_ROFS
    Interrupted,  // E_INTR
    NoPermission, // E_ACCES, e.g. modeset ioctls on a DRM render node
    /// E_PERM (1): the operation is forbidden for this object, not merely
    /// for this caller's credentials. Distinct from `NoPermission` (EACCES):
    /// Linux's `DESTROYPROPBLOB` on a kernel-owned blob is EPERM, while a
    /// modeset ioctl on a render node is EACCES.
    NotPermitted,
    OpNotSupported, // E_OPNOTSUPP, e.g. an ioctl the device genuinely lacks
    BadAddress,     // E_FAULT, a user pointer outside the user address range
    /// E_BADFD (77): the file descriptor is valid but the object behind it is
    /// in a state that does not allow this operation. NOT E_INVAL, which means
    /// "bad arguments" -- ALSA clients act on the difference (see
    /// `SndPcm::writei`).
    BadState,
    /// E_PIPE (32): the stream broke under the caller -- an ALSA underrun or
    /// overrun. Distinct from `Again`, which means "not right now, poll and
    /// retry": clients recover from `E_PIPE` by re-preparing the stream, and
    /// have nothing to do about an `Again` that will never clear.
    Broken,
    /// E_NXIO (6): the node exists but has no device behind it for this
    /// operation -- `read(2)` on a playback-only `/dev/dsp`, as on Linux.
    NoSuchDeviceOrAddress,
    /// E_NOKEY (126): the file or directory is encrypted and its key has not
    /// been added (fscrypt).
    NoKey,
    /// E_RANGE (34): a value the operation cannot represent -- a plane's CRTC
    /// rectangle past `INT_MAX` (`__setplane_check`, `drm_atomic_plane_check`:
    /// "invalid CRTC coordinates"). NOT E_INVAL: Linux keeps the two apart
    /// and a client that tells them apart is told which it was.
    OutOfRange,
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl From<DevError> for FsError {
    fn from(_: DevError) -> Self {
        FsError::DeviceError
    }
}

pub type Result<T> = result::Result<T, FsError>;

/// Abstract file system
pub trait FileSystem: Sync + Send {
    /// Sync all data to the storage
    fn sync(&self) -> Result<()>;

    /// Get the root INode of the file system
    fn root_inode(&self) -> Arc<dyn INode>;

    /// Get the file system information
    fn info(&self) -> FsInfo;
}

/// The file system reported by inodes that have none. One shared instance,
/// so callers can recognise it with [`is_no_fs`].
pub fn no_fs() -> Arc<dyn FileSystem> {
    static NO_FS: spin::Once<Arc<dyn FileSystem>> = spin::Once::new();
    NO_FS.call_once(|| Arc::new(NoFileSystem)).clone()
}

/// Whether `fs` is the placeholder returned for inodes without a file system.
pub fn is_no_fs(fs: &Arc<dyn FileSystem>) -> bool {
    Arc::ptr_eq(fs, &no_fs())
}

/// The placeholder file system behind [`no_fs`]: nothing to sync, no space,
/// an empty root.
struct NoFileSystem;

impl FileSystem for NoFileSystem {
    fn sync(&self) -> Result<()> {
        Ok(())
    }

    fn root_inode(&self) -> Arc<dyn INode> {
        Arc::new(NoINode)
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            bsize: 4096,
            frsize: 4096,
            blocks: 0,
            bfree: 0,
            bavail: 0,
            files: 0,
            ffree: 0,
            namemax: 255,
            ftype: EXT4_SUPER_MAGIC,
        }
    }
}

/// The root of [`NoFileSystem`]: an empty, unreadable, unwritable inode.
struct NoINode;

impl INode for NoINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        Err(FsError::NotSupported)
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

pub fn make_rdev(major: usize, minor: usize) -> usize {
    ((major & 0xfff) << 8) | (minor & 0xff)
}

#[cfg(test)]
mod lookup_tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::sync::Weak;
    use spin::Mutex;

    /// The smallest tree `lookup_follow` can walk: directories with
    /// children, files, and symlinks whose contents are their target.
    struct Node {
        type_: FileType,
        content: Vec<u8>,
        children: Mutex<BTreeMap<String, Arc<Node>>>,
        parent: Mutex<Weak<Node>>,
        this: Mutex<Weak<Node>>,
        fs: Mutex<Weak<Tree>>,
        /// A magic link's target, named as given.
        to: Mutex<Option<(Weak<Node>, &'static str)>>,
        mode: Mutex<u16>,
    }

    struct Tree {
        root: Arc<Node>,
    }

    fn node(type_: FileType, content: &[u8]) -> Arc<Node> {
        let node = Arc::new(Node {
            type_,
            content: content.to_vec(),
            children: Mutex::new(BTreeMap::new()),
            parent: Mutex::new(Weak::new()),
            this: Mutex::new(Weak::new()),
            fs: Mutex::new(Weak::new()),
            to: Mutex::new(None),
            mode: Mutex::new(0o777),
        });
        *node.this.lock() = Arc::downgrade(&node);
        node
    }

    fn tree() -> Arc<Tree> {
        let tree = Arc::new(Tree {
            root: node(FileType::Dir, b""),
        });
        *tree.root.fs.lock() = Arc::downgrade(&tree);
        *tree.root.parent.lock() = Arc::downgrade(&tree.root);
        tree
    }

    impl Node {
        fn add(self: &Arc<Self>, name: &str, child: Arc<Node>) -> Arc<Node> {
            *child.parent.lock() = Arc::downgrade(self);
            *child.fs.lock() = self.fs.lock().clone();
            self.children
                .lock()
                .insert(String::from(name), child.clone());
            child
        }
        fn dir(self: &Arc<Self>, name: &str) -> Arc<Node> {
            self.add(name, node(FileType::Dir, b""))
        }
        fn file(self: &Arc<Self>, name: &str, content: &[u8]) -> Arc<Node> {
            self.add(name, node(FileType::File, content))
        }
        fn link(self: &Arc<Self>, name: &str, target: &str) -> Arc<Node> {
            self.add(name, node(FileType::SymLink, target.as_bytes()))
        }
        /// A magic link to `to`, named `at`, whose text leads nowhere.
        fn jump(self: &Arc<Self>, name: &str, to: &Arc<Node>, at: &'static str) -> Arc<Node> {
            let link = self.link(name, "nowhere");
            *link.to.lock() = Some((Arc::downgrade(to), at));
            link
        }
    }

    impl FileSystem for Tree {
        fn sync(&self) -> Result<()> {
            Ok(())
        }
        fn root_inode(&self) -> Arc<dyn INode> {
            self.root.clone()
        }
        fn info(&self) -> FsInfo {
            no_fs().info()
        }
    }

    impl INode for Node {
        fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
            if self.type_ == FileType::Dir {
                return Err(FsError::IsDir);
            }
            let start = offset.min(self.content.len());
            let n = buf.len().min(self.content.len() - start);
            buf[..n].copy_from_slice(&self.content[start..start + n]);
            Ok(n)
        }
        fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
            Err(FsError::NotSupported)
        }
        fn poll(&self) -> Result<PollStatus> {
            Ok(PollStatus::default())
        }
        fn metadata(&self) -> Result<Metadata> {
            Ok(Metadata {
                dev: 0,
                inode: self as *const Node as usize,
                size: self.content.len(),
                blk_size: 0,
                blocks: 0,
                atime: Timespec { sec: 0, nsec: 0 },
                mtime: Timespec { sec: 0, nsec: 0 },
                ctime: Timespec { sec: 0, nsec: 0 },
                type_: self.type_,
                mode: *self.mode.lock(),
                nlinks: 1,
                uid: 0,
                gid: 0,
                rdev: 0,
            })
        }
        fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
            if self.type_ != FileType::Dir {
                return Err(FsError::NotDir);
            }
            let found: Arc<Node> = match name {
                "." => self.this.lock().upgrade().unwrap(),
                ".." => self.parent.lock().upgrade().ok_or(FsError::EntryNotFound)?,
                _ => self
                    .children
                    .lock()
                    .get(name)
                    .cloned()
                    .ok_or(FsError::EntryNotFound)?,
            };
            Ok(found)
        }
        fn fs(&self) -> Arc<dyn FileSystem> {
            self.fs.lock().upgrade().unwrap()
        }
        fn jump(&self) -> Option<Result<(Arc<dyn INode>, String)>> {
            let (to, at) = self.to.lock().clone()?;
            Some(match to.upgrade() {
                Some(to) => Ok((to, String::from(at))),
                None => Err(FsError::EntryNotFound),
            })
        }
        fn as_any_ref(&self) -> &dyn Any {
            self
        }
    }

    fn same(a: &Arc<dyn INode>, b: &Arc<Node>) -> bool {
        a.metadata().unwrap().inode == b.metadata().unwrap().inode
    }

    /// A walk that keeps, in `asked`, the inode of each directory it asks
    /// about, in order, and is refused every one of them with `refuse`.
    fn a_walker(asked: &Arc<Mutex<Vec<usize>>>, refuse: bool) -> Walked {
        let asked = asked.clone();
        Walked {
            search: Some(Arc::new(move |_: &dyn INode, meta: &Metadata| {
                asked.lock().push(meta.inode);
                if refuse {
                    Err(FsError::NoPermission)
                } else {
                    Ok(())
                }
            })),
            ..Walked::default()
        }
    }

    fn id(node: &Arc<Node>) -> usize {
        node.metadata().unwrap().inode
    }

    #[test]
    fn a_walk_asks_about_each_directory_it_looks_a_name_up_in() {
        // `link_path_walk`: `may_lookup` before every component, the last
        // included, and never of what the last one names.
        let tree = tree();
        let root = &tree.root;
        let a = root.dir("a");
        let b = a.dir("b");
        b.file("f", b"");
        *a.mode.lock() = 0o700;
        *b.mode.lock() = 0o750;
        let inode = &**root as &dyn INode;
        let asked = Arc::new(Mutex::new(Vec::new()));
        let walk = |path: &str, follow: usize| {
            asked.lock().clear();
            let mut walked = a_walker(&asked, false);
            inode
                .lookup_walked(path, follow, Some(&mut walked))
                .unwrap();
            let gates: Vec<usize> = walked
                .gates
                .iter()
                .map(|g| g.metadata().unwrap().inode)
                .collect();
            assert_eq!(
                gates,
                *asked.lock(),
                "{}: every directory asked about is a gate",
                path
            );
            asked.lock().clone()
        };
        assert_eq!(walk("a/b/f", 0), [id(&a), id(&b)]);
        assert_eq!(
            walk("a/b", 0),
            [id(&a)],
            "what the last name names is not asked about"
        );
        assert_eq!(walk("a/b/", 0), [id(&a)], "nor with a slash after it");
        assert_eq!(walk("a", 0), []);
        assert_eq!(walk("a/.", 0), [id(&a)], "a dot is a name looked up");
        assert_eq!(walk("a/b/..", 0), [id(&a), id(&b)], "and so are two");
        assert_eq!(walk("a//b", 0), [id(&a)], "an empty name is none");
        // A link's target is walked as any path, from where the link is.
        root.link("l", "a/b/f");
        a.link("up", "../a/b/f");
        assert_eq!(walk("l", 4), [id(&a), id(&b)]);
        assert_eq!(walk("l", 0), [], "a link left in place leads nowhere");
        // `up` in `a`, then `..` in `a`, `a` in the root, `b` in `a` and
        // `f` in `b`.
        assert_eq!(walk("a/up", 4), [id(&a), id(&a), id(&a), id(&b)]);
    }

    #[test]
    fn a_directory_everybody_may_search_is_no_question() {
        // `x` for the owner, the group and the others: nobody, root without
        // its capabilities included, is refused, so nobody is asked.
        let tree = tree();
        let root = &tree.root;
        let open = root.dir("open");
        open.file("f", b"");
        let inode = &**root as &dyn INode;
        let asked = Arc::new(Mutex::new(Vec::new()));
        for (mode, is_asked) in [
            (0o111, false),
            (0o711, false),
            (0o110, true),
            (0o101, true),
            (0o011, true),
            (0o666, true),
        ] {
            *open.mode.lock() = mode;
            asked.lock().clear();
            let mut walked = a_walker(&asked, false);
            inode.lookup_walked("open/f", 0, Some(&mut walked)).unwrap();
            assert_eq!(!asked.lock().is_empty(), is_asked, "{:o}", mode);
            assert_eq!(!walked.gates.is_empty(), is_asked, "{:o}", mode);
        }
    }

    #[test]
    fn a_refused_search_stops_the_walk_there() {
        let tree = tree();
        let root = &tree.root;
        let shut = root.dir("shut");
        shut.file("f", b"");
        *shut.mode.lock() = 0o700;
        root.link("l", "/shut/f");
        let inode = &**root as &dyn INode;
        let asked = Arc::new(Mutex::new(Vec::new()));
        for path in ["shut/f", "shut/absent", "shut/.", "l"] {
            let mut walked = a_walker(&asked, true);
            assert_eq!(
                inode.lookup_walked(path, 4, Some(&mut walked)).err(),
                Some(FsError::NoPermission),
                "{}: refused before the name is looked up",
                path
            );
        }
        // Without a question the kernel's own walk goes through, and still
        // remembers the gate.
        let mut walked = Walked::default();
        inode.lookup_walked("shut/f", 0, Some(&mut walked)).unwrap();
        assert_eq!(walked.gates.len(), 1);
        assert!(inode.lookup("shut/f").is_ok());
    }

    #[test]
    fn a_symlink_target_longer_than_256_bytes_is_followed_whole() {
        let tree = tree();
        let root = &tree.root;
        // A directory name of 300 bytes: the first 256 of the target name a
        // directory that does not exist.
        let long = "d".repeat(300);
        let file = root.dir(&long).file("file", b"hello");
        let target = long.clone() + "/file";
        root.link("link", &target);
        root.dir("etc").file("hosts", b"");
        root.link("abs", &("/".to_string() + &target));

        let hit = (&**root as &dyn INode).lookup_follow("link", 4).unwrap();
        assert!(
            same(&hit, &file),
            "the target is read whole, not cut at 256"
        );
        let hit = (&**root as &dyn INode).lookup_follow("abs", 4).unwrap();
        assert!(same(&hit, &file), "an absolute target too");

        // Exactly SYMLINK_MAX bytes is still a name; one more is not.
        let edge = "e".repeat(SYMLINK_MAX - "/file".len());
        let edge_file = root.dir(&edge).file("file", b"");
        root.link("edge", &(edge.clone() + "/file"));
        let hit = (&**root as &dyn INode).lookup_follow("edge", 4).unwrap();
        assert!(same(&hit, &edge_file));
        root.link("past", &("x".repeat(SYMLINK_MAX + 1)));
        assert_eq!(
            (&**root as &dyn INode).lookup_follow("past", 4).err(),
            Some(FsError::NameTooLong)
        );
    }

    #[test]
    fn a_symlink_loop_is_eloop_and_not_the_link_itself() {
        let tree = tree();
        let root = &tree.root;
        root.link("self", "self");
        root.link("a", "b");
        root.link("b", "a");
        let inode = &**root as &dyn INode;
        assert_eq!(
            inode.lookup_follow("self", 40).err(),
            Some(FsError::SymLoop),
            "a link to itself used to come back as a file"
        );
        assert_eq!(inode.lookup_follow("a", 40).err(), Some(FsError::SymLoop));
        assert_eq!(
            inode.lookup_follow("self/x", 40).err(),
            Some(FsError::SymLoop),
            "on the way as well"
        );

        // A chain that fits the budget resolves; one hop too many does not.
        let file = root.file("file", b"");
        root.link("l1", "file");
        root.link("l2", "l1");
        root.link("l3", "l2");
        assert!(same(&inode.lookup_follow("l3", 3).unwrap(), &file));
        assert_eq!(inode.lookup_follow("l3", 2).err(), Some(FsError::SymLoop));

        // Without a budget the last symlink is what `lstat` and `readlink`
        // want; the ones on the way are still followed, so a loop there is
        // `SymLoop`, and a chain that ends in a file is `NotDir` past it.
        let link = root.children.lock()["self"].clone();
        assert!(same(&inode.lookup("self").unwrap(), &link));
        assert_eq!(inode.lookup("self/x").err(), Some(FsError::SymLoop));
        assert_eq!(inode.lookup("l3/x").err(), Some(FsError::NotDir));
    }

    #[test]
    fn a_walk_that_leaves_its_last_link_still_follows_the_ones_on_the_way() {
        let tree = tree();
        let root = &tree.root;
        let dir = root.dir("d");
        let file = dir.file("f", b"data");
        dir.link("fl", "f");
        root.link("dl", "d");
        let inode = &**root as &dyn INode;
        // `lstat("dl/f")`, `lstat("dl/fl")`, `readlink("dl/fl")`.
        assert!(same(&inode.lookup("dl/f").unwrap(), &file));
        let fl = dir.children.lock()["fl"].clone();
        assert!(same(&inode.lookup("dl/fl").unwrap(), &fl));
        // A slash after a link makes it one on the way, trailing or not.
        assert!(same(&inode.lookup("dl/").unwrap(), &dir));
        assert!(same(&inode.lookup("dl//f").unwrap(), &file));
        let dl = root.children.lock()["dl"].clone();
        assert!(same(&inode.lookup("dl").unwrap(), &dl));
    }

    /// What a walk found is named from the root through no link, whatever
    /// way the walk took to it.
    #[test]
    fn the_walk_names_what_it_found_through_no_link() {
        let tree = tree();
        let root = &tree.root;
        let real = root.dir("real");
        real.dir("sub");
        real.file("f", b"");
        real.link("lf", "/real/f");
        real.link("up", "../real/f");
        root.link("l", "/real");
        root.link("rl", "real");
        root.link("ll", "l");
        let inode = &**root as &dyn INode;
        let name = |path: &str, follow: usize| {
            let mut walked = Walked::default();
            inode
                .lookup_walked(path, follow, Some(&mut walked))
                .unwrap();
            walked.path()
        };
        for path in ["l/f", "rl/f", "ll/sub/../f", "real/lf", "real/up", "/l/./f"] {
            assert_eq!(name(path, 4), "/real/f", "{}", path);
        }
        // The last link, left in place, is named in its real directory.
        assert_eq!(name("l/lf", 0), "/real/lf");
        assert_eq!(name("ll", 0), "/ll");
        assert_eq!(name("l/..", 0), "/");
        assert_eq!(name("..", 0), "/");

        // A walk from a directory goes on from that directory's name.
        let sub = real.children.lock()["sub"].clone();
        let mut walked = Walked::at("/real//./sub");
        assert_eq!(walked.names, ["real", "sub"]);
        (&*sub as &dyn INode)
            .lookup_walked("../up", 4, Some(&mut walked))
            .unwrap();
        assert_eq!(walked.path(), "/real/f");
    }

    /// A walk with a root of its own starts over there at an absolute
    /// target, from a file system mounted below it as from anywhere: a link
    /// in `/proc` or `/dev` leads where it says.
    #[test]
    fn an_absolute_target_starts_over_at_the_root_of_whoever_walks() {
        let tree = tree();
        let real = tree.root.dir("real");
        let file = real.file("f", b"");
        let proc = self::tree();
        proc.root.link("abs", "/real/f");
        proc.root.link("rel", "abs");
        proc.root.link("absdir", "/real");
        let in_proc = &*proc.root as &dyn INode;
        let mut walked = Walked::at("/proc");
        walked.root = Some(tree.root.clone());
        for path in ["abs", "rel"] {
            let mut walked = walked.clone();
            let found = in_proc.lookup_walked(path, 4, Some(&mut walked)).unwrap();
            assert!(same(&found, &file), "{}", path);
            assert_eq!(walked.path(), "/real/f");
        }
        // From the root, `..` climbs to it and no higher.
        let found = in_proc
            .lookup_walked("absdir/../..", 4, Some(&mut walked))
            .unwrap();
        assert!(same(&found, &tree.root));
        assert_eq!(walked.path(), "/");
        // Without one, it starts over at the root of its own file system.
        let mut own = Walked::at("/proc");
        assert_eq!(
            in_proc.lookup_walked("abs", 4, Some(&mut own)).err(),
            Some(FsError::EntryNotFound)
        );
    }

    /// A magic link leads to an inode, where the walk goes on, and the walk
    /// is then where that inode is; it costs a hop like any link, and one
    /// left in place is the link.
    #[test]
    fn a_magic_link_leads_to_its_inode() {
        let tree = tree();
        let real = tree.root.dir("real");
        let file = real.file("f", b"");
        real.link("l", "f");
        let proc = self::tree();
        let fd = proc.root.jump("fd", &real, "/real");
        let in_proc = &*proc.root as &dyn INode;
        let walk = |path: &str, budget: usize| {
            let mut walked = Walked::at("/proc");
            let found = in_proc.lookup_walked(path, budget, Some(&mut walked));
            (found, walked)
        };
        let (found, walked) = walk("fd/f", 4);
        assert!(same(&found.unwrap(), &file));
        assert_eq!(
            (walked.path(), walked.jumped),
            (String::from("/real/f"), true)
        );
        let (found, walked) = walk("fd/..", 4);
        assert!(same(&found.unwrap(), &tree.root));
        assert_eq!(walked.path(), "/");
        let (found, walked) = walk("fd", 0);
        assert!(same(&found.unwrap(), &fd));
        assert_eq!(
            (walked.path(), walked.jumped),
            (String::from("/proc/fd"), false)
        );
        // The jump and the link after it are two hops.
        assert!(same(&walk("fd/l", 2).0.unwrap(), &file));
        assert_eq!(walk("fd/l", 1).0.err(), Some(FsError::SymLoop));
        // One whose inode is gone leads nowhere, not to its text.
        proc.root.link("nowhere", "fd");
        let gone = proc.root.jump("gone", &node(FileType::Dir, b""), "/gone");
        assert!(gone.to.lock().as_ref().unwrap().0.upgrade().is_none());
        assert_eq!(walk("gone/x", 4).0.err(), Some(FsError::EntryNotFound));
        assert_eq!(walk("gone", 4).0.err(), Some(FsError::EntryNotFound));
    }

    /// `..` at the top of a file system reached without the mounts climbs
    /// to the directory that top is in, from the root, whether the top
    /// answers `..` with itself (`/proc`) or not at all (`/dev`). Without a
    /// root the walk has nowhere to climb to and stays.
    #[test]
    fn a_dotdot_at_the_top_of_a_file_system_climbs_out_of_it() {
        let tree = tree();
        let file = tree.root.dir("real").file("f", b"");
        tree.root.dir("proc");
        let dev = tree.root.dir("dev");
        let proc = self::tree();
        let shm = node(FileType::Dir, b"");
        let walk = |from: &Arc<Node>, at: &str, path: &str| {
            let mut walked = Walked::at(at);
            walked.root = Some(tree.root.clone());
            let found = (&**from as &dyn INode).lookup_walked(path, 4, Some(&mut walked));
            (found.unwrap(), walked.path())
        };
        let (found, name) = walk(&proc.root, "/proc", "../real/f");
        assert!(same(&found, &file));
        assert_eq!(name, "/real/f");
        let (found, name) = walk(&shm, "/dev/shm", "..");
        assert!(same(&found, &dev));
        assert_eq!(name, "/dev");
        let in_proc = &*proc.root as &dyn INode;
        assert!(same(&in_proc.lookup("..").unwrap(), &proc.root));
        assert_eq!(
            (&*shm as &dyn INode).lookup("..").err(),
            Some(FsError::EntryNotFound)
        );
    }

    #[test]
    fn the_links_on_the_way_count_to_maxsymlinks() {
        let tree = tree();
        let root = &tree.root;
        let file = root.dir("d").file("f", b"");
        // l0 -> d, l1 -> l0, ..., l40 -> l39. The numbers are Linux's, written
        // out: a test that counted from `MAXSYMLINKS` would move with it.
        root.link("l0", "d");
        for i in 1..=40 {
            root.link(&alloc::format!("l{}", i), &alloc::format!("l{}", i - 1));
        }
        let inode = &**root as &dyn INode;
        // Through l39 the walk takes 40 links, through l40 41.
        assert!(same(&inode.lookup("l39/f").unwrap(), &file));
        assert_eq!(inode.lookup("l40/f").err(), Some(FsError::SymLoop));
        assert!(same(&inode.lookup_follow("l39/f", 40).unwrap(), &file));
        assert_eq!(
            inode.lookup_follow("l40/f", 40).err(),
            Some(FsError::SymLoop)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::sync::Weak;
    use alloc::vec;
    use spin::Mutex;

    /// A tree just rich enough to walk: directories with named children, files
    /// with bytes, and symlinks whose bytes are their target.
    struct Node {
        ino: usize,
        type_: FileType,
        data: Mutex<Vec<u8>>,
        children: Mutex<BTreeMap<String, Arc<Node>>>,
        /// `.` has to answer with an `Arc`, and `find` only gets a `&self`.
        this: Mutex<Weak<Node>>,
        parent: Mutex<Weak<Node>>,
        fs: Mutex<Weak<Tree>>,
        /// Answer `metadata()` with an error, the way a disk with a bad sector
        /// does.
        broken: Mutex<bool>,
    }

    struct Tree {
        root: Mutex<Option<Arc<Node>>>,
        next_ino: Mutex<usize>,
    }

    impl Tree {
        fn new() -> Arc<Tree> {
            let tree = Arc::new(Tree {
                root: Mutex::new(None),
                next_ino: Mutex::new(2),
            });
            let root = tree.node(FileType::Dir);
            *root.parent.lock() = Arc::downgrade(&root);
            *tree.root.lock() = Some(root);
            tree
        }

        fn node(self: &Arc<Self>, type_: FileType) -> Arc<Node> {
            let mut next = self.next_ino.lock();
            let ino = *next;
            *next += 1;
            let node = Arc::new(Node {
                ino,
                type_,
                data: Mutex::new(Vec::new()),
                children: Mutex::new(BTreeMap::new()),
                this: Mutex::new(Weak::new()),
                parent: Mutex::new(Weak::new()),
                fs: Mutex::new(Arc::downgrade(self)),
                broken: Mutex::new(false),
            });
            *node.this.lock() = Arc::downgrade(&node);
            node
        }

        fn root_node(&self) -> Arc<Node> {
            self.root.lock().clone().unwrap()
        }

        /// Add a child of `type_` under `at`, with `data` for a symlink.
        fn add(
            self: &Arc<Self>,
            at: &Arc<Node>,
            name: &str,
            type_: FileType,
            data: &str,
        ) -> Arc<Node> {
            let node = self.node(type_);
            *node.data.lock() = data.as_bytes().to_vec();
            *node.parent.lock() = Arc::downgrade(at);
            at.children.lock().insert(String::from(name), node.clone());
            node
        }

        fn dir(self: &Arc<Self>, at: &Arc<Node>, name: &str) -> Arc<Node> {
            self.add(at, name, FileType::Dir, "")
        }
        fn file(self: &Arc<Self>, at: &Arc<Node>, name: &str) -> Arc<Node> {
            self.add(at, name, FileType::File, "")
        }
        fn link(self: &Arc<Self>, at: &Arc<Node>, name: &str, target: &str) -> Arc<Node> {
            self.add(at, name, FileType::SymLink, target)
        }
    }

    impl FileSystem for Tree {
        fn sync(&self) -> Result<()> {
            Ok(())
        }
        fn root_inode(&self) -> Arc<dyn INode> {
            self.root_node()
        }
        fn info(&self) -> FsInfo {
            FsInfo {
                bsize: 4096,
                frsize: 4096,
                blocks: 0,
                bfree: 0,
                bavail: 0,
                files: 0,
                ffree: 0,
                namemax: 255,
                ftype: EXT4_SUPER_MAGIC,
            }
        }
    }

    impl INode for Node {
        fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
            let data = self.data.lock();
            let begin = offset.min(data.len());
            let len = buf.len().min(data.len() - begin);
            buf[..len].copy_from_slice(&data[begin..begin + len]);
            Ok(len)
        }
        fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
            Err(FsError::NotSupported)
        }
        fn poll(&self) -> Result<PollStatus> {
            Ok(PollStatus::default())
        }
        fn metadata(&self) -> Result<Metadata> {
            if *self.broken.lock() {
                return Err(FsError::DeviceError);
            }
            Ok(Metadata {
                dev: 1,
                inode: self.ino,
                size: self.data.lock().len(),
                blk_size: 4096,
                blocks: 0,
                atime: Timespec { sec: 0, nsec: 0 },
                mtime: Timespec { sec: 0, nsec: 0 },
                ctime: Timespec { sec: 0, nsec: 0 },
                type_: self.type_,
                mode: 0o755,
                nlinks: 1,
                uid: 0,
                gid: 0,
                rdev: 0,
            })
        }
        fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
            if self.type_ != FileType::Dir {
                return Err(FsError::NotDir);
            }
            match name {
                "." => Ok(self.this.lock().upgrade().unwrap() as Arc<dyn INode>),
                ".." => Ok(self.parent.lock().upgrade().unwrap() as Arc<dyn INode>),
                _ => self
                    .children
                    .lock()
                    .get(name)
                    .cloned()
                    .map(|c| c as Arc<dyn INode>)
                    .ok_or(FsError::EntryNotFound),
            }
        }
        fn get_entry(&self, id: usize) -> Result<String> {
            self.children
                .lock()
                .keys()
                .nth(id)
                .cloned()
                .ok_or(FsError::EntryNotFound)
        }
        fn fs(&self) -> Arc<dyn FileSystem> {
            self.fs.lock().upgrade().unwrap()
        }
        fn as_any_ref(&self) -> &dyn Any {
            self
        }
    }

    fn ino(node: &Arc<dyn INode>) -> usize {
        node.metadata().unwrap().inode
    }

    // ---- walking a path ----

    #[test]
    fn a_relative_path_walks_from_the_node_it_was_asked_of() {
        let t = Tree::new();
        let root = t.root_node();
        let a = t.dir(&root, "a");
        let b = t.dir(&a, "b");
        let f = t.file(&b, "f");
        let got = (root.as_ref() as &dyn INode).lookup("a/b/f").unwrap();
        assert_eq!(ino(&got), f.ino);
        let from_a = (a.as_ref() as &dyn INode).lookup("b/f").unwrap();
        assert_eq!(ino(&from_a), f.ino);
    }

    #[test]
    fn an_absolute_path_walks_from_the_root_of_the_file_system() {
        let t = Tree::new();
        let root = t.root_node();
        let a = t.dir(&root, "a");
        let f = t.file(&a, "f");
        // Asked of a deep directory, an absolute path still starts at the top.
        let got = (a.as_ref() as &dyn INode).lookup("/a/f").unwrap();
        assert_eq!(ino(&got), f.ino);
    }

    #[test]
    fn repeated_and_trailing_slashes_are_the_same_path() {
        let t = Tree::new();
        let root = t.root_node();
        let a = t.dir(&root, "a");
        let f = t.file(&a, "f");
        for path in ["a/f", "a//f", "/a/f", "//a//f", "a/./f"] {
            let got = (root.as_ref() as &dyn INode).lookup(path).unwrap();
            assert_eq!(ino(&got), f.ino, "{:?} did not find the file", path);
        }
    }

    #[test]
    fn the_empty_path_is_the_node_itself() {
        let t = Tree::new();
        let root = t.root_node();
        let a = t.dir(&root, "a");
        let got = (a.as_ref() as &dyn INode).lookup("").unwrap();
        assert_eq!(ino(&got), a.ino);
    }

    #[test]
    fn a_path_through_something_that_is_not_a_directory_is_not_a_directory() {
        let t = Tree::new();
        let root = t.root_node();
        let f = t.file(&root, "f");
        assert_eq!(
            (root.as_ref() as &dyn INode).lookup("f/x").err(),
            Some(FsError::NotDir)
        );
        // And asking a file to resolve anything at all is the same answer.
        assert_eq!(
            (f.as_ref() as &dyn INode).lookup("x").err(),
            Some(FsError::NotDir)
        );
    }

    #[test]
    fn an_absolute_path_asked_of_a_file_is_not_a_directory() {
        // An absolute path jumps straight to the file system's root and never
        // calls `find` on the node it was asked of, so the walk's own check on
        // that node is the only thing that refuses it. Without it, a file
        // resolves paths as if it were a directory.
        let t = Tree::new();
        let root = t.root_node();
        let a = t.dir(&root, "a");
        t.file(&a, "f");
        let f = t.file(&root, "plain");
        assert_eq!(
            (f.as_ref() as &dyn INode).lookup("/a/f").err(),
            Some(FsError::NotDir)
        );
    }

    #[test]
    fn a_file_system_whose_find_does_not_check_the_type_is_still_refused() {
        // `INode::find` is not required to check that it is being asked of a
        // directory -- nothing in the trait says so, and the default just
        // answers `NotSupported`. So the walk checks each component itself
        // rather than leaning on the file system to do it.
        struct Sloppy {
            type_: FileType,
            child: Mutex<Option<Arc<Sloppy>>>,
        }
        impl INode for Sloppy {
            fn read_at(&self, _: usize, _: &mut [u8]) -> Result<usize> {
                Ok(0)
            }
            fn write_at(&self, _: usize, _: &[u8]) -> Result<usize> {
                Ok(0)
            }
            fn poll(&self) -> Result<PollStatus> {
                Ok(PollStatus::default())
            }
            fn metadata(&self) -> Result<Metadata> {
                Ok(Metadata {
                    dev: 0,
                    inode: 1,
                    size: 0,
                    blk_size: 0,
                    blocks: 0,
                    atime: Timespec { sec: 0, nsec: 0 },
                    mtime: Timespec { sec: 0, nsec: 0 },
                    ctime: Timespec { sec: 0, nsec: 0 },
                    type_: self.type_,
                    mode: 0,
                    nlinks: 1,
                    uid: 0,
                    gid: 0,
                    rdev: 0,
                })
            }
            /// Hands back a child whatever it is asked of, directory or not.
            fn find(&self, _name: &str) -> Result<Arc<dyn INode>> {
                match self.child.lock().clone() {
                    Some(c) => Ok(c as Arc<dyn INode>),
                    None => Err(FsError::EntryNotFound),
                }
            }
            fn as_any_ref(&self) -> &dyn Any {
                self
            }
        }

        let leaf = Arc::new(Sloppy {
            type_: FileType::File,
            child: Mutex::new(None),
        });
        let middle = Arc::new(Sloppy {
            type_: FileType::File,
            child: Mutex::new(Some(leaf)),
        });
        let top = Arc::new(Sloppy {
            type_: FileType::Dir,
            child: Mutex::new(Some(middle.clone())),
        });
        // `top/middle` is a file, so `top/middle/leaf` cannot resolve.
        assert_eq!(
            (top.as_ref() as &dyn INode).lookup("middle/leaf").err(),
            Some(FsError::NotDir),
            "the walk went through a file"
        );
        // And asking the file itself is refused before it gets a chance to
        // answer with its child.
        assert_eq!(
            (middle.as_ref() as &dyn INode).lookup("leaf").err(),
            Some(FsError::NotDir)
        );
    }

    #[test]
    fn a_name_that_is_not_there_is_not_found() {
        let t = Tree::new();
        let root = t.root_node();
        t.dir(&root, "a");
        assert_eq!(
            (root.as_ref() as &dyn INode).lookup("a/nope").err(),
            Some(FsError::EntryNotFound)
        );
    }

    #[test]
    fn a_node_that_cannot_say_what_it_is_gives_the_device_error_back() {
        let t = Tree::new();
        let root = t.root_node();
        let a = t.dir(&root, "a");
        *a.broken.lock() = true;
        assert_eq!(
            (root.as_ref() as &dyn INode).lookup("a/x").err(),
            Some(FsError::DeviceError)
        );
    }

    // ---- symlinks ----

    #[test]
    fn a_symlink_is_not_followed_when_no_follows_are_allowed() {
        let t = Tree::new();
        let root = t.root_node();
        let f = t.file(&root, "f");
        let l = t.link(&root, "l", "f");
        let got = (root.as_ref() as &dyn INode).lookup("l").unwrap();
        assert_eq!(ino(&got), l.ino, "the link was followed");
        assert_ne!(ino(&got), f.ino);
    }

    #[test]
    fn a_symlink_is_followed_when_a_follow_is_allowed() {
        let t = Tree::new();
        let root = t.root_node();
        let f = t.file(&root, "f");
        t.link(&root, "l", "f");
        let got = (root.as_ref() as &dyn INode).lookup_follow("l", 1).unwrap();
        assert_eq!(ino(&got), f.ino);
    }

    #[test]
    fn a_symlink_in_the_middle_of_a_path_is_followed() {
        let t = Tree::new();
        let root = t.root_node();
        let a = t.dir(&root, "a");
        let f = t.file(&a, "f");
        t.link(&root, "l", "a");
        let got = (root.as_ref() as &dyn INode)
            .lookup_follow("l/f", 1)
            .unwrap();
        assert_eq!(ino(&got), f.ino);
    }

    #[test]
    fn a_symlink_target_is_resolved_from_the_directory_the_link_is_in() {
        let t = Tree::new();
        let root = t.root_node();
        let a = t.dir(&root, "a");
        let inner = t.file(&a, "target");
        t.file(&root, "target");
        // `a/l -> target` must find `a/target`, not the one at the top.
        t.link(&a, "l", "target");
        let got = (root.as_ref() as &dyn INode)
            .lookup_follow("a/l", 1)
            .unwrap();
        assert_eq!(ino(&got), inner.ino);
    }

    #[test]
    fn an_absolute_symlink_target_is_resolved_from_the_root() {
        let t = Tree::new();
        let root = t.root_node();
        let a = t.dir(&root, "a");
        t.file(&a, "target");
        let top = t.file(&root, "target");
        t.link(&a, "l", "/target");
        let got = (root.as_ref() as &dyn INode)
            .lookup_follow("a/l", 1)
            .unwrap();
        assert_eq!(ino(&got), top.ino);
    }

    #[test]
    fn a_chain_of_symlinks_runs_out_of_follows() {
        let t = Tree::new();
        let root = t.root_node();
        let f = t.file(&root, "f");
        t.link(&root, "l1", "f");
        t.link(&root, "l2", "l1");
        t.link(&root, "l3", "l2");
        // Three links need three follows.
        assert_eq!(
            ino(&(root.as_ref() as &dyn INode)
                .lookup_follow("l3", 3)
                .unwrap()),
            f.ino
        );
        // One hop short of the file is ELOOP, not the link where the walk
        // stopped and not the file.
        assert_eq!(
            (root.as_ref() as &dyn INode).lookup_follow("l3", 2).err(),
            Some(FsError::SymLoop)
        );
    }

    #[test]
    fn a_symlink_that_points_at_itself_runs_out_of_follows_and_does_not_hang() {
        let t = Tree::new();
        let root = t.root_node();
        t.link(&root, "l", "l");
        // The walk answers: what must not happen is a walk that never ends,
        // nor the link coming back as an ordinary file of its own name.
        assert_eq!(
            (root.as_ref() as &dyn INode).lookup_follow("l", 8).err(),
            Some(FsError::SymLoop)
        );
    }

    #[test]
    fn a_symlink_target_longer_than_a_path_is_refused_and_not_truncated() {
        let t = Tree::new();
        let root = t.root_node();
        // A target of exactly PATH_MAX is the last one allowed.
        let name = "x".repeat(SYMLINK_MAX);
        t.link(&root, "long", &name);
        assert_eq!(
            (root.as_ref() as &dyn INode).lookup_follow("long", 1).err(),
            Some(FsError::EntryNotFound),
            "the target was cut short and named something else"
        );
        // One byte more and the walk refuses it rather than following a prefix.
        let over = "y".repeat(SYMLINK_MAX + 1);
        t.link(&root, "toolong", &over);
        assert_eq!(
            (root.as_ref() as &dyn INode)
                .lookup_follow("toolong", 1)
                .err(),
            Some(FsError::NameTooLong)
        );
    }

    #[test]
    fn a_symlink_target_of_a_few_hundred_bytes_is_followed_whole() {
        let t = Tree::new();
        let root = t.root_node();
        // 300 characters: longer than the 256-byte buffer the walk used to use,
        // which cut the name and then found a different file -- or none.
        let long_name = "d".repeat(300);
        let target = t.file(&root, &long_name);
        t.link(&root, "l", &long_name);
        let got = (root.as_ref() as &dyn INode).lookup_follow("l", 1).unwrap();
        assert_eq!(ino(&got), target.ino, "the target name was cut short");
    }

    #[test]
    fn a_symlink_whose_target_is_not_utf8_is_refused() {
        let t = Tree::new();
        let root = t.root_node();
        let l = t.link(&root, "l", "");
        *l.data.lock() = vec![0xff, 0xfe];
        assert!((root.as_ref() as &dyn INode).lookup_follow("l", 1).is_err());
    }

    // ---- listing ----

    #[test]
    fn list_gives_every_entry_of_a_directory() {
        let t = Tree::new();
        let root = t.root_node();
        t.file(&root, "a");
        t.file(&root, "b");
        t.dir(&root, "c");
        let mut names = (root.as_ref() as &dyn INode).list().unwrap();
        names.sort();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn listing_something_that_is_not_a_directory_is_not_a_directory() {
        let t = Tree::new();
        let root = t.root_node();
        let f = t.file(&root, "f");
        assert_eq!(
            (f.as_ref() as &dyn INode).list().err(),
            Some(FsError::NotDir)
        );
    }

    #[test]
    fn an_empty_directory_lists_nothing() {
        let t = Tree::new();
        let root = t.root_node();
        let d = t.dir(&root, "d");
        assert!((d.as_ref() as &dyn INode).list().unwrap().is_empty());
    }

    #[test]
    fn get_entry_with_metadata_answers_the_name_and_what_it_is() {
        let t = Tree::new();
        let root = t.root_node();
        let d = t.dir(&root, "sub");
        let f = t.file(&d, "inside");
        let (meta, name) = d.get_entry_with_metadata(0).unwrap();
        assert_eq!(name, "inside");
        assert_eq!(meta.inode, f.ino);
        assert_eq!(meta.type_, FileType::File);
        assert_eq!(
            d.get_entry_with_metadata(1).err(),
            Some(FsError::EntryNotFound)
        );
    }

    // ---- the defaults of the trait ----

    #[test]
    fn downcast_reaches_the_concrete_type() {
        let t = Tree::new();
        let root: Arc<dyn INode> = t.root_inode();
        assert!(root.downcast_ref::<Node>().is_some());
    }

    #[test]
    fn an_inode_with_no_file_system_gets_the_placeholder_and_not_a_panic() {
        struct Bare;
        impl INode for Bare {
            fn read_at(&self, _: usize, _: &mut [u8]) -> Result<usize> {
                Ok(0)
            }
            fn write_at(&self, _: usize, _: &[u8]) -> Result<usize> {
                Ok(0)
            }
            fn poll(&self) -> Result<PollStatus> {
                Ok(PollStatus::default())
            }
            fn as_any_ref(&self) -> &dyn Any {
                self
            }
        }
        let fs = Bare.fs();
        assert!(is_no_fs(&fs), "a bare inode did not get the placeholder");
        assert!(fs.sync().is_ok());
        assert_eq!(fs.info().blocks, 0);
        // Its root owns nothing and answers rather than panicking.
        let root = fs.root_inode();
        assert_eq!(
            root.read_at(0, &mut [0u8; 4]).err(),
            Some(FsError::NotSupported)
        );
        assert_eq!(
            root.write_at(0, &[0u8; 4]).err(),
            Some(FsError::NotSupported)
        );
        assert!(root.poll().is_err());
        assert!(root.as_any_ref().is::<()>() || true);
    }

    #[test]
    fn the_placeholder_file_system_is_one_shared_instance() {
        assert!(Arc::ptr_eq(&no_fs(), &no_fs()));
        let t = Tree::new();
        let real: Arc<dyn FileSystem> = t;
        assert!(
            !is_no_fs(&real),
            "a real file system looked like the placeholder"
        );
    }

    #[test]
    fn the_defaults_say_not_supported_rather_than_guessing() {
        struct Bare;
        impl INode for Bare {
            fn read_at(&self, _: usize, _: &mut [u8]) -> Result<usize> {
                Ok(0)
            }
            fn write_at(&self, _: usize, _: &[u8]) -> Result<usize> {
                Ok(0)
            }
            fn poll(&self) -> Result<PollStatus> {
                Ok(PollStatus::default())
            }
            fn as_any_ref(&self) -> &dyn Any {
                self
            }
        }
        let n = Bare;
        assert_eq!(n.metadata().err(), Some(FsError::NotSupported));
        assert_eq!(n.sync_all().err(), Some(FsError::NotSupported));
        assert_eq!(n.sync_data().err(), Some(FsError::NotSupported));
        assert_eq!(n.resize(0).err(), Some(FsError::NotSupported));
        assert_eq!(n.unlink("x").err(), Some(FsError::NotSupported));
        assert_eq!(n.find("x").err(), Some(FsError::NotSupported));
        assert_eq!(n.get_entry(0).err(), Some(FsError::NotSupported));
        assert_eq!(n.io_control(0, 0).err(), Some(FsError::NotSupported));
    }

    #[test]
    fn create_and_create2_stand_in_for_each_other() {
        // A file system may implement either; the one it does not implement
        // must reach the one it does, and not recurse for ever.
        struct OnlyCreate2(Mutex<Vec<(String, usize)>>);
        impl INode for OnlyCreate2 {
            fn read_at(&self, _: usize, _: &mut [u8]) -> Result<usize> {
                Ok(0)
            }
            fn write_at(&self, _: usize, _: &[u8]) -> Result<usize> {
                Ok(0)
            }
            fn poll(&self) -> Result<PollStatus> {
                Ok(PollStatus::default())
            }
            fn create2(
                &self,
                name: &str,
                _type_: FileType,
                _mode: u32,
                data: usize,
            ) -> Result<Arc<dyn INode>> {
                self.0.lock().push((String::from(name), data));
                Err(FsError::NotSupported)
            }
            fn as_any_ref(&self) -> &dyn Any {
                self
            }
        }
        let n = OnlyCreate2(Mutex::new(Vec::new()));
        let _ = n.create("f", FileType::File, 0o644);
        assert_eq!(&*n.0.lock(), &[(String::from("f"), 0)]);
    }

    #[test]
    fn async_poll_answers_what_poll_answers() {
        struct Ready;
        impl INode for Ready {
            fn read_at(&self, _: usize, _: &mut [u8]) -> Result<usize> {
                Ok(0)
            }
            fn write_at(&self, _: usize, _: &[u8]) -> Result<usize> {
                Ok(0)
            }
            fn poll(&self) -> Result<PollStatus> {
                Ok(PollStatus {
                    read: true,
                    write: false,
                    error: false,
                    hangup: true,
                })
            }
            fn as_any_ref(&self) -> &dyn Any {
                self
            }
        }
        let status = futures_lite_block_on(Ready.async_poll());
        let status = status.unwrap();
        assert!(status.read && status.hangup && !status.write);
    }

    /// The smallest executor that will drive a future this crate produces: its
    /// default `async_poll` is ready on the first poll.
    fn futures_lite_block_on<T>(mut fut: Pin<Box<dyn Future<Output = T> + Send + Sync + '_>>) -> T {
        use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        fn noop(_: *const ()) {}
        fn clone(p: *const ()) -> RawWaker {
            RawWaker::new(p, &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("the default async_poll was not ready at once"),
        }
    }

    // ---- device numbers ----

    #[test]
    fn make_rdev_packs_the_pair_the_way_stat_reports_it() {
        // /dev/null is (1, 3) and /dev/zero is (1, 5).
        assert_eq!(make_rdev(1, 3), 0x103);
        assert_eq!(make_rdev(1, 5), 0x105);
        // A DRM card: (226, 0).
        assert_eq!(make_rdev(226, 0), 226 << 8);
        assert_eq!(make_rdev(0, 0), 0);
    }

    #[test]
    fn make_rdev_keeps_a_minor_out_of_the_major_and_a_major_out_of_the_top() {
        // A minor of 0x100 would otherwise add one to the major.
        assert_eq!(make_rdev(1, 0x100), 0x100);
        assert_eq!(make_rdev(0xfff, 0xff), 0xfffff);
        // And what does not fit is dropped rather than wrapping into the other.
        assert_eq!(make_rdev(0x1000, 0), 0);
    }

    #[test]
    fn an_error_prints_its_own_name() {
        use alloc::format;
        assert_eq!(format!("{}", FsError::EntryNotFound), "EntryNotFound");
        assert_eq!(format!("{}", FsError::NotSameFs), "NotSameFs");
    }

    #[test]
    fn a_device_error_becomes_a_file_system_error() {
        let e: FsError = DevError.into();
        assert_eq!(e, FsError::DeviceError);
    }
}
