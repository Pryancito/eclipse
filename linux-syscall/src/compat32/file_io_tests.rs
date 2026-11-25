//! A 32-bit program's file I/O and limits, made through the i386 dispatch
//! as the program makes them: the 64-bit offsets and lengths that arrive in
//! two registers (`arch/x86/ia32/sys_ia32.c`), `_llseek`'s result through a
//! pointer, the 8-byte `struct iovec` of `preadv`/`pwritev`, and the clamp
//! of `struct rlimit` (`compat_sys_getrlimit`, `compat_sys_old_getrlimit`).

use super::*;
use crate::on_a_thread::{claim_low_user_pages, on_a_thread};
use alloc::boxed::Box;
use alloc::vec::Vec;
use kernel_hal::MMUFlags;
use linux_object::fs::FileDesc;
use linux_object::process::{Abi, LinuxProcess};
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::{Process, ROOT_JOB};
use zircon_object::vm::VmObject;

const SEEK_SET: usize = 0;
const SEEK_END: usize = 2;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0o100;
const RLIMIT_CORE: usize = 4;
const EINVAL: isize = -22;
const EFAULT: isize = -14;

/// Pages of the process's own below 4 GiB, where a 32-bit `iov_base` can
/// point.
const LOW: usize = 0x2d00_0000;

fn a_process(pid: KoID) -> alloc::sync::Arc<Process> {
    claim_low_user_pages();
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(Abi::Linux32);
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "file32", lp).unwrap();
    proc.vmar()
        .map_at(
            LOW,
            VmObject::new_paged(1),
            0,
            0x1000,
            MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER,
        )
        .unwrap();
    proc
}

fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// Open (creating) `path`, a NUL-terminated name, read-write.
fn open(sys: &Syscall<'_>, path: &'static [u8]) -> usize {
    sys.sys_openat(
        FileDesc::CWD,
        UserInPtr::from(path.as_ptr() as usize),
        O_RDWR | O_CREAT,
        0o600,
    )
    .unwrap()
}

fn size_of_fd(sys: &Syscall<'_>, fd: usize) -> usize {
    sys.linux_process()
        .get_file(FileDesc::from(fd as i32))
        .unwrap()
        .metadata()
        .unwrap()
        .size
}

/// `pwrite64` and `pread64` take the offset low half first, and an upper
/// half past 32 bits in a register is not part of it.
#[test]
fn pread64_and_pwrite64_put_the_offset_together_low_first() {
    let proc = a_process(48_950);
    let (wrote, read, back) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let fd = open(&sys, b"/p64\0");
            let data = b"halves";
            let wrote = sys
                .compat32_syscall(
                    I386_PWRITE64,
                    [fd, data.as_ptr() as usize, data.len(), 4, 0, 0],
                )
                .await;
            // Stray bits above the 32 of each register: still offset 4.
            let mut buf = [0u8; 6];
            let read = sys
                .compat32_syscall(
                    I386_PREAD64,
                    [
                        fd,
                        buf.as_mut_ptr() as usize,
                        buf.len(),
                        0xdead_0000_0000_0004,
                        0xbeef_0000_0000_0000,
                        0,
                    ],
                )
                .await;
            (wrote, read, buf)
        })
    });
    assert_eq!(wrote, 6);
    assert_eq!(read, 6);
    assert_eq!(&back, b"halves");
}

/// `_llseek` takes the offset **high** half first, writes the position to
/// `*result` and returns 0; a negative offset from the end spans both
/// halves.
#[test]
fn llseek_takes_the_high_half_first_and_answers_through_the_pointer() {
    let proc = a_process(48_951);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let fd = open(&sys, b"/llseek\0");
            let data = b"0123456789";
            sys.sys_pwrite(
                FileDesc::from(fd as i32),
                (data.as_ptr() as usize).into(),
                10,
                0,
            )
            .await
            .unwrap();
            let mut at = u64::MAX;
            let r = &mut at as *mut u64 as usize;
            let set = sys
                .compat32_syscall(I386_LLSEEK, [fd, 0, 3, r, SEEK_SET, 0])
                .await;
            let after_set = at;
            // -2, as a 32-bit program passes it: 0xffffffff, 0xfffffffe.
            let end = sys
                .compat32_syscall(I386_LLSEEK, [fd, 0xffff_ffff, 0xffff_fffe, r, SEEK_END, 0])
                .await;
            let after_end = at;
            // Before the start of the file is EINVAL, and *result is left.
            at = 77;
            let before = sys
                .compat32_syscall(I386_LLSEEK, [fd, 0xffff_ffff, 0xffff_fff0, r, SEEK_SET, 0])
                .await;
            (set, after_set, end, after_end, before, at)
        })
    });
    assert_eq!(got, (0, 3, 0, 8, EINVAL, 77));
}

/// `ftruncate64` and `truncate64` take the length low half first.
#[test]
fn the_truncates_put_the_length_together_low_first() {
    let proc = a_process(48_952);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let fd = open(&sys, b"/trunc\0");
            let f = sys
                .compat32_syscall(I386_FTRUNCATE64, [fd, 300, 0, 0, 0, 0])
                .await;
            let after_f = size_of_fd(&sys, fd);
            let path = b"/trunc\0";
            let t = sys
                .compat32_syscall(
                    I386_TRUNCATE64,
                    [path.as_ptr() as usize, 0xffff_ffff_0000_0007, 0, 0, 0, 0],
                )
                .await;
            let after_t = size_of_fd(&sys, fd);
            (f, after_f, t, after_t)
        })
    });
    assert_eq!(got, (0, 300, 0, 7));
}

/// `preadv` and `pwritev` read a list of 8-byte `struct iovec`s, and their
/// offset is low half first like `pread64`'s.
#[test]
fn preadv_and_pwritev_read_the_narrow_list() {
    let proc = a_process(48_953);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let fd = open(&sys, b"/iov\0");
            // Two buffers in the low page, and the list after them.
            put(LOW, b"abc");
            put(LOW + 0x10, b"defgh");
            let list = LOW + 0x100;
            let mut iov = Vec::new();
            for (base, len) in [(LOW as u32, 3u32), ((LOW + 0x10) as u32, 5)] {
                iov.extend_from_slice(&base.to_ne_bytes());
                iov.extend_from_slice(&len.to_ne_bytes());
            }
            put(list, &iov);
            let wrote = sys
                .compat32_syscall(I386_PWRITEV, [fd, list, 2, 2, 0, 0])
                .await;
            let mut whole = [0u8; 10];
            sys.sys_pread(
                FileDesc::from(fd as i32),
                (whole.as_mut_ptr() as usize).into(),
                10,
                0,
            )
            .await
            .unwrap();
            // And back the other way, into the same two buffers: 3 bytes
            // and then 5, from offset 3.
            put(LOW, b"...");
            put(LOW + 0x10, b".....");
            let read = sys
                .compat32_syscall(I386_PREADV, [fd, list, 2, 3, 0, 0])
                .await;
            (wrote, whole, read, get(LOW, 3), get(LOW + 0x10, 5))
        })
    });
    let (wrote, whole, read, first, second) = got;
    assert_eq!(wrote, 8);
    assert_eq!(&whole, b"\0\0abcdefgh");
    // From offset 3: "bcd" then "efgh" (one short: the file ends).
    assert_eq!(read, 7);
    assert_eq!(first, b"bcd");
    assert_eq!(second, b"efgh.");
}

fn put(at: usize, bytes: &[u8]) {
    let mut p: UserOutPtr<u8> = at.into();
    p.write_array(bytes).unwrap();
}

fn get(at: usize, len: usize) -> Vec<u8> {
    let p: UserInPtr<u8> = at.into();
    p.read_array(len).unwrap()
}

fn words(b: &[u8; 8]) -> (u32, u32) {
    (
        u32::from_ne_bytes([b[0], b[1], b[2], b[3]]),
        u32::from_ne_bytes([b[4], b[5], b[6], b[7]]),
    )
}

/// A limit set through the narrow `setrlimit` reads back through both
/// narrow getters, `0xffffffff` going in is infinity, and infinity comes
/// out of `ugetrlimit` as `0xffffffff` and of the old `getrlimit` as
/// `0x7fffffff`, as measured on Linux.
#[test]
fn a_narrow_limit_widens_infinity_and_each_getter_clamps_at_its_own() {
    let proc = a_process(48_954);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let mut new = [0u8; 8];
            new[..4].copy_from_slice(&1000u32.to_ne_bytes());
            new[4..].copy_from_slice(&u32::MAX.to_ne_bytes());
            let set = sys
                .compat32_syscall(
                    I386_SETRLIMIT,
                    [RLIMIT_CORE, new.as_ptr() as usize, 0, 0, 0, 0],
                )
                .await;
            let wide = sys.do_prlimit(0, RLIMIT_CORE, None).unwrap();
            let mut u = [0u8; 8];
            let ug = sys
                .compat32_syscall(
                    I386_UGETRLIMIT,
                    [RLIMIT_CORE, u.as_mut_ptr() as usize, 0, 0, 0, 0],
                )
                .await;
            let mut o = [0u8; 8];
            let og = sys
                .compat32_syscall(
                    I386_OLD_GETRLIMIT,
                    [RLIMIT_CORE, o.as_mut_ptr() as usize, 0, 0, 0, 0],
                )
                .await;
            // Soft above hard is EINVAL, as natively.
            let mut bad = [0u8; 8];
            bad[..4].copy_from_slice(&1000u32.to_ne_bytes());
            bad[4..].copy_from_slice(&500u32.to_ne_bytes());
            let above = sys
                .compat32_syscall(
                    I386_SETRLIMIT,
                    [RLIMIT_CORE, bad.as_ptr() as usize, 0, 0, 0, 0],
                )
                .await;
            // A resource that is not one is EINVAL and leaves the buffer.
            let mut left = [0xaau8; 8];
            let unknown = sys
                .compat32_syscall(
                    I386_UGETRLIMIT,
                    [99, left.as_mut_ptr() as usize, 0, 0, 0, 0],
                )
                .await;
            let null_get = sys
                .compat32_syscall(I386_UGETRLIMIT, [RLIMIT_CORE, 0, 0, 0, 0, 0])
                .await;
            let null_set = sys
                .compat32_syscall(I386_SETRLIMIT, [RLIMIT_CORE, 0, 0, 0, 0, 0])
                .await;
            (
                set,
                (wide.cur, wide.max),
                ug,
                words(&u),
                og,
                words(&o),
                above,
                unknown,
                left,
                null_get,
                null_set,
            )
        })
    });
    let (set, wide, ug, u, og, o, above, unknown, left, null_get, null_set) = got;
    assert_eq!(set, 0);
    assert_eq!(wide, (1000, u64::MAX), "0xffffffff is RLIM64_INFINITY");
    assert_eq!((ug, u), (0, (1000, 0xffff_ffff)));
    assert_eq!((og, o), (0, (1000, 0x7fff_ffff)));
    assert_eq!(above, EINVAL);
    assert_eq!((unknown, left), (EINVAL, [0xaa; 8]));
    assert_eq!((null_get, null_set), (EFAULT, EFAULT));
}
