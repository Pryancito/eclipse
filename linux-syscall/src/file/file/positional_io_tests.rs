//! Positional and vectored I/O made the way a program makes it, which
//! nothing did.
//!
//! The twenty-two test modules beside this one all ask the pure helpers of
//! `file.rs` -- `writes_a_file`, `grows_past_fsize`, `after_write_error`,
//! `seek_from`, `access_mode`, the ioctl numbers -- and **not one of them
//! calls a syscall**. That is why the file sits at a third of its production
//! lines: of its thirty-five `sys_*` entry points, twenty-nine are not named
//! by any test in the crate.
//!
//! These eight are the family where the offset is the whole point. `pread`
//! and `pwrite` exist ONLY because they do not move it: that is what makes
//! them safe for two threads sharing one descriptor, and it is why glibc's
//! stdio and every database reach for them instead of `lseek` plus `read`.
//! Nothing checked it, in either direction.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use core::mem::size_of;
use linux_object::fs::{File, OpenFlags};
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// `SEEK_CUR`, the number userspace sends. `/usr/include/unistd.h` says 1.
/// Written as the number rather than read back out of `seek_from`, which is
/// the code this asks about.
const SEEK_CUR: usize = 1;

/// `RWF_HIPRI`, from `/usr/include/linux/fs.h`: a flag Linux supports and
/// this kernel does not, which is what makes it the one to ask with.
const RWF_HIPRI: usize = 0x1;

/// What ten bytes of content look like, so an offset can be read off the
/// answer: the byte at offset N is the digit N.
const TEN_DIGITS: &[u8; 10] = b"0123456789";

/// `struct iovec` as userspace lays it out: a pointer and a length, two
/// words.
///
/// Built here rather than through `IoVec`, whose fields are private and
/// which has no constructor -- what the syscall reads is the memory, and
/// this is that memory. `the_iovec_this_module_builds_is_the_one_the_kernel_reads`
/// holds the two layouts together, so a change to one is a failing test and
/// not a wild pointer.
#[repr(C)]
struct AnIoVec {
    base: usize,
    len: usize,
}

fn an_iovec(buf: &mut [u8]) -> AnIoVec {
    AnIoVec {
        base: buf.as_mut_ptr() as usize,
        len: buf.len(),
    }
}

/// A process whose root holds a `/<top>` of its own, because a lookup from
/// the root goes through a path cache every process in this binary shares.
fn a_process(pid: KoID, name: &str, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

/// A file of `dir_path` holding `content`, opened read-write with its cursor
/// at zero, and the descriptor it got.
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

/// What the file holds, asked of the filesystem and not of the syscalls
/// under test.
fn on_disk(lp: &LinuxProcess, dir_path: &str) -> [u8; 10] {
    let mut bytes = [0u8; 10];
    lp.lookup_inode(&alloc::format!("{}/f", dir_path))
        .unwrap()
        .read_at(0, &mut bytes)
        .unwrap();
    bytes
}

#[test]
fn the_iovec_this_module_builds_is_the_one_the_kernel_reads() {
    // The harness below hands the kernel a pointer to `AnIoVec`, which it
    // reads as `IoVec`. If the two ever stop agreeing, that is a wild
    // pointer in every test in this file, so it is checked once here rather
    // than discovered as a crash.
    assert_eq!(size_of::<AnIoVec>(), size_of::<IoVecOut>());
    assert_eq!(size_of::<AnIoVec>(), size_of::<IoVecIn>());
    assert_eq!(
        size_of::<AnIoVec>(),
        2 * size_of::<usize>(),
        "a struct iovec is a pointer and a length and nothing else"
    );
}

#[test]
fn pread_and_pwrite_leave_the_offset_alone_and_read_and_write_move_it() {
    // The reason the positional calls exist, asked in both directions in one
    // process so that a kernel which made them agree cannot pass: whatever
    // `read` and `write` do to the cursor, `pread` and `pwrite` must not.
    let proc = a_process(45_580, "pio-offset", "pio_offset");
    let dir_path = String::from("/pio_offset");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = FileDesc::from(a_file_with(&lp, &dir_path, TEN_DIGITS));

            // A read takes four bytes from the cursor and moves it.
            let mut four = [0u8; 4];
            let read = sys
                .sys_read(fd, UserOutPtr::from(four.as_mut_ptr() as usize), 4)
                .await;
            assert_eq!(read, Ok(4), "a plain read did not take four bytes");
            assert_eq!(&four, b"0123", "a read did not start at the cursor");
            assert_eq!(
                sys.sys_lseek(fd, 0, SEEK_CUR),
                Ok(4),
                "a read did not move the cursor"
            );

            // A pread of the same length reads from the offset it was GIVEN,
            // not from the cursor, and leaves the cursor where it was.
            let mut again = [0u8; 4];
            let pread = sys
                .sys_pread(fd, UserOutPtr::from(again.as_mut_ptr() as usize), 4, 0)
                .await;
            assert_eq!(pread, Ok(4), "a pread did not take four bytes");
            assert_eq!(
                &again, b"0123",
                "a pread read from the cursor instead of its own offset"
            );
            assert_eq!(
                sys.sys_lseek(fd, 0, SEEK_CUR),
                Ok(4),
                "a pread MOVED the cursor, which is the one thing it must not do"
            );

            // A pwrite goes where it was told and still does not move it.
            let abcd = *b"ABCD";
            let pwrite = sys.sys_pwrite(fd, UserInPtr::from(abcd.as_ptr() as usize), 4, 0);
            assert_eq!(pwrite, Ok(4), "a pwrite did not put four bytes");
            assert_eq!(
                sys.sys_lseek(fd, 0, SEEK_CUR),
                Ok(4),
                "a pwrite MOVED the cursor"
            );

            // And a write goes to the cursor, which the pwrite left at 4.
            let xy = *b"XY";
            let write = sys
                .sys_write(fd, UserInPtr::from(xy.as_ptr() as usize), 2)
                .await;
            assert_eq!(write, Ok(2), "a plain write did not put two bytes");
            assert_eq!(
                sys.sys_lseek(fd, 0, SEEK_CUR),
                Ok(6),
                "a write did not move the cursor"
            );

            assert_eq!(
                &on_disk(&lp, &dir_path),
                b"ABCDXY6789",
                "the four bytes went in at the wrong offset"
            );
        })
    });
}

#[test]
fn preadv_leaves_the_offset_alone_and_readv_moves_it_by_the_total() {
    // The same split one level up, where the count is a sum: `readv` moves
    // the cursor by the TOTAL of the iovecs, and `preadv` by nothing. The
    // two are asked of the same two buffers, so the second answer is also a
    // check that the first did not consume the file.
    let proc = a_process(45_581, "pio-vec", "pio_vec");
    let dir_path = String::from("/pio_vec");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = FileDesc::from(a_file_with(&lp, &dir_path, TEN_DIGITS));
            let mut head = [0u8; 3];
            let mut tail = [0u8; 2];

            let set = [an_iovec(&mut head), an_iovec(&mut tail)];
            let readv = sys
                .sys_readv(fd, UserInPtr::from(set.as_ptr() as usize), 2)
                .await;
            assert_eq!(readv, Ok(5), "readv did not answer with the total");
            assert_eq!(&head, b"012", "the first buffer took the wrong bytes");
            assert_eq!(&tail, b"34", "the second buffer did not carry on");
            assert_eq!(
                sys.sys_lseek(fd, 0, SEEK_CUR),
                Ok(5),
                "readv did not move the cursor by the total"
            );

            // Now from offset 0 again, with the cursor at 5: the same five
            // bytes must come back, and the cursor must still be at 5.
            head = [0u8; 3];
            tail = [0u8; 2];
            let set = [an_iovec(&mut head), an_iovec(&mut tail)];
            let preadv = sys
                .sys_preadv(fd, UserInPtr::from(set.as_ptr() as usize), 2, 0)
                .await;
            assert_eq!(preadv, Ok(5), "preadv did not answer with the total");
            assert_eq!(
                (&head, &tail),
                (b"012", b"34"),
                "preadv read from the cursor instead of its own offset"
            );
            assert_eq!(
                sys.sys_lseek(fd, 0, SEEK_CUR),
                Ok(5),
                "preadv MOVED the cursor"
            );
        })
    });
}

#[test]
fn pwritev_leaves_the_offset_alone_and_writev_moves_it_by_the_total() {
    // The gather twin, and the one place a zero-length entry matters: POSIX
    // says it is legal and contributes nothing, so it must be stepped over
    // without ending the gather at it.
    let proc = a_process(45_582, "pio-gather", "pio_gather");
    let dir_path = String::from("/pio_gather");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = FileDesc::from(a_file_with(&lp, &dir_path, TEN_DIGITS));
            let mut abc = *b"ABC";
            let mut nothing = [0u8; 0];
            let mut de = *b"DE";

            // An empty entry between two full ones: five bytes in, in order.
            let set = [
                an_iovec(&mut abc),
                an_iovec(&mut nothing),
                an_iovec(&mut de),
            ];
            let pwritev = sys.sys_pwritev(fd, UserInPtr::from(set.as_ptr() as usize), 3, 5);
            assert_eq!(
                pwritev,
                Ok(5),
                "pwritev did not gather past the empty entry"
            );
            assert_eq!(
                sys.sys_lseek(fd, 0, SEEK_CUR),
                Ok(0),
                "pwritev MOVED the cursor"
            );
            assert_eq!(
                &on_disk(&lp, &dir_path),
                b"01234ABCDE",
                "pwritev did not gather at the offset it was given"
            );

            // And writev, from the cursor it was left at, moves it.
            let mut xy = *b"XY";
            let set = [an_iovec(&mut xy)];
            let writev = sys
                .sys_writev(fd, UserInPtr::from(set.as_ptr() as usize), 1)
                .await;
            assert_eq!(writev, Ok(2), "writev did not put two bytes");
            assert_eq!(
                sys.sys_lseek(fd, 0, SEEK_CUR),
                Ok(2),
                "writev did not move the cursor by the total"
            );
            assert_eq!(
                &on_disk(&lp, &dir_path),
                b"XY234ABCDE",
                "writev did not gather at the cursor"
            );
        })
    });
}

#[test]
fn the_offset_of_minus_one_turns_preadv2_and_pwritev2_back_into_the_cursor_calls() {
    // The documented special case of `preadv2`/`pwritev2`, and a classic
    // place to get it backwards: an offset of -1 means "use and UPDATE the
    // current position", so the one call in this family that moves the
    // cursor is the one that looks most like the ones that do not. Asked
    // against the same call with a real offset, which must not.
    let proc = a_process(45_583, "pio-minus-one", "pio_minus_one");
    let dir_path = String::from("/pio_minus_one");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = FileDesc::from(a_file_with(&lp, &dir_path, TEN_DIGITS));
            let mut three = [0u8; 3];

            let set = [an_iovec(&mut three)];
            let fell_back = sys
                .sys_preadv2(fd, UserInPtr::from(set.as_ptr() as usize), 1, -1, 0)
                .await;
            assert_eq!(fell_back, Ok(3), "preadv2 at -1 did not read");
            assert_eq!(&three, b"012", "preadv2 at -1 did not start at the cursor");
            assert_eq!(
                sys.sys_lseek(fd, 0, SEEK_CUR),
                Ok(3),
                "preadv2 at -1 did not move the cursor, so it is not readv"
            );

            // The same call with a real offset leaves the cursor at 3.
            let set = [an_iovec(&mut three)];
            let positional = sys
                .sys_preadv2(fd, UserInPtr::from(set.as_ptr() as usize), 1, 0, 0)
                .await;
            assert_eq!(positional, Ok(3), "preadv2 at 0 did not read");
            assert_eq!(
                sys.sys_lseek(fd, 0, SEEK_CUR),
                Ok(3),
                "preadv2 with a real offset moved the cursor"
            );

            // And the write side, which also has to come back to the cursor.
            let mut zz = *b"ZZ";
            let set = [an_iovec(&mut zz)];
            let wrote = sys
                .sys_pwritev2(fd, UserInPtr::from(set.as_ptr() as usize), 1, -1, 0)
                .await;
            assert_eq!(wrote, Ok(2), "pwritev2 at -1 did not write");
            assert_eq!(
                sys.sys_lseek(fd, 0, SEEK_CUR),
                Ok(5),
                "pwritev2 at -1 did not move the cursor, so it is not writev"
            );
            assert_eq!(
                &on_disk(&lp, &dir_path),
                b"012ZZ56789",
                "pwritev2 at -1 did not write at the cursor"
            );
        })
    });
}

#[test]
fn a_negative_offset_is_einval_in_every_one_of_the_four_positional_calls() {
    // `off_t` is signed and these four take it as a `u64`, so a caller's -1
    // arrives as 2^64 - 1. Handed to the filesystem it is an offset sixteen
    // exabytes in; refused, it is the EINVAL Linux gives. All four are asked
    // because each one calls `user_offset` for itself, and a missing call is
    // invisible from the other three.
    let proc = a_process(45_584, "pio-negative", "pio_negative");
    let dir_path = String::from("/pio_negative");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = FileDesc::from(a_file_with(&lp, &dir_path, TEN_DIGITS));
            let minus_one = -1i64 as u64;
            let mut four = [0u8; 4];

            assert_eq!(
                sys.sys_pread(
                    fd,
                    UserOutPtr::from(four.as_mut_ptr() as usize),
                    4,
                    minus_one
                )
                .await,
                Err(LxError::EINVAL),
                "pread took a negative offset"
            );
            assert_eq!(
                sys.sys_pwrite(fd, UserInPtr::from(four.as_ptr() as usize), 4, minus_one),
                Err(LxError::EINVAL),
                "pwrite took a negative offset"
            );
            let set = [an_iovec(&mut four)];
            assert_eq!(
                sys.sys_preadv(fd, UserInPtr::from(set.as_ptr() as usize), 1, minus_one)
                    .await,
                Err(LxError::EINVAL),
                "preadv took a negative offset"
            );
            assert_eq!(
                sys.sys_pwritev(fd, UserInPtr::from(set.as_ptr() as usize), 1, minus_one),
                Err(LxError::EINVAL),
                "pwritev took a negative offset"
            );

            // Nothing was written by any of them.
            assert_eq!(
                &on_disk(&lp, &dir_path),
                TEN_DIGITS,
                "a refused call wrote anyway"
            );
        })
    });
}

#[test]
fn a_flag_this_kernel_does_not_implement_is_refused_and_not_silently_ignored() {
    // `RWF_NOWAIT` accepted and then blocked would be worse than refused, so
    // an unimplemented flag answers EOPNOTSUPP -- which callers read as "fall
    // back to preadv". The flags that ARE implemented must not be caught by
    // the same check, which is the half a mask that is too wide would break:
    // `RWF_DSYNC` makes the write durable and has to go through.
    let proc = a_process(45_585, "pio-flags", "pio_flags");
    let dir_path = String::from("/pio_flags");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = FileDesc::from(a_file_with(&lp, &dir_path, TEN_DIGITS));
            let mut three = [0u8; 3];

            let set = [an_iovec(&mut three)];
            assert_eq!(
                sys.sys_preadv2(fd, UserInPtr::from(set.as_ptr() as usize), 1, 0, RWF_HIPRI)
                    .await,
                Err(LxError::EOPNOTSUPP),
                "preadv2 took a flag it does not implement"
            );
            let mut abc = *b"ABC";
            let set = [an_iovec(&mut abc)];
            assert_eq!(
                sys.sys_pwritev2(fd, UserInPtr::from(set.as_ptr() as usize), 1, 0, RWF_HIPRI)
                    .await,
                Err(LxError::EOPNOTSUPP),
                "pwritev2 took a flag it does not implement"
            );
            assert_eq!(
                &on_disk(&lp, &dir_path),
                TEN_DIGITS,
                "a refused call wrote anyway"
            );

            // And the one it does implement goes through.
            let set = [an_iovec(&mut abc)];
            assert_eq!(
                sys.sys_pwritev2(fd, UserInPtr::from(set.as_ptr() as usize), 1, 0, RWF_DSYNC)
                    .await,
                Ok(3),
                "pwritev2 refused RWF_DSYNC, which it implements"
            );
            assert_eq!(
                &on_disk(&lp, &dir_path),
                b"ABC3456789",
                "the durable write did not land"
            );
        })
    });
}
