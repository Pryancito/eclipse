//! `/dev/pts`: devpts, whose entries are the slaves of the live pairs.
//!
//! A pair a `/dev/ptmx` open makes is `/dev/pts/N` from then until its master
//! is closed, as `devpts_pty_new` and `devpts_pty_kill` make and remove the
//! node on Linux. `ls /dev/pts` lists it, `stat` of the name is the inode a
//! descriptor of the slave is, and any name of it opens it: `/dev/pts/N`, `N`
//! from `/dev/pts`, a link. `/dev/pts` used to be an empty directory of devfs
//! and only the exact string `/dev/pts/N` opened a slave, so `ttyname` of a
//! pty, which reads `/proc/self/fd/N` and compares that name's `stat` with the
//! descriptor's, failed: `ENOENT` with musl, and `ENODEV` with glibc once it
//! had searched `/dev/pts` for the device and found nothing there.
//!
//! There are no mount options: `/dev/pts` behaves as systemd and OpenRC mount
//! it, `gid=5,mode=620,ptmxmode=000`, which is what its line in
//! `/proc/mounts` says. The kernel's own default is `mode=600` and the
//! opener's group, which nothing a desktop runs on uses.

use super::*;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// `DEVPTS_SUPER_MAGIC`.
pub const DEVPTS_SUPER_MAGIC: u64 = 0x1cd1;
/// The `tty` group, `gid=5`.
pub const TTY_GID: u32 = 5;
/// `mode=620`: the owner reads and writes, the `tty` group writes (`write`
/// and `wall` are setgid `tty`, and `mesg n` takes that bit away).
const SLAVE_MODE: u16 = 0o620;
/// `UNIX98_PTY_SLAVE_MAJOR`. The minor is the number, all twenty bits of it.
const SLAVE_MAJOR: usize = 136;
/// `s_blocksize` of devpts (`devpts_fill_super`), the `st_blksize` of
/// everything in it.
const BLOCK_SIZE: usize = 1024;

/// `st_dev` of `/dev/pts` and of every slave: a device of its own, as every
/// devpts instance is.
pub fn dev() -> usize {
    lazy_static! {
        static ref DEV: usize = rcore_fs::vfs::anon_dev();
    }
    *DEV
}

/// Who a slave belongs to and its mode: the fsuid of whoever opened the
/// master, the `tty` group and `mode=620` to begin with, then what a `chown`
/// or a `chmod` of it makes them.
pub(super) struct Owner {
    uid: u32,
    gid: u32,
    mode: u16,
}

impl Owner {
    pub(super) fn of(uid: u32) -> Self {
        Owner {
            uid,
            gid: TTY_GID,
            mode: SLAVE_MODE,
        }
    }
}

/// What `stat` of `/dev/pts/N` and `fstat` of a descriptor of the slave say,
/// which `ttyname` needs to be one inode. Its number is `N + 3`, as devpts
/// numbers them: the root is 1 and `ptmx` 2.
pub(super) fn slave_metadata(pty: &Pty) -> Metadata {
    let owner = pty.owner.lock();
    Metadata {
        dev: dev(),
        inode: pty.id as usize + 3,
        size: 0,
        blk_size: BLOCK_SIZE,
        blocks: 0,
        atime: Timespec { sec: 0, nsec: 0 },
        mtime: Timespec { sec: 0, nsec: 0 },
        ctime: Timespec { sec: 0, nsec: 0 },
        type_: FileType::CharDevice,
        mode: owner.mode,
        nlinks: 1,
        uid: owner.uid as usize,
        gid: owner.gid as usize,
        rdev: make_rdev(SLAVE_MAJOR, pty.id as usize),
    }
}

/// `chown` and `chmod` of a slave, by its name or by a descriptor of it: what
/// `simple_setattr` keeps. Who may make them is the syscall's to ask.
pub(super) fn set_slave_metadata(pty: &Pty, metadata: &Metadata) -> Result<()> {
    let mut owner = pty.owner.lock();
    owner.uid = metadata.uid as u32;
    owner.gid = metadata.gid as u32;
    owner.mode = metadata.mode & 0o7777;
    Ok(())
}

/// The pair `/dev/pts/<name>` names: `name` written as devpts writes the
/// number (`"%d"`), so neither `01` nor `+1` is `1`.
fn number(name: &str) -> Option<u32> {
    let id: u32 = name.parse().ok()?;
    (id.to_string() == name).then_some(id)
}

/// A slave as `/dev/pts` names it. It is only a name until it is opened
/// ([`PtsNode::open`]): `stat`, `chmod` and an `O_PATH` descriptor reach the
/// pair without counting as an open slave.
pub struct PtsNode {
    pty: Arc<Pty>,
}

impl PtsNode {
    /// Open the slave this names, as opening `/dev/pts/N` does. `None` once
    /// its master is closed.
    pub fn open(&self) -> Option<Arc<dyn INode>> {
        open_slave(self.pty.clone())
    }
}

impl INode for PtsNode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus::default())
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(slave_metadata(&self.pty))
    }
    fn set_metadata(&self, metadata: &Metadata) -> Result<()> {
        set_slave_metadata(&self.pty, metadata)
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(DevPtsFs)
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// The root of `/dev/pts`.
pub struct DevPts;

lazy_static! {
    static ref ROOT: Arc<DevPts> = Arc::new(DevPts);
}

/// `/dev/pts`, for devfs to hold.
pub fn root() -> Arc<dyn INode> {
    ROOT.clone()
}

/// The pairs whose master is open, by number.
fn live() -> Vec<(u32, Arc<Pty>)> {
    PTYS.lock()
        .iter()
        .filter(|(_, pty)| !pty.master_closed.load(Ordering::Relaxed))
        .map(|(id, pty)| (*id, pty.clone()))
        .collect()
}

impl INode for DevPts {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Err(FsError::IsDir)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::IsDir)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus::default())
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: dev(),
            inode: 1,
            size: 0,
            blk_size: BLOCK_SIZE,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::Dir,
            mode: 0o755,
            nlinks: 2,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            // The top of a filesystem the walk reaches without the mounts, as
            // `/proc` is: the walk finds what `..` is (`Walked::up`).
            "." | ".." => Ok(root()),
            name => {
                let id = number(name).ok_or(FsError::EntryNotFound)?;
                let (_, pty) = live()
                    .into_iter()
                    .find(|(n, _)| *n == id)
                    .ok_or(FsError::EntryNotFound)?;
                Ok(Arc::new(PtsNode { pty }))
            }
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        match id {
            0 => Ok(String::from(".")),
            1 => Ok(String::from("..")),
            i => live()
                .get(i - 2)
                .map(|(n, _)| n.to_string())
                .ok_or(FsError::EntryNotFound),
        }
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(DevPtsFs)
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// What `statfs` of `/dev/pts`, or of a slave, answers: `simple_statfs`.
pub struct DevPtsFs;

impl FileSystem for DevPtsFs {
    fn sync(&self) -> Result<()> {
        Ok(())
    }
    fn root_inode(&self) -> Arc<dyn INode> {
        root()
    }
    fn info(&self) -> FsInfo {
        FsInfo {
            bsize: kernel_hal::PAGE_SIZE,
            frsize: kernel_hal::PAGE_SIZE,
            blocks: 0,
            bfree: 0,
            bavail: 0,
            files: 0,
            ffree: 0,
            namemax: 255,
            ftype: DEVPTS_SUPER_MAGIC,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slave_is_named_by_its_number_as_devpts_writes_it() {
        assert_eq!(number("0"), Some(0));
        assert_eq!(number("42"), Some(42));
        for name in ["", "ptmx", "-1", "+1", "01", "1 ", "4294967296"] {
            assert_eq!(number(name), None, "{:?}", name);
        }
    }
}
