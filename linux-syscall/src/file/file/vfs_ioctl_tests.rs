//! The five ioctls the VFS answers for every kind of descriptor, and the
//! errno every other number comes back as.
//!
//! `sys_ioctl` is the largest uncovered block in this file by a wide margin:
//! 213 of its 229 lines never ran. Most of what is left is DRM routing that
//! belongs to the graphics side. What does not is the part Linux keeps in
//! `fs/ioctl.c` `do_vfs_ioctl`: five numbers that apply to EVERY fd kind --
//! pipes, sockets, plain files, device nodes -- which is exactly why no
//! per-inode handler ever sees them, and why nothing below this layer can
//! be asked about them.
//!
//! Each of the five was added for a crash that reached a user. `FIONBIO`
//! falling through to "Not a tty" aborted every `Command::output()` in
//! std -- that was lunarbar's respawn loop -- and `FIONREAD` doing the same
//! killed Firefox's Wayland proxy. The numbers themselves are pinned against
//! the headers next door in `ioctl_number_tests`; what nothing checked is
//! that the arm behind each number does the thing the number means.
//!
//! The two pairs are the reason that matters. `FIONBIO` and `FIOASYNC` are
//! the same five lines with a different flag bit, and `FIOCLEX` and
//! `FIONCLEX` differ in a `true` and a `false`. Asking one number for one
//! answer cannot tell either pair apart, so each test below asks one arm to
//! change its own bit and leave its twin's alone.
//!
//! The tail of the function is the other half: what an ioctl nobody handled
//! comes back as. Three rules stack there -- `ENOSYS` becomes `ENOTTY`, an
//! `EINVAL` from a number in the terminal block becomes `ENOTTY` unless the
//! fd is a char device, and a char device that cannot answer `TIOCGWINSZ`
//! gets a window size synthesised for it -- and each rule has a boundary
//! that a number on the wrong side of must NOT cross. The inode here is one
//! that answers a chosen error to every command, which is the only way to
//! put a number on each side of those boundaries: a ramfs file answers
//! `ENOSYS` to all of them and so cannot tell the first rule from the
//! second.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use core::any::Any;
use linux_object::fs::{File, OpenFlags};
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::{FileSystem, FileType, FsError, Metadata, PollStatus, Timespec};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// `F_GETFD` and `F_GETFL` (`<fcntl.h>`), the two commands that read back
/// what these ioctls are supposed to have changed. Written as the numbers
/// userspace sends rather than through `FcntlCmd`, which is the code the
/// answer is being checked against.
const F_GETFD: usize = 1;
const F_GETFL: usize = 3;

/// `SEEK_SET`, the whence userspace sends to put the position at an exact
/// offset. `<unistd.h>` says 0.
const SEEK_SET: usize = 0;

/// `O_NONBLOCK` and `O_ASYNC` as `F_GETFL` reports them.
const O_NONBLOCK: usize = 1 << 11;
const O_ASYNC: usize = 1 << 13;

/// A descriptor this harness never hands out.
const A_CLOSED_FD: usize = 100;

/// Ten bytes, so what is left to read can be read off the answer: the byte
/// at offset N is the digit N.
const TEN_DIGITS: &[u8; 10] = b"0123456789";

/// A request number in no block this kernel serves, for the questions that
/// are about the descriptor and not about the number.
const A_NUMBER_OF_NOBODY: usize = 0x1234;

/// `DRM_IOCTL_MODE_CREATE_LEASE` as libdrm's `_IOWR` builds it: type `'d'`
/// (0x64), direction read-write, so bit 31 is set.
const A_DRM_NUMBER: usize = 0xc018_64c6;

/// The same number after the variadic `ioctl(int, unsigned long, ...)` has
/// sign-extended libdrm's `int` to 64 bits, which is how it really arrives.
const A_DRM_NUMBER_SIGN_EXTENDED: usize = 0xffff_ffff_c018_64c6;

/// The bottom and the top of the terminal block `tty_block_wants_enotty`
/// covers, and one number just outside each end. The block is
/// `0x5400..=FIOQSIZE` inclusive at both ends, so 0x5400 and 0x5460 are in
/// it and 0x53ff and 0x5461 are not.
const JUST_BELOW_THE_TTY_BLOCK: usize = 0x53ff;
const THE_BOTTOM_OF_THE_TTY_BLOCK: usize = 0x5400;
const JUST_ABOVE_THE_TTY_BLOCK: usize = 0x5461;

/// What the inode's own `io_control` answers, which is the input the
/// normalisation in `sys_ioctl` works on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Answer {
    /// `FsError::IOCTLError`, which arrives as `EINVAL`: what sfs says
    /// about every command it does not know.
    Einval,
    /// `FsError::NotSupported`, which arrives as `ENOSYS`: what ramfs and
    /// the default `INode::io_control` say.
    Enosys,
}

/// An inode that answers one chosen error to every ioctl and reports one
/// chosen file type. Those two are the whole input to the three rules at
/// the end of `sys_ioctl`.
struct Quirky {
    kind: FileType,
    answer: Answer,
}

impl Quirky {
    fn open(lp: &LinuxProcess, kind: FileType, answer: Answer) -> usize {
        let fd: i32 = lp
            .add_file(File::new(
                Arc::new(Quirky { kind, answer }),
                OpenFlags::RDWR,
                String::from("/quirky"),
            ))
            .unwrap()
            .into();
        fd as usize
    }
}

impl rcore_fs::vfs::INode for Quirky {
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

    fn io_control(&self, _cmd: u32, _data: usize) -> rcore_fs::vfs::Result<usize> {
        Err(match self.answer {
            Answer::Einval => FsError::IOCTLError,
            Answer::Enosys => FsError::NotSupported,
        })
    }

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
            type_: self.kind,
            mode: 0o644,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        RamFS::new()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

/// A process whose root holds a `/<top>` of its own, because a lookup from
/// the root goes through a path cache every process in this binary shares.
fn a_process(pid: KoID, name: &str, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

/// A file of `dir_path` holding `content`, opened read-write with its
/// cursor at zero, and the descriptor it got.
fn a_file_with(lp: &LinuxProcess, dir_path: &str, content: &[u8]) -> usize {
    let dir = lp.lookup_inode(dir_path).unwrap();
    let inode = dir.create("f", FileType::File, 0o644).unwrap();
    inode.write_at(0, content).unwrap();
    let path = alloc::format!("{}/f", dir_path);
    let fd: i32 = lp
        .add_file(File::new(inode, OpenFlags::RDWR, path))
        .unwrap()
        .into();
    fd as usize
}

/// What `-e` reads as, the way a syscall's answer carries it.
fn errno(e: LxError) -> SysResult {
    Err(e)
}

#[test]
fn the_two_look_alike_flag_ioctls_each_move_only_their_own_bit() {
    // `FIONBIO` and `FIOASYNC` are the same arm twice over: read an int
    // through the pointer, set or clear one flag by whether it is zero.
    // Swap the two bits and every call still succeeds, which is why this
    // asks each number to leave the other's flag where it found it. The
    // ints are a two-word array so the width of the read is pinned too:
    // the second word is the one that says `on`, so a read of eight bytes
    // where four were meant turns an "off" into an "on".
    let proc = a_process(48_200, "vfs-ioctl-flags", "flags");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = FileDesc::from(a_file_with(&lp, "/flags", TEN_DIGITS) as i32);

            let mut on = [1_i32, 0];
            let on_ptr = on.as_mut_ptr() as usize;
            let mut off = [0_i32, 1];
            let off_ptr = off.as_mut_ptr() as usize;

            assert_eq!(sys.sys_fcntl(fd, F_GETFL, 0).await, Ok(2));

            assert_eq!(sys.sys_ioctl(fd, FIONBIO, on_ptr, 0, 0).await, Ok(0));
            assert_eq!(
                sys.sys_fcntl(fd, F_GETFL, 0).await,
                Ok(2 | O_NONBLOCK),
                "FIONBIO did not set O_NONBLOCK, or set something else as well"
            );

            assert_eq!(sys.sys_ioctl(fd, FIOASYNC, on_ptr, 0, 0).await, Ok(0));
            assert_eq!(
                sys.sys_fcntl(fd, F_GETFL, 0).await,
                Ok(2 | O_NONBLOCK | O_ASYNC),
                "FIOASYNC did not set O_ASYNC on top of what was already there"
            );

            // Each number clears its own bit and only its own: this is the
            // assertion a swap of the two flags fails.
            assert_eq!(sys.sys_ioctl(fd, FIONBIO, off_ptr, 0, 0).await, Ok(0));
            assert_eq!(
                sys.sys_fcntl(fd, F_GETFL, 0).await,
                Ok(2 | O_ASYNC),
                "FIONBIO(0) took O_ASYNC with it, or left O_NONBLOCK on"
            );

            assert_eq!(sys.sys_ioctl(fd, FIOASYNC, off_ptr, 0, 0).await, Ok(0));
            assert_eq!(
                sys.sys_fcntl(fd, F_GETFL, 0).await,
                Ok(2),
                "FIOASYNC(0) did not clear O_ASYNC"
            );

            // The argument is a pointer and it is read, so a null one is
            // refused rather than taken for a zero.
            assert_eq!(
                sys.sys_ioctl(fd, FIONBIO, 0, 0, 0).await,
                errno(LxError::EFAULT),
                "FIONBIO read no pointer at all"
            );
            assert_eq!(
                sys.sys_ioctl(fd, FIOASYNC, 0, 0, 0).await,
                errno(LxError::EFAULT),
                "FIOASYNC read no pointer at all"
            );
        })
    });
}

#[test]
fn the_two_close_on_exec_ioctls_move_one_descriptor_and_leave_the_other() {
    // `FIOCLEX` and `FIONCLEX` differ in a single `true`/`false`, and what
    // they touch is per-DESCRIPTOR state in the process fd table -- not the
    // shared `File` the two descriptors here have in common. So the test
    // needs two descriptors on one file: a flag written into the file
    // object would show up on both of them, and a swapped pair would clear
    // what it was asked to set.
    let proc = a_process(48_201, "vfs-ioctl-cloexec", "cloexec");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let first = FileDesc::from(a_file_with(&lp, "/cloexec", TEN_DIGITS) as i32);
            let same = lp.lookup_inode("/cloexec/f").unwrap();
            let second: i32 = lp
                .add_file(File::new(same, OpenFlags::RDWR, String::from("/cloexec/f")))
                .unwrap()
                .into();
            let second = FileDesc::from(second);
            assert_ne!(first, second, "the two descriptors are the same one");

            assert_eq!(sys.sys_fcntl(first, F_GETFD, 0).await, Ok(0));
            assert_eq!(sys.sys_fcntl(second, F_GETFD, 0).await, Ok(0));

            assert_eq!(sys.sys_ioctl(first, FIOCLEX, 0, 0, 0).await, Ok(0));
            assert_eq!(
                sys.sys_fcntl(first, F_GETFD, 0).await,
                Ok(1),
                "FIOCLEX did not set close-on-exec"
            );
            assert_eq!(
                sys.sys_fcntl(second, F_GETFD, 0).await,
                Ok(0),
                "FIOCLEX reached the other descriptor on the same file"
            );
            assert_eq!(
                sys.sys_fcntl(first, F_GETFL, 0).await,
                Ok(2),
                "FIOCLEX went into the file's status flags as well"
            );

            assert_eq!(sys.sys_ioctl(first, FIONCLEX, 0, 0, 0).await, Ok(0));
            assert_eq!(
                sys.sys_fcntl(first, F_GETFD, 0).await,
                Ok(0),
                "FIONCLEX did not clear close-on-exec"
            );

            // It is the fd table that is asked, so it is the fd table that
            // refuses: a number nobody has is EBADF from either of them.
            assert_eq!(
                sys.sys_ioctl(FileDesc::from(A_CLOSED_FD as i32), FIOCLEX, 0, 0, 0)
                    .await,
                errno(LxError::EBADF)
            );
            assert_eq!(
                sys.sys_ioctl(FileDesc::from(A_CLOSED_FD as i32), FIONCLEX, 0, 0, 0)
                    .await,
                errno(LxError::EBADF)
            );
        })
    });
}

#[test]
fn what_is_left_to_read_is_counted_from_the_position_and_written_as_an_int() {
    // `FIONREAD` is `i_size_read(inode) - filp->f_pos`, so a test that only
    // reads it at offset zero cannot tell it from "the size of the file".
    // The position is moved twice here, once by a read and once by a seek.
    //
    // The answer is an `int`, four bytes, and the word after it is a
    // sentinel: a write of eight bytes where four were meant clobbers it.
    let proc = a_process(48_202, "vfs-ioctl-fionread", "fionread");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = FileDesc::from(a_file_with(&lp, "/fionread", TEN_DIGITS) as i32);
            let mut pair = [0_i32, -1];
            let pair_ptr = pair.as_mut_ptr() as usize;

            assert_eq!(sys.sys_ioctl(fd, FIONREAD, pair_ptr, 0, 0).await, Ok(0));
            assert_eq!(
                pair,
                [10, -1],
                "FIONREAD on a ten-byte file at offset zero did not answer ten, \
                 or wrote past the int it was given"
            );

            let mut four = [0_u8; 4];
            assert_eq!(
                sys.sys_read(fd, UserOutPtr::from(four.as_mut_ptr() as usize), 4)
                    .await,
                Ok(4)
            );
            assert_eq!(&four, b"0123");
            assert_eq!(sys.sys_ioctl(fd, FIONREAD, pair_ptr, 0, 0).await, Ok(0));
            assert_eq!(
                pair,
                [6, -1],
                "FIONREAD did not count from the position the read left behind"
            );

            // At the end there is nothing left, and past the end the
            // subtraction saturates rather than wrapping into a huge count.
            assert_eq!(sys.sys_lseek(fd, 10, SEEK_SET), Ok(10));
            assert_eq!(sys.sys_ioctl(fd, FIONREAD, pair_ptr, 0, 0).await, Ok(0));
            assert_eq!(pair, [0, -1], "a file read to its end has bytes waiting");
            assert_eq!(sys.sys_lseek(fd, 40, SEEK_SET), Ok(40));
            assert_eq!(sys.sys_ioctl(fd, FIONREAD, pair_ptr, 0, 0).await, Ok(0));
            assert_eq!(
                pair,
                [0, -1],
                "a position past the end answered with a wrapped count"
            );

            assert_eq!(
                sys.sys_ioctl(fd, FIONREAD, 0, 0, 0).await,
                errno(LxError::EFAULT),
                "FIONREAD wrote its answer nowhere and said it had"
            );
        })
    });
}

#[test]
fn the_terminal_block_turns_an_einval_into_enotty_and_the_five_file_ioctls_are_out_of_it() {
    // The rule is `tty_block_wants_enotty`: a number in `0x5400..=FIOQSIZE`
    // that the fd could not answer, on something that is not a char
    // device, is `ENOTTY` and not the `EINVAL` the inode gave -- because
    // `isatty()` callers check the errno and sfs answers EINVAL to
    // everything. Both ends of that range are inclusive and the five file
    // ioctls are exempt from it, and none of that is visible unless the
    // inode answers something other than ENOSYS, which every inode in the
    // tree does.
    let proc = a_process(48_203, "vfs-ioctl-enotty", "enotty");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            // A type whose `readable_bytes` does not answer, so FIONREAD
            // falls through to the inode like the rest of them.
            let fd = FileDesc::from(Quirky::open(&lp, FileType::NamedPipe, Answer::Einval) as i32);

            for inside in [THE_BOTTOM_OF_THE_TTY_BLOCK, TIOCGWINSZ, FIOQSIZE] {
                assert_eq!(
                    sys.sys_ioctl(fd, inside, 0, 0, 0).await,
                    errno(LxError::ENOTTY),
                    "{:#x} is inside the terminal block and kept its EINVAL",
                    inside
                );
            }

            for outside in [JUST_BELOW_THE_TTY_BLOCK, JUST_ABOVE_THE_TTY_BLOCK] {
                assert_eq!(
                    sys.sys_ioctl(fd, outside, 0, 0, 0).await,
                    errno(LxError::EINVAL),
                    "{:#x} is outside the terminal block and was rewritten anyway",
                    outside
                );
            }

            // FIONREAD is inside the range by number and exempt by rule,
            // so its EINVAL survives. FIONBIO, FIOASYNC, FIOCLEX and
            // FIONCLEX never reach the rule at all -- they are answered
            // above it -- so this is the one of the five that can say the
            // exemption list is read.
            assert_eq!(
                sys.sys_ioctl(fd, FIONREAD, 0, 0, 0).await,
                errno(LxError::EINVAL),
                "FIONREAD was taken for a terminal question"
            );

            // A char device is left alone by the same rule, and by the
            // same number: this is the pair that says the device kind is
            // what the rule looks at.
            let chr =
                FileDesc::from(Quirky::open(&lp, FileType::CharDevice, Answer::Einval) as i32);
            assert_eq!(
                sys.sys_ioctl(chr, THE_BOTTOM_OF_THE_TTY_BLOCK, 0, 0, 0)
                    .await,
                errno(LxError::EINVAL),
                "a char device's own EINVAL was rewritten to ENOTTY"
            );

            // And the rule under it, which has no range and no device kind
            // at all: an unhandled ioctl is ENOSYS by the generic
            // conversion, and ENOSYS is never what an ioctl answers.
            let nosys =
                FileDesc::from(Quirky::open(&lp, FileType::NamedPipe, Answer::Enosys) as i32);
            for anywhere in [
                JUST_BELOW_THE_TTY_BLOCK,
                THE_BOTTOM_OF_THE_TTY_BLOCK,
                JUST_ABOVE_THE_TTY_BLOCK,
                A_NUMBER_OF_NOBODY,
            ] {
                assert_eq!(
                    sys.sys_ioctl(nosys, anywhere, 0, 0, 0).await,
                    errno(LxError::ENOTTY),
                    "{:#x} came back as ENOSYS, which no ioctl answers",
                    anywhere
                );
            }
        })
    });
}

#[test]
fn a_window_size_is_synthesised_for_a_char_device_that_has_somewhere_to_put_it() {
    // Programs that insist on a window size retry `TIOCGWINSZ` forever when
    // it fails on a char device that is not a full tty backend, so one is
    // made up for them. Three things gate it -- the request number, a
    // non-null pointer, and a char device that is not an input node -- and
    // the two that can be asked here are asked both ways.
    let proc = a_process(48_204, "vfs-ioctl-winsize", "winsize");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let ws = kernel_hal::console::console_win_size();
            assert_eq!(
                (ws.ws_row, ws.ws_col),
                (24, 80),
                "this bench's console is not the serial default, so the \
                 size written below is not the one this test can name"
            );

            // Four shorts, and a fifth word as a sentinel: the write is a
            // whole `ConsoleWinSize` and nothing past it.
            let mut out = [0_u16; 5];
            out[4] = 0xbeef;
            let out_ptr = out.as_mut_ptr() as usize;

            // Both errors the arm accepts from the device, because they are
            // what the two inode families answer. Each one is paired with
            // what the SAME call answers once the pointer is taken away and
            // the synthesis cannot run: the device's own error then stands,
            // and only the ENOSYS one is rewritten -- by the last rule in
            // the chain, not by this arm. A char device is exempt from the
            // terminal-block rule in between, which is why the EINVAL one
            // comes back as it went in.
            for (answer, without_a_pointer) in [
                (Answer::Einval, LxError::EINVAL),
                (Answer::Enosys, LxError::ENOTTY),
            ] {
                let chr = FileDesc::from(Quirky::open(&lp, FileType::CharDevice, answer) as i32);
                out[0] = 0;
                out[1] = 0;
                assert_eq!(
                    sys.sys_ioctl(chr, TIOCGWINSZ, out_ptr, 0, 0).await,
                    Ok(0),
                    "a char device that cannot answer TIOCGWINSZ got no size ({:?})",
                    answer
                );
                assert_eq!(
                    (out[0], out[1], out[4]),
                    (24, 80, 0xbeef),
                    "the synthesised size is not the console's, or ran past it"
                );

                // Nowhere to put it is not an answer: the arm needs a
                // pointer, and without one the device's own error stands.
                assert_eq!(
                    sys.sys_ioctl(chr, TIOCGWINSZ, 0, 0, 0).await,
                    errno(without_a_pointer),
                    "a null pointer was written to anyway ({:?})",
                    answer
                );
            }

            // A regular file must keep the ENOTTY, or `isatty()` lies about
            // every redirected stream.
            let reg = FileDesc::from(Quirky::open(&lp, FileType::File, Answer::Einval) as i32);
            out[0] = 0;
            out[1] = 0;
            assert_eq!(
                sys.sys_ioctl(reg, TIOCGWINSZ, out_ptr, 0, 0).await,
                errno(LxError::ENOTTY),
                "a regular file was given a window size, so isatty() lies"
            );
            assert_eq!(
                (out[0], out[1]),
                (0, 0),
                "the refused call wrote a size anyway"
            );

            // And one number away from TIOCGWINSZ on the same char device,
            // so the number is read and not just the device kind.
            let chr =
                FileDesc::from(Quirky::open(&lp, FileType::CharDevice, Answer::Enosys) as i32);
            assert_eq!(
                sys.sys_ioctl(chr, TIOCGWINSZ + 1, out_ptr, 0, 0).await,
                errno(LxError::ENOTTY),
                "a neighbouring number was served a window size"
            );
        })
    });
}

#[test]
fn an_ioctl_refuses_a_descriptor_nobody_has_before_it_reads_anything() {
    // The fd table is consulted first, which is what makes the twelve
    // number tests below it safe to write: every one of them can assume a
    // file. The DRM-family numbers go down the same path with a trace of
    // their own on the way -- a stale fd in a graphics client is worth
    // seeing -- and a trace must not change the answer.
    //
    // `FIONBIO` with a null pointer is what says the order is this way
    // round: if the number were served before the descriptor, the answer
    // would be the EFAULT of the read.
    let proc = a_process(48_205, "vfs-ioctl-ebadf", "ebadf");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let closed = FileDesc::from(A_CLOSED_FD as i32);
            for request in [
                A_NUMBER_OF_NOBODY,
                A_DRM_NUMBER,
                A_DRM_NUMBER_SIGN_EXTENDED,
                TIOCGWINSZ,
                FIONREAD,
            ] {
                assert_eq!(
                    sys.sys_ioctl(closed, request, 0, 0, 0).await,
                    errno(LxError::EBADF),
                    "{:#x} on a descriptor nobody has answered something \
                     other than EBADF",
                    request
                );
            }
            assert_eq!(
                sys.sys_ioctl(closed, FIONBIO, 0, 0, 0).await,
                errno(LxError::EBADF),
                "the pointer was read before the descriptor was checked"
            );
        })
    });
}
