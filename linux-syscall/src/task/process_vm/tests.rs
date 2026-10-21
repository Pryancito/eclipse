//! `process_vm_readv` and `process_vm_writev` made by number, between a
//! caller on a thread and a target process with memory mapped at [`BASE`].
//! Fixed pids live in 47_6xx here.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::ProcessExt;
use linux_object::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, Thread, ROOT_JOB};
use zircon_object::vm::{MMUFlags, VmObject};

const BASE: usize = 0x30_0000;
const WORDS: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
const EINVAL: isize = -(LxError::EINVAL as isize);
const EFAULT: isize = -(LxError::EFAULT as isize);
const ESRCH: isize = -(LxError::ESRCH as isize);
const EPERM: isize = -(LxError::EPERM as isize);

/// A process of user `uid`, with one page at [`BASE`] holding [`WORDS`]
/// and a hole after it. A uid that is not root's leaves it dumpable all
/// the same, as `prctl(PR_SET_DUMPABLE, 1)` would.
fn a_process(pid: KoID, uid: u32) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    if uid != 0 {
        lp.set_resgid(uid, uid, uid).unwrap();
        lp.set_resuid(uid, uid, uid).unwrap();
        lp.set_dumpable(1);
    }
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "vm", lp).unwrap();
    Thread::create_linux(&proc).unwrap();
    let flags = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
    let vmo = VmObject::new_paged(1);
    vmo.write(0, WORDS).unwrap();
    assert_eq!(
        proc.vmar().map(Some(BASE), vmo, 0, PAGE_SIZE, flags),
        Ok(BASE)
    );
    proc
}

/// Make syscall `num` the way userspace makes it, through the table, as
/// `dispatch_tests` does.
async fn by_number(sys: &Syscall<'_>, num: u32, args: [usize; 6]) -> isize {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    own.syscall(num, args).await
}

/// An iovec as userspace lays one out.
#[repr(C)]
#[derive(Clone, Copy)]
struct Iov(usize, usize);

fn iov(buf: &mut [u8]) -> Iov {
    Iov(buf.as_mut_ptr() as usize, buf.len())
}

async fn readv(sys: &Syscall<'_>, pid: KoID, local: &[Iov], remote: &[Iov], flags: usize) -> isize {
    let args = [
        pid as usize,
        local.as_ptr() as usize,
        local.len(),
        remote.as_ptr() as usize,
        remote.len(),
        flags,
    ];
    by_number(sys, Sys::PROCESS_VM_READV as u32, args).await
}

async fn writev(sys: &Syscall<'_>, pid: KoID, local: &[Iov], remote: &[Iov]) -> isize {
    let args = [
        pid as usize,
        local.as_ptr() as usize,
        local.len(),
        remote.as_ptr() as usize,
        remote.len(),
        0,
    ];
    by_number(sys, Sys::PROCESS_VM_WRITEV as u32, args).await
}

/// What the target reads at `addr`, `len` bytes of it.
fn memory_of(target: &Process, addr: usize, len: usize) -> Vec<u8> {
    let mut buf = alloc::vec![0u8; len];
    assert_eq!(target.vmar().read_memory(addr, &mut buf), Ok(len));
    buf
}

/// The caller's buffers and the target's stretches need not line up: the
/// bytes go from one list to the other in order.
#[test]
fn readv_scatters_another_process_s_memory_into_the_caller_s() {
    let caller = a_process(47_601, 0);
    let target = a_process(47_602, 0);
    let pid = target.id();
    let got = on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let (mut a, mut b, mut c) = ([0u8; 5], [0u8; 7], [0u8; 10]);
            let local = [iov(&mut a), iov(&mut b), iov(&mut c)];
            let remote = [Iov(BASE, 12), Iov(BASE + 14, 10)];
            let n = readv(sys, pid, &local, &remote, 0).await;
            (n, [&a[..], &b[..], &c[..]].concat())
        })
    });
    assert_eq!(got, (22, b"abcdefghijklopqrstuvwx".to_vec()));
    target.exit(0);
    caller.exit(0);
}

/// `writev` puts the caller's bytes into the target, and nowhere else.
#[test]
fn writev_gathers_the_caller_s_buffers_into_another_process() {
    let caller = a_process(47_603, 0);
    let target = a_process(47_604, 0);
    let pid = target.id();
    let n = on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let (mut a, mut b) = (*b"ONE", *b"TWO!");
            let local = [iov(&mut a), iov(&mut b)];
            let remote = [Iov(BASE + 1, 2), Iov(BASE + 10, 5)];
            writev(sys, pid, &local, &remote).await
        })
    });
    assert_eq!(n, 7);
    assert_eq!(memory_of(&target, BASE, 16), b"aONdefghijETWO!p");
    assert_eq!(memory_of(&caller, BASE, 16), &WORDS[..16]);
    target.exit(0);
    caller.exit(0);
}

/// The copy goes as far as both sides have room and memory: a short local
/// buffer takes what fits, and a stretch that runs into a hole stops there,
/// whatever comes after it. A copy that could not start is `EFAULT`.
#[test]
fn a_copy_goes_as_far_as_it_can_and_says_how_far() {
    let caller = a_process(47_605, 0);
    let target = a_process(47_606, 0);
    let pid = target.id();
    let got = on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let mut small = [0u8; 4];
            let short = readv(sys, pid, &[iov(&mut small)], &[Iov(BASE, 10)], 0).await;
            let mut buf = [0u8; 20];
            let edge = [Iov(BASE + PAGE_SIZE - 6, 20)];
            let to_the_hole = readv(sys, pid, &[iov(&mut buf)], &edge, 0).await;
            let past = [Iov(BASE, 3), Iov(BASE + PAGE_SIZE, 3)];
            let good_then_bad = readv(sys, pid, &[iov(&mut buf)], &past, 0).await;
            let across = [Iov(BASE, 2), Iov(BASE + PAGE_SIZE, 3), Iov(BASE + 3, 3)];
            let good_hole_good = readv(sys, pid, &[iov(&mut buf)], &across, 0).await;
            let after = [Iov(BASE + PAGE_SIZE, 3), Iov(BASE, 3)];
            let hole_then_good = readv(sys, pid, &[iov(&mut buf)], &after, 0).await;
            let hole = [Iov(BASE + PAGE_SIZE, 3)];
            let in_the_hole = readv(sys, pid, &[iov(&mut buf)], &hole, 0).await;
            let into_the_hole = writev(sys, pid, &[iov(&mut buf)], &hole).await;
            (
                small,
                [
                    short,
                    to_the_hole,
                    good_then_bad,
                    good_hole_good,
                    hole_then_good,
                    in_the_hole,
                    into_the_hole,
                ],
            )
        })
    });
    assert_eq!(got, (*b"abcd", [4, 6, 3, 2, EFAULT, EFAULT, EFAULT]));
    target.exit(0);
    caller.exit(0);
}

/// The questions in Linux's order: the flags, the caller's iovecs, nothing
/// to copy (which is 0 whoever `pid` is), the target's iovecs, the process.
#[test]
fn the_errors_come_in_linux_s_order() {
    let caller = a_process(47_607, 0);
    let got = on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let mut buf = [0u8; 4];
            let one = [iov(&mut buf)];
            let remote = [Iov(BASE, 4)];
            let nobody: KoID = 47_699;
            let too_many = alloc::vec![Iov(BASE, 0); 1025];
            let empty = alloc::vec![Iov(0, 0); 1024];
            // A null array of `count` iovecs: too many is `EINVAL` before
            // the array is read, none is no array to read.
            let null_array = |count: usize| {
                let args = [nobody as usize, 0, count, remote.as_ptr() as usize, 1, 0];
                by_number(sys, Sys::PROCESS_VM_READV as u32, args)
            };
            [
                readv(sys, nobody, &one, &remote, 1).await,
                readv(sys, nobody, &too_many, &remote, 0).await,
                null_array(1025).await,
                null_array(1).await,
                readv(sys, nobody, &empty, &remote, 0).await,
                null_array(0).await,
                readv(sys, nobody, &one, &too_many, 0).await,
                readv(sys, nobody, &one, &empty, 0).await,
                readv(sys, nobody, &one, &remote, 0).await,
            ]
        })
    });
    let order = [EINVAL, EINVAL, EINVAL, EFAULT, 0, 0, EINVAL, 0, ESRCH];
    assert_eq!(got, order);
    caller.exit(0);
}

/// A zombie has no memory to copy, and a thread's id names its process.
#[test]
fn a_zombie_is_esrch_and_a_thread_names_its_process() {
    let caller = a_process(47_608, 0);
    let target = a_process(47_609, 0);
    let second = Thread::create_linux(&target).unwrap();
    let tid = second.id();
    // The caller's own child, dead and not yet waited for: still there to
    // be found, with nothing left in it.
    let zombie = Process::fork_from(&caller).unwrap();
    zombie.exit(0);
    let zpid = zombie.id();
    let got = on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let mut buf = [0u8; 3];
            let remote = [Iov(BASE, 3)];
            let by_tid = readv(sys, tid, &[iov(&mut buf)], &remote, 0).await;
            let of_zombie = readv(sys, zpid, &[iov(&mut buf)], &remote, 0).await;
            (by_tid, buf, of_zombie)
        })
    });
    assert_eq!(got, (3, *b"abc", ESRCH));
    assert!(find_task(zpid).is_some(), "the zombie was reaped");
    target.exit(0);
    caller.exit(0);
}

/// `PTRACE_MODE_ATTACH_REALCREDS`: another user's process is `EPERM`, the
/// same user's is not unless it made itself not dumpable, and root reaches
/// anything.
#[test]
fn reaching_into_a_process_is_the_ptrace_attach_check() {
    let caller = a_process(47_611, 1000);
    let other = a_process(47_612, 1001);
    let same = a_process(47_613, 1000);
    let hardened = a_process(47_614, 1000);
    hardened.linux().set_dumpable(0);
    let root_caller = a_process(47_615, 0);
    let pids = [other.id(), same.id(), hardened.id()];
    let as_user = on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let mut buf = [0u8; 3];
            let remote = [Iov(BASE, 3)];
            let mut got = Vec::new();
            for pid in pids {
                got.push(readv(sys, pid, &[iov(&mut buf)], &remote, 0).await);
                got.push(writev(sys, pid, &[iov(&mut buf)], &remote).await);
            }
            got
        })
    });
    assert_eq!(as_user, [EPERM, EPERM, 3, 3, EPERM, EPERM]);
    let as_root = on_a_thread(&root_caller, move |sys| {
        Box::pin(async move {
            let mut buf = [0u8; 3];
            let remote = [Iov(BASE, 3)];
            readv(sys, pids[2], &[iov(&mut buf)], &remote, 0).await
        })
    });
    assert_eq!(as_root, 3);
    // Nor does a process that is not dumpable keep itself out.
    let own = hardened.id();
    let itself = on_a_thread(&hardened, move |sys| {
        Box::pin(async move {
            let mut buf = [0u8; 3];
            let remote = [Iov(BASE, 3)];
            readv(sys, own, &[iov(&mut buf)], &remote, 0).await
        })
    });
    assert_eq!(itself, 3);
    for p in [caller, other, same, hardened, root_caller] {
        p.exit(0);
    }
}

/// More than a page in one go, both ways, and a buffer the caller does not
/// have: the copy is made a page at a time, and a bad buffer of the
/// caller's is `EFAULT` as a bad page of the target's is.
#[test]
fn a_copy_of_several_pages_and_a_bad_buffer_of_the_caller_s() {
    let caller = a_process(47_616, 0);
    let target = a_process(47_617, 0);
    let flags = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
    let at = BASE + 0x10_0000;
    let vmo = VmObject::new_paged(3);
    let mut bytes = alloc::vec![0u8; 3 * PAGE_SIZE];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    vmo.write(0, &bytes).unwrap();
    assert_eq!(
        target.vmar().map(Some(at), vmo, 0, 3 * PAGE_SIZE, flags),
        Ok(at)
    );
    let pid = target.id();
    let len = 2 * PAGE_SIZE + 100;
    let got = on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let mut buf = alloc::vec![0u8; len];
            let remote = [Iov(at + 50, len)];
            let read = readv(sys, pid, &[iov(&mut buf)], &remote, 0).await;
            let mut back = alloc::vec![7u8; len];
            let written = writev(sys, pid, &[iov(&mut back)], &remote).await;
            let nowhere = [Iov(0, 3)];
            let into_nowhere = readv(sys, pid, &nowhere, &[Iov(at, 3)], 0).await;
            let from_nowhere = writev(sys, pid, &nowhere, &[Iov(at, 3)]).await;
            (read, buf, written, into_nowhere, from_nowhere)
        })
    });
    assert_eq!(got.0, len as isize);
    assert!(got.1 == bytes[50..50 + len], "readv read other bytes");
    assert_eq!((got.2, got.3, got.4), (len as isize, EFAULT, EFAULT));
    assert!(memory_of(&target, at + 50, len).iter().all(|&b| b == 7));
    assert_eq!(memory_of(&target, at, 50), &bytes[..50]);
    target.exit(0);
    caller.exit(0);
}
