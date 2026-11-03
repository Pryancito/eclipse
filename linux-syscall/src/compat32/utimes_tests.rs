//! The `times` of `utimensat(2)` and `utimes(2)` from a 32-bit process, by
//! i386's own numbers (320 and 271).
//!
//! The pair is the thing: a kernel that read the native stride does not fail,
//! it takes the modification time out of the sixteen bytes AFTER the caller's
//! buffer. So each test writes the sixteen bytes a 32-bit caller owns,
//! poisons everything after them, and then asks the FILESYSTEM what time the
//! file ended up with. A test that only looked at the return value would pass
//! on a kernel that set the file to a poisoned time and said 0.
//!
//! Fixed pids live in 48_48x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::{Abi, LinuxProcess};
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::{Process, ROOT_JOB};
use zircon_object::vm::{MMUFlags, VmObject};

/// i386's numbers for the two calls whose `times` is an array.
const I386_UTIMES: u32 = 271;
const I386_UTIMENSAT: u32 = 320;
/// And the two the table must still refuse, each for its own reason: `utime`
/// (30) passes a `struct utimbuf` and `futimesat` (299) has no native handler
/// in this kernel to be pointed at.
const I386_UTIME: u32 = 30;
const I386_FUTIMESAT: u32 = 299;

const AT_FDCWD: usize = -100isize as usize;
const ENOSYS: isize = -(LxError::ENOSYS as isize);

const USER_AT: usize = 0x2300_0000;
const USER_PAGES: usize = 2;
const PAGE_SIZE: usize = 0x1000;
/// Where the path goes.
const PATH_AT: usize = USER_AT;
/// And where the pair goes: `USER_AT + 0x404`, so that it is 4-aligned and
/// NOT 8-aligned --- which is what a 32-bit program's `struct timespec *`
/// actually is, every field of these structures being a `long` on i386.
const TIMES_AT: usize = USER_AT + 0x404;

/// A second in 1970 no clock here produces by accident, so an assertion on it
/// cannot pass because something happened to be "now".
const AN_OLD_SECOND: u32 = 1_000_000;
/// Another, to tell the access time from the modification time.
const A_DIFFERENT_OLD_SECOND: u32 = 2_000_000;

/// A 32-bit (or native) process with a page of its own and a `/f` whose times
/// are both zero.
fn a_process(pid: KoID, abi: Abi) -> alloc::sync::Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(abi);
    let inode = lp.root_inode().create("f", FileType::File, 0o644).unwrap();
    let mut metadata = inode.metadata().unwrap();
    metadata.atime = rcore_fs::vfs::Timespec { sec: 0, nsec: 0 };
    metadata.mtime = rcore_fs::vfs::Timespec { sec: 0, nsec: 0 };
    inode.set_metadata(&metadata).unwrap();
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "utimes32", lp).unwrap();
    proc.vmar()
        .map_at(
            USER_AT,
            VmObject::new_paged(USER_PAGES),
            0,
            USER_PAGES * PAGE_SIZE,
            MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER,
        )
        .expect("pages of the process's own, at an address a u32 can hold");
    proc
}

fn put(at: usize, bytes: &[u8]) {
    let mut p: kernel_hal::user::UserOutPtr<u8> = at.into();
    p.write_array(bytes).expect("mapped above");
}

/// The four narrow words of a pair, and `0xaa` for the sixteen bytes after
/// them: the bytes a native-stride read would take the second entry from.
fn put_narrow_pair(at: usize, words: [u32; 4]) {
    let mut bytes = alloc::vec::Vec::new();
    for w in words {
        bytes.extend_from_slice(&w.to_ne_bytes());
    }
    bytes.extend_from_slice(&[0xAAu8; 16]);
    put(at, &bytes);
}

/// What the file really has, asked of the filesystem and not of the syscall.
fn times_of(proc: &alloc::sync::Arc<Process>) -> (i64, i64) {
    let lp = proc.linux();
    let metadata = lp.lookup_inode("/f").unwrap().metadata().unwrap();
    (metadata.atime.sec, metadata.mtime.sec)
}

/// A `Syscall` as a `&mut` one, since `compat32_syscall` takes `&mut self`.
fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// `utimensat(AT_FDCWD, "/f", times, 0)` from a 32-bit process: the
/// modification time comes from offset **8** of the pair. Read at the native
/// stride it would come from offset 16, which here is poison --- so this
/// fails as a wrong time on the file rather than as an error code.
#[test]
fn the_utimensat_pair_of_a_32_bit_process_is_sixteen_bytes() {
    let proc = a_process(48_480, Abi::Linux32);
    let ret = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(PATH_AT, b"/f\0");
            put_narrow_pair(TIMES_AT, [AN_OLD_SECOND, 0, A_DIFFERENT_OLD_SECOND, 0]);
            sys.compat32_syscall(I386_UTIMENSAT, [AT_FDCWD, PATH_AT, TIMES_AT, 0, 0, 0])
                .await
        })
    });
    assert_eq!(ret, 0, "utimensat (320) from a 32-bit process");
    assert_eq!(
        times_of(&proc),
        (AN_OLD_SECOND as i64, A_DIFFERENT_OLD_SECOND as i64),
        "the modification time came from offset 8 of the pair, not from 16"
    );
    proc.exit(0);
}

/// The same thing at an **8-aligned** address, which is the test that
/// measures the stride and nothing else.
///
/// At [`TIMES_AT`] a native-stride read fails for TWO reasons: the stride and
/// the alignment. Either one alone would turn the test green, and the lesson
/// of the batch-17 tests is that a check with two reasons measures neither.
/// Here the alignment is the one a native caller would use, so the only thing
/// left to be wrong is where the second entry starts.
#[test]
fn the_narrow_stride_is_wrong_on_its_own_even_at_a_native_alignment() {
    let proc = a_process(48_485, Abi::Linux32);
    const AT: usize = USER_AT + 0x400;
    assert!(AT.is_multiple_of(8), "a native caller's alignment");
    let ret = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(PATH_AT, b"/f\0");
            put_narrow_pair(AT, [AN_OLD_SECOND, 0, A_DIFFERENT_OLD_SECOND, 0]);
            sys.compat32_syscall(I386_UTIMENSAT, [AT_FDCWD, PATH_AT, AT, 0, 0, 0])
                .await
        })
    });
    assert_eq!(ret, 0, "utimensat (320) at an 8-aligned pair");
    assert_eq!(
        times_of(&proc),
        (AN_OLD_SECOND as i64, A_DIFFERENT_OLD_SECOND as i64),
        "the modification time came from offset 8 and not from the poison at 16"
    );
    proc.exit(0);
}

/// The same call from a NATIVE process, which is the other half of the claim:
/// 32 bytes there, and the handler is the same handler. Without this an
/// assertion about the narrow stride alone would pass on a kernel that
/// narrowed everything.
#[test]
fn the_utimensat_pair_of_a_native_process_is_thirty_two_bytes() {
    let proc = a_process(48_481, Abi::Linux);
    let ret = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(PATH_AT, b"/f\0");
            // The native pair, at an 8-aligned address as a native caller's
            // is: seconds at 0 and at 16.
            let mut bytes = alloc::vec::Vec::new();
            bytes.extend_from_slice(&(AN_OLD_SECOND as u64).to_ne_bytes());
            bytes.extend_from_slice(&0u64.to_ne_bytes());
            bytes.extend_from_slice(&(A_DIFFERENT_OLD_SECOND as u64).to_ne_bytes());
            bytes.extend_from_slice(&0u64.to_ne_bytes());
            put(TIMES_AT + 4, &bytes);
            sys.syscall(
                crate::Sys::UTIMENSAT as u32,
                [AT_FDCWD, PATH_AT, TIMES_AT + 4, 0, 0, 0],
            )
            .await
        })
    });
    assert_eq!(ret, 0, "utimensat from a native process");
    assert_eq!(
        times_of(&proc),
        (AN_OLD_SECOND as i64, A_DIFFERENT_OLD_SECOND as i64),
        "the native stride is still sixteen bytes to the entry"
    );
    proc.exit(0);
}

/// `utimes(2)` carries `timeval`s, whose sub-second field is MICROseconds, so
/// its pair is the same shape and its second word means something else. Both
/// halves matter here: the stride, and the thousand.
#[test]
fn the_utimes_pair_of_a_32_bit_process_is_sixteen_bytes_of_timevals() {
    let proc = a_process(48_482, Abi::Linux32);
    let ret = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(PATH_AT, b"/f\0");
            // 999_999 microseconds is the largest `utimes` accepts, and a
            // value a `timespec` would refuse to read as nanoseconds.
            put_narrow_pair(
                TIMES_AT,
                [AN_OLD_SECOND, 999_999, A_DIFFERENT_OLD_SECOND, 1],
            );
            sys.compat32_syscall(I386_UTIMES, [PATH_AT, TIMES_AT, 0, 0, 0, 0])
                .await
        })
    });
    assert_eq!(ret, 0, "utimes (271) from a 32-bit process");
    assert_eq!(
        times_of(&proc),
        (AN_OLD_SECOND as i64, A_DIFFERENT_OLD_SECOND as i64),
        "the modification time came from offset 8 of the pair"
    );
    let lp = proc.linux();
    let metadata = lp.lookup_inode("/f").unwrap().metadata().unwrap();
    assert_eq!(
        (metadata.atime.nsec, metadata.mtime.nsec),
        (999_999_000, 1_000),
        "microseconds times a thousand, and from the narrow offsets"
    );
    proc.exit(0);
}

/// A null `times` is "now" in both calls, and a 32-bit caller sends it as a
/// null pointer like anybody else: the file stops being at the epoch.
#[test]
fn a_null_pair_from_a_32_bit_process_is_still_a_touch() {
    let proc = a_process(48_483, Abi::Linux32);
    let ret = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(PATH_AT, b"/f\0");
            sys.compat32_syscall(I386_UTIMENSAT, [AT_FDCWD, PATH_AT, 0, 0, 0, 0])
                .await
        })
    });
    assert_eq!(ret, 0, "utimensat with a null times");
    let (atime, mtime) = times_of(&proc);
    assert!(atime > 0 && mtime > 0, "a touch, not the epoch");
    proc.exit(0);
}

/// What is still refused, said by name rather than guessed. `utime` (30)
/// passes a `struct utimbuf`, which is a pair of words and not an array of
/// these structures; `futimesat` (299) would need the native 261, which this
/// kernel has a number for and no handler. Wiring either must take it out of
/// here.
#[test]
fn the_two_the_table_still_refuses_say_so_by_name() {
    let proc = a_process(48_484, Abi::Linux32);
    let (utime, futimesat) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(PATH_AT, b"/f\0");
            let a = sys
                .compat32_syscall(I386_UTIME, [PATH_AT, TIMES_AT, 0, 0, 0, 0])
                .await;
            let b = sys
                .compat32_syscall(I386_FUTIMESAT, [AT_FDCWD, PATH_AT, TIMES_AT, 0, 0, 0])
                .await;
            (a, b)
        })
    });
    assert_eq!(utime, ENOSYS, "utime (30): a struct utimbuf, not an array");
    assert_eq!(futimesat, ENOSYS, "futimesat (299): no native handler");
    assert_eq!(times_of(&proc), (0, 0), "and neither touched the file");
    proc.exit(0);
}
