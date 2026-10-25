//! A working directory that is renamed or removed while a process is in it,
//! through the syscalls, as Linux 6.18 answers (`cwd-survey`, not in the
//! repository: 22 checks, glibc and musl).
//!
//! Fixed pids live in 45_71x here, and every test has a top directory of its
//! own, for the path cache every process in this binary shares.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::fs::{File, OpenFlags};
use linux_object::process::ProcessExt;
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// A process `pid` in `/<top>/d`, with `/<top>/d/f`.
fn a_process_in(pid: u64, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let top_dir = lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    let d = top_dir.create("d", FileType::Dir, 0o755).unwrap();
    d.create("f", FileType::File, 0o644).unwrap();
    lp.change_directory(&format!("/{}/d", top));
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "cwd", lp).unwrap()
}

fn path(s: &[u8]) -> UserInPtr<u8> {
    UserInPtr::from(s.as_ptr() as usize)
}

fn getcwd(sys: &Syscall<'_>) -> LxResult<String> {
    let mut buf = [0u8; 128];
    let len = sys.sys_getcwd(UserOutPtr::from(buf.as_mut_ptr() as usize), buf.len())?;
    Ok(String::from_utf8(buf[..len - 1].to_vec()).unwrap())
}

#[test]
fn getcwd_and_relative_names_follow_a_rename_of_the_working_directory() {
    let proc = a_process_in(45_711, "held_a");
    let (cwd, f) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            sys.sys_renameat(
                FileDesc::CWD,
                path(b"/held_a/d\0"),
                FileDesc::CWD,
                path(b"/held_a/e\0"),
            )
            .unwrap();
            let cwd = getcwd(sys);
            let f = sys.sys_openat(FileDesc::CWD, path(b"f\0"), 0, 0);
            (cwd, f.is_ok())
        })
    });
    assert_eq!(cwd.as_deref(), Ok("/held_a/e"));
    assert!(f, "a relative open still finds the file");
}

#[test]
fn chdir_dot_dot_gets_out_of_a_removed_working_directory() {
    let proc = a_process_in(45_712, "held_b");
    let (dot, gone, out, after) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            sys.sys_unlinkat(FileDesc::CWD, path(b"/held_b/d/f\0"), 0)
                .unwrap();
            sys.sys_unlinkat(
                FileDesc::CWD,
                path(b"/held_b/d\0"),
                0x200, /* AT_REMOVEDIR */
            )
            .unwrap();
            // `chdir(".")` stays in it, by the directory and not its path.
            sys.sys_chdir(path(b".\0")).unwrap();
            let dot = sys.sys_openat(
                FileDesc::CWD,
                path(b".\0"),
                0o200000, /* O_DIRECTORY */
                0,
            );
            let gone = getcwd(sys);
            let out = sys.sys_chdir(path(b"..\0"));
            (dot.is_ok(), gone, out, getcwd(sys))
        })
    });
    assert!(dot, "`.` still opens");
    assert_eq!(gone, Err(LxError::ENOENT));
    assert_eq!(out, Ok(0), "`..` is still the directory it was in");
    assert_eq!(after.as_deref(), Ok("/held_b"));
}

/// Nothing can be made in a removed working directory, and listing it is
/// `ENOENT` too.
#[test]
fn a_removed_working_directory_takes_nothing_in_and_lists_nothing() {
    let proc = a_process_in(45_713, "held_c");
    let (created, made, listed) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let proc = sys.linux_process();
            let dir = proc.lookup_inode("/held_c/d").unwrap();
            let fd = proc
                .add_file(File::new(dir, OpenFlags::RDONLY, String::from("/held_c/d")))
                .unwrap();
            sys.sys_unlinkat(FileDesc::CWD, path(b"/held_c/d/f\0"), 0)
                .unwrap();
            sys.sys_unlinkat(FileDesc::CWD, path(b"/held_c/d\0"), 0x200)
                .unwrap();
            let created = sys.sys_openat(
                FileDesc::CWD,
                path(b"new\0"),
                0o101, /* O_CREAT|O_WRONLY */
                0o644,
            );
            let made = sys.sys_mkdirat(FileDesc::CWD, path(b"sub\0"), 0o755);
            let mut buf = [0u8; 512];
            let listed =
                sys.sys_getdents64(fd, UserOutPtr::from(buf.as_mut_ptr() as usize), buf.len());
            (created.err(), made.err(), listed.err())
        })
    });
    assert_eq!(created, Some(LxError::ENOENT));
    assert_eq!(made, Some(LxError::ENOENT));
    assert_eq!(listed, Some(LxError::ENOENT));
}

/// `fchdir` into a directory removed since it was opened, and a fork's child
/// is there too.
#[test]
fn fchdir_into_a_removed_directory_and_a_fork_is_in_it_too() {
    let proc = a_process_in(45_714, "held_d");
    let (entered, gone, dot_links, child_gone) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let proc = sys.linux_process();
            let x = proc
                .lookup_inode("/held_d")
                .unwrap()
                .create("x", FileType::Dir, 0o755)
                .unwrap();
            let fd = proc
                .add_file(File::new(x, OpenFlags::RDONLY, String::from("/held_d/x")))
                .unwrap();
            sys.sys_unlinkat(FileDesc::CWD, path(b"/held_d/x\0"), 0x200)
                .unwrap();
            let entered = sys.sys_fchdir(fd);
            let gone = getcwd(sys);
            let dot_links = proc.lookup_inode(".").unwrap().metadata().unwrap().nlinks;
            // `fstatat(AT_FDCWD, "", AT_EMPTY_PATH)` is the directory too.
            let mut st = [0u64; 32];
            let empty = sys.sys_fstatat(
                FileDesc::CWD,
                path(b"\0"),
                UserOutPtr::from(st.as_mut_ptr() as usize),
                0x1000, /* AT_EMPTY_PATH */
            );
            assert_eq!(empty, Ok(0));
            let child = Process::fork_from(sys.zircon_process()).unwrap();
            let child_gone = child.linux().working_directory_removed()
                && child.linux().lookup_inode(".").is_ok();
            child.exit(0);
            (entered, gone, dot_links, child_gone)
        })
    });
    assert_eq!(entered, Ok(0));
    assert_eq!(gone, Err(LxError::ENOENT));
    assert_eq!(dot_links, 0, "`.` is the removed directory");
    assert!(child_gone);
}

/// A descriptor opened before its directory was renamed: `fchdir` puts the
/// process in the directory, and `getcwd` names it by its new name, not by
/// the one the descriptor was opened with.
#[test]
fn fchdir_by_a_descriptor_opened_before_a_rename_is_in_it_by_its_new_name() {
    let proc = a_process_in(45_715, "held_e");
    let cwd = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let proc = sys.linux_process();
            let d = proc.lookup_inode("/held_e/d").unwrap();
            let fd = proc
                .add_file(File::new(d, OpenFlags::RDONLY, String::from("/held_e/d")))
                .unwrap();
            sys.sys_chdir(path(b"/\0")).unwrap();
            sys.sys_renameat(
                FileDesc::CWD,
                path(b"/held_e/d\0"),
                FileDesc::CWD,
                path(b"/held_e/e\0"),
            )
            .unwrap();
            sys.sys_fchdir(fd).unwrap();
            getcwd(sys)
        })
    });
    assert_eq!(cwd.as_deref(), Ok("/held_e/e"));
}
