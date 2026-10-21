//! A walk asks each directory on its way for search permission, as Linux
//! 6.18 does (`walk-survey` and `parent-survey`, measured there with glibc
//! and musl): as the real user for `access(2)`, through a process's own `fd`
//! directory whatever its owner, from the mode a `chmod` has just given, and
//! of the directory a syscall then looks its last name up in.

use super::pipe_tests::{a_process, pipe};
use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use rcore_fs::vfs::FileType;

const R_OK: usize = 4;
const AT_EACCESS: usize = 0x200;
const O_DIRECTORY: usize = 0o200_000;
const O_WRONLY: usize = 1;
const O_CREAT: usize = 0o100;
const O_EXCL: usize = 0o200;
const O_NOFOLLOW: usize = 0o400_000;
const O_PATH: usize = 0o10_000_000;
const S_IFIFO: usize = 0o010_000;
const AT_REMOVEDIR: usize = 0x200;
const AT_EMPTY_PATH: usize = 0x1000;

fn c(path: &str) -> String {
    format!("{}\0", path)
}

/// `name` in the root, a directory with `mode` owned by `uid`, holding `f`.
fn a_directory(sys: &Syscall<'_>, name: &str, mode: u16, uid: usize) {
    let dir = sys
        .linux_process()
        .root_inode()
        .create(name, FileType::Dir, mode as u32)
        .unwrap();
    let mut meta = dir.metadata().unwrap();
    (meta.mode, meta.uid, meta.gid) = (mode, uid, uid);
    dir.set_metadata(&meta).unwrap();
    dir.create("f", FileType::File, 0o644).unwrap();
}

fn access(sys: &Syscall<'_>, path: &str, flags: usize) -> SysResult {
    let path = c(path);
    sys.sys_faccessat(
        FileDesc::CWD,
        UserInPtr::from(path.as_ptr() as usize),
        R_OK,
        flags,
    )
}

fn open(sys: &Syscall<'_>, path: &str, flags: usize) -> SysResult {
    let path = c(path);
    sys.sys_openat(
        FileDesc::CWD,
        UserInPtr::from(path.as_ptr() as usize),
        flags,
        0,
    )
}

#[test]
fn access_walks_as_the_real_user_unless_asked_for_the_effective_one() {
    // setresuid(1000, 0, 0) and a 0700 directory of uid 2000: open goes
    // through, access(2) does not, and `AT_EACCESS` does again.
    let proc = a_process(45_601);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            a_directory(sys, "shut", 0o700, 2000);
            sys.linux_process().set_resuid(1000, 0, 0).unwrap();
            (
                access(sys, "/shut/f", 0),
                access(sys, "/shut/f", AT_EACCESS),
                open(sys, "/shut/f", 0).map(|_| 0),
            )
        })
    });
    assert_eq!(got, (Err(LxError::EACCES), Ok(0), Ok(0)));
}

#[test]
fn a_process_that_is_not_dumpable_walks_through_its_own_fd_by_its_pid() {
    // Its `fd` is root's and 0500 then (`proc_fd_permission`); `/proc/self/fd`
    // has a way of its own, `/proc/<pid>/fd` is walked.
    let proc = a_process(45_602);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            sys.linux_process().set_resgid(2000, 2000, 2000).unwrap();
            sys.linux_process().set_resuid(1000, 1000, 1000).unwrap();
            // Made by the user, whose it is to open again.
            let (read, _write) = pipe(sys);
            let pid = sys.zircon_process().id();
            open(sys, &format!("/proc/{}/fd/{}", pid, i32::from(read)), 0).map(|_| ())
        })
    });
    assert_eq!(got, Ok(()));
}

#[test]
fn chmod_of_a_directory_is_asked_about_by_the_next_walk() {
    // The owner walks `/d/f` and the path cache keeps it, with no gate: `d`
    // is 0755. A chmod to 0600 takes `x` from the owner, by each of the three
    // calls that make it.
    for call in 0..3 {
        let proc = a_process(45_603 + call);
        let got = on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                a_directory(sys, "d", 0o755, 1000);
                sys.linux_process().set_resgid(1000, 1000, 1000).unwrap();
                sys.linux_process().set_resuid(1000, 1000, 1000).unwrap();
                let before = access(sys, "/d/f", 0);
                let d = c("/d");
                let at = UserInPtr::from(d.as_ptr() as usize);
                let chmod = match call {
                    0 => sys.sys_chmod(at, 0o600),
                    1 => match open(sys, "/d", O_DIRECTORY) {
                        Ok(fd) => sys.sys_fchmod((fd as i32).into(), 0o600),
                        Err(e) => Err(e),
                    },
                    _ => sys.sys_fchmodat(FileDesc::CWD, at, 0o600, 0),
                };
                (before, chmod, access(sys, "/d/f", 0))
            })
        });
        assert_eq!(got, (Ok(0), Ok(0), Err(LxError::EACCES)), "call {}", call);
    }
}

/// `path`, made by [`c`], as the pointer a syscall takes it by.
fn p(path: &String) -> UserInPtr<u8> {
    UserInPtr::from(path.as_ptr() as usize)
}

/// A file of uid 1000 in the root.
fn mine(sys: &Syscall<'_>) {
    let file = sys
        .linux_process()
        .root_inode()
        .create("mine", FileType::File, 0o644)
        .unwrap();
    let mut meta = file.metadata().unwrap();
    (meta.uid, meta.gid) = (1000, 1000);
    file.set_metadata(&meta).unwrap();
}

fn become_1000(sys: &Syscall<'_>) {
    sys.linux_process().set_resgid(1000, 1000, 1000).unwrap();
    sys.linux_process().set_resuid(1000, 1000, 1000).unwrap();
}

#[test]
fn a_last_name_is_not_looked_up_in_a_directory_the_caller_may_not_search() {
    // `parent-survey`: `link_path_walk` asks the directory of the last name
    // too, so whatever is or is not in it, each call is EACCES.
    let proc = a_process(45_606);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            a_directory(sys, "shut", 0o700, 2000);
            let shut = sys.linux_process().root_inode().find("shut").unwrap();
            let l = shut.create("l", FileType::SymLink, 0o777).unwrap();
            l.write_at(0, b"f").unwrap();
            mine(sys);
            become_1000(sys);
            let (f, absent, n, m) = (c("/shut/f"), c("/shut/absent"), c("/n"), c("/mine"));
            let gone = c("/gone");
            [
                sys.sys_mkdirat(FileDesc::CWD, p(&f), 0o755),
                sys.sys_mknodat(FileDesc::CWD, p(&f), S_IFIFO | 0o644, 0),
                sys.sys_symlinkat(p(&n), FileDesc::CWD, p(&f)),
                sys.sys_linkat(FileDesc::CWD, p(&m), FileDesc::CWD, p(&f), 0),
                sys.sys_unlinkat(FileDesc::CWD, p(&absent), 0),
                sys.sys_unlinkat(FileDesc::CWD, p(&absent), AT_REMOVEDIR),
                sys.sys_renameat(FileDesc::CWD, p(&absent), FileDesc::CWD, p(&n)),
                // Both directories are walked to before the source is looked
                // up: EACCES, not the ENOENT of a source that is not there.
                sys.sys_renameat(FileDesc::CWD, p(&gone), FileDesc::CWD, p(&absent)),
                open(sys, "/shut/l", O_NOFOLLOW),
                open(sys, "/shut/l", O_PATH | O_NOFOLLOW),
                open(sys, "/shut/f", O_CREAT),
            ]
        })
    });
    assert_eq!(got, [Err(LxError::EACCES); 11]);
}

#[test]
fn a_directory_searched_but_not_written_answers_from_the_lookup() {
    // Only a name to make, or to take away, needs `w`: `O_CREAT` opens a
    // name that is there (`lookup_open` keeps `may_o_create`'s error for a
    // name that is not) and `O_EXCL` is EEXIST.
    let proc = a_process(45_607);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            a_directory(sys, "ro", 0o555, 2000);
            let ro = sys.linux_process().root_inode().find("ro").unwrap();
            let f = ro.find("f").unwrap();
            let mut meta = f.metadata().unwrap();
            meta.mode = 0o666;
            f.set_metadata(&meta).unwrap();
            become_1000(sys);
            let (f, absent) = (c("/ro/f"), c("/ro/absent"));
            [
                open(sys, "/ro/f", O_CREAT | O_WRONLY).map(|_| 0),
                open(sys, "/ro/f", O_CREAT | O_EXCL | O_WRONLY),
                open(sys, "/ro/n", O_CREAT | O_WRONLY),
                sys.sys_mkdirat(FileDesc::CWD, p(&f), 0o755),
                sys.sys_unlinkat(FileDesc::CWD, p(&absent), 0),
            ]
        })
    });
    use LxError::*;
    assert_eq!(
        got,
        [Ok(0), Err(EEXIST), Err(EACCES), Err(EEXIST), Err(ENOENT)]
    );
}

#[test]
fn a_last_name_under_a_file_is_enotdir_before_any_permission() {
    let proc = a_process(45_608);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            mine(sys);
            become_1000(sys);
            let x = c("/mine/x");
            [
                sys.sys_mkdirat(FileDesc::CWD, p(&x), 0o755),
                sys.sys_unlinkat(FileDesc::CWD, p(&x), 0),
                open(sys, "/mine/x", O_CREAT | O_WRONLY),
            ]
        })
    });
    assert_eq!(got, [Err(LxError::ENOTDIR); 3]);
}

#[test]
fn the_working_directory_itself_is_no_walk() {
    // `AT_EMPTY_PATH` names the working directory, which nothing is looked
    // up in: a cwd its user may no longer search still answers, `.` not.
    let proc = a_process(45_609);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            a_directory(sys, "d", 0o755, 1000);
            become_1000(sys);
            sys.linux_process().change_directory("/d");
            let d = sys.linux_process().root_inode().find("d").unwrap();
            let mut meta = d.metadata().unwrap();
            meta.mode = 0o600;
            d.set_metadata(&meta).unwrap();
            let empty = c("");
            (
                sys.sys_faccessat(FileDesc::CWD, p(&empty), 0, AT_EMPTY_PATH),
                access(sys, ".", 0),
            )
        })
    });
    assert_eq!(got, (Ok(0), Err(LxError::EACCES)));
}
