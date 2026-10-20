//! `AT_EMPTY_PATH` names the descriptor, an empty path without it is
//! `ENOENT`, and a pathname that is there but unreadable is an error rather
//! than an invitation to stat the descriptor instead.
//!
//! This is the door every `fstat` in the system goes through: musl spells
//! `fstat(fd, ...)` as `fstatat(fd, "", AT_EMPTY_PATH)`, and as
//! `statx(fd, NULL, AT_EMPTY_PATH, ...)` where it has `statx`. The two
//! syscalls write the same rule two different ways -- one reads the pathname
//! first, the other asks whether it is null -- so each one is pinned here on
//! its own.
//!
//! Every test has a top directory of its own: a lookup from the root goes
//! through a path cache that every process in this binary shares.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::mem::{offset_of, size_of, size_of_val};
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::{FileType, INode};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// `AT_EMPTY_PATH`.
const EMPTY_PATH: usize = 0x1000;
/// `AT_SYMLINK_NOFOLLOW`.
const NOFOLLOW: usize = 0x100;
/// `O_DIRECTORY` on x86_64 and the generic table.
const O_DIRECTORY: usize = 0o200_000;
/// The mask `statx` is asked for. It is advisory, and what comes back is
/// checked field by field in `stat_conversion_tests`, not here.
const BASIC_STATS: u32 = 0x07ff;

const REG: u32 = StatMode::FILE.bits();
const LNK: u32 = StatMode::LINK.bits();
const DIR: u32 = StatMode::DIR.bits();

/// A process whose root holds `/<top>/f`, `/<top>/d/f`, `/<top>/fl` -> `f`
/// and `/<top>/dl` -> `d`.
fn a_process(pid: KoID, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let link = |dir: &Arc<dyn INode>, name: &str, to: &str| {
        let l = dir.create(name, FileType::SymLink, 0o777).unwrap();
        l.write_at(0, to.as_bytes()).unwrap();
    };
    let top_dir = lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    top_dir.create("f", FileType::File, 0o644).unwrap();
    let d = top_dir.create("d", FileType::Dir, 0o755).unwrap();
    d.create("f", FileType::File, 0o644).unwrap();
    link(&top_dir, "fl", "f");
    link(&top_dir, "dl", "d");
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "empty-path", lp).unwrap()
}

fn c_path(path: &str) -> String {
    alloc::format!("{}\0", path)
}

fn open_at(sys: &Syscall<'_>, path: &str, flags: usize) -> FileDesc {
    let path = c_path(path);
    let fd = sys
        .sys_openat(
            FileDesc::CWD,
            UserInPtr::from(path.as_ptr() as usize),
            flags,
            0o600,
        )
        .unwrap_or_else(|e| panic!("open {:?}: {:?}", path, e));
    (fd as i32).into()
}

fn chdir(sys: &Syscall<'_>, path: &str) {
    let path = c_path(path);
    sys.sys_chdir(UserInPtr::from(path.as_ptr() as usize))
        .unwrap();
}

/// The buffer the syscall wrote, as bytes, so a field can be read at the
/// offset its ABI gives it.
fn bytes(buf: &[u64; 32]) -> Vec<u8> {
    buf.iter().flat_map(|w| w.to_ne_bytes()).collect()
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_ne_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(&b[at..at + 8]);
    u64::from_ne_bytes(w)
}

/// The file type and the inode number `fstatat(dirfd, <pathname>, flags)`
/// reports, with the pathname passed as the raw pointer it is.
fn stat_raw(
    sys: &Syscall<'_>,
    dirfd: FileDesc,
    pathname: usize,
    flags: usize,
) -> LxResult<(u32, u64)> {
    let mut st = [0u64; 32];
    assert!(size_of::<Stat>() <= size_of_val(&st));
    sys.sys_fstatat(
        dirfd,
        UserInPtr::from(pathname),
        UserOutPtr::from(st.as_mut_ptr() as usize),
        flags,
    )?;
    let b = bytes(&st);
    Ok((
        u32_at(&b, offset_of!(Stat, mode)) & StatMode::TYPE_MASK.bits(),
        u64_at(&b, offset_of!(Stat, ino)),
    ))
}

fn stat_at(sys: &Syscall<'_>, dirfd: FileDesc, path: &str, flags: usize) -> LxResult<(u32, u64)> {
    let path = c_path(path);
    stat_raw(sys, dirfd, path.as_ptr() as usize, flags)
}

/// The same two fields through `stat(2)` or `lstat(2)`, which take no
/// `dirfd` and no flags: the flags each one passes on are what is under test.
fn one_arg(sys: &Syscall<'_>, path: &str, lstat: bool) -> LxResult<(u32, u64)> {
    let path = c_path(path);
    let mut st = [0u64; 32];
    let p = UserInPtr::from(path.as_ptr() as usize);
    let out = UserOutPtr::from(st.as_mut_ptr() as usize);
    if lstat {
        sys.sys_lstat(p, out)?;
    } else {
        sys.sys_stat(p, out)?;
    }
    let b = bytes(&st);
    Ok((
        u32_at(&b, offset_of!(Stat, mode)) & StatMode::TYPE_MASK.bits(),
        u64_at(&b, offset_of!(Stat, ino)),
    ))
}

/// The same two fields out of `statx`, whose `stx_mode` is sixteen bits.
fn statx_raw(
    sys: &Syscall<'_>,
    dirfd: FileDesc,
    pathname: usize,
    flags: usize,
) -> LxResult<(u32, u64)> {
    let mut x = [0u64; 32];
    assert_eq!(size_of::<Statx>(), size_of_val(&x));
    sys.sys_statx(
        dirfd,
        UserInPtr::from(pathname),
        flags,
        BASIC_STATS,
        UserOutPtr::from(x.as_mut_ptr() as usize),
    )?;
    let b = bytes(&x);
    Ok((
        u32::from(u16_at(&b, offset_of!(Statx, stx_mode))) & StatMode::TYPE_MASK.bits(),
        u64_at(&b, offset_of!(Statx, stx_ino)),
    ))
}

fn statx_at(sys: &Syscall<'_>, dirfd: FileDesc, path: &str, flags: usize) -> LxResult<(u32, u64)> {
    let path = c_path(path);
    statx_raw(sys, dirfd, path.as_ptr() as usize, flags)
}

/// The flag and the empty path are an AND, and it reads both ways: without
/// the flag an empty path names nothing, and with it a pathname that is
/// there still wins.
#[test]
fn an_empty_path_names_the_descriptor_only_with_at_empty_path() {
    let proc = a_process(45_531, "ep_flag");
    let (kinds, inos) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let f = open_at(sys, "/ep_flag/f", 0);
            let d = open_at(sys, "/ep_flag/d", O_DIRECTORY);
            let kinds = [
                // The descriptor itself, which is what musl's `fstat` asks.
                stat_at(sys, f, "", EMPTY_PATH).map(|(t, _)| t),
                // Without the flag an empty path is `ENOENT`, whatever the
                // descriptor is: `stat("")` names no file.
                stat_at(sys, f, "", 0).map(|(t, _)| t),
                stat_at(sys, d, "", 0).map(|(t, _)| t),
                // And a pathname that is not empty wins over the flag: this
                // is `d/f`, not `d`.
                stat_at(sys, d, "f", EMPTY_PATH).map(|(t, _)| t),
            ];
            let inos = (
                stat_at(sys, d, "f", EMPTY_PATH).map(|(_, i)| i),
                stat_at(sys, FileDesc::CWD, "/ep_flag/d/f", 0).map(|(_, i)| i),
                stat_at(sys, d, "", EMPTY_PATH).map(|(_, i)| i),
            );
            (kinds, inos)
        })
    });
    assert_eq!(
        kinds,
        [Ok(REG), Err(LxError::ENOENT), Err(LxError::ENOENT), Ok(REG)]
    );
    assert_eq!(inos.0, inos.1, "`d/f` with AT_EMPTY_PATH is the file `f`");
    assert_ne!(inos.0, inos.2, "and not the directory `d` the fd names");
}

/// `AT_FDCWD` is not a descriptor in the table, so the empty path is served
/// from the working directory there and from the file behind the descriptor
/// everywhere else.
#[test]
fn at_fdcwd_with_an_empty_path_is_the_working_directory() {
    let proc = a_process(45_532, "ep_cwd");
    let (cwd, named_dir, on_a_fd, named_file) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            chdir(sys, "/ep_cwd/d");
            let cwd = stat_at(sys, FileDesc::CWD, "", EMPTY_PATH);
            let named_dir = stat_at(sys, FileDesc::CWD, "/ep_cwd/d", 0);
            let f = open_at(sys, "/ep_cwd/f", 0);
            let on_a_fd = stat_at(sys, f, "", EMPTY_PATH);
            let named_file = stat_at(sys, FileDesc::CWD, "/ep_cwd/f", 0);
            (cwd, named_dir, on_a_fd, named_file)
        })
    });
    assert_eq!(cwd, named_dir, "AT_FDCWD is the directory, not a bad fd");
    assert_eq!(cwd.map(|(t, _)| t), Ok(DIR));
    assert_eq!(on_a_fd, named_file, "and any other fd is its own file");
    assert_eq!(on_a_fd.map(|(t, _)| t), Ok(REG));
}

/// `stat` and `lstat` are one call with one flag between them, and nothing
/// here called either of them by its own name until now.
#[test]
fn stat_follows_the_last_link_and_lstat_leaves_it() {
    let proc = a_process(45_533, "ep_one");
    let (kinds, inos) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let kinds = [
                one_arg(sys, "/ep_one/fl", false).map(|(t, _)| t),
                one_arg(sys, "/ep_one/fl", true).map(|(t, _)| t),
                // A link on the way to the last component is followed by
                // both of them.
                one_arg(sys, "/ep_one/dl/f", false).map(|(t, _)| t),
                one_arg(sys, "/ep_one/dl/f", true).map(|(t, _)| t),
            ];
            let inos = (
                one_arg(sys, "/ep_one/fl", false).map(|(_, i)| i),
                one_arg(sys, "/ep_one/f", false).map(|(_, i)| i),
                one_arg(sys, "/ep_one/fl", true).map(|(_, i)| i),
            );
            (kinds, inos)
        })
    });
    assert_eq!(kinds, [Ok(REG), Ok(LNK), Ok(REG), Ok(REG)]);
    assert_eq!(inos.0, inos.1, "`stat` of a link is the file it names");
    assert_ne!(inos.0, inos.2, "and `lstat` of it is the link itself");
}

/// `statx` reads the pathname before it decides, so it has a third case
/// `fstatat` does not: one that is there and cannot be read.
#[test]
fn statx_takes_the_descriptor_for_a_null_or_empty_pathname_only() {
    let proc = a_process(45_534, "ep_statx");
    let (null, empty, not_utf8, named) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let f = open_at(sys, "/ep_statx/f", 0);
            // What musl's `fstat` sends: no pathname at all.
            let null = statx_raw(sys, f, 0, EMPTY_PATH);
            let empty = statx_at(sys, f, "", EMPTY_PATH);
            // A pathname that is there and cannot be read is an error, not
            // an invitation to stat the descriptor instead.
            let bad = b"\xff\xfe\0";
            let not_utf8 = statx_raw(sys, f, bad.as_ptr() as usize, EMPTY_PATH);
            let named = statx_at(sys, FileDesc::CWD, "/ep_statx/f", 0);
            (null, empty, not_utf8, named)
        })
    });
    assert_eq!(null, named, "a null pathname with AT_EMPTY_PATH is the fd");
    assert_eq!(empty, named, "and so is an empty one");
    assert_eq!(null.map(|(t, _)| t), Ok(REG));
    assert_eq!(
        not_utf8,
        Err(LxError::EINVAL),
        "an unreadable pathname is refused, not dropped"
    );
}

/// The flag `statx` resolves its pathname with, which is the one thing
/// `stat_conversion_tests` could not see: it never gave `statx` a link.
#[test]
fn statx_leaves_the_last_link_with_at_symlink_nofollow() {
    let proc = a_process(45_535, "ep_sx_nf");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            [
                statx_at(sys, FileDesc::CWD, "/ep_sx_nf/fl", 0).map(|(t, _)| t),
                statx_at(sys, FileDesc::CWD, "/ep_sx_nf/fl", NOFOLLOW).map(|(t, _)| t),
                // And follows every link on the way to the last component.
                statx_at(sys, FileDesc::CWD, "/ep_sx_nf/dl/f", NOFOLLOW).map(|(t, _)| t),
            ]
        })
    });
    assert_eq!(got, [Ok(REG), Ok(LNK), Ok(REG)]);
}
