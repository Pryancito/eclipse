//! Which sync each of the five durability syscalls actually asks for.
//!
//! They all return `Ok(0)` on a ramfs, and that is the problem: `sync`,
//! `fsync`, `fdatasync`, `syncfs` and `sync_file_range` differ **only** in
//! which method they reach for -- `FileSystem::sync`, `INode::sync_all`,
//! `INode::sync_data`, or none at all -- and `RamFS` answers every one of
//! them with `Ok(())` without counting anything. So a kernel where
//! `fsync` quietly calls `sync_data`, or where `syncfs` syncs one file, or
//! where `sync_file_range` syncs nothing at all, passes a suite that only
//! looks at return values.
//!
//! Which is not a small difference to get wrong. `fdatasync` exists so a
//! database can commit a record without paying for the inode's metadata;
//! `fsync` promises the metadata too, and a journal that gets `sync_data`
//! when it asked for `fsync` loses the size of the file it just grew.
//! `syncfs` is one filesystem and `sync` is all of them. None of that is
//! visible from a return value, so this module hands the syscalls a file
//! that **counts** what it was asked for.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use linux_object::fs::{File, OpenFlags};
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::{FileSystem, FileType, FsError, Metadata, PollStatus, Timespec};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// A descriptor number this harness never hands out.
const A_CLOSED_FD: usize = 100;

/// A length userspace wrote as `-1`: `loff_t` is signed and `EINVAL` is the
/// answer, whichever of these calls reads it.
const A_NEGATIVE_LENGTH: usize = -1_i64 as usize;

/// How many times each kind of sync arrived, and whether the device is
/// pretending to be broken.
#[derive(Default)]
struct Tally {
    all: AtomicUsize,
    data: AtomicUsize,
    fs: AtomicUsize,
    broken: AtomicBool,
}

impl Tally {
    fn counts(&self) -> (usize, usize, usize) {
        (
            self.all.load(Ordering::SeqCst),
            self.data.load(Ordering::SeqCst),
            self.fs.load(Ordering::SeqCst),
        )
    }

    /// What a sync answers once the device is broken. `NoDeviceSpace` maps
    /// to `ENOSPC`, which is what a real failing sync reports and what
    /// every caller tests for.
    fn answer(&self) -> rcore_fs::vfs::Result<()> {
        if self.broken.load(Ordering::SeqCst) {
            Err(FsError::NoDeviceSpace)
        } else {
            Ok(())
        }
    }
}

/// A file whose only job is to say which sync it was asked for.
struct Counted {
    tally: Arc<Tally>,
}

impl rcore_fs::vfs::INode for Counted {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
        Ok(0)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> rcore_fs::vfs::Result<usize> {
        Ok(buf.len())
    }

    fn poll(&self) -> rcore_fs::vfs::Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: true,
            error: false,
            hangup: false,
        })
    }

    /// A regular file, because that is what these calls are for and what
    /// `readahead` insists on.
    fn metadata(&self) -> rcore_fs::vfs::Result<Metadata> {
        Ok(Metadata {
            dev: 0,
            inode: 1,
            size: 0,
            blk_size: 4096,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::File,
            mode: 0o644,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }

    fn sync_all(&self) -> rcore_fs::vfs::Result<()> {
        self.tally.all.fetch_add(1, Ordering::SeqCst);
        self.tally.answer()
    }

    fn sync_data(&self) -> rcore_fs::vfs::Result<()> {
        self.tally.data.fetch_add(1, Ordering::SeqCst);
        self.tally.answer()
    }

    /// A fresh handle to the same filesystem each time. Sharing the tally
    /// through an `Arc` and building the filesystem on demand is what keeps
    /// this out of the inode-holds-fs-holds-inode cycle a real filesystem
    /// needs a `Weak` for.
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(CountedFs {
            tally: self.tally.clone(),
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// The filesystem that file belongs to, which counts its own sync.
struct CountedFs {
    tally: Arc<Tally>,
}

impl FileSystem for CountedFs {
    fn sync(&self) -> rcore_fs::vfs::Result<()> {
        self.tally.fs.fetch_add(1, Ordering::SeqCst);
        self.tally.answer()
    }

    fn root_inode(&self) -> Arc<dyn rcore_fs::vfs::INode> {
        Arc::new(Counted {
            tally: self.tally.clone(),
        })
    }

    fn info(&self) -> linux_object::process::FsInfo {
        linux_object::process::FsInfo {
            bsize: 0,
            frsize: 0,
            blocks: 0,
            bfree: 0,
            bavail: 0,
            files: 0,
            ffree: 0,
            namemax: 0,
        }
    }
}

/// A process whose root holds a `/<top>` of its own, because a lookup from
/// the root goes through a path cache every process in this binary shares.
fn a_process(pid: KoID, name: &str, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

/// A descriptor on a counting file, opened as `flags` says, and the tally
/// it reports into. The file is not in any directory: these calls name it
/// by descriptor and never by path.
fn a_counting_file(lp: &LinuxProcess, flags: OpenFlags) -> (usize, Arc<Tally>) {
    let tally = Arc::new(Tally::default());
    let inode = Arc::new(Counted {
        tally: tally.clone(),
    });
    let fd: i32 = lp
        .add_file(File::new(inode, flags, String::from("/counted")))
        .unwrap()
        .into();
    (fd as usize, tally)
}

/// A descriptor on a node of `type_` in the process's own ramfs, for the
/// calls that refuse one.
fn a_node_named(lp: &LinuxProcess, dir_path: &str, name: &str, type_: FileType) -> usize {
    let dir = lp.lookup_inode(dir_path).unwrap();
    let inode = dir.create(name, type_, 0o666).unwrap();
    let path = alloc::format!("{}/{}", dir_path, name);
    let fd: i32 = lp
        .add_file(File::new(inode, OpenFlags::RDWR, path))
        .unwrap()
        .into();
    fd as usize
}

#[test]
fn fsync_syncs_the_whole_file_and_fdatasync_only_its_data() {
    // The one difference between the two, and the only place it shows: the
    // method each of them reaches for. Asked of one descriptor in turn, so
    // the counters carry over and a call that reached the wrong method is
    // visible as the wrong counter moving.
    let proc = a_process(45_900, "sync-two", "sync_two");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let (fd, tally) = a_counting_file(&lp, OpenFlags::RDWR);

            assert_eq!(sys.sys_fsync(FileDesc::from(fd)), Ok(0));
            assert_eq!(
                tally.counts(),
                (1, 0, 0),
                "fsync did not ask for the file's metadata as well as its data"
            );

            assert_eq!(sys.sys_fdatasync(FileDesc::from(fd)), Ok(0));
            assert_eq!(
                tally.counts(),
                (1, 1, 0),
                "fdatasync asked for more (or less) than the file's data"
            );
        })
    });
}

#[test]
fn syncfs_syncs_the_filesystem_the_descriptor_is_on_and_not_the_one_file() {
    // `syncfs(2)` takes a descriptor but is not about that file: it is
    // "commit the filesystem this descriptor lives on". A kernel that
    // served it with `sync_all` would be an `fsync` under another name,
    // and a program calling it to flush a whole tree would flush one file.
    let proc = a_process(45_901, "sync-fs", "sync_fs");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let (fd, tally) = a_counting_file(&lp, OpenFlags::RDWR);

            assert_eq!(sys.sys_syncfs(FileDesc::from(fd)), Ok(0));
            assert_eq!(
                tally.counts(),
                (0, 0, 1),
                "syncfs did not reach the filesystem of that descriptor"
            );
        })
    });
}

#[test]
fn sync_goes_to_the_process_root_and_leaves_the_descriptors_alone() {
    // `sync(2)` takes no argument at all, so the only thing it can mean
    // here is the process's own root -- and what it must NOT be is the
    // descriptor-shaped call next to it. The counting file is open the
    // whole time and its filesystem is not the root's, so a `sync` that
    // had wandered onto a descriptor would show up on the tally.
    let proc = a_process(45_902, "sync-root", "sync_root");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let (_fd, tally) = a_counting_file(&lp, OpenFlags::RDWR);

            assert_eq!(sys.sys_sync(), Ok(0));
            assert_eq!(
                tally.counts(),
                (0, 0, 0),
                "sync reached a filesystem that is not the process's root"
            );
        })
    });
}

#[test]
fn a_device_that_cannot_sync_says_so_through_every_one_of_the_three() {
    // Every one of these calls exists to be *believed*: a caller that is
    // told the data is down and finds out later that it is not has no
    // second chance. So the error has to come back, and it has to come
    // back from each of the three separately -- they each have their own
    // `?` and losing one of them is invisible from the other two.
    //
    // The counters are checked afterwards as well: the error must be the
    // device's answer and not a refusal that never got there.
    let proc = a_process(45_903, "sync-broken", "sync_broken");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let (fd, tally) = a_counting_file(&lp, OpenFlags::RDWR);
            tally.broken.store(true, Ordering::SeqCst);
            let fd = FileDesc::from(fd);

            assert_eq!(
                sys.sys_fsync(fd),
                Err(LxError::ENOSPC),
                "fsync reported success on a device that could not sync"
            );
            assert_eq!(
                sys.sys_fdatasync(fd),
                Err(LxError::ENOSPC),
                "fdatasync reported success on a device that could not sync"
            );
            assert_eq!(
                sys.sys_syncfs(fd),
                Err(LxError::ENOSPC),
                "syncfs reported success on a device that could not sync"
            );
            assert_eq!(
                tally.counts(),
                (1, 1, 1),
                "one of the three never reached the device at all"
            );
        })
    });
}

#[test]
fn sync_file_range_writes_back_only_when_it_is_asked_to() {
    // `sync_file_range(2)` is the one call in this family that can be
    // asked for *nothing*: with a flag word of zero it is a no-op, which
    // is a documented way to validate arguments. Serving that one with a
    // `sync_data` would be harmless; serving a real `SYNC_FILE_RANGE_WRITE`
    // *without* one is the silent lie, so both halves are here.
    let proc = a_process(45_904, "sync-range", "sync_range");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let (fd, tally) = a_counting_file(&lp, OpenFlags::RDWR);
            let fd = FileDesc::from(fd);

            assert_eq!(sys.sys_sync_file_range(fd, 0, 0, 0), Ok(0));
            assert_eq!(
                tally.counts(),
                (0, 0, 0),
                "a flag word of zero wrote something back anyway"
            );

            assert_eq!(
                sys.sys_sync_file_range(fd, 0, 0, SYNC_FILE_RANGE_WRITE),
                Ok(0)
            );
            assert_eq!(
                tally.counts(),
                (0, 1, 0),
                "SYNC_FILE_RANGE_WRITE did not write anything back"
            );
        })
    });
}

#[test]
fn sync_file_range_judges_its_flags_then_its_range_then_its_descriptor() {
    // Three answers about three arguments, and which one a caller gets is
    // an order: `ksys_sync_file_range` reads the flags, then the range,
    // then looks up the descriptor. The same closed descriptor is used for
    // all three, so nothing but the argument under test can account for
    // the difference.
    let proc = a_process(45_905, "sync-order", "sync_order");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let closed = FileDesc::from(A_CLOSED_FD);
            // A bit outside the three the call defines.
            let unknown_flag = 8;

            assert_eq!(
                sys.sys_sync_file_range(closed, A_NEGATIVE_LENGTH, 0, unknown_flag),
                Err(LxError::EINVAL),
                "the flag word was read after the range or the descriptor"
            );
            assert_eq!(
                sys.sys_sync_file_range(closed, A_NEGATIVE_LENGTH, 0, 0),
                Err(LxError::EINVAL),
                "a negative offset was taken, or judged after the descriptor"
            );
            assert_eq!(
                sys.sys_sync_file_range(closed, 0, A_NEGATIVE_LENGTH, 0),
                Err(LxError::EINVAL),
                "a negative length was taken, or judged after the descriptor"
            );
            assert_eq!(
                sys.sys_sync_file_range(closed, 0, 0, 0),
                Err(LxError::EBADF),
                "the descriptor this test relies on being closed was open"
            );
        })
    });
}

#[test]
fn readahead_wants_a_readable_descriptor_on_something_with_a_page_cache() {
    // Nothing here has a page cache to fill ahead of a read, so
    // `readahead(2)` is a successful no-op -- which makes **the errors the
    // man page requires** the whole of its contract, and a call that
    // answered `Ok(0)` to everything would look identical on a file.
    //
    // The order matters too: `ksys_readahead` asks about the descriptor's
    // access mode before it asks what kind of file it is, so a write-only
    // descriptor on a FIFO is `EBADF` and not `EINVAL`.
    let proc = a_process(45_906, "readahead", "readahead_t");
    let dir_path = String::from("/readahead_t");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let (readable, _tally) = a_counting_file(&lp, OpenFlags::RDWR);
            let (write_only, _wtally) = a_counting_file(&lp, OpenFlags::WRONLY);
            let fifo = a_node_named(&lp, &dir_path, "fifo", FileType::NamedPipe);

            assert_eq!(
                sys.sys_readahead(FileDesc::from(readable), 0, 4096),
                Ok(0),
                "readahead refused a readable regular file"
            );
            assert_eq!(
                sys.sys_readahead(FileDesc::from(write_only), 0, 4096),
                Err(LxError::EBADF),
                "readahead accepted a descriptor it may not read through"
            );
            assert_eq!(
                sys.sys_readahead(FileDesc::from(fifo), 0, 4096),
                Err(LxError::EINVAL),
                "readahead accepted something with no page cache to fill"
            );
            assert_eq!(
                sys.sys_readahead(FileDesc::from(readable), 0, A_NEGATIVE_LENGTH),
                Err(LxError::EINVAL),
                "readahead took a negative count"
            );
            assert_eq!(
                sys.sys_readahead(FileDesc::from(A_CLOSED_FD), 0, 4096),
                Err(LxError::EBADF)
            );
        })
    });
}
