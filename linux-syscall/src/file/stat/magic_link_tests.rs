//! A link in `/proc` or `/dev` leads where it says from the root of the
//! process, and a magic one (`/proc/<pid>/fd/N`, `cwd`, `root`, `exe`) to
//! what the process holds, as Linux 6.18 does (measured there with glibc and
//! musl). From `/proc` an absolute link started over at the root of `/proc`,
//! and `/proc/<pid>/fd/N` led to its text, which names no file for a pipe.
//!
//! Every test has a top directory of its own, as in `opened_path_tests`.

use super::opened_path_tests::{a_process, c_path, chdir, name_of, open_at};
use super::pipe_tests::{fd_link, pipe, readlink};
use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rcore_fs::vfs::FileType;

const O_WRONLY: usize = 1;
const O_NONBLOCK: usize = 0o4000;
const O_DIRECTORY: usize = 0o200_000;
const O_NOFOLLOW: usize = 0o400_000;
const O_PATH: usize = 0o10_000_000;

fn unlink(sys: &Syscall<'_>, path: &str) {
    let path = c_path(path);
    sys.sys_unlinkat(FileDesc::CWD, UserInPtr::from(path.as_ptr() as usize), 0)
        .unwrap();
}

fn symlink(sys: &Syscall<'_>, to: &str, at: &str) {
    let (to, at) = (c_path(to), c_path(at));
    sys.sys_symlinkat(
        UserInPtr::from(to.as_ptr() as usize),
        FileDesc::CWD,
        UserInPtr::from(at.as_ptr() as usize),
    )
    .unwrap();
}

async fn write(sys: &Syscall<'_>, fd: FileDesc, data: &[u8]) -> LxResult<usize> {
    sys.sys_write(fd, UserInPtr::from(data.as_ptr() as usize), data.len())
        .await
}

async fn read(sys: &Syscall<'_>, fd: FileDesc) -> LxResult<Vec<u8>> {
    let mut buf = [0u8; 16];
    let n = sys
        .sys_read(fd, UserOutPtr::from(buf.as_mut_ptr() as usize), buf.len())
        .await?;
    Ok(buf[..n].to_vec())
}

fn open_err(sys: &Syscall<'_>, path: &str, flags: usize) -> Option<LxError> {
    let path = c_path(path);
    sys.sys_openat(
        FileDesc::CWD,
        UserInPtr::from(path.as_ptr() as usize),
        flags,
        0,
    )
    .err()
}

#[test]
fn cwd_and_root_lead_from_the_root_of_the_process() {
    let proc = a_process(45_531, "pm_cwd");
    let pid = proc.id();
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            chdir(sys, "/pm_cwd/real");
            [
                name_of(sys, "/proc/self/cwd/f", 0),
                name_of(sys, &format!("/proc/{}/cwd/f", pid), 0),
                name_of(sys, "/proc/thread-self/cwd/f", 0),
                name_of(sys, "/proc/self/cwd/..", O_DIRECTORY),
                name_of(sys, "/proc/self/root/pm_cwd/l/f", 0),
                name_of(sys, "/proc/self/root", O_DIRECTORY),
            ]
        })
    });
    assert_eq!(
        got,
        [
            "/pm_cwd/real/f",
            "/pm_cwd/real/f",
            "/pm_cwd/real/f",
            "/pm_cwd",
            "/pm_cwd/real/f",
            "/",
        ]
    );
}

/// The program, through a link on its way, by any name of the process.
#[test]
fn exe_leads_to_the_program_by_any_pid() {
    let proc = a_process(45_532, "pm_exe");
    let pid = proc.id();
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            sys.linux_process().set_execute_path("/pm_exe/l/f");
            [
                name_of(sys, &format!("/proc/{}/exe", pid), 0),
                name_of(sys, "/proc/thread-self/exe", 0),
            ]
        })
    });
    assert_eq!(got, ["/pm_exe/real/f", "/pm_exe/real/f"]);
}

/// A process that has run no program has none, and one recorded in `/proc`
/// would lead back to its own link.
#[test]
fn an_exe_that_is_none_or_in_proc_leads_nowhere() {
    let proc = a_process(45_533, "pm_loop");
    let pid = proc.id();
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let exe = format!("/proc/{}/exe", pid);
            let none = open_err(sys, &exe, O_DIRECTORY);
            sys.linux_process().set_execute_path(&exe);
            [none, open_err(sys, &exe, 0)]
        })
    });
    assert_eq!(got, [Some(LxError::ENOENT), Some(LxError::ENOENT)]);
}

#[test]
fn a_descriptor_by_any_pid_is_the_file_even_once_it_is_unlinked() {
    let proc = a_process(45_534, "pm_fd");
    let pid = proc.id();
    let (before, after) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let fd = i32::from(open_at(sys, FileDesc::CWD, "/pm_fd/l/f", 0));
            let by_pid = format!("/proc/{}/fd/{}", pid, fd);
            let before = [
                name_of(sys, &by_pid, 0),
                name_of(sys, &format!("/proc/thread-self/fd/{}", fd), 0),
            ];
            unlink(sys, "/pm_fd/real/f");
            (before, name_of(sys, &by_pid, 0))
        })
    });
    assert_eq!(before, ["/pm_fd/real/f", "/pm_fd/real/f"]);
    assert_eq!(after, "/pm_fd/real/f (deleted)");
}

/// A directory's descriptor leads into the directory.
#[test]
fn a_name_after_a_directory_descriptor_is_in_the_directory() {
    let proc = a_process(45_535, "pm_dir");
    let pid = proc.id();
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let dir = i32::from(open_at(sys, FileDesc::CWD, "/pm_dir/l", O_DIRECTORY));
            [
                name_of(sys, &format!("/proc/self/fd/{}/f", dir), 0),
                name_of(sys, &format!("/dev/fd/{}/f", dir), 0),
                name_of(sys, &format!("/proc/{}/fd/{}/sub/../f", pid, dir), 0),
            ]
        })
    });
    assert_eq!(got, ["/pm_dir/real/f", "/pm_dir/real/f", "/pm_dir/real/f"]);
}

#[test]
fn dev_fd_and_the_std_links_lead_into_proc_self_fd() {
    let proc = a_process(45_536, "pm_std");
    let pid = proc.id();
    let (links, fd_dir) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let links = ["/dev/stdin", "/dev/stdout", "/dev/stderr", "/dev/fd"]
                .map(|link| readlink(sys, link));
            (links, name_of(sys, "/dev/fd", O_DIRECTORY))
        })
    });
    assert_eq!(
        links,
        [
            "/proc/self/fd/0",
            "/proc/self/fd/1",
            "/proc/self/fd/2",
            "/proc/self/fd"
        ]
        .map(|to| Ok(String::from(to)))
    );
    assert_eq!(fd_dir, format!("/proc/{}/fd", pid));
}

/// `cmd < file` and then `cat /dev/stdin`: the file, from its start.
#[test]
fn dev_stdin_of_a_file_reopens_it_from_its_start() {
    let proc = a_process(45_537, "pm_in");
    let (data, name) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let w = open_at(sys, FileDesc::CWD, "/pm_in/real/f", O_WRONLY);
            write(sys, w, b"hello").await.unwrap();
            let fd = open_at(sys, FileDesc::CWD, "/pm_in/l/f", 0);
            read(sys, fd).await.unwrap();
            sys.sys_dup2(fd, 0).unwrap();
            let again = open_at(sys, FileDesc::CWD, "/dev/stdin", 0);
            (read(sys, again).await, readlink(sys, &fd_link(again)))
        })
    });
    assert_eq!(data, Ok(b"hello".to_vec()));
    assert_eq!(name, Ok(String::from("/pm_in/real/f")));
}

/// `echo hi | cat /dev/stdin` and `cmd > /dev/stdout` into a pipe.
#[test]
fn dev_stdin_and_stdout_of_a_pipe_are_the_pipe() {
    let proc = a_process(45_538, "pm_pipe");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let (r, w) = pipe(sys);
            sys.sys_dup2(r, 0).unwrap();
            sys.sys_dup2(w, 1).unwrap();
            let out = open_at(sys, FileDesc::CWD, "/dev/stdout", O_WRONLY);
            let written = write(sys, out, b"ok").await;
            let input = open_at(sys, FileDesc::CWD, "/dev/stdin", 0);
            (written, read(sys, input).await)
        })
    });
    assert_eq!(got, (Ok(2), Ok(b"ok".to_vec())));
}

/// A descriptor of the link itself does not hold the open file: the last
/// writer of a pipe that closes is the end of it, as it would be without one.
#[test]
fn a_descriptor_of_an_fd_link_keeps_no_pipe_open() {
    let proc = a_process(45_539, "pm_hold");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut fds = [0i32; 2];
            sys.sys_pipe2(UserOutPtr::from(fds.as_mut_ptr() as usize), O_NONBLOCK)
                .unwrap();
            let (r, w) = (FileDesc::from(fds[0]), FileDesc::from(fds[1]));
            let link = open_at(sys, FileDesc::CWD, &fd_link(w), O_PATH | O_NOFOLLOW);
            let empty = read(sys, r).await;
            sys.sys_close(w).unwrap();
            let closed = read(sys, r).await;
            sys.sys_close(link).unwrap();
            (empty, closed)
        })
    });
    assert_eq!(got, (Err(LxError::EAGAIN), Ok(Vec::new())));
}

/// A link in `/dev/shm`, which is a file system of its own, to `/dev/null`.
#[test]
fn a_link_in_dev_shm_to_dev_null_opens_it() {
    let proc = a_process(45_540, "pm_shm");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            symlink(sys, "/dev/null", "/dev/shm/pm_shm_null");
            let fd = open_at(sys, FileDesc::CWD, "/dev/shm/pm_shm_null", O_WRONLY);
            let written = write(sys, fd, b"x").await;
            unlink(sys, "/dev/shm/pm_shm_null");
            (written, readlink(sys, &fd_link(fd)))
        })
    });
    assert_eq!(got, (Ok(1), Ok(String::from("/dev/null"))));
}

/// A link on the root that leads into `/proc` (`/etc/mtab ->
/// ../proc/self/mounts`), or through a magic link there, finds another file
/// for each process, so the path cache does not hand one process's to the
/// next. Both processes are made before either looks: making one changes
/// the namespace, which empties the cache.
#[test]
fn a_link_through_proc_is_not_kept_for_the_next_process() {
    let procs = [(45_541, "/pm_keep/real"), (45_542, "/pm_keep/real/sub")].map(|(pid, dir)| {
        let proc = a_process(pid, "pm_keep");
        let root = proc.linux().root_inode().clone();
        let top = root.find("pm_keep").unwrap();
        let link = |name: &str, to: &str| {
            let l = top.create(name, FileType::SymLink, 0o777).unwrap();
            l.write_at(0, to.as_bytes()).unwrap();
        };
        link("me", "/proc/self/stat");
        link("here", "/proc/self/cwd");
        link("prog", "/proc/self/exe");
        let sub = root.lookup("pm_keep/real/sub").unwrap();
        sub.create("f", FileType::File, 0o644).unwrap();
        proc.linux().set_execute_path(&format!("{}/f", dir));
        (proc, dir)
    });
    let got = procs.map(|(proc, dir)| {
        on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                chdir(sys, dir);
                ["/pm_keep/me", "/pm_keep/here/f", "/pm_keep/prog"]
                    .map(|path| name_of(sys, path, 0))
            })
        })
    });
    assert_eq!(
        got,
        [
            ["/proc/45541/stat", "/pm_keep/real/f", "/pm_keep/real/f"],
            [
                "/proc/45542/stat",
                "/pm_keep/real/sub/f",
                "/pm_keep/real/sub/f"
            ],
        ]
        .map(|names| names.map(String::from))
    );
}

/// `..` at the top of `/proc`, `/sys`, `/dev` and the `/dev/shm` in it is
/// the directory each is in, as from any other mount, and so from a
/// descriptor of one of them: `/proc/..` was `/proc`, and `/dev/..`
/// `ENOENT`.
#[test]
fn dotdot_climbs_out_of_proc_sys_dev_and_dev_shm() {
    let proc = a_process(45_543, "pm_up");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let dir = open_at(sys, FileDesc::CWD, "/proc", O_DIRECTORY);
            let up = open_at(sys, dir, "../pm_up/real", O_DIRECTORY);
            [
                name_of(sys, "/proc/..", O_DIRECTORY),
                name_of(sys, "/proc/self/../../pm_up/l/f", 0),
                name_of(sys, "/sys/../pm_up/real/f", 0),
                name_of(sys, "/dev/../pm_up/real/f", 0),
                name_of(sys, "/dev/shm/../null", O_WRONLY),
                readlink(sys, &fd_link(up)).unwrap(),
            ]
        })
    });
    assert_eq!(
        got,
        [
            "/",
            "/pm_up/real/f",
            "/pm_up/real/f",
            "/pm_up/real/f",
            "/dev/null",
            "/pm_up/real"
        ]
    );
}
