//! The two `fcntl` families that are not about descriptors at all.
//!
//! `fcntl(2)` is a junk drawer, and two of the things in it have nothing to
//! do with file descriptors: the **seals** of a memfd, and the **capacity**
//! of a pipe. Both are served before the `FcntlCmd` table is even consulted,
//! by four command numbers that live as `const`s inside the function body --
//! which is why nothing checked them. A `const` in a function body is
//! invisible to every test in the crate, so it was not that the test was
//! missing, it was that there was no way to write one.
//!
//! The numbers are the whole ABI here. `F_LINUX_SPECIFIC_BASE` is 1024 and
//! the four sit at +7, +8, +9 and +10, so each is one away from its twin:
//! swap `F_SETPIPE_SZ` with `F_GETPIPE_SZ`, or `F_ADD_SEALS` with
//! `F_GET_SEALS`, and every call still succeeds while doing the other thing.
//! Each pair is therefore asked to do its own job AND to leave the other's
//! alone: a `F_GET_SEALS` that quietly added one, or a `F_GETPIPE_SZ` that
//! resized the pipe, is what these tests are for.
//!
//! Why it matters, in the words of the comment above the seals branch:
//! Firefox seals every shared-memory segment against shrinking and then
//! refuses to map any segment whose `F_GET_SEALS` does not report it
//! (`Platform::IsSafeToMap`), so a kernel that answers "no seals set"
//! fails every IPC buffer and every frame. And the pipe capacity is what
//! `F_SETPIPE_SZ` callers resize before a big splice; Linux answers with the
//! rounded capacity rather than zero, and a caller that believes a zero
//! sizes its next write to nothing.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::fs::{File, OpenFlags, F_SEAL_GROW, F_SEAL_SEAL, F_SEAL_SHRINK, F_SEAL_WRITE};
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// `F_LINUX_SPECIFIC_BASE` and the four commands this module is about, as
/// `<linux/fcntl.h>` numbers them. Written out rather than taken from the
/// kernel, which declares them inside the function body where no test can
/// reach them -- and where a second copy of an ABI number can drift.
const F_LINUX_SPECIFIC_BASE: usize = 1024;
const F_SETPIPE_SZ: usize = F_LINUX_SPECIFIC_BASE + 7;
const F_GETPIPE_SZ: usize = F_LINUX_SPECIFIC_BASE + 8;
const F_ADD_SEALS: usize = F_LINUX_SPECIFIC_BASE + 9;
const F_GET_SEALS: usize = F_LINUX_SPECIFIC_BASE + 10;

/// A command number `fcntl` has no name for at all.
const A_COMMAND_OF_NOBODY: usize = 999;

/// A descriptor this harness never hands out.
const A_CLOSED_FD: usize = 100;

/// What pipe(7) says a pipe holds before anybody resizes it, and the floor
/// and ceiling `F_SETPIPE_SZ` rounds into: one page up, one mebibyte down
/// (the value `/proc/sys/fs/pipe-max-size` advertises).
const PIPE_DEFAULT: usize = 65536;
const PIPE_MIN: usize = 4096;
const OVER_THE_PIPE_MAX: usize = 1024 * 1024 + 1;

/// A seal bit that is not one of the five Linux defines.
const A_SEAL_OF_NOBODY: usize = 0x8000;

/// `MFD_ALLOW_SEALING` (`<linux/memfd.h>`). Without it a memfd is created
/// already carrying `F_SEAL_SEAL` -- Linux's rule, and the reason the first
/// thing this module asks for is a memfd that can still be sealed.
const MFD_ALLOW_SEALING: usize = 0x0002;

/// A process whose root holds a `/<top>` of its own, because a lookup from
/// the root goes through a path cache every process in this binary shares.
fn a_process(pid: KoID, name: &str, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

/// A descriptor on an ordinary file of `dir_path`: a `File` like a memfd is
/// a `File`, and like a pipe end is a `File`, so it is what says these two
/// families look at more than the descriptor's class.
fn an_ordinary_file(lp: &LinuxProcess, dir_path: &str) -> usize {
    let dir = lp.lookup_inode(dir_path).unwrap();
    let inode = dir.create("plain", FileType::File, 0o644).unwrap();
    let fd: i32 = lp
        .add_file(File::new(
            inode,
            OpenFlags::RDWR,
            alloc::format!("{}/plain", dir_path),
        ))
        .unwrap()
        .into();
    fd as usize
}

#[test]
fn the_seals_of_a_memfd_are_read_and_added_and_neither_command_does_the_other_s_job() {
    // `F_ADD_SEALS` and `F_GET_SEALS` are one number apart and share a
    // branch, so the questions are: does each do its own thing, does each
    // leave the other's alone, and is the seal word that comes back the one
    // that went in. Seals accumulate and are never removed, which is the
    // whole guarantee, so they are added one at a time and read between.
    let proc = a_process(48_220, "seals", "seals");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let name = b"a-segment\0";
            let memfd = FileDesc::from(
                sys.sys_memfd_create(UserInPtr::from(name.as_ptr() as usize), MFD_ALLOW_SEALING)
                    .unwrap() as i32,
            );

            assert_eq!(
                sys.sys_fcntl(memfd, F_GET_SEALS, 0).await,
                Ok(0),
                "a memfd created to allow sealing came with seals already on it"
            );

            // A read must not be a write: asking for the seals while passing
            // a seal bit as the argument has to leave the memfd unsealed.
            assert_eq!(
                sys.sys_fcntl(memfd, F_GET_SEALS, F_SEAL_SHRINK as usize)
                    .await,
                Ok(0)
            );
            assert_eq!(
                sys.sys_fcntl(memfd, F_GET_SEALS, 0).await,
                Ok(0),
                "F_GET_SEALS added the seal it was handed"
            );

            assert_eq!(
                sys.sys_fcntl(memfd, F_ADD_SEALS, F_SEAL_SHRINK as usize)
                    .await,
                Ok(0)
            );
            assert_eq!(
                sys.sys_fcntl(memfd, F_GET_SEALS, 0).await,
                Ok(F_SEAL_SHRINK as usize),
                "the seal that was added is not the seal that is reported"
            );

            // And they accumulate rather than replace: this is what Firefox
            // relies on when it seals a segment twice over.
            assert_eq!(
                sys.sys_fcntl(memfd, F_ADD_SEALS, F_SEAL_GROW as usize)
                    .await,
                Ok(0)
            );
            assert_eq!(
                sys.sys_fcntl(memfd, F_GET_SEALS, 0).await,
                Ok((F_SEAL_SHRINK | F_SEAL_GROW) as usize),
                "a second seal replaced the first instead of joining it"
            );

            // A bit Linux does not define is refused, and refusing it must
            // not have taken the bits beside it either.
            assert_eq!(
                sys.sys_fcntl(memfd, F_ADD_SEALS, A_SEAL_OF_NOBODY | F_SEAL_WRITE as usize)
                    .await,
                Err(LxError::EINVAL),
                "a seal bit nobody defines was accepted"
            );
            assert_eq!(
                sys.sys_fcntl(memfd, F_GET_SEALS, 0).await,
                Ok((F_SEAL_SHRINK | F_SEAL_GROW) as usize),
                "the refused request sealed something anyway"
            );

            // `F_SEAL_SEAL` closes the door on sealing itself: everything
            // after it is EPERM, and the seals already there stay readable.
            assert_eq!(
                sys.sys_fcntl(memfd, F_ADD_SEALS, F_SEAL_SEAL as usize)
                    .await,
                Ok(0)
            );
            assert_eq!(
                sys.sys_fcntl(memfd, F_ADD_SEALS, F_SEAL_WRITE as usize)
                    .await,
                Err(LxError::EPERM),
                "a memfd sealed against sealing took another seal"
            );
            assert_eq!(
                sys.sys_fcntl(memfd, F_GET_SEALS, 0).await,
                Ok((F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL) as usize)
            );

            // A memfd created WITHOUT `MFD_ALLOW_SEALING` is born sealed
            // shut, which is memfd_create(2)'s rule and the reason the one
            // above had to ask for sealing: it reports `F_SEAL_SEAL` from
            // the start and refuses every seal with EPERM, through the same
            // two commands.
            let shut = FileDesc::from(
                sys.sys_memfd_create(UserInPtr::from(name.as_ptr() as usize), 0)
                    .unwrap() as i32,
            );
            assert_eq!(
                sys.sys_fcntl(shut, F_GET_SEALS, 0).await,
                Ok(F_SEAL_SEAL as usize),
                "a memfd created without MFD_ALLOW_SEALING can still be sealed"
            );
            assert_eq!(
                sys.sys_fcntl(shut, F_ADD_SEALS, F_SEAL_SHRINK as usize)
                    .await,
                Err(LxError::EPERM)
            );

            // Seals exist only on memfds. An ordinary file is a `File` just
            // like a memfd is, so this is the pair that says the branch asks
            // the inode and not the descriptor's class.
            let plain = FileDesc::from(an_ordinary_file(&lp, "/seals") as i32);
            assert_eq!(
                sys.sys_fcntl(plain, F_GET_SEALS, 0).await,
                Err(LxError::EINVAL),
                "an ordinary file reported a seal word"
            );
            assert_eq!(
                sys.sys_fcntl(plain, F_ADD_SEALS, F_SEAL_WRITE as usize)
                    .await,
                Err(LxError::EINVAL),
                "an ordinary file took a seal"
            );

            // And a descriptor that is not a file at all is refused one
            // layer earlier, by the downcast.
            let evt = FileDesc::from(sys.sys_eventfd2(0, 0).unwrap() as i32);
            assert_eq!(
                sys.sys_fcntl(evt, F_GET_SEALS, 0).await,
                Err(LxError::EINVAL),
                "an eventfd reported a seal word"
            );
        })
    });
}

#[test]
fn the_capacity_of_a_pipe_is_read_and_set_and_the_answer_is_the_rounded_size() {
    // `F_SETPIPE_SZ` answers with the capacity it actually set, not zero:
    // the request is rounded up to a power of two and floored at one page,
    // so a caller that sized its next splice from a zero would write
    // nothing. `F_GETPIPE_SZ` must then agree with that answer, and must
    // not itself resize anything when handed a size.
    let proc = a_process(48_221, "pipesz", "pipesz");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut fds = [0_i32; 2];
            assert_eq!(
                sys.sys_pipe2(UserOutPtr::from(fds.as_mut_ptr() as usize), 0),
                Ok(0)
            );
            let (r, w) = (FileDesc::from(fds[0]), FileDesc::from(fds[1]));

            assert_eq!(
                sys.sys_fcntl(r, F_GETPIPE_SZ, 0).await,
                Ok(PIPE_DEFAULT),
                "a fresh pipe does not hold what pipe(7) says it holds"
            );

            // A read must not be a write.
            assert_eq!(
                sys.sys_fcntl(r, F_GETPIPE_SZ, PIPE_MIN).await,
                Ok(PIPE_DEFAULT)
            );
            assert_eq!(
                sys.sys_fcntl(r, F_GETPIPE_SZ, 0).await,
                Ok(PIPE_DEFAULT),
                "F_GETPIPE_SZ resized the pipe it was asked about"
            );

            // The answer to a set is the size that was set, and both ends
            // see it: the capacity belongs to the pipe, not to a descriptor.
            assert_eq!(
                sys.sys_fcntl(w, F_SETPIPE_SZ, 8192).await,
                Ok(8192),
                "F_SETPIPE_SZ did not answer with the capacity it set"
            );
            assert_eq!(sys.sys_fcntl(r, F_GETPIPE_SZ, 0).await, Ok(8192));

            // A size that is not a power of two is rounded UP, and the
            // rounded value is what comes back.
            assert_eq!(
                sys.sys_fcntl(w, F_SETPIPE_SZ, 8193).await,
                Ok(16384),
                "a request between two powers of two was not rounded up"
            );
            assert_eq!(sys.sys_fcntl(r, F_GETPIPE_SZ, 0).await, Ok(16384));

            // Below one page the floor applies.
            assert_eq!(sys.sys_fcntl(w, F_SETPIPE_SZ, 1).await, Ok(PIPE_MIN));
            assert_eq!(sys.sys_fcntl(r, F_GETPIPE_SZ, 0).await, Ok(PIPE_MIN));

            // Zero and anything over the maximum are refused, each in its
            // own way, and neither changes the capacity.
            assert_eq!(
                sys.sys_fcntl(w, F_SETPIPE_SZ, 0).await,
                Err(LxError::EINVAL)
            );
            assert_eq!(
                sys.sys_fcntl(w, F_SETPIPE_SZ, OVER_THE_PIPE_MAX).await,
                Err(LxError::EPERM)
            );
            assert_eq!(
                sys.sys_fcntl(r, F_GETPIPE_SZ, 0).await,
                Ok(PIPE_MIN),
                "a refused resize changed the capacity anyway"
            );

            // Shrinking below what is already queued is EBUSY, as
            // `pipe_set_size` answers: the bytes have nowhere to go.
            let bytes = [b'x'; 8192];
            assert_eq!(
                sys.sys_fcntl(w, F_SETPIPE_SZ, 8192).await,
                Ok(8192),
                "the pipe would not grow back"
            );
            assert_eq!(
                sys.sys_write(w, UserInPtr::from(bytes.as_ptr() as usize), 8192)
                    .await,
                Ok(8192)
            );
            assert_eq!(
                sys.sys_fcntl(w, F_SETPIPE_SZ, PIPE_MIN).await,
                Err(LxError::EBUSY),
                "the pipe shrank below the bytes it was already holding"
            );
            assert_eq!(
                sys.sys_fcntl(r, F_GETPIPE_SZ, 0).await,
                Ok(8192),
                "the refused shrink took effect anyway"
            );
        })
    });
}

#[test]
fn the_pipe_capacity_commands_are_only_for_pipes_and_an_unknown_command_is_refused() {
    // Linux answers `EBADF` for `F_SETPIPE_SZ` on a descriptor that is not a
    // pipe -- not `EINVAL`, which is what the seals next door answer, and not
    // `ENOTTY`. Two kinds of non-pipe are asked, because the refusal comes
    // from two different `ok_or`s: an ordinary file passes the `File`
    // downcast and fails the `Pipe` one, and an eventfd fails the first.
    let proc = a_process(48_222, "notapipe", "notapipe");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let plain = FileDesc::from(an_ordinary_file(&lp, "/notapipe") as i32);
            let evt = FileDesc::from(sys.sys_eventfd2(0, 0).unwrap() as i32);

            for fd in [plain, evt] {
                assert_eq!(
                    sys.sys_fcntl(fd, F_GETPIPE_SZ, 0).await,
                    Err(LxError::EBADF),
                    "something that is not a pipe answered with a capacity"
                );
                assert_eq!(
                    sys.sys_fcntl(fd, F_SETPIPE_SZ, 8192).await,
                    Err(LxError::EBADF),
                    "something that is not a pipe took a capacity"
                );
            }

            // A command number `fcntl` has no name for is EINVAL and not a
            // panic, a zero, or the nearest command it does know.
            assert_eq!(
                sys.sys_fcntl(plain, A_COMMAND_OF_NOBODY, 0).await,
                Err(LxError::EINVAL),
                "a command number nobody defines was served"
            );

            // And the descriptor is looked up before any of the four
            // commands is: a closed one is EBADF even for the two that
            // would otherwise answer EINVAL.
            let closed = FileDesc::from(A_CLOSED_FD as i32);
            for cmd in [F_GET_SEALS, F_ADD_SEALS, F_GETPIPE_SZ, F_SETPIPE_SZ] {
                assert_eq!(
                    sys.sys_fcntl(closed, cmd, 0).await,
                    Err(LxError::EBADF),
                    "command {} was served before the descriptor was looked up",
                    cmd
                );
            }
        })
    });
}
