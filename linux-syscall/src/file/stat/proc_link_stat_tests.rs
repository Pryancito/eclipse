//! `lstat` of a link of `/proc/self` says what `lstat` of the same link by
//! pid says, as in Linux, where they are the same file: size 0 (64 for
//! `fd/N`), blocks of 1024 and a number of its own. `/proc/self/exe` and
//! `/proc/self/fd/N` are found apart from the rest of `/proc`, and `exe`
//! still said inode 0, the length of its text and blocks of 0.

use super::pipe_tests::{a_process, pipe};
use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;

const AT_SYMLINK_NOFOLLOW: usize = 0x100;

/// `(ino, size, blocks, blksize)` of `lstat(path)`.
fn lstat(sys: &Syscall<'_>, path: &str) -> (u64, u64, u64, u64) {
    let c_path = format!("{}\0", path);
    let mut st = [0u64; 32];
    sys.sys_fstatat(
        FileDesc::CWD,
        UserInPtr::from(c_path.as_ptr() as usize),
        UserOutPtr::from(st.as_mut_ptr() as usize),
        AT_SYMLINK_NOFOLLOW,
    )
    .unwrap_or_else(|e| panic!("lstat {}: {:?}", path, e));
    let bytes: Vec<u8> = st.iter().flat_map(|w| w.to_ne_bytes()).collect();
    let u32_at =
        |at: usize| u32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    let u64_at = |at: usize| u64::from(u32_at(at)) | u64::from(u32_at(at + 4)) << 32;
    (
        u64_at(core::mem::offset_of!(Stat, ino)),
        u64_at(core::mem::offset_of!(Stat, size)),
        u64_at(core::mem::offset_of!(Stat, blocks)),
        u64_at(core::mem::offset_of!(Stat, blksize)),
    )
}

#[test]
fn a_link_of_proc_self_is_the_link_of_proc_pid() {
    let proc = a_process(45_561);
    let pid = proc.id();
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            sys.linux_process().set_execute_path("/bin/a-program");
            let (fd, _) = pipe(sys);
            let fd = format!("fd/{}", i32::from(fd));
            ["exe", "cwd", "root", fd.as_str()].map(|name| {
                (
                    lstat(sys, &format!("/proc/self/{}", name)),
                    lstat(sys, &format!("/proc/{}/{}", pid, name)),
                )
            })
        })
    });
    let mut seen = Vec::new();
    for (i, &(by_self, by_pid)) in got.iter().enumerate() {
        let (ino, size, blocks, blksize) = by_self;
        assert_eq!(by_self, by_pid, "link {}", i);
        let fd = i == 3;
        assert_eq!(
            (size, blocks, blksize),
            (if fd { 64 } else { 0 }, 0, 1024),
            "link {}",
            i
        );
        assert!(ino != 0 && !seen.contains(&ino), "link {}: {}", i, ino);
        seen.push(ino);
    }
}
