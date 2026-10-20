//! A file is named in `/proc/self/fd` by the path the walk took, as Linux
//! 6.18 names it (measured there with glibc and musl): from the root,
//! through no link, with the pid in place of `/proc/self`.
//!
//! Every test has a top directory of its own: the path cache, which keeps
//! each name with its inode, is shared by every process in this binary.

use super::pipe_tests::{fd_link, readlink};
use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use linux_object::process::LinuxProcess;
use linux_object::thread::ThreadExt;
use rcore_fs::vfs::{FileType, INode};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, Thread, ROOT_JOB};

const O_WRONLY: usize = 1;
const O_CREAT: usize = 0o100;
const O_EXCL: usize = 0o200;
const O_DIRECTORY: usize = 0o200_000;
const O_NOFOLLOW: usize = 0o400_000;
const O_PATH: usize = 0o10_000_000;

/// A process whose root holds, under `/<top>`: `real/f`, `real/sub/`,
/// `l` -> `/<top>/real`, `rl` -> `real`, `ll` -> `l`, `real/lf` ->
/// `/<top>/real/f` and `real/up` -> `../real/f`.
fn a_process(pid: KoID, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let link = |dir: &Arc<dyn INode>, name: &str, to: &str| {
        let l = dir.create(name, FileType::SymLink, 0o777).unwrap();
        l.write_at(0, to.as_bytes()).unwrap();
    };
    let top_dir = lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    let real = top_dir.create("real", FileType::Dir, 0o755).unwrap();
    real.create("f", FileType::File, 0o644).unwrap();
    real.create("sub", FileType::Dir, 0o755).unwrap();
    link(&top_dir, "l", &alloc::format!("/{}/real", top));
    link(&top_dir, "rl", "real");
    link(&top_dir, "ll", "l");
    link(&real, "lf", &alloc::format!("/{}/real/f", top));
    link(&real, "up", "../real/f");
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "names", lp).unwrap()
}

fn c_path(path: &str) -> String {
    alloc::format!("{}\0", path)
}

fn open_at(sys: &Syscall<'_>, dir: FileDesc, path: &str, flags: usize) -> FileDesc {
    let path = c_path(path);
    let fd = sys
        .sys_openat(dir, UserInPtr::from(path.as_ptr() as usize), flags, 0o600)
        .unwrap_or_else(|e| panic!("open {:?}: {:?}", path, e));
    (fd as i32).into()
}

/// What `/proc/self/fd/N` says of `open(path, flags)`.
fn name_of(sys: &Syscall<'_>, path: &str, flags: usize) -> String {
    let fd = open_at(sys, FileDesc::CWD, path, flags);
    let name = readlink(sys, &fd_link(fd)).unwrap();
    sys.sys_close(fd).unwrap();
    name
}

fn chdir(sys: &Syscall<'_>, path: &str) {
    let path = c_path(path);
    sys.sys_chdir(UserInPtr::from(path.as_ptr() as usize))
        .unwrap();
}

/// The calls run on a second thread: the leader's tid is the pid, so on it
/// a `thread-self` that put the pid where the tid goes would look right.
#[test]
fn proc_self_and_thread_self_are_named_by_the_pid_and_the_tid() {
    let proc = a_process(45_511, "pn_proc");
    let _leader = Thread::create_linux(&proc).unwrap();
    let (got, tid) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut got = Vec::new();
            got.push(name_of(sys, "/proc/self/stat", 0));
            got.push(name_of(sys, "/proc/self", O_DIRECTORY));
            got.push(name_of(sys, "/proc/thread-self/stat", 0));
            got.push(name_of(sys, "/proc/self/fd", O_DIRECTORY));
            // `exe` leads to the file the process runs.
            sys.linux_process().set_execute_path("/pn_proc/l/f");
            got.push(name_of(sys, "/proc/self/exe", 0));
            chdir(sys, "/proc");
            got.push(name_of(sys, "self/../self/status", 0));
            (got, sys.sys_gettid().unwrap())
        })
    });
    assert_ne!(tid, 45_511);
    assert_eq!(
        got,
        [
            String::from("/proc/45511/stat"),
            String::from("/proc/45511"),
            alloc::format!("/proc/45511/task/{}/stat", tid),
            String::from("/proc/45511/fd"),
            String::from("/pn_proc/real/f"),
            String::from("/proc/45511/status"),
        ]
    );
}

/// The second open of each path is served by the path cache, which keeps
/// the name with the inode.
#[test]
fn a_file_is_named_through_no_link_on_the_way_or_at_the_end() {
    let proc = a_process(45_512, "pn_links");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            [
                "/pn_links/l/f",
                "/pn_links/rl/f",
                "/pn_links/ll/sub/../f",
                "/pn_links/real/lf",
                "/pn_links/real/up",
                "//pn_links//real/./sub/..//f",
            ]
            .map(|path| [name_of(sys, path, 0), name_of(sys, path, 0)])
        })
    });
    assert_eq!(got, [["/pn_links/real/f"; 2]; 6]);
}

#[test]
fn a_directory_through_a_link_names_what_is_opened_from_it() {
    let proc = a_process(45_513, "pn_dirs");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let dir = open_at(sys, FileDesc::CWD, "/pn_dirs/l", O_DIRECTORY);
            let in_dir = open_at(sys, dir, "f", 0);
            let created = name_of(sys, "/pn_dirs/l/new", O_CREAT | O_EXCL | O_WRONLY);
            // `O_CREAT` over a file that is there, and over a link to one.
            let there = name_of(sys, "/pn_dirs/l/f", O_CREAT | O_WRONLY);
            let over_link = name_of(sys, "/pn_dirs/l/lf", O_CREAT | O_WRONLY);
            chdir(sys, "/pn_dirs/l");
            let from_cwd = name_of(sys, "f", 0);
            [
                readlink(sys, &fd_link(dir)).unwrap(),
                readlink(sys, &fd_link(in_dir)).unwrap(),
                created,
                there,
                over_link,
                from_cwd,
            ]
        })
    });
    assert_eq!(
        got,
        [
            "/pn_dirs/real",
            "/pn_dirs/real/f",
            "/pn_dirs/real/new",
            "/pn_dirs/real/f",
            "/pn_dirs/real/f",
            "/pn_dirs/real/f",
        ]
        .map(String::from)
    );
}

/// `O_PATH|O_NOFOLLOW` holds a link itself, in the directory it is really
/// in; a last component that is no name is where it leads.
#[test]
fn a_link_held_by_o_path_is_named_in_its_real_directory() {
    let proc = a_process(45_518, "pn_opath");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let (to, at) = (c_path("/pn_opath/real/f"), c_path("/pn_opath_root"));
            sys.sys_symlinkat(
                UserInPtr::from(to.as_ptr() as usize),
                FileDesc::CWD,
                UserInPtr::from(at.as_ptr() as usize),
            )
            .unwrap();
            [
                "/pn_opath/l/lf",
                "/pn_opath/l/.",
                "/pn_opath/l/sub/..",
                "/pn_opath_root",
            ]
            .map(|path| name_of(sys, path, O_PATH | O_NOFOLLOW))
        })
    });
    assert_eq!(
        got,
        [
            "/pn_opath/real/lf",
            "/pn_opath/real",
            "/pn_opath/real",
            "/pn_opath_root",
        ]
        .map(String::from)
    );
}

#[test]
fn a_reopen_through_a_descriptor_is_the_file_even_once_it_is_unlinked() {
    let proc = a_process(45_514, "pn_reopen");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let fd = open_at(sys, FileDesc::CWD, "/pn_reopen/l/f", 0);
            let reopened = name_of(sys, &fd_link(fd), 0);
            let path = c_path("/pn_reopen/real/f");
            sys.sys_unlinkat(FileDesc::CWD, UserInPtr::from(path.as_ptr() as usize), 0)
                .unwrap();
            [reopened, name_of(sys, &fd_link(fd), 0)]
        })
    });
    assert_eq!(
        got,
        ["/pn_reopen/real/f", "/pn_reopen/real/f (deleted)"].map(String::from)
    );
}

#[test]
fn a_link_that_leads_elsewhere_now_names_where_it_leads_now() {
    let proc = a_process(45_515, "pn_moved");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let before = name_of(sys, "/pn_moved/l/f", 0);
            let (l, other) = (c_path("/pn_moved/l"), c_path("/pn_moved/other"));
            sys.sys_unlinkat(FileDesc::CWD, UserInPtr::from(l.as_ptr() as usize), 0)
                .unwrap();
            sys.sys_mkdirat(
                FileDesc::CWD,
                UserInPtr::from(other.as_ptr() as usize),
                0o755,
            )
            .unwrap();
            sys.sys_symlinkat(
                UserInPtr::from(other.as_ptr() as usize),
                FileDesc::CWD,
                UserInPtr::from(l.as_ptr() as usize),
            )
            .unwrap();
            name_of(sys, "/pn_moved/other/f", O_CREAT | O_WRONLY);
            [before, name_of(sys, "/pn_moved/l/f", 0)]
        })
    });
    assert_eq!(
        got,
        ["/pn_moved/real/f", "/pn_moved/other/f"].map(String::from)
    );
}

/// The same path through `/proc/self` is another file to each process, so
/// it is not kept for the next one to open.
#[test]
fn each_process_names_proc_self_after_itself() {
    let name = |pid| {
        on_a_thread(&a_process(pid, &alloc::format!("pn_each{}", pid)), |sys| {
            Box::pin(async move { name_of(sys, "/proc/self/stat", 0) })
        })
    };
    assert_eq!(
        [name(45_516), name(45_517)],
        ["/proc/45516/stat", "/proc/45517/stat"].map(String::from)
    );
}

/// `/proc`, `/sys` and `/dev` are each a file system of its own, and a
/// directory whose name only starts like one is on the root.
#[test]
fn a_directory_named_like_proc_is_not_in_it() {
    let proc = a_process(45_519, "proc_pn");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move { name_of(sys, "/proc_pn/l/f", 0) })
    });
    assert_eq!(got, "/proc_pn/real/f");
}

/// A lookup that leaves the last link in place, as `lstat` and `readlink`
/// make, ends at the link itself, in the directory it is really in.
#[test]
fn a_lookup_that_keeps_the_last_link_ends_at_the_link() {
    let proc = a_process(45_520, "pn_keep");
    let (got, fd) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let fd = open_at(sys, FileDesc::CWD, "/pn_keep/real/f", 0);
            let lp = sys.linux_process();
            lp.set_execute_path("/pn_keep/real/f");
            let got = ["/proc/self/exe", &fd_link(fd), "/pn_keep/l/lf"].map(|path| {
                let (_, walked) = lp.lookup_inode_walked(FileDesc::CWD, path, false).unwrap();
                walked.path()
            });
            (got, i32::from(fd))
        })
    });
    assert_eq!(
        got,
        [
            String::from("/proc/45520/exe"),
            alloc::format!("/proc/45520/fd/{}", fd),
            String::from("/pn_keep/real/lf"),
        ]
    );
}

/// `/dev` and `/sys`, like `/proc`, are named from their own top.
#[test]
fn a_file_in_dev_or_sys_is_named_from_there() {
    let proc = a_process(45_521, "pn_virt");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            [
                name_of(sys, "/dev/null", O_WRONLY),
                name_of(sys, "/dev/", O_DIRECTORY),
                name_of(sys, "/sys/class/../power", O_DIRECTORY),
            ]
        })
    });
    assert_eq!(got, ["/dev/null", "/dev", "/sys/power"]);
}
