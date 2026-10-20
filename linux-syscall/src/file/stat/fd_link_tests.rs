//! Every descriptor in `/proc/self/fd` has the name Linux gives it, its link
//! answers `lstat` as Linux's does, and following it reaches the object. Only
//! a file had a name before: the entry of a socket or an eventfd was in the
//! listing and `ENOENT` when looked at, a memfd and an unlinked file lost
//! their ` (deleted)`, and every link had the mode and size of an ordinary
//! one. The anonymous objects were also open for reading only, where Linux
//! opens all but inotify's for reading and writing.

use super::pipe_tests::{a_process, fd_link, fstat, readlink};
use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use linux_object::net::UnixSocketState;

const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0o100;
const O_PATH: usize = 0o10_000_000;
const AT_SYMLINK_NOFOLLOW: usize = 0x100;
const F_GETFL: usize = 3;
const CLOCK_MONOTONIC: usize = 1;
const EFD_SEMAPHORE: usize = 1;

/// `(type and mode, ino, size)` of `stat` (or of `lstat`) of `path`.
fn stat_of(sys: &Syscall<'_>, path: &str, flags: usize) -> LxResult<(u32, u64, u64)> {
    let path = alloc::format!("{}\0", path);
    let mut st = [0u64; 32];
    sys.sys_fstatat(
        FileDesc::CWD,
        UserInPtr::from(path.as_ptr() as usize),
        UserOutPtr::from(st.as_mut_ptr() as usize),
        flags,
    )?;
    let bytes: Vec<u8> = st.iter().flat_map(|w| w.to_ne_bytes()).collect();
    let u32_at =
        |at: usize| u32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    let u64_at = |at: usize| u64::from(u32_at(at)) | u64::from(u32_at(at + 4)) << 32;
    Ok((
        u32_at(core::mem::offset_of!(Stat, mode)),
        u64_at(core::mem::offset_of!(Stat, ino)),
        u64_at(core::mem::offset_of!(Stat, size)),
    ))
}

fn open(sys: &Syscall<'_>, path: &str, flags: usize) -> LxResult<FileDesc> {
    let path = alloc::format!("{}\0", path);
    sys.sys_openat(
        FileDesc::CWD,
        UserInPtr::from(path.as_ptr() as usize),
        flags,
        0o600,
    )
    .map(|fd| (fd as i32).into())
}

fn fd(n: usize) -> FileDesc {
    (n as i32).into()
}

/// One of each anonymous object, in this order: eventfd, epoll, timerfd,
/// signalfd, inotify, a pidfd on the process itself and a perf event (a
/// disabled software one, which never samples).
fn anonymous_objects(sys: &Syscall<'_>, pid: usize) -> [FileDesc; 7] {
    let mask: u64 = 1 << 9;
    let mut attr = [0u8; 64];
    attr[0] = 1;
    attr[40] = 1;
    [
        sys.sys_eventfd2(0, 0),
        sys.sys_epoll_create1(0),
        sys.sys_timerfd_create(CLOCK_MONOTONIC, 0),
        sys.sys_signalfd4(
            (-1i32).into(),
            UserInPtr::from(&mask as *const u64 as usize),
            8,
            0,
        ),
        sys.sys_inotify_init1(0),
        sys.sys_pidfd_open(pid, 0),
        sys.sys_perf_event_open(attr.as_ptr() as usize, 0, -1, -1, 0),
    ]
    .map(|got| fd(got.unwrap()))
}

#[test]
fn each_anonymous_object_is_named_as_linux_names_it() {
    let proc = a_process(45_491);
    let names = on_a_thread(&proc, |sys| {
        Box::pin(async move { anonymous_objects(sys, 45_491).map(|f| readlink(sys, &fd_link(f))) })
    });
    let want = [
        "anon_inode:[eventfd]",
        "anon_inode:[eventpoll]",
        "anon_inode:[timerfd]",
        "anon_inode:[signalfd]",
        "anon_inode:inotify",
        "anon_inode:[pidfd]",
        "anon_inode:[perf_event]",
    ];
    assert_eq!(names, want.map(|name| Ok(String::from(name))));
}

#[test]
fn anonymous_objects_are_open_for_reading_and_writing_but_inotify() {
    let proc = a_process(45_492);
    let modes = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            // `EFD_SEMAPHORE` is bit 0 of `eventfd2`'s word, where `O_WRONLY`
            // is in an open flag word.
            let semaphore = fd(sys.sys_eventfd2(0, EFD_SEMAPHORE).unwrap());
            let mut all = anonymous_objects(sys, 45_492).to_vec();
            all.push(semaphore);
            let mut modes = Vec::new();
            for f in all {
                modes.push(sys.sys_fcntl(f, F_GETFL, 0).await.map(|fl| fl & 3));
            }
            modes
        })
    });
    assert_eq!(modes, [2, 2, 2, 2, 0, 2, 2, 2].map(Ok));
}

#[test]
fn a_socket_is_named_by_its_inode() {
    let proc = a_process(45_493);
    let socket = proc.linux().add_file(UnixSocketState::new()).unwrap();
    let (ino, name) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            (
                fstat(sys, socket).unwrap().1,
                readlink(sys, &fd_link(socket)),
            )
        })
    });
    assert_eq!(name, Ok(alloc::format!("socket:[{}]", ino)));
}

#[test]
fn a_memfd_and_an_unlinked_file_are_deleted() {
    let proc = a_process(45_494);
    let path = "/fd-link-45494";
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let memfd = fd(sys
                .sys_memfd_create(UserInPtr::from(b"hello\0".as_ptr() as usize), 0)
                .unwrap());
            let file = open(sys, path, O_CREAT | O_RDWR).unwrap();
            let before = readlink(sys, &fd_link(file));
            let name = alloc::format!("{}\0", path);
            sys.sys_unlinkat(FileDesc::CWD, UserInPtr::from(name.as_ptr() as usize), 0)
                .unwrap();
            (
                readlink(sys, &fd_link(memfd)),
                before,
                readlink(sys, &fd_link(file)),
            )
        })
    });
    assert_eq!(
        got,
        (
            Ok(String::from("/memfd:hello (deleted)")),
            Ok(String::from(path)),
            Ok(alloc::format!("{} (deleted)", path)),
        )
    );
}

/// `tid_fd_update_inode`: `r-x` for reading, `-wx` for writing, nothing for
/// `O_PATH`, and a size of 64 whatever the name is.
#[test]
fn the_link_says_how_the_descriptor_was_opened() {
    let proc = a_process(45_495);
    let path = "/fd-link-45495";
    let links = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let [eventfd, _, _, _, inotify, ..] = anonymous_objects(sys, 45_495);
            [
                open(sys, path, O_CREAT | O_RDWR).unwrap(),
                open(sys, path, 0).unwrap(),
                open(sys, path, O_WRONLY).unwrap(),
                open(sys, path, O_PATH).unwrap(),
                eventfd,
                inotify,
            ]
            .map(|f| {
                stat_of(sys, &fd_link(f), AT_SYMLINK_NOFOLLOW).map(|(mode, _, size)| (mode, size))
            })
        })
    });
    let link = StatMode::LINK.bits();
    assert_eq!(
        links,
        [0o700, 0o500, 0o300, 0, 0o700, 0o500].map(|mode| Ok((link | mode, 64)))
    );
}

/// What the link leads to, for a socket and for an eventfd: `stat` through
/// it is `fstat` of the descriptor, an open of it is `ENXIO`, and an
/// `O_PATH` open takes hold of the object under the same name.
#[test]
fn an_object_through_its_link_is_the_object_and_opens_only_as_o_path() {
    let proc = a_process(45_496);
    let socket = proc.linux().add_file(UnixSocketState::new()).unwrap();
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let eventfd = fd(sys.sys_eventfd2(0, 0).unwrap());
            [socket, eventfd].map(|f| {
                let own = fstat(sys, f).map(|st| (st.0, st.1));
                let through = stat_of(sys, &fd_link(f), 0).map(|st| (st.0, st.1));
                let reopened = open(sys, &fd_link(f), 0);
                let held = open(sys, &fd_link(f), O_PATH).unwrap();
                (
                    own.is_ok() && own == through,
                    reopened,
                    fstat(sys, held).map(|st| st.1) == own.map(|st| st.1),
                    readlink(sys, &fd_link(held)) == readlink(sys, &fd_link(f)),
                )
            })
        })
    });
    assert_eq!(got, [(true, Err(LxError::ENXIO), true, true); 2]);
}
