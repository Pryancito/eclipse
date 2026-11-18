//! The filesystems a [`Volume`](crate::volume::Volume) serves: btrfs and
//! EclipseFS, whose code is the kernel's (`vendor/btrfs-rs`,
//! `vendor/eclipsefs-rs`). Both keep their trees in memory and write them
//! out when asked to, and both have the same calls; they differ in how a
//! writing of the trees reaches the disk. btrfs writes its tree blocks
//! where they were ([`Tree::IN_PLACE`]), so the volume makes each writing
//! one group of the disk's journal. EclipseFS copies on write and writes
//! its superblock last, so whatever a server that died left in the disk's
//! memory is a whole volume, the last commit or the one before; its
//! generation says which.

use std::sync::Arc;

use fatserve::disk::Blocks;

use crate::dev::Dev;
use crate::volume::Errno;

/// The kind of a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Regular,
    Dir,
    Symlink,
    Fifo,
    Socket,
    /// A device node, which a served mount does not make.
    Device,
}

/// A file's attributes.
#[derive(Clone, Debug)]
pub struct Stat {
    pub kind: Kind,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub rdev: u64,
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
}

/// An entry of a directory.
pub struct Entry {
    pub ino: u64,
    pub name: String,
}

/// The numbers of `statfs`: block size, bytes, bytes used.
pub struct Numbers {
    pub block_size: u64,
    pub total_bytes: u64,
    pub bytes_used: u64,
}

/// A filesystem the volume serves, with the calls it uses.
pub trait Tree: Sized {
    /// Its name in `/proc/mounts`.
    const NAME: &'static str;
    /// The `statfs` magic the kernel's own mount of it says.
    const MAGIC: u64;
    /// Whether a writing of the trees overwrites the blocks that were
    /// there (btrfs), or only writes new ones before the superblock
    /// (EclipseFS).
    const IN_PLACE: bool;
    /// Mount the volume on `dev`.
    fn mount<B: Blocks + Send + 'static>(dev: Dev<B>, read_only: bool) -> Result<Self, String>;
    /// What a first change does before any, and the volume taking the
    /// whole of a larger device: whether it grew.
    fn prepare(&mut self) -> Result<bool, Errno>;
    /// The number of the last writing of the trees that reached the disk
    /// (EclipseFS's generation); btrfs keeps none that is of use here.
    fn generation(&self) -> u64;
    /// The trees go to the disk.
    fn sync(&mut self) -> Result<(), Errno>;
    fn root_ino(&self) -> u64;
    fn lookup(&mut self, dir: u64, name: &str) -> Result<u64, Errno>;
    fn stat(&mut self, ino: u64) -> Result<Stat, Errno>;
    fn readdir(&mut self, dir: u64) -> Result<Vec<Entry>, Errno>;
    fn read(&mut self, ino: u64, offset: u64, out: &mut [u8]) -> Result<usize, Errno>;
    fn read_link(&mut self, ino: u64) -> Result<Vec<u8>, Errno>;
    fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize, Errno>;
    fn write_symlink(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize, Errno>;
    fn create(&mut self, dir: u64, name: &str, kind: Kind, perms: u32) -> Result<u64, Errno>;
    fn unlink(&mut self, dir: u64, name: &str) -> Result<(), Errno>;
    fn rename(&mut self, from: u64, name: &str, to: u64, to_name: &str) -> Result<(), Errno>;
    fn truncate(&mut self, ino: u64, len: u64) -> Result<(), Errno>;
    /// Each that is given: permissions, owner, group, the last write.
    fn set_attr(
        &mut self,
        ino: u64,
        perms: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        mtime: Option<i64>,
    ) -> Result<(), Errno>;
    fn numbers(&self) -> Numbers;
}

fn wall_clock() -> (u64, u32) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (now.as_secs(), now.subsec_nanos())
}

/// The calls both crates have alike, in the terms of [`Tree`]: `$fs` is
/// the crate.
macro_rules! common {
    ($fs:ident) => {
        fn root_ino(&self) -> u64 {
            self.root_ino()
        }

        fn lookup(&mut self, dir: u64, name: &str) -> Result<u64, Errno> {
            self.lookup(dir, name).map_err(errno)
        }

        fn stat(&mut self, ino: u64) -> Result<Stat, Errno> {
            let st = self.stat(ino).map_err(errno)?;
            use $fs::FileKind as K;
            let kind = match st.kind {
                K::Regular => Kind::Regular,
                K::Dir => Kind::Dir,
                K::Symlink => Kind::Symlink,
                K::Fifo => Kind::Fifo,
                K::Socket => Kind::Socket,
                K::CharDevice | K::BlockDevice => Kind::Device,
            };
            Ok(Stat {
                kind,
                mode: st.mode,
                nlink: st.nlink,
                uid: st.uid,
                gid: st.gid,
                size: st.size,
                rdev: st.rdev,
                atime: st.atime.0 as i64,
                mtime: st.mtime.0 as i64,
                ctime: st.ctime.0 as i64,
            })
        }

        fn readdir(&mut self, dir: u64) -> Result<Vec<Entry>, Errno> {
            Ok(self
                .readdir(dir)
                .map_err(errno)?
                .into_iter()
                .map(|e| Entry {
                    ino: e.ino,
                    name: e.name,
                })
                .collect())
        }

        fn read(&mut self, ino: u64, offset: u64, out: &mut [u8]) -> Result<usize, Errno> {
            self.read(ino, offset, out).map_err(errno)
        }

        fn read_link(&mut self, ino: u64) -> Result<Vec<u8>, Errno> {
            self.read_link(ino).map_err(errno)
        }

        fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize, Errno> {
            self.write(ino, offset, data).map_err(errno)
        }

        fn write_symlink(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<usize, Errno> {
            self.write_symlink(ino, offset, data).map_err(errno)
        }

        fn create(&mut self, dir: u64, name: &str, kind: Kind, perms: u32) -> Result<u64, Errno> {
            use $fs::FileKind as K;
            let kind = match kind {
                Kind::Regular => K::Regular,
                Kind::Dir => K::Dir,
                Kind::Symlink => K::Symlink,
                Kind::Fifo => K::Fifo,
                Kind::Socket => K::Socket,
                Kind::Device => return Err(libc::EOPNOTSUPP),
            };
            self.create(dir, name, kind, perms, 0).map_err(errno)
        }

        fn unlink(&mut self, dir: u64, name: &str) -> Result<(), Errno> {
            self.unlink(dir, name).map_err(errno)
        }

        fn rename(&mut self, from: u64, name: &str, to: u64, to_name: &str) -> Result<(), Errno> {
            self.rename(from, name, to, to_name).map_err(errno)
        }

        fn truncate(&mut self, ino: u64, len: u64) -> Result<(), Errno> {
            self.truncate(ino, len).map_err(errno)
        }

        fn set_attr(
            &mut self,
            ino: u64,
            perms: Option<u32>,
            uid: Option<u32>,
            gid: Option<u32>,
            mtime: Option<i64>,
        ) -> Result<(), Errno> {
            let mtime = mtime.map(|secs| (secs.max(0) as u64, 0));
            self.set_attr(ino, perms, uid, gid, None, mtime)
                .map_err(errno)
        }

        fn numbers(&self) -> Numbers {
            let st = self.fsinfo();
            Numbers {
                block_size: st.block_size,
                total_bytes: st.total_bytes,
                bytes_used: st.bytes_used,
            }
        }
    };
}

mod btrfs_tree {
    use super::*;
    use btrfs::Btrfs;

    fn errno(e: btrfs::Error) -> Errno {
        use btrfs::Error as E;
        match e {
            E::Io | E::Corrupt(_) => libc::EIO,
            E::BadSuperblock | E::Invalid => libc::EINVAL,
            E::Unsupported(_) => libc::EOPNOTSUPP,
            E::NotFound => libc::ENOENT,
            E::Exists => libc::EEXIST,
            E::NotDir => libc::ENOTDIR,
            E::IsDir => libc::EISDIR,
            E::NotEmpty => libc::ENOTEMPTY,
            E::NoSpace => libc::ENOSPC,
        }
    }

    impl Tree for Btrfs {
        const NAME: &'static str = "btrfs";
        const MAGIC: u64 = 0x9123_683e;
        const IN_PLACE: bool = true;
        fn mount<B: Blocks + Send + 'static>(dev: Dev<B>, read_only: bool) -> Result<Self, String> {
            let dev: Arc<dyn btrfs::BlockDevice> = Arc::new(dev);
            let mut fs = Btrfs::mount(dev, read_only).map_err(|e| format!("{e:?}"))?;
            fs.set_clock(wall_clock);
            // The trees are written when the volume says, and only then.
            fs.set_commit_interval(u32::MAX);
            Ok(fs)
        }

        fn prepare(&mut self) -> Result<bool, Errno> {
            // A free space tree the code does not keep is marked stale.
            self.prepare_writes().map_err(errno)?;
            self.grow_to_device().map_err(errno)
        }

        fn generation(&self) -> u64 {
            0
        }

        fn sync(&mut self) -> Result<(), Errno> {
            Btrfs::sync(self).map_err(errno)
        }

        common!(btrfs);
    }
}

mod eclipsefs_tree {
    use super::*;
    use eclipsefs::EclipseFS;

    fn errno(e: eclipsefs::Error) -> Errno {
        use eclipsefs::Error as E;
        match e {
            E::Io | E::Corrupt(_) => libc::EIO,
            E::BadSuperblock | E::Invalid => libc::EINVAL,
            E::Unsupported(_) => libc::EOPNOTSUPP,
            E::NotFound => libc::ENOENT,
            E::Exists => libc::EEXIST,
            E::NotDir => libc::ENOTDIR,
            E::IsDir => libc::EISDIR,
            E::NotEmpty => libc::ENOTEMPTY,
            E::NoSpace => libc::ENOSPC,
            E::NoKey => libc::ENOKEY,
        }
    }

    fn random(out: &mut [u8]) {
        // SAFETY: the call writes at most `out.len()` bytes into `out`.
        let mut done = 0;
        while done < out.len() {
            let n =
                unsafe { libc::getrandom(out[done..].as_mut_ptr().cast(), out.len() - done, 0) };
            if n <= 0 {
                break;
            }
            done += n as usize;
        }
    }

    impl Tree for EclipseFS {
        const NAME: &'static str = "eclipsefs";
        /// What the kernel's EclipseFS mount says (`EXT4_SUPER_MAGIC`).
        const MAGIC: u64 = 0xef53;
        const IN_PLACE: bool = false;
        fn mount<B: Blocks + Send + 'static>(dev: Dev<B>, read_only: bool) -> Result<Self, String> {
            let dev: Arc<dyn eclipsefs::BlockDevice> = Arc::new(dev);
            let probe = eclipsefs::probe(&*dev).map_err(|e| format!("{e:?}"))?;
            if probe.num_devices > 1 {
                return Err("a volume of several devices is mounted by the kernel".into());
            }
            let mut fs = EclipseFS::mount(dev, read_only).map_err(|e| format!("{e:?}"))?;
            if fs.format_version() < 4 {
                // v0.3 writes in place, with a journal of its own.
                return Err("a v0.3 image is mounted by the kernel".into());
            }
            fs.set_clock(wall_clock);
            fs.set_random(random);
            // Commits when the volume says: a clock and an interval that
            // never passes.
            fs.set_commit_interval(Some(u64::MAX));
            Ok(fs)
        }

        fn prepare(&mut self) -> Result<bool, Errno> {
            self.grow_to_device().map_err(errno)
        }

        fn generation(&self) -> u64 {
            EclipseFS::generation(self)
        }

        fn sync(&mut self) -> Result<(), Errno> {
            EclipseFS::sync(self).map_err(errno)
        }

        common!(eclipsefs);
    }
}
