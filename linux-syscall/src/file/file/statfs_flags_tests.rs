//! The mount flags `statfs(2)` and `fstatfs(2)` report in `f_flags`, as
//! Linux 6.18 reports them (measured there with glibc and musl):
//! `ST_VALID`, and read-only, nosuid, nodev and noexec from the mount the
//! path is on -- through links, from the working directory, and for a bind
//! remounted read-only apart from its source.
//!
//! Every test has a top directory and a pid of its own: the mount table and
//! the path cache are shared by every process in this binary.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use linux_object::process::{FsInfo, LinuxProcess};
use rcore_fs::vfs::{FileType, INode};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

const MS_RDONLY: usize = 1;
const MS_NOSUID: usize = 2;
const MS_NODEV: usize = 4;
const MS_NOEXEC: usize = 8;
const MS_REMOUNT: usize = 32;
const MS_BIND: usize = 4096;
const MNT_DETACH: usize = 2;

/// `include/linux/statfs.h`, written out rather than imported: what is
/// asked is whether the kernel's bits are these.
const ST_RDONLY: usize = 0x0001;
const ST_NOSUID: usize = 0x0002;
const ST_NODEV: usize = 0x0004;
const ST_NOEXEC: usize = 0x0008;
const ST_VALID: usize = 0x0020;

/// A root process whose root holds, under `/<top>`, the directories `n`,
/// `r`, `e` and `b`, each with a file `f` and a directory `d`, and a link
/// `l` -> `/<top>/n`.
fn a_process(pid: KoID, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let top_dir = lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    for name in ["n", "r", "e", "b"] {
        let dir = top_dir.create(name, FileType::Dir, 0o755).unwrap();
        dir.create("f", FileType::File, 0o644).unwrap();
        dir.create("d", FileType::Dir, 0o755).unwrap();
    }
    let link: Arc<dyn INode> = top_dir.create("l", FileType::SymLink, 0o777).unwrap();
    link.write_at(0, format!("/{}/n", top).as_bytes()).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, top, lp).unwrap()
}

/// A NUL-terminated copy of `s`.
fn c(s: &str) -> String {
    format!("{}\0", s)
}

fn ptr(s: &Option<String>) -> UserInPtr<u8> {
    UserInPtr::from(s.as_ref().map_or(0, |s| s.as_ptr() as usize))
}

/// A tmpfs on `target` with `flags`.
fn tmpfs(sys: &Syscall<'_>, target: &str, flags: usize) {
    let (source, target, fstype) = (Some(c("sf")), Some(c(target)), Some(c("tmpfs")));
    let got = sys.sys_mount(ptr(&source), ptr(&target), ptr(&fstype), flags, ptr(&None));
    assert_eq!(got, Ok(0), "the tmpfs on {:?}", target);
}

fn mount(sys: &Syscall<'_>, source: Option<&str>, target: &str, flags: usize) -> SysResult {
    let (source, target) = (source.map(c), Some(c(target)));
    sys.sys_mount(ptr(&source), ptr(&target), ptr(&None), flags, ptr(&None))
}

fn umount(sys: &Syscall<'_>, target: &str) {
    let _ = sys.sys_umount2(ptr(&Some(c(target))), MNT_DETACH);
}

/// A `struct statfs` for a call to write, with nothing in it yet.
fn blank() -> StatFs {
    let zero = FsInfo {
        bsize: 0,
        frsize: 0,
        blocks: 0,
        bfree: 0,
        bavail: 0,
        files: 0,
        ffree: 0,
        namemax: 0,
        ftype: 0,
    };
    StatFs {
        f_flags: -1,
        ..zero.into()
    }
}

fn out(st: &mut StatFs) -> UserOutPtr<StatFs> {
    UserOutPtr::from(st as *mut StatFs as usize)
}

/// `statfs(path).f_flags`.
fn flags_at(sys: &Syscall<'_>, path: &str) -> LxResult<usize> {
    let path = Some(c(path));
    let mut st = blank();
    sys.sys_statfs(ptr(&path), out(&mut st))?;
    Ok(st.f_flags as usize)
}

/// `fstatfs(open(path)).f_flags`.
fn flags_of_fd(sys: &Syscall<'_>, path: &str, flags: usize) -> LxResult<usize> {
    let path = Some(c(path));
    let fd = FileDesc::from(sys.sys_openat(FileDesc::CWD, ptr(&path), flags, 0)?);
    let mut st = blank();
    let got = sys.sys_fstatfs(fd, out(&mut st));
    sys.sys_close(fd)?;
    got?;
    Ok(st.f_flags as usize)
}

/// The tree [`a_process`] makes, with `n` mounted nosuid,nodev,noexec, `r`
/// read-only and `e` with none of them.
fn mounted(sys: &Syscall<'_>, top: &str) {
    tmpfs(
        sys,
        &format!("/{}/n", top),
        MS_NOSUID | MS_NODEV | MS_NOEXEC,
    );
    tmpfs(sys, &format!("/{}/r", top), MS_RDONLY);
    tmpfs(sys, &format!("/{}/e", top), 0);
}

fn unmounted(sys: &Syscall<'_>, top: &str) {
    for name in ["b", "n", "r", "e"] {
        umount(sys, &format!("/{}/{}", top, name));
    }
}

#[test]
fn statfs_of_a_nosuid_nodev_noexec_mount_says_so() {
    let proc = a_process(45_630, "sf_n");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            mounted(sys, "sf_n");
            let got = (flags_at(sys, "/sf_n/n/f"), flags_at(sys, "/sf_n/n/d"));
            unmounted(sys, "sf_n");
            got
        })
    });
    let want = Ok(ST_VALID | ST_NOSUID | ST_NODEV | ST_NOEXEC);
    assert_eq!(got.0, want, "a file");
    assert_eq!(got.1, want, "a directory");
}

#[test]
fn statfs_of_a_read_only_mount_has_st_rdonly_and_a_plain_one_none() {
    let proc = a_process(45_631, "sf_r");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            mounted(sys, "sf_r");
            let got = (flags_at(sys, "/sf_r/r/f"), flags_at(sys, "/sf_r/e/f"));
            unmounted(sys, "sf_r");
            got
        })
    });
    assert_eq!(got.0, Ok(ST_VALID | ST_RDONLY), "the read-only mount");
    assert_eq!(got.1, Ok(ST_VALID), "the plain mount");
}

#[test]
fn fstatfs_says_what_statfs_says() {
    const O_RDONLY: usize = 0;
    const O_DIRECTORY: usize = 0o200000;
    let proc = a_process(45_632, "sf_fd");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            mounted(sys, "sf_fd");
            let got = (
                flags_of_fd(sys, "/sf_fd/n/f", O_RDONLY),
                flags_of_fd(sys, "/sf_fd/r/d", O_DIRECTORY),
            );
            unmounted(sys, "sf_fd");
            got
        })
    });
    assert_eq!(got.0, Ok(ST_VALID | ST_NOSUID | ST_NODEV | ST_NOEXEC));
    assert_eq!(
        got.1,
        Ok(ST_VALID | ST_RDONLY),
        "a directory on the read-only mount"
    );
}

#[test]
fn a_bind_remounted_read_only_is_st_rdonly_and_its_source_is_not() {
    let proc = a_process(45_633, "sf_b");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            mounted(sys, "sf_b");
            let bind = mount(sys, Some("/sf_b/e/d"), "/sf_b/b", MS_BIND);
            let remount = mount(sys, None, "/sf_b/b", MS_REMOUNT | MS_BIND | MS_RDONLY);
            let got = (flags_at(sys, "/sf_b/b"), flags_at(sys, "/sf_b/e/d"));
            unmounted(sys, "sf_b");
            (bind, remount, got)
        })
    });
    assert_eq!((got.0, got.1), (Ok(0), Ok(0)), "the bind and its remount");
    assert_eq!(got.2 .0, Ok(ST_VALID | ST_RDONLY), "the bind");
    assert_eq!(got.2 .1, Ok(ST_VALID), "its source");
}

#[test]
fn statfs_through_a_link_is_of_the_mount_it_leads_to() {
    let proc = a_process(45_634, "sf_l");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            mounted(sys, "sf_l");
            let got = (flags_at(sys, "/sf_l/l/f"), flags_at(sys, "/sf_l/l"));
            unmounted(sys, "sf_l");
            got
        })
    });
    let want = Ok(ST_VALID | ST_NOSUID | ST_NODEV | ST_NOEXEC);
    assert_eq!(got.0, want, "through the link");
    assert_eq!(got.1, want, "the link itself, which statfs follows");
}

#[test]
fn statfs_of_a_relative_path_is_of_the_working_directorys_mount() {
    let proc = a_process(45_635, "sf_rel");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            mounted(sys, "sf_rel");
            let dir = Some(c("/sf_rel/r"));
            sys.sys_chdir(ptr(&dir)).unwrap();
            let got = (flags_at(sys, "f"), flags_at(sys, "."));
            sys.sys_chdir(ptr(&Some(c("/")))).unwrap();
            unmounted(sys, "sf_rel");
            got
        })
    });
    assert_eq!(got.0, Ok(ST_VALID | ST_RDONLY), "f");
    assert_eq!(got.1, Ok(ST_VALID | ST_RDONLY), ".");
}

#[test]
fn nosuid_nodev_and_noexec_are_each_their_own_bit() {
    let proc = a_process(45_636, "sf_bit");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut got = [Ok(0); 3];
            for (i, &flag) in [MS_NOSUID, MS_NODEV, MS_NOEXEC].iter().enumerate() {
                tmpfs(sys, "/sf_bit/e", flag);
                got[i] = flags_at(sys, "/sf_bit/e/f");
                umount(sys, "/sf_bit/e");
            }
            got
        })
    });
    assert_eq!(
        got,
        [
            Ok(ST_VALID | ST_NOSUID),
            Ok(ST_VALID | ST_NODEV),
            Ok(ST_VALID | ST_NOEXEC)
        ]
    );
}
