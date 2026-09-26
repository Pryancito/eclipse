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

    /// Lookup path from current INode, and do not follow symlinks
    pub fn lookup(&self, path: &str) -> Result<Arc<dyn INode>> {
        self.lookup_follow(path, 0)
    }

    /// Lookup path from current INode, and follow symlinks at most `follow_times` times
    ///
    /// With `follow_times == 0` no symlink is followed: one met as the last
    /// component is returned as it is (`lstat`, `readlink`), one met on the
    /// way is `NotDir`. With a budget, running out of it is `SymLoop`, as
    /// `ELOOP` on Linux, never the symlink itself: a link that points at
    /// itself used to come back as an ordinary file whose contents were its
    /// own name.
    pub fn lookup_follow(&self, path: &str, follow_times: usize) -> Result<Arc<dyn INode>> {
        lookup_with_budget(self, path, follow_times, follow_times > 0)
    }
}

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
    follow_times: usize,
    following: bool,
) -> Result<Arc<dyn INode>> {
    if start.metadata()?.type_ != FileType::Dir {
        return Err(FsError::NotDir);
    }

    // handle absolute path
    let (mut result, mut rest_path) = if let Some(rest) = path.strip_prefix('/') {
        (start.fs().root_inode(), String::from(rest))
    } else {
        (start.find(".")?, String::from(path))
    };

    while !rest_path.is_empty() {
        if result.metadata()?.type_ != FileType::Dir {
            return Err(FsError::NotDir);
        }
        let name;
        match rest_path.find('/') {
            None => {
                name = rest_path;
                rest_path = String::new();
            }
            Some(pos) => {
                name = String::from(&rest_path[0..pos]);
                rest_path = String::from(&rest_path[pos + 1..]);
            }
        };
        if name.is_empty() {
            continue;
        }
        let inode = result.find(&name)?;
        // Handle symlink. `following` stays true after the budget is spent, so
        // the next link is `SymLoop` rather than the link itself. A walk that
        // was never asked to follow (`follow_times == 0`) leaves the link in
        // place: the last component for `lstat`, `NotDir` if the path goes on.
        if inode.metadata()?.type_ == FileType::SymLink && following {
            if follow_times == 0 {
                return Err(FsError::SymLoop);
            }
            let link_path = read_symlink(&*inode)?;
            // result remains unchanged: a relative target is resolved from the
            // directory that holds the link.
            let new_path = link_path + "/" + &rest_path;
            return lookup_with_budget(&*result, &new_path, follow_times - 1, following);
        } else {
            result = inode
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

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Timespec {
    pub sec: i64,
    pub nsec: i32,
}

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
    DirRemoved,    // E_NOENT, when the current dir was remove by a previous unlink
    DirNotEmpty,   // E_NOTEMPTY
    WrongFs,       // E_INVAL, when we find the content on disk is wrong when opening the device
    DeviceError,
    IOCTLError,
    NoDevice,
    Again,          // E_AGAIN, when no data is available, never happens in fs
    TimedOut,       // E_TIME (62), a wait reached its deadline -- e.g. DRM_IOCTL_SYNCOBJ_WAIT
    SymLoop,        // E_LOOP
    NameTooLong,    // E_NAMETOOLONG, e.g. a symlink target past `SYMLINK_MAX`
    Busy,           // E_BUSY
    ReadOnly,       // E_ROFS
    Interrupted,    // E_INTR
    NoPermission,   // E_ACCES, e.g. modeset ioctls on a DRM render node
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
