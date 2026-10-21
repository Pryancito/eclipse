//! The descriptors that have no file behind them, asked through the syscall:
//! `eventfd2`, `memfd_create`, the three `inotify` calls, `signalfd4`, and
//! the two calls that take descriptors away again, `close` and
//! `close_range`.
//!
//! Everything this file's neighbours cover is pure. `anon_fd_flag_tests`
//! calls [`anon_fd_flags`] with a mask it writes out itself,
//! `fd_abi_number_tests` calls [`close_range_action`], `perf_attr_size_tests`
//! and the rest are the same shape: a decision function with its own tests
//! and nothing that runs the syscall around it. So three things were never
//! executed:
//!
//! **That each syscall hands the gate its OWN mask.** The gate takes the
//! allowed set as an argument, and a test that passes the right one proves
//! nothing about the caller that passes the wrong one. `EFD_SEMAPHORE` is
//! `eventfd2`'s alone, and it is bit 0, which `inotify_init1` and
//! `signalfd4` must refuse.
//!
//! **That the close-on-exec bit reaches the descriptor.** This is a two-step
//! journey no pure test can follow: the syscall's own flag bit becomes
//! `OpenFlags::CLOEXEC` on the object, and `add_file` reads it back off the
//! object into the per-descriptor set that `execve` consults. The two steps
//! are in different crates, and the bit is spelled differently at each end
//! of the trip -- `EFD_CLOEXEC`, `IN_CLOEXEC` and `SFD_CLOEXEC` are all
//! `0o2000000`, while `MFD_CLOEXEC` is 1.
//!
//! **Order, and that a refusal changes nothing.** `inotify_add_watch` judges
//! five things in a fixed order, and each one has its own errno, so the
//! sequence is observable end to end: the string, then the mask, then the
//! descriptor, then what the descriptor is, and only then the path. The path
//! lookup is the one that was missing entirely, and `inotifywait
//! /nonexistent` reported success and waited forever. `signalfd4` has an
//! order of its own for the same reason: flags, then the size of the
//! caller's `sigset_t`, then the read of it.
//!
//! And the smaller claims around them: that `memfd_create` keeps the name it
//! was given rather than a default, that a memfd really is memory, that a
//! watch wants read permission on what it watches, that
//! `CLOSE_RANGE_CLOEXEC` leaves every descriptor open, and that a refused
//! call leaves no descriptor behind.
//!
//! Fixed pids live in 47_36x and 47_37x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::vec::Vec;
use linux_object::fs::{MFD_CLOEXEC, MFD_HUGETLB};
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// The `inotify` mask bits this file uses, as `<linux/inotify.h>` gives
/// them. The kernel's own copies live in a private module of
/// `linux-object`, so these are written out here: a watch asked for with
/// the number a C program sends is what the test is about.
const IN_ALL_EVENTS: u32 = 0x0000_0fff;
const IN_ONLYDIR: u32 = 0x0100_0000;
const IN_DONT_FOLLOW: u32 = 0x0200_0000;
const IN_MASK_CREATE: u32 = 0x1000_0000;
const IN_MASK_ADD: u32 = 0x2000_0000;
/// A mask bit that is outside `ALL_INOTIFY_BITS`: no event, no instruction
/// for the lookup, and not one of the three the kernel sends.
const A_MASK_BIT_OF_NOBODY: u32 = 1 << 16;

/// How wide a `sigset_t` is here, which is the only width `signalfd4`
/// accepts.
const A_SIGSET: usize = 8;
/// Any other width. glibc's own `sigset_t` is 128 bytes, so this is what a
/// caller that forgot to narrow it would send.
const A_SIGSET_OF_ANOTHER_KERNEL: usize = 128;

/// A flag bit no syscall in this file has. Outside `OpenFlags` and outside
/// every `MFD_` and `IN_` flag, so it is the same mistake for all of them.
const A_FLAG_OF_NOBODY: usize = 1 << 20;

/// Somebody who is not root, for the one test about permission.
const A_USER: u32 = 1000;

/// `SIGUSR1` and `SIGUSR2` as a `sigset_t`: signal n is bit n - 1.
const TWO_SIGNALS: u64 = (1 << 9) | (1 << 11);
/// Another pair, to tell one mask from another.
const TWO_OTHER_SIGNALS: u64 = (1 << 0) | (1 << 14);
/// `SIGKILL` and `SIGSTOP`, the two no mask may hold.
const THE_TWO_THAT_CANNOT_BE_CAUGHT: u64 = (1 << 8) | (1 << 18);

/// A process with a `/<top>` of its own, because a lookup from the root goes
/// through a path cache every process in this binary shares.
fn a_process(pid: KoID, name: &str, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

/// What this process acts as from here on. The group goes first: dropping
/// the effective uid takes away the privilege `set_resgid` needs.
fn acting_as(sys: &Syscall<'_>, uid: u32) {
    let lp = sys.linux_process();
    lp.set_groups(Vec::new());
    lp.set_resgid(uid, uid, uid).unwrap();
    lp.set_resuid(uid, uid, uid).unwrap();
}

/// A path as userspace writes one: NUL-terminated.
fn c(s: &str) -> Vec<u8> {
    let mut bytes = Vec::from(s.as_bytes());
    bytes.push(0);
    bytes
}

/// A file of `dir_path`, with exactly this owner and mode.
fn a_file_named(lp: &LinuxProcess, dir_path: &str, name: &str, uid: u32, mode: u16) {
    let dir = lp.lookup_inode(dir_path).unwrap();
    let inode = dir.create(name, FileType::File, 0o644).unwrap();
    let mut metadata = inode.metadata().unwrap();
    metadata.uid = uid as _;
    metadata.gid = uid as _;
    metadata.mode = mode;
    inode.set_metadata(&metadata).unwrap();
}

/// A symbolic link of `dir_path` pointing at `to`, which need not exist.
fn a_link_named(lp: &LinuxProcess, dir_path: &str, name: &str, to: &str) {
    let dir = lp.lookup_inode(dir_path).unwrap();
    let inode = dir.create(name, FileType::SymLink, 0o777).unwrap();
    inode.write_at(0, to.as_bytes()).unwrap();
}

/// The same `Syscall` with a mutable borrow, which the dispatch table needs.
fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// A syscall made the way userspace makes it: by number, through the table.
async fn by_number(sys: &Syscall<'_>, which: crate::Sys, args: [usize; 6]) -> isize {
    let mut own = as_mut(sys);
    own.syscall(which as u32, args).await
}

fn eventfd2(sys: &Syscall<'_>, initval: u32, flags: usize) -> SysResult {
    sys.sys_eventfd2(initval, flags)
}

fn memfd_create(sys: &Syscall<'_>, name: &[u8], flags: usize) -> SysResult {
    sys.sys_memfd_create(UserInPtr::from(name.as_ptr() as usize), flags)
}

fn inotify_init1(sys: &Syscall<'_>, flags: usize) -> SysResult {
    sys.sys_inotify_init1(flags)
}

fn add_watch(sys: &Syscall<'_>, fd: usize, path: &[u8], mask: u32) -> SysResult {
    sys.sys_inotify_add_watch(fd, UserInPtr::from(path.as_ptr() as usize), mask)
}

fn rm_watch(sys: &Syscall<'_>, fd: usize, wd: i32) -> SysResult {
    sys.sys_inotify_rm_watch(fd, wd)
}

/// `signalfd4` with the mask handed over as a pointer, as the syscall takes
/// it. `fd` is the raw register: -1 is "make me a new one".
fn signalfd4(sys: &Syscall<'_>, fd: i32, mask: &u64, sizemask: usize, flags: usize) -> SysResult {
    sys.sys_signalfd4(
        FileDesc::from(fd),
        UserInPtr::from(mask as *const u64 as usize),
        sizemask,
        flags,
    )
}

/// The same with no mask to read, which is what a caller who got the
/// pointer wrong sends.
fn signalfd4_at_a_null_mask(
    sys: &Syscall<'_>,
    fd: i32,
    sizemask: usize,
    flags: usize,
) -> SysResult {
    sys.sys_signalfd4(FileDesc::from(fd), UserInPtr::from(0), sizemask, flags)
}

/// What a descriptor says about itself, which is `/proc/<pid>/fdinfo/<fd>`
/// and the only way to read an anonymous fd's state without consuming it.
fn fdinfo_of(lp: &LinuxProcess, fd: usize) -> alloc::string::String {
    lp.get_file_like(FileDesc::from(fd)).unwrap().fdinfo()
}

/// The eight bytes an eventfd read hands back, as the number they are.
async fn read_a_count(sys: &Syscall<'_>, fd: usize) -> Result<u64, LxError> {
    let mut eight = [0u8; 8];
    let got = sys
        .sys_read(
            FileDesc::from(fd),
            UserOutPtr::from(eight.as_mut_ptr() as usize),
            8,
        )
        .await?;
    assert_eq!(got, 8, "an eventfd read is eight bytes or nothing");
    Ok(u64::from_ne_bytes(eight))
}

#[test]
fn the_close_on_exec_bit_of_each_anonymous_fd_reaches_the_descriptor() {
    // The journey no pure test can follow, for all four families at once:
    // the syscall's own flag bit becomes `OpenFlags::CLOEXEC` on the object,
    // and `add_file` reads it back off the object into the per-descriptor
    // set `execve` consults. Both ends are asked, so a kernel that set the
    // flag on every descriptor passes neither.
    let proc = a_process(47_360, "anon-cloexec", "anon_cloexec");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let name = c("ring");

            // Three of the four spell it `0o2000000`, the `O_CLOEXEC` bit.
            for (what, without, with) in [
                (
                    "eventfd2",
                    eventfd2(sys, 0, 0),
                    eventfd2(sys, 0, ANON_CLOEXEC),
                ),
                (
                    "inotify_init1",
                    inotify_init1(sys, 0),
                    inotify_init1(sys, ANON_CLOEXEC),
                ),
                (
                    "signalfd4",
                    signalfd4(sys, -1, &TWO_SIGNALS, A_SIGSET, 0),
                    signalfd4(sys, -1, &TWO_SIGNALS, A_SIGSET, ANON_CLOEXEC),
                ),
                // And `memfd_create` spells it 1, which is why it cannot
                // share a check with the others.
                (
                    "memfd_create",
                    memfd_create(sys, &name, 0),
                    memfd_create(sys, &name, MFD_CLOEXEC),
                ),
            ] {
                let plain = FileDesc::from(without.unwrap_or_else(|e| panic!("{}: {:?}", what, e)));
                let cloexec = FileDesc::from(with.unwrap_or_else(|e| panic!("{}: {:?}", what, e)));
                assert_eq!(
                    lp.fd_cloexec(plain),
                    Ok(false),
                    "{what} without its flag left a close-on-exec descriptor"
                );
                assert_eq!(
                    lp.fd_cloexec(cloexec),
                    Ok(true),
                    "{what}'s close-on-exec flag never reached the descriptor"
                );
            }
        })
    });
}

#[test]
fn an_eventfd_starts_at_the_value_it_was_given_and_the_semaphore_bit_changes_what_a_read_takes() {
    // `initval` and `EFD_SEMAPHORE` are the whole of `eventfd2`'s own
    // arguments, and both are asked twice over: of the object, which says
    // what it holds, and of a read, which is what a program does with it.
    // The two counts differ and the two reads differ, so one answer cannot
    // stand in for the other.
    let proc = a_process(47_361, "anon-eventfd", "anon_eventfd");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            const PLAIN_START: u64 = 7;
            const SEMAPHORE_START: u64 = 5;

            let plain = eventfd2(sys, PLAIN_START as u32, ANON_NONBLOCK).unwrap();
            let semaphore =
                eventfd2(sys, SEMAPHORE_START as u32, EFD_SEMAPHORE | ANON_NONBLOCK).unwrap();

            // What each one says it is. The count is the value the syscall
            // was handed, and the semaphore line is the bit.
            let plain_info = fdinfo_of(&lp, plain);
            assert!(
                plain_info.contains(&alloc::format!("eventfd-count: {PLAIN_START:16x}")),
                "the initial value never reached the counter: {}",
                plain_info
            );
            assert!(
                plain_info.contains("eventfd-semaphore: 0"),
                "a plain eventfd came out a semaphore: {}",
                plain_info
            );
            let semaphore_info = fdinfo_of(&lp, semaphore);
            assert!(
                semaphore_info.contains(&alloc::format!("eventfd-count: {SEMAPHORE_START:16x}")),
                "the initial value never reached the counter: {}",
                semaphore_info
            );
            assert!(
                semaphore_info.contains("eventfd-semaphore: 1"),
                "EFD_SEMAPHORE never reached the object: {}",
                semaphore_info
            );

            // And what a read does, which is the difference the bit makes:
            // a plain read empties the counter in one go, a semaphore read
            // takes one and leaves the rest.
            assert_eq!(read_a_count(sys, plain).await, Ok(PLAIN_START));
            assert_eq!(
                read_a_count(sys, plain).await,
                Err(LxError::EAGAIN),
                "the counter was not emptied, or EFD_NONBLOCK never reached the object"
            );
            assert_eq!(read_a_count(sys, semaphore).await, Ok(1));
            let left = fdinfo_of(&lp, semaphore);
            assert!(
                left.contains(&alloc::format!(
                    "eventfd-count: {:16x}",
                    SEMAPHORE_START - 1
                )),
                "a semaphore read took more than one: {}",
                left
            );
        })
    });
}

#[test]
fn each_of_these_syscalls_is_held_to_its_own_list_of_flags() {
    // The gate takes the allowed set as an argument, so what a pure test of
    // it cannot see is the caller that hands it the wrong one.
    // `EFD_SEMAPHORE` is bit 0 and belongs to `eventfd2` alone; the other
    // two must refuse it, and all three must refuse a bit nobody has.
    //
    // And the refusal must leave nothing behind: the lowest free descriptor
    // is the same afterwards, which it would not be if a descriptor had
    // been installed before the flags were judged.
    let proc = a_process(47_362, "anon-flags", "anon_flags");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let name = c("flagged");
            let free_before = lp.get_free_fd();

            assert_eq!(
                inotify_init1(sys, EFD_SEMAPHORE),
                Err(LxError::EINVAL),
                "inotify_init1 took eventfd2's own flag"
            );
            assert_eq!(
                signalfd4(sys, -1, &TWO_SIGNALS, A_SIGSET, EFD_SEMAPHORE),
                Err(LxError::EINVAL),
                "signalfd4 took eventfd2's own flag"
            );
            assert_eq!(
                eventfd2(sys, 0, A_FLAG_OF_NOBODY),
                Err(LxError::EINVAL),
                "eventfd2 took a flag it does not have"
            );
            assert_eq!(
                inotify_init1(sys, A_FLAG_OF_NOBODY),
                Err(LxError::EINVAL),
                "inotify_init1 took a flag it does not have"
            );
            assert_eq!(
                signalfd4(sys, -1, &TWO_SIGNALS, A_SIGSET, A_FLAG_OF_NOBODY),
                Err(LxError::EINVAL),
                "signalfd4 took a flag it does not have"
            );
            assert_eq!(
                memfd_create(sys, &name, A_FLAG_OF_NOBODY),
                Err(LxError::EINVAL),
                "memfd_create took a flag it does not have"
            );
            // `MFD_HUGETLB` is a flag that exists and is refused: there is
            // no hugepage pool, and handing back an ordinary memfd would
            // tell a probe it got huge pages.
            assert_eq!(
                memfd_create(sys, &name, MFD_HUGETLB),
                Err(LxError::EINVAL),
                "memfd_create promised huge pages"
            );

            assert_eq!(
                lp.get_free_fd(),
                free_before,
                "a refused call left a descriptor behind"
            );
            // The flags each of them does have, to show the refusals above
            // are about the bit and not about the call.
            assert!(eventfd2(sys, 0, EFD_SEMAPHORE | ANON_NONBLOCK).is_ok());
            assert!(inotify_init1(sys, ANON_NONBLOCK | ANON_CLOEXEC).is_ok());
            assert!(signalfd4(sys, -1, &TWO_SIGNALS, A_SIGSET, ANON_NONBLOCK).is_ok());
            assert!(memfd_create(sys, &name, MFD_CLOEXEC).is_ok());
        })
    });
}

#[test]
fn the_name_memfd_create_is_given_is_the_one_the_memfd_keeps() {
    // The name is the one thing a memfd carries for the rest of its life
    // (it is what `/proc/<pid>/maps` shows for a mapping of it), and a
    // pointer the caller got wrong used to come back as a working
    // descriptor called "memfd" -- the default that `unwrap_or` supplied.
    let proc = a_process(47_363, "anon-memfd-name", "anon_memfd_name");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let free_before = lp.get_free_fd();

            let fd = memfd_create(sys, &c("pulseaudio"), 0).unwrap();
            let file = lp.get_file_like(FileDesc::from(fd)).unwrap();
            let file = file
                .downcast_ref::<linux_object::fs::File>()
                .expect("memfd_create did not make a file");
            assert_eq!(
                file.path(),
                "/memfd:pulseaudio",
                "the memfd is not called what the caller called it"
            );

            // A name that cannot be read is EFAULT, as it is for every
            // other syscall that takes a string, and no descriptor comes
            // back.
            assert_eq!(
                sys.sys_memfd_create(UserInPtr::from(0), 0),
                Err(LxError::EFAULT),
                "a null name was given a default instead of being refused"
            );
            // 249 bytes is the longest name there is room for: the kernel
            // puts "memfd:" in front of it and the whole thing has to fit
            // in 255.
            let longest = alloc::vec![b'n'; linux_object::fs::MFD_NAME_MAX_LEN];
            assert!(memfd_create(sys, &c(core::str::from_utf8(&longest).unwrap()), 0).is_ok());
            let one_too_many = alloc::vec![b'n'; linux_object::fs::MFD_NAME_MAX_LEN + 1];
            assert_eq!(
                memfd_create(sys, &c(core::str::from_utf8(&one_too_many).unwrap()), 0),
                Err(LxError::EINVAL),
                "a name too long for the kernel's buffer was accepted"
            );
            assert_eq!(
                lp.get_free_fd(),
                FileDesc::from(usize::from(free_before) + 2),
                "the two refusals left descriptors behind"
            );
        })
    });
}

#[test]
fn a_memfd_is_memory_that_a_read_gets_back() {
    // That the descriptor is a file and not a token: a write goes in and a
    // positional read gets the same bytes out, with no mode argument asked
    // for anywhere, because a memfd is read-write by construction. Two
    // memfds of the SAME name are two files, which is what the hidden
    // sequence number in the backing name is for.
    let proc = a_process(47_364, "anon-memfd-io", "anon_memfd_io");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let name = c("frame");
            let one = FileDesc::from(memfd_create(sys, &name, 0).unwrap());
            let other = FileDesc::from(memfd_create(sys, &name, 0).unwrap());

            let written = *b"ABCDEFGH";
            assert_eq!(
                sys.sys_write(one, UserInPtr::from(written.as_ptr() as usize), 8)
                    .await,
                Ok(8),
                "a memfd is not writable"
            );
            let mut read_back = [0u8; 8];
            assert_eq!(
                sys.sys_pread(one, UserOutPtr::from(read_back.as_mut_ptr() as usize), 8, 0)
                    .await,
                Ok(8),
                "a memfd is not readable"
            );
            assert_eq!(&read_back, &written, "the memfd did not keep the bytes");

            // The second memfd of the same name is empty: they are two
            // files, not two descriptors of one.
            let mut other_bytes = [0xffu8; 8];
            assert_eq!(
                sys.sys_pread(
                    other,
                    UserOutPtr::from(other_bytes.as_mut_ptr() as usize),
                    8,
                    0
                )
                .await,
                Ok(0),
                "two memfds of one name share their contents"
            );
        })
    });
}

#[test]
fn the_order_inotify_add_watch_judges_its_arguments_in() {
    // Five judgements, each with its own errno, so the order is observable
    // end to end. The last of them is the one that was missing: the path
    // was never looked up, so `inotifywait /nonexistent` reported success
    // and waited forever for events about a file that does not exist.
    let proc = a_process(47_365, "anon-watch-order", "anon_watch_order");
    let dir_path = alloc::string::String::from("/anon_watch_order");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            a_file_named(&lp, &dir_path, "f", 0, 0o644);
            let path = c(&alloc::format!("{dir_path}/f"));
            let nowhere = c(&alloc::format!("{dir_path}/not-here"));
            let watcher = inotify_init1(sys, 0).unwrap();
            let not_a_watcher = eventfd2(sys, 0, 0).unwrap();
            let no_such_fd = usize::from(lp.get_free_fd());

            // The string first: a mask of nothing and a descriptor of
            // nothing are both beside the point while the pointer is bad.
            assert_eq!(
                sys.sys_inotify_add_watch(no_such_fd, UserInPtr::from(0), 0),
                Err(LxError::EFAULT),
                "the pathname was not read first"
            );
            // Then the mask. An empty one is EINVAL, and so is a bit that
            // no `IN_` flag names; both are judged before the descriptor.
            assert_eq!(
                add_watch(sys, no_such_fd, &path, 0),
                Err(LxError::EINVAL),
                "an empty mask was not judged before the descriptor"
            );
            assert_eq!(
                add_watch(sys, no_such_fd, &path, A_MASK_BIT_OF_NOBODY),
                Err(LxError::EINVAL),
                "a mask bit that names no event was accepted"
            );
            // And the two that contradict each other: extend this watch's
            // mask, and refuse to touch it if it already exists.
            assert_eq!(
                add_watch(
                    sys,
                    no_such_fd,
                    &path,
                    IN_ALL_EVENTS | IN_MASK_ADD | IN_MASK_CREATE
                ),
                Err(LxError::EINVAL),
                "IN_MASK_ADD and IN_MASK_CREATE were accepted together"
            );
            // Then the descriptor, and then what the descriptor is: a good
            // one that is not an inotify is EINVAL, not EBADF.
            assert_eq!(
                add_watch(sys, no_such_fd, &path, IN_ALL_EVENTS),
                Err(LxError::EBADF),
                "a descriptor nobody opened was not EBADF"
            );
            assert_eq!(
                add_watch(sys, not_a_watcher, &path, IN_ALL_EVENTS),
                Err(LxError::EINVAL),
                "an eventfd was taken for an inotify"
            );
            // And only then the path.
            assert_eq!(
                add_watch(sys, watcher, &nowhere, IN_ALL_EVENTS),
                Err(LxError::ENOENT),
                "a watch was added for a path that does not exist"
            );
            assert!(
                add_watch(sys, watcher, &path, IN_ALL_EVENTS).is_ok(),
                "and the same call on a path that does exist must work"
            );
        })
    });
}

#[test]
fn in_onlydir_and_in_dont_follow_change_which_inode_the_watch_lands_on() {
    // The two mask bits that are not events at all: they are instructions
    // for the lookup, which is why they have to reach it. `IN_ONLYDIR` is
    // how a watcher of a config DIRECTORY refuses to be handed a file, and
    // `IN_DONT_FOLLOW` is how one watches a symlink itself -- asked here of
    // a link whose target does not exist, so following it and not following
    // it cannot both come back the same.
    let proc = a_process(47_366, "anon-watch-lookup", "anon_watch_lookup");
    let dir_path = alloc::string::String::from("/anon_watch_lookup");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            a_file_named(&lp, &dir_path, "f", 0, 0o644);
            lp.lookup_inode(&dir_path)
                .unwrap()
                .create("d", FileType::Dir, 0o755)
                .unwrap();
            a_link_named(&lp, &dir_path, "dangling", "not-here");
            let file = c(&alloc::format!("{dir_path}/f"));
            let dir = c(&alloc::format!("{dir_path}/d"));
            let link = c(&alloc::format!("{dir_path}/dangling"));
            let watcher = inotify_init1(sys, 0).unwrap();

            assert_eq!(
                add_watch(sys, watcher, &file, IN_ALL_EVENTS | IN_ONLYDIR),
                Err(LxError::ENOTDIR),
                "IN_ONLYDIR let a file be watched"
            );
            assert!(
                add_watch(sys, watcher, &dir, IN_ALL_EVENTS | IN_ONLYDIR).is_ok(),
                "IN_ONLYDIR refused a directory"
            );
            assert!(
                add_watch(sys, watcher, &file, IN_ALL_EVENTS).is_ok(),
                "and without it a file is fine"
            );

            assert_eq!(
                add_watch(sys, watcher, &link, IN_ALL_EVENTS),
                Err(LxError::ENOENT),
                "the link was not followed to its missing target"
            );
            assert!(
                add_watch(sys, watcher, &link, IN_ALL_EVENTS | IN_DONT_FOLLOW).is_ok(),
                "IN_DONT_FOLLOW never reached the lookup: the link itself is there"
            );
        })
    });
}

#[test]
fn a_watch_wants_read_permission_on_what_it_watches() {
    // `inotify_add_watch` ends in `inode_permission(MAY_READ)`, which is
    // the whole reason watching is not a way around file permissions.
    // Asked as somebody who is not root, because root is allowed anyway,
    // and of the same file at two modes, so the refusal is about the mode.
    let proc = a_process(47_367, "anon-watch-perm", "anon_watch_perm");
    let dir_path = alloc::string::String::from("/anon_watch_perm");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            a_file_named(&lp, &dir_path, "secret", 0, 0o600);
            a_file_named(&lp, &dir_path, "public", 0, 0o644);
            let secret = c(&alloc::format!("{dir_path}/secret"));
            let public = c(&alloc::format!("{dir_path}/public"));
            let watcher = inotify_init1(sys, 0).unwrap();
            acting_as(sys, A_USER);

            assert_eq!(
                add_watch(sys, watcher, &secret, IN_ALL_EVENTS),
                Err(LxError::EACCES),
                "a file this user may not read was watched anyway"
            );
            assert!(
                add_watch(sys, watcher, &public, IN_ALL_EVENTS).is_ok(),
                "a file this user may read was refused"
            );
        })
    });
}

#[test]
fn the_same_path_twice_is_the_same_watch_and_rm_watch_takes_it_away() {
    // What a watch descriptor is, from both ends. A second call for a path
    // already watched returns the SAME number (which is how systemd and
    // GLib learn a path is already watched) unless `IN_MASK_CREATE` says it
    // must be new, and `inotify_rm_watch` is the only way one goes away.
    let proc = a_process(47_368, "anon-watch-ids", "anon_watch_ids");
    let dir_path = alloc::string::String::from("/anon_watch_ids");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            a_file_named(&lp, &dir_path, "f", 0, 0o644);
            a_file_named(&lp, &dir_path, "g", 0, 0o644);
            let f = c(&alloc::format!("{dir_path}/f"));
            let g = c(&alloc::format!("{dir_path}/g"));
            let watcher = inotify_init1(sys, 0).unwrap();
            let not_a_watcher = eventfd2(sys, 0, 0).unwrap();

            let wd = add_watch(sys, watcher, &f, IN_ALL_EVENTS).unwrap();
            assert_eq!(
                add_watch(sys, watcher, &f, IN_ALL_EVENTS),
                Ok(wd),
                "the same path was given a second watch descriptor"
            );
            assert_eq!(
                add_watch(sys, watcher, &f, IN_ALL_EVENTS | IN_MASK_CREATE),
                Err(LxError::EEXIST),
                "IN_MASK_CREATE never reached the watch list"
            );
            let other = add_watch(sys, watcher, &g, IN_ALL_EVENTS).unwrap();
            assert_ne!(other, wd, "two paths share one watch descriptor");

            // And away again. A descriptor that is gone is EINVAL, as
            // Linux has it, and a descriptor that was never there is the
            // same answer.
            assert_eq!(rm_watch(sys, watcher, wd as i32), Ok(0));
            assert_eq!(
                rm_watch(sys, watcher, wd as i32),
                Err(LxError::EINVAL),
                "a watch that was removed is still there"
            );
            assert_eq!(rm_watch(sys, watcher, -1), Err(LxError::EINVAL));
            // The other one is untouched by its neighbour's removal.
            assert_eq!(rm_watch(sys, watcher, other as i32), Ok(0));
            // And the file descriptor itself is judged the same way here as
            // it is on the way in.
            assert_eq!(
                rm_watch(sys, not_a_watcher, 1),
                Err(LxError::EINVAL),
                "an eventfd was taken for an inotify"
            );
            assert_eq!(
                rm_watch(sys, usize::from(lp.get_free_fd()), 1),
                Err(LxError::EBADF)
            );
        })
    });
}

#[test]
fn what_signalfd4_judges_before_it_reads_the_mask() {
    // Three judgements in order, each told apart by its errno: the flag
    // word first (as Linux does, whether or not the descriptor is good),
    // then how wide the caller says its `sigset_t` is -- which this call
    // took, named `_sizemask`, and ignored while reading eight bytes
    // regardless -- and only then the read of the mask itself.
    let proc = a_process(47_369, "anon-signalfd-order", "anon_signalfd_order");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let free_before = lp.get_free_fd();

            assert_eq!(
                signalfd4_at_a_null_mask(sys, -1, A_SIGSET, A_FLAG_OF_NOBODY),
                Err(LxError::EINVAL),
                "the flags were not judged before the mask was read"
            );
            assert_eq!(
                signalfd4_at_a_null_mask(sys, -1, A_SIGSET_OF_ANOTHER_KERNEL, 0),
                Err(LxError::EINVAL),
                "a sigset of the wrong width was not judged before the read"
            );
            assert_eq!(
                signalfd4_at_a_null_mask(sys, -1, A_SIGSET, 0),
                Err(LxError::EFAULT),
                "a mask that cannot be read was not EFAULT"
            );
            assert_eq!(
                lp.get_free_fd(),
                free_before,
                "a refused signalfd4 left a descriptor behind"
            );

            // And what the mask becomes: the signals asked for, less the
            // two no mask may hold. A signalfd for SIGKILL would be a way
            // to stop a process being killed.
            let fd = signalfd4(
                sys,
                -1,
                &(TWO_SIGNALS | THE_TWO_THAT_CANNOT_BE_CAUGHT),
                A_SIGSET,
                0,
            )
            .unwrap();
            assert!(
                fdinfo_of(&lp, fd).contains(&alloc::format!("sigmask:\t{TWO_SIGNALS:016x}")),
                "the mask the kernel kept is not the one asked for, less SIGKILL and SIGSTOP: {}",
                fdinfo_of(&lp, fd)
            );
        })
    });
}

#[test]
fn a_signalfd_with_a_descriptor_changes_that_one_and_makes_no_other() {
    // The second half of the call, which is why it takes a descriptor at
    // all: a non-negative `fd` means "this signalfd's mask is now that
    // one", and the answer is the same descriptor. A new one is what a
    // negative `fd` asks for, and anything else under a good descriptor is
    // EINVAL.
    let proc = a_process(47_370, "anon-signalfd-update", "anon_signalfd_update");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = signalfd4(sys, -1, &TWO_SIGNALS, A_SIGSET, 0).unwrap();
            let not_a_signalfd = eventfd2(sys, 0, 0).unwrap();
            let free_before = lp.get_free_fd();

            assert_eq!(
                signalfd4(sys, fd as i32, &TWO_OTHER_SIGNALS, A_SIGSET, 0),
                Ok(fd),
                "an update came back as a different descriptor"
            );
            assert_eq!(
                lp.get_free_fd(),
                free_before,
                "an update made a second signalfd"
            );
            assert!(
                fdinfo_of(&lp, fd).contains(&alloc::format!("sigmask:\t{TWO_OTHER_SIGNALS:016x}")),
                "the new mask never reached the signalfd: {}",
                fdinfo_of(&lp, fd)
            );
            assert_eq!(
                signalfd4(sys, not_a_signalfd as i32, &TWO_SIGNALS, A_SIGSET, 0),
                Err(LxError::EINVAL),
                "an eventfd was taken for a signalfd"
            );
            assert_eq!(
                signalfd4(
                    sys,
                    usize::from(lp.get_free_fd()) as i32,
                    &TWO_SIGNALS,
                    A_SIGSET,
                    0
                ),
                Err(LxError::EBADF)
            );
        })
    });
}

#[test]
fn close_range_closes_what_it_is_told_to_close_and_marks_what_it_is_told_to_mark() {
    // `CLOSE_RANGE_CLOEXEC` and no flag at all are not variations on one
    // another: one of them leaves every descriptor open. glibc, dbus,
    // systemd and GLib all make the marking call before `execve`, and a
    // kernel that read it as "close" would mass-close their live
    // descriptors. Both are asked of the same shape of range, with a
    // descriptor outside it that must be left alone either way.
    let proc = a_process(47_371, "anon-close-range", "anon_close_range");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let outside = FileDesc::from(eventfd2(sys, 1, 0).unwrap());
            let marked: Vec<FileDesc> = (0..3)
                .map(|_| FileDesc::from(eventfd2(sys, 1, 0).unwrap()))
                .collect();
            let first = usize::from(marked[0]);
            let last = usize::from(marked[2]);

            // Marking: every one of them is close-on-exec afterwards, and
            // every one of them is still open.
            assert_eq!(sys.sys_close_range(first, last, CLOSE_RANGE_CLOEXEC), Ok(0));
            for fd in &marked {
                assert_eq!(
                    lp.fd_cloexec(*fd),
                    Ok(true),
                    "CLOSE_RANGE_CLOEXEC did not mark {fd:?}"
                );
            }
            assert_eq!(
                lp.fd_cloexec(outside),
                Ok(false),
                "a descriptor outside the range was marked"
            );
            // `CLOSE_RANGE_UNSHARE` asks for a private fd table, which this
            // kernel never shares, so it is a no-op and not an error -- and
            // not a reason to close anything either.
            assert_eq!(
                sys.sys_close_range(first, last, CLOSE_RANGE_UNSHARE | CLOSE_RANGE_CLOEXEC),
                Ok(0)
            );
            assert_eq!(lp.fd_cloexec(marked[1]), Ok(true));

            // Closing: now they are gone, and the one outside is not.
            assert_eq!(sys.sys_close_range(first, last, 0), Ok(0));
            for fd in &marked {
                assert_eq!(
                    lp.fd_cloexec(*fd),
                    Err(LxError::EBADF),
                    "{fd:?} was marked instead of closed"
                );
            }
            assert_eq!(lp.fd_cloexec(outside), Ok(false));
            // A flag it does not have is EINVAL, which is how a caller
            // finds out the kernel is older than the flag it asked for,
            // and an inverted range is EINVAL too.
            assert_eq!(
                sys.sys_close_range(first, last, A_FLAG_OF_NOBODY),
                Err(LxError::EINVAL)
            );
            assert_eq!(sys.sys_close_range(last, first, 0), Err(LxError::EINVAL));
            // And `close` of the one that is left, twice: the second is
            // EBADF, which is what makes a double close a bug a program
            // can see.
            assert_eq!(sys.sys_close(outside), Ok(0));
            assert_eq!(sys.sys_close(outside), Err(LxError::EBADF));
        })
    });
}

#[test]
fn the_arguments_arrive_in_the_order_the_syscall_table_names_them() {
    // All seven by number, through the table, which is the only thing
    // userspace does. A handler declared after another's shape, or two
    // arguments the wrong way round, passes every test above and nothing
    // here.
    let proc = a_process(47_372, "anon-by-number", "anon_by_number");
    let dir_path = alloc::string::String::from("/anon_by_number");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            a_file_named(&lp, &dir_path, "f", 0, 0o644);
            let path = c(&alloc::format!("{dir_path}/f"));
            let name = c("by-number");

            // `eventfd2(initval, flags)`: the count is the first register
            // and the flags the second. The value arrives as an `unsigned
            // int`, so the high half of the register is dropped, and a
            // kernel that kept it would hand back a counter of its own.
            let fd = by_number(
                sys,
                crate::Sys::EVENTFD2,
                [(1 << 32) | 5, ANON_NONBLOCK, 0, 0, 0, 0],
            )
            .await;
            assert!(fd >= 0, "eventfd2 by number: {}", fd);
            assert_eq!(read_a_count(sys, fd as usize).await, Ok(5));

            // `memfd_create(name, flags)`.
            let memfd = by_number(
                sys,
                crate::Sys::MEMFD_CREATE,
                [name.as_ptr() as usize, MFD_CLOEXEC, 0, 0, 0, 0],
            )
            .await;
            assert!(memfd >= 0, "memfd_create by number: {}", memfd);
            assert_eq!(lp.fd_cloexec(FileDesc::from(memfd as usize)), Ok(true));

            // `inotify_init1(flags)`, then `inotify_add_watch(fd, path,
            // mask)` and `inotify_rm_watch(fd, wd)`. The watch calls take
            // three and two registers, and the mask is the only one of
            // them that is not a pointer or a descriptor.
            let watcher = by_number(
                sys,
                crate::Sys::INOTIFY_INIT1,
                [ANON_CLOEXEC, 0, 0, 0, 0, 0],
            )
            .await;
            assert!(watcher >= 0, "inotify_init1 by number: {}", watcher);
            assert_eq!(lp.fd_cloexec(FileDesc::from(watcher as usize)), Ok(true));
            let wd = by_number(
                sys,
                crate::Sys::INOTIFY_ADD_WATCH,
                [
                    watcher as usize,
                    path.as_ptr() as usize,
                    IN_ALL_EVENTS as usize,
                    0,
                    0,
                    0,
                ],
            )
            .await;
            assert!(wd >= 0, "inotify_add_watch by number: {}", wd);
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::INOTIFY_RM_WATCH,
                    [watcher as usize, wd as usize, 0, 0, 0, 0],
                )
                .await,
                0,
                "inotify_rm_watch by number did not take the watch away"
            );

            // `signalfd4(fd, mask, sizemask, flags)`: four registers, and
            // the first is a descriptor that may be -1.
            let mask = TWO_SIGNALS;
            let signalfd = by_number(
                sys,
                crate::Sys::SIGNALFD4,
                [
                    usize::MAX,
                    &mask as *const u64 as usize,
                    A_SIGSET,
                    ANON_CLOEXEC,
                    0,
                    0,
                ],
            )
            .await;
            assert!(signalfd >= 0, "signalfd4 by number: {}", signalfd);
            assert_eq!(lp.fd_cloexec(FileDesc::from(signalfd as usize)), Ok(true));
            assert!(fdinfo_of(&lp, signalfd as usize)
                .contains(&alloc::format!("sigmask:\t{TWO_SIGNALS:016x}")));

            // `close_range(first, last, flags)`: the flag word is the
            // third register, and reading it as a descriptor would close
            // the range it was told to mark.
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::CLOSE_RANGE,
                    [fd as usize, signalfd as usize, CLOSE_RANGE_CLOEXEC, 0, 0, 0],
                )
                .await,
                0
            );
            assert_eq!(lp.fd_cloexec(FileDesc::from(fd as usize)), Ok(true));
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::CLOSE_RANGE,
                    [fd as usize, signalfd as usize, 0, 0, 0, 0],
                )
                .await,
                0
            );
            assert_eq!(
                lp.fd_cloexec(FileDesc::from(fd as usize)),
                Err(LxError::EBADF),
                "the range was marked instead of closed"
            );
        })
    });
}
