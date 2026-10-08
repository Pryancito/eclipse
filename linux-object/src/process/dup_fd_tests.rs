//! What a second descriptor for an already-open file shares with the
//! first, and what it does not.
//!
//! `dup(2)`, `dup2(2)`, `dup3(2)`, `fcntl(F_DUPFD)` and `pidfd_getfd(2)`
//! all install the SAME open file description under another descriptor --
//! `fs/file.c` does `get_file(file)` and stores that one pointer -- so the
//! two descriptors share the file offset and the status flags. The only
//! thing the new descriptor owns is `FD_CLOEXEC`, which lives in the fd
//! table and not in the description.
//!
//! This kernel had it the other way round: every `FileLike` carried a
//! hand-written `dup` that built a NEW object, and the syscalls wrote the
//! close-on-exec flag back onto it. A shell's `prog >log 2>&1` therefore
//! gave stdout and stderr an offset each, both starting at zero, and the
//! second stream wrote over the first from the beginning of the file.

use super::*;
use crate::fs::SeekFrom;
use rcore_fs::vfs::{FsError, PollStatus, Timespec};

/// An inode that remembers where each write landed, so a test can see the
/// offset a second descriptor actually used.
pub(super) struct Log {
    writes: Mutex<Vec<(usize, usize)>>,
}

impl Log {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Log {
            writes: Mutex::new(Vec::new()),
        })
    }
    fn offsets(&self) -> Vec<usize> {
        self.writes.lock().iter().map(|(off, _)| *off).collect()
    }
    fn end(&self) -> usize {
        self.writes
            .lock()
            .iter()
            .map(|(off, len)| off + len)
            .max()
            .unwrap_or(0)
    }
}

impl INode for Log {
    fn read_at(&self, _: usize, _: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
        Err(FsError::NotSupported)
    }
    fn write_at(&self, offset: usize, buf: &[u8]) -> rcore_fs::vfs::Result<usize> {
        self.writes.lock().push((offset, buf.len()));
        Ok(buf.len())
    }
    fn poll(&self) -> rcore_fs::vfs::Result<PollStatus> {
        Err(FsError::NotSupported)
    }
    fn metadata(&self) -> rcore_fs::vfs::Result<Metadata> {
        Ok(Metadata {
            dev: 1,
            inode: 11,
            size: self.end(),
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
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }
}

pub(super) fn a_process() -> LinuxProcess {
    LinuxProcess {
        root_inode: Log::new(),
        parent: Mutex::new(Weak::default()),
        vt: 0,
        perf: crate::perf::ProcPerf::new(),
        futexes: Default::default(),
        itimers: Default::default(),
        aspace_lock: Mutex::new(()),
        inner: Mutex::new(LinuxProcessInner::default()),
    }
}

/// `sh -c 'prog >log'`: one open file description, opened for writing.
pub(super) fn an_open_log(inode: Arc<Log>, flags: OpenFlags) -> Arc<dyn FileLike> {
    File::new(inode, flags, String::from("/var/log/prog.log"))
}

fn file_at(proc: &LinuxProcess, fd: FileDesc) -> Arc<File> {
    proc.get_file_like(fd)
        .unwrap()
        .downcast_arc::<File>()
        .ok()
        .unwrap()
}

/// The `2>&1` bug, at the fd table: `dup2` must not give the second
/// descriptor an offset of its own. With a copy, both start at zero and
/// the second stream writes over the first from the top of the file.
#[test]
fn two_descriptors_for_one_file_share_the_offset() {
    let proc = a_process();
    let log = Log::new();
    let stdout = proc
        .add_file(an_open_log(log.clone(), OpenFlags::WRONLY))
        .unwrap();
    // `dup2(stdout, stderr)`.
    let stderr = FileDesc::from(2);
    proc.replace_file(stderr, proc.get_file_like(stdout).unwrap(), false)
        .unwrap();

    proc.get_file_like(stdout)
        .unwrap()
        .write(b"hello\n")
        .unwrap();
    proc.get_file_like(stderr)
        .unwrap()
        .write(b"world\n")
        .unwrap();

    assert_eq!(
        log.offsets(),
        vec![0, 6],
        "the second stream must continue the file, not restart it"
    );
}

/// The same rule the other way: a seek through one descriptor moves the
/// other. `dup(2)`: "the two file descriptors ... share file offset and
/// file status flags".
#[test]
fn a_seek_through_one_descriptor_moves_the_other() {
    let proc = a_process();
    let log = Log::new();
    let one = proc.add_file(an_open_log(log, OpenFlags::RDWR)).unwrap();
    let two = proc
        .add_file_cloexec(proc.get_file_like(one).unwrap(), false)
        .unwrap();

    file_at(&proc, one).seek(SeekFrom::Start(4096)).unwrap();
    assert_eq!(
        file_at(&proc, two).seek(SeekFrom::Current(0)).unwrap(),
        4096
    );
}

/// And the two descriptors are literally one object, which is the reason
/// for both of the above.
#[test]
fn the_table_hands_out_the_same_description_for_both_descriptors() {
    let proc = a_process();
    let one = proc
        .add_file(an_open_log(Log::new(), OpenFlags::RDWR))
        .unwrap();
    let two = proc
        .add_file_cloexec(proc.get_file_like(one).unwrap(), false)
        .unwrap();
    assert!(Arc::ptr_eq(
        &proc.get_file_like(one).unwrap(),
        &proc.get_file_like(two).unwrap()
    ));
}

/// The status flags are part of the description, so `fcntl(F_SETFL)`
/// through either descriptor is in force on both.
#[test]
fn the_status_flags_are_shared() {
    let proc = a_process();
    let one = proc
        .add_file(an_open_log(Log::new(), OpenFlags::RDWR))
        .unwrap();
    let two = proc
        .add_file_cloexec(proc.get_file_like(one).unwrap(), false)
        .unwrap();

    let flags = proc.get_file_like(two).unwrap().flags();
    proc.get_file_like(two)
        .unwrap()
        .set_flags(flags | OpenFlags::NON_BLOCK)
        .unwrap();
    assert!(proc.get_file_like(one).unwrap().flags().non_block());
}

/// `FD_CLOEXEC` is the one thing that is NOT shared. An `O_CLOEXEC` file
/// gives its first descriptor the flag, and a `dup` of it must not carry
/// the flag over -- POSIX says the copy is created with `FD_CLOEXEC`
/// clear, which is how `sh` installs an `O_CLOEXEC` fd as stdout and
/// expects it to survive the exec.
#[test]
fn a_dup_starts_close_on_exec_clear_even_from_an_o_cloexec_file() {
    let proc = a_process();
    let one = proc
        .add_file(an_open_log(
            Log::new(),
            OpenFlags::WRONLY | OpenFlags::CLOEXEC,
        ))
        .unwrap();
    assert!(proc.fd_cloexec(one).unwrap(), "opened with O_CLOEXEC");

    let two = proc
        .add_file_cloexec(proc.get_file_like(one).unwrap(), false)
        .unwrap();
    assert!(!proc.fd_cloexec(two).unwrap(), "the dup is not");
    assert!(
        proc.fd_cloexec(one).unwrap(),
        "and the original keeps its own flag"
    );
}

/// Per-descriptor in both directions: marking one does not mark the other,
/// and neither shows up in the shared object's flags.
#[test]
fn marking_one_descriptor_close_on_exec_leaves_the_other_alone() {
    let proc = a_process();
    let one = proc
        .add_file(an_open_log(Log::new(), OpenFlags::WRONLY))
        .unwrap();
    let two = proc
        .add_file_cloexec(proc.get_file_like(one).unwrap(), false)
        .unwrap();

    proc.set_fd_cloexec(two, true).unwrap();
    assert!(proc.fd_cloexec(two).unwrap());
    assert!(!proc.fd_cloexec(one).unwrap());
    assert!(
        !proc.get_file_like(one).unwrap().flags().close_on_exec(),
        "the description is not where this flag lives"
    );
}

/// `dup2(2)` leaves the target descriptor close-on-exec CLEAR, whatever
/// either side had: the old flag goes with the old entry, and the new
/// one does not come from the description being installed. Both halves
/// matter -- a copying `dup` got the first by writing the flag onto its
/// copy, and the second only by accident.
#[test]
fn dup2_leaves_the_target_close_on_exec_clear_whatever_the_two_sides_had() {
    let proc = a_process();
    let marked_target = proc
        .add_file(an_open_log(
            Log::new(),
            OpenFlags::WRONLY | OpenFlags::CLOEXEC,
        ))
        .unwrap();
    assert!(proc.fd_cloexec(marked_target).unwrap());
    let plain_source = proc
        .add_file(an_open_log(Log::new(), OpenFlags::WRONLY))
        .unwrap();
    proc.replace_file(
        marked_target,
        proc.get_file_like(plain_source).unwrap(),
        false,
    )
    .unwrap();
    assert!(
        !proc.fd_cloexec(marked_target).unwrap(),
        "the target's own flag went with the entry it replaced"
    );

    // And the other way: an `O_CLOEXEC` description installed over a
    // plain descriptor must not bring its flag along.
    let plain_target = proc
        .add_file(an_open_log(Log::new(), OpenFlags::WRONLY))
        .unwrap();
    let marked_source = proc
        .add_file(an_open_log(
            Log::new(),
            OpenFlags::WRONLY | OpenFlags::CLOEXEC,
        ))
        .unwrap();
    proc.replace_file(
        plain_target,
        proc.get_file_like(marked_source).unwrap(),
        false,
    )
    .unwrap();
    assert!(
        !proc.fd_cloexec(plain_target).unwrap(),
        "O_CLOEXEC is not a property of the description a dup2 installs"
    );
    assert!(
        proc.fd_cloexec(marked_source).unwrap(),
        "and the source descriptor keeps its own"
    );
}

/// `pidfd_getfd(2)`: "the close-on-exec flag is set on the file
/// descriptor" -- on the new one, and it must not follow the description
/// back to the descriptor the target process is still using.
#[test]
fn pidfd_getfd_marks_only_the_descriptor_it_creates() {
    let target = a_process();
    let theirs = target
        .add_file(an_open_log(Log::new(), OpenFlags::RDWR))
        .unwrap();
    let caller = a_process();
    let ours = caller
        .add_file_cloexec(target.get_file_like(theirs).unwrap(), true)
        .unwrap();

    assert!(caller.fd_cloexec(ours).unwrap());
    assert!(!target.fd_cloexec(theirs).unwrap());
}

/// A descriptor created by OPENING something does take its close-on-exec
/// from the `O_CLOEXEC` that opened it -- the rule `opened_cloexec`
/// states, and the reason the dup paths have to say otherwise explicitly.
#[test]
fn opening_registers_the_o_cloexec_that_was_asked_for() {
    let proc = a_process();
    let plain = proc
        .add_file(an_open_log(Log::new(), OpenFlags::RDONLY))
        .unwrap();
    let tagged = proc
        .add_file(an_open_log(
            Log::new(),
            OpenFlags::RDONLY | OpenFlags::CLOEXEC,
        ))
        .unwrap();
    assert!(!proc.fd_cloexec(plain).unwrap());
    assert!(proc.fd_cloexec(tagged).unwrap());
}

/// The same rule for a descriptor that is not a file: an eventfd's
/// counter and its status flags both belong to the description, so a dup
/// of a `EFD_NONBLOCK` eventfd reads the same counter and loses
/// `O_NONBLOCK` when the other descriptor clears it. Seventeen
/// hand-written `dup`s got the first half right and the second wrong.
#[test]
fn a_dup_of_an_eventfd_shares_the_counter_and_the_flags() {
    use crate::fs::EventFd;
    let proc = a_process();
    let one = proc
        .add_file(EventFd::new(0, OpenFlags::NON_BLOCK))
        .unwrap();
    let two = proc
        .add_file_cloexec(proc.get_file_like(one).unwrap(), false)
        .unwrap();

    proc.get_file_like(one)
        .unwrap()
        .write(&7u64.to_ne_bytes())
        .unwrap();
    let mut buf = [0u8; 8];
    let n = async_std::task::block_on(proc.get_file_like(two).unwrap().read(&mut buf)).unwrap();
    assert_eq!((n, u64::from_ne_bytes(buf)), (8, 7), "one counter");

    proc.get_file_like(two)
        .unwrap()
        .set_flags(OpenFlags::empty())
        .unwrap();
    assert!(
        !proc.get_file_like(one).unwrap().flags().non_block(),
        "one set of status flags"
    );
}

/// And the whole point of the flag: the exec sweep closes the marked
/// descriptor and leaves its twin, even though both name one description.
#[test]
fn the_exec_sweep_closes_the_marked_descriptor_and_keeps_its_twin() {
    let proc = a_process();
    let kept = proc
        .add_file(an_open_log(Log::new(), OpenFlags::WRONLY))
        .unwrap();
    let swept = proc
        .add_file_cloexec(proc.get_file_like(kept).unwrap(), true)
        .unwrap();

    proc.remove_cloexec_files();
    assert!(proc.get_file_like(kept).is_ok());
    assert_eq!(proc.get_file_like(swept).err(), Some(LxError::EBADF));
}
