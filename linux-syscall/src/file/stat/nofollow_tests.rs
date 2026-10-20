//! `lstat`, `readlink` and `O_NOFOLLOW` leave the last component alone and
//! follow every link on the way to it, as Linux does. With "do not follow"
//! the walk used to follow no link at all, so one in the middle of the path
//! was `ENOTDIR`: `lstat("/proc/self/stat")` failed, and `ls -l /proc/self`
//! printed an error for every file in it.
//!
//! Every test has a top directory of its own: a lookup from the root goes
//! through a path cache that every process in this binary shares.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::{FileType, INode};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// `AT_SYMLINK_NOFOLLOW`.
const NOFOLLOW: usize = 0x100;
/// `O_NOFOLLOW` on x86_64 and the generic table.
const O_NOFOLLOW: usize = 0o400_000;

/// A process whose root holds `/<top>/d/f`, `/<top>/d/fl` -> `f` and
/// `/<top>/dl` -> `d`.
fn a_process(pid: KoID, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let link = |dir: &Arc<dyn INode>, name: &str, to: &str| {
        let l = dir.create(name, FileType::SymLink, 0o777).unwrap();
        l.write_at(0, to.as_bytes()).unwrap();
    };
    let top_dir = lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    let d = top_dir.create("d", FileType::Dir, 0o755).unwrap();
    d.create("f", FileType::File, 0o644).unwrap();
    link(&d, "fl", "f");
    link(&top_dir, "dl", "d");
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "nofollow", lp).unwrap()
}

/// The file type `fstatat(AT_FDCWD, path, flags)` reports.
fn kind(sys: &Syscall<'_>, path: &str, flags: usize) -> LxResult<u32> {
    let path = alloc::format!("{}\0", path);
    // Room for a `Stat`, aligned for it; its `mode` is read back by offset.
    let mut st = [0u64; 32];
    assert!(core::mem::size_of::<Stat>() <= core::mem::size_of_val(&st));
    sys.sys_fstatat(
        FileDesc::CWD,
        UserInPtr::from(path.as_ptr() as usize),
        UserOutPtr::from(st.as_mut_ptr() as usize),
        flags,
    )?;
    let at = core::mem::offset_of!(Stat, mode);
    let bytes: alloc::vec::Vec<u8> = st.iter().flat_map(|w| w.to_ne_bytes()).collect();
    let mode = u32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    Ok(mode & StatMode::TYPE_MASK.bits())
}

const REG: u32 = StatMode::FILE.bits();
const LNK: u32 = StatMode::LINK.bits();
const DIR: u32 = StatMode::DIR.bits();

#[test]
fn lstat_follows_a_link_on_the_way_and_leaves_the_last_one() {
    let proc = a_process(45_471, "nf_a");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            [
                kind(sys, "/nf_a/dl/f", NOFOLLOW),
                kind(sys, "/nf_a/dl/fl", NOFOLLOW),
                kind(sys, "/nf_a/dl/fl", 0),
                kind(sys, "/nf_a/dl", NOFOLLOW),
                kind(sys, "/nf_a/dl/", NOFOLLOW),
            ]
        })
    });
    assert_eq!(got, [Ok(REG), Ok(LNK), Ok(REG), Ok(LNK), Ok(DIR)]);
}

#[test]
fn proc_self_is_a_link_on_the_way_like_any_other() {
    let proc = a_process(45_472, "nf_b");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            [
                kind(sys, "/proc/self/stat", NOFOLLOW),
                kind(sys, "/proc/self/status", NOFOLLOW),
                kind(sys, "/proc/self", NOFOLLOW),
            ]
        })
    });
    assert_eq!(got, [Ok(REG), Ok(REG), Ok(LNK)]);
}

#[test]
fn readlink_and_o_nofollow_through_a_link_on_the_way() {
    let proc = a_process(45_473, "nf_c");
    let (target, last, on_the_way) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut buf = [0u8; 16];
            let n = sys
                .sys_readlinkat(
                    FileDesc::CWD,
                    UserInPtr::from(b"/nf_c/dl/fl\0".as_ptr() as usize),
                    UserOutPtr::from(buf.as_mut_ptr() as usize),
                    buf.len(),
                )
                .map(|n| buf[..n].to_vec());
            let last = sys.sys_openat(
                FileDesc::CWD,
                UserInPtr::from(b"/nf_c/dl/fl\0".as_ptr() as usize),
                O_NOFOLLOW,
                0,
            );
            let on_the_way = sys.sys_openat(
                FileDesc::CWD,
                UserInPtr::from(b"/nf_c/dl/f\0".as_ptr() as usize),
                O_NOFOLLOW,
                0,
            );
            (n, last, on_the_way.map(|_| ()))
        })
    });
    assert_eq!(target, Ok(b"f".to_vec()));
    assert_eq!(
        last,
        Err(LxError::ELOOP),
        "O_NOFOLLOW still refuses the last link"
    );
    assert_eq!(on_the_way, Ok(()));
}
