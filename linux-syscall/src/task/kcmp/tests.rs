//! `kcmp` made by number, between a caller on a thread and the tasks a
//! `fork`, a `vfork`, a thread and an exit leave. Fixed pids live in 47_7xx
//! here.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::fs::EpollEvent;
use linux_object::process::ProcessExt;
use linux_object::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

const EBADF: isize = -(LxError::EBADF as isize);
const EFAULT: isize = -(LxError::EFAULT as isize);
const EINVAL: isize = -(LxError::EINVAL as isize);
const ENOENT: isize = -(LxError::ENOENT as isize);
const EPERM: isize = -(LxError::EPERM as isize);
const ESRCH: isize = -(LxError::ESRCH as isize);
const BEST_EFFORT_4: usize = (2 << 13) | 4;

/// A process of user `uid`, with one thread so it lives on between calls.
/// A uid that is not root's leaves it dumpable all the same, as
/// `prctl(PR_SET_DUMPABLE, 1)` would.
fn a_process(pid: KoID, uid: u32) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    if uid != 0 {
        lp.set_resgid(uid, uid, uid).unwrap();
        lp.set_resuid(uid, uid, uid).unwrap();
        lp.set_dumpable(1);
    }
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "kcmp", lp).unwrap();
    Thread::create_linux(&proc).unwrap();
    proc
}

/// A child of `parent` as `fork` makes one: the process, and a first thread
/// that takes from `parent`'s first what a forked thread takes.
fn a_child(parent: &Arc<Process>) -> Arc<Process> {
    let child = Process::fork_from(parent).unwrap();
    let thread = Thread::create_linux(&child).unwrap();
    *thread.lock_linux() = parent.threads()[0].lock_linux().forked_child();
    child
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

async fn kcmp(sys: &Syscall<'_>, a: KoID, b: KoID, kind: u32, idx1: usize, idx2: usize) -> isize {
    let args = [a as usize, b as usize, kind as usize, idx1, idx2, 0];
    by_number(sys, Sys::KCMP as u32, args).await
}

/// `kcmp` of every type from `KCMP_FILE` to `KCMP_SYSVSEM`, fd `fd` on both
/// sides for the first.
async fn every_type(sys: &Syscall<'_>, a: KoID, b: KoID, fd: usize) -> Vec<isize> {
    let mut got = Vec::new();
    for kind in KCMP_FILE..=KCMP_SYSVSEM {
        got.push(kcmp(sys, a, b, kind, fd, fd).await);
    }
    got
}

async fn pipe(sys: &Syscall<'_>) -> (usize, usize) {
    let mut fds = [0i32; 2];
    let args = [fds.as_mut_ptr() as usize, 0, 0, 0, 0, 0];
    assert_eq!(by_number(sys, Sys::PIPE2 as u32, args).await, 0);
    (fds[0] as usize, fds[1] as usize)
}

async fn call(sys: &Syscall<'_>, num: Sys, a: usize, b: usize, c: usize) -> isize {
    by_number(sys, num as u32, [a, b, c, 0, 0, 0]).await
}

/// 1 one way and 2 the other: two resources, in one order.
fn ordered(ab: isize, ba: isize) -> bool {
    matches!((ab, ba), (1, 2) | (2, 1))
}

/// An fd, its `dup` and its `F_DUPFD_CLOEXEC` are one open file; the two
/// ends of a pipe are two. The index is an `unsigned int`, so what is above
/// its 32 bits is not looked at, and a bad index on either side is `EBADF`.
#[test]
fn kcmp_file_is_the_open_file_and_not_the_number() {
    let caller = a_process(47_701, 0);
    let pid = caller.id();
    let got = on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let (r, w) = pipe(sys).await;
            let d = call(sys, Sys::DUP, r, 0, 0).await as usize;
            let c = call(sys, Sys::FCNTL, r, 1030, 40).await as usize;
            [
                kcmp(sys, pid, pid, KCMP_FILE, r, r).await,
                kcmp(sys, pid, pid, KCMP_FILE, r, d).await,
                kcmp(sys, pid, pid, KCMP_FILE, c, d).await,
                kcmp(sys, pid, pid, KCMP_FILE, (1 << 40) | r, d).await,
                kcmp(sys, pid, pid, KCMP_FILE, r, w).await,
                kcmp(sys, pid, pid, KCMP_FILE, w, r).await,
                kcmp(sys, pid, pid, KCMP_FILE, 999, r).await,
                kcmp(sys, pid, pid, KCMP_FILE, r, 999).await,
                kcmp(sys, pid, pid, KCMP_FILE, r, 0xffff_ffff).await,
            ]
        })
    });
    assert_eq!(got[..4], [0, 0, 0, 0]);
    assert!(ordered(got[4], got[5]), "the two ends of a pipe: {:?}", got);
    assert_eq!(got[6..], [EBADF, EBADF, EBADF]);
    caller.exit(0);
}

/// The answers order the files: one way round is the other way's mirror,
/// no three of them go round in a circle, and asking again answers the same.
#[test]
fn the_order_is_a_total_order_and_it_holds() {
    let caller = a_process(47_702, 0);
    let pid = caller.id();
    let (first, again) = on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let mut fds = Vec::new();
            for _ in 0..3 {
                let (r, w) = pipe(sys).await;
                fds.extend([r, w]);
            }
            let mut rounds = Vec::new();
            for _ in 0..2 {
                let mut table = Vec::new();
                for &a in &fds {
                    for &b in &fds {
                        table.push(kcmp(sys, pid, pid, KCMP_FILE, a, b).await);
                    }
                }
                rounds.push(table);
            }
            (rounds[0].clone(), rounds[1].clone())
        })
    });
    assert_eq!(first, again);
    let at = |a: usize, b: usize| first[a * 6 + b];
    for a in 0..6 {
        assert_eq!(at(a, a), 0);
        for b in 0..6 {
            if a != b {
                assert!(ordered(at(a, b), at(b, a)), "{} and {}: {:?}", a, b, first);
            }
            for c in 0..6 {
                let circle = at(a, b) == 1 && at(b, c) == 1 && at(a, c) != 1;
                assert!(!circle, "{} < {} < {} but not {} < {}", a, b, c, a, c);
            }
        }
    }
    caller.exit(0);
}

/// Both tasks first (`ESRCH`, whatever the type), then the type (`EINVAL`).
/// A pid and a type are `int`s: what is above their 32 bits is not looked
/// at.
#[test]
fn the_errors_come_in_linux_s_order() {
    let caller = a_process(47_703, 0);
    let pid = caller.id();
    let got = on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let nobody: KoID = 47_799;
            [
                kcmp(sys, nobody, pid, KCMP_VM, 0, 0).await,
                kcmp(sys, pid, nobody, KCMP_VM, 0, 0).await,
                kcmp(sys, nobody, pid, 99, 0, 0).await,
                kcmp(sys, 0, pid, KCMP_VM, 0, 0).await,
                kcmp(sys, u32::MAX as KoID, pid, KCMP_VM, 0, 0).await,
                kcmp(sys, pid, pid, KCMP_TYPES as u32, 0, 0).await,
                kcmp(sys, pid, pid, u32::MAX, 0, 0).await,
                kcmp(sys, pid, pid, 99, 0, 0).await,
                by_number(
                    sys,
                    Sys::KCMP as u32,
                    [pid as usize, pid as usize, 1 << 32 | 1, 0, 0, 0],
                )
                .await,
                by_number(
                    sys,
                    Sys::KCMP as u32,
                    [1 << 32 | pid as usize, pid as usize, 1, 0, 0, 0],
                )
                .await,
            ]
        })
    });
    let order = [
        ESRCH, ESRCH, ESRCH, ESRCH, ESRCH, EINVAL, EINVAL, EINVAL, 0, 0,
    ];
    assert_eq!(got, order);
    caller.exit(0);
}

/// A thread's id names the thread, and a thread shares everything with its
/// process: the files, the address space, the fd table, the filesystem
/// context, the signal handlers, and (having none) the I/O context and the
/// undo list.
#[test]
fn a_thread_shares_everything_with_its_process() {
    let caller = a_process(47_704, 0);
    let pid = caller.id();
    let tid = Thread::create_linux(&caller).unwrap().id();
    let got = on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let (r, _) = pipe(sys).await;
            every_type(sys, pid, tid, r).await
        })
    });
    assert_eq!(got, [0; 7]);
    caller.exit(0);
}

/// A forked child has the parent's open files under the same numbers, and
/// its own address space, fd table, filesystem context and signal handlers.
/// A `vfork` child runs in the parent's address space and shares nothing
/// else.
#[test]
fn a_child_has_its_own_and_a_vfork_child_borrows_the_memory() {
    let caller = a_process(47_705, 0);
    let pid = caller.id();
    let parent = caller.clone();
    let (forked, back, borrowed) = on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let (r, _) = pipe(sys).await;
            let child = a_child(&parent);
            let vforked = Process::vfork_from(&parent).unwrap();
            let forked = every_type(sys, pid, child.id(), r).await;
            let back = every_type(sys, child.id(), pid, r).await;
            let borrowed = every_type(sys, vforked.id(), pid, r).await;
            child.exit(0);
            vforked.exit(0);
            (forked, back, borrowed)
        })
    });
    assert_eq!(forked[KCMP_FILE as usize], 0, "the inherited fd");
    for kind in KCMP_VM..=KCMP_SIGHAND {
        let kind = kind as usize;
        assert!(ordered(forked[kind], back[kind]), "{}: {:?}", kind, forked);
    }
    assert_eq!(forked[KCMP_IO as usize..], [0, 0], "neither has one");
    assert_eq!(borrowed[KCMP_VM as usize], 0, "the vfork child's memory");
    for kind in KCMP_FILES..=KCMP_SIGHAND {
        let kind = kind as usize;
        assert!(matches!(borrowed[kind], 1 | 2), "{}: {:?}", kind, borrowed);
    }
    caller.exit(0);
}

/// A zombie has no files, address space, fd table, filesystem context,
/// I/O context or undo list left, so two zombies share all of those, but it
/// keeps its own signal handlers until it is reaped.
#[test]
fn a_zombie_keeps_only_its_signal_handlers() {
    let caller = a_process(47_706, 0);
    let pid = caller.id();
    let parent = caller.clone();
    let (got, zombie) = on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            // Forked with the pipe open, so each had it under `r` too.
            let (r, _) = pipe(sys).await;
            let (z1, z2) = (a_child(&parent), a_child(&parent));
            z1.exit(0);
            z2.exit(0);
            let (z1, z2) = (z1.id(), z2.id());
            let got = [
                kcmp(sys, z1, pid, KCMP_FILE, r, r).await,
                kcmp(sys, pid, z1, KCMP_FILE, r, r).await,
                kcmp(sys, z1, z2, KCMP_VM, 0, 0).await,
                kcmp(sys, z1, z2, KCMP_FILES, 0, 0).await,
                kcmp(sys, z1, z2, KCMP_FS, 0, 0).await,
                kcmp(sys, z1, pid, KCMP_IO, 0, 0).await,
                kcmp(sys, z1, pid, KCMP_SYSVSEM, 0, 0).await,
                kcmp(sys, z1, z2, KCMP_SIGHAND, 0, 0).await,
                kcmp(sys, z1, pid, KCMP_VM, 0, 0).await,
            ];
            (got, z1)
        })
    });
    assert_eq!(got[..7], [EBADF, EBADF, 0, 0, 0, 0, 0]);
    assert!(matches!(got[7], 1 | 2), "two zombies' handlers: {:?}", got);
    assert!(matches!(got[8], 1 | 2), "a zombie's memory: {:?}", got);
    assert!(find_task(zombie).is_some(), "the zombie was reaped");
    caller.exit(0);
}

/// An I/O context comes with an I/O priority: none before `ioprio_set`, one
/// of its own after, and one of its own for a child forked or a thread made
/// after, which takes the priority with it.
#[test]
fn an_io_context_comes_with_an_io_priority() {
    let caller = a_process(47_707, 0);
    let pid = caller.id();
    let before = a_child(&caller);
    let parent = caller.clone();
    let child = before.id();
    let got = on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let none = kcmp(sys, pid, child, KCMP_IO, 0, 0).await;
            let set = call(sys, Sys::IOPRIO_SET, 1, pid as usize, BEST_EFFORT_4).await;
            let after = a_child(&parent);
            let thread = Thread::create_linux(&parent).unwrap();
            *thread.lock_linux() = parent.threads()[0].lock_linux().new_thread();
            let got = [
                none,
                set,
                kcmp(sys, pid, thread.id(), KCMP_VM, 0, 0).await,
                kcmp(sys, pid, child, KCMP_IO, 0, 0).await,
                kcmp(sys, after.id(), pid, KCMP_IO, 0, 0).await,
                kcmp(sys, child, after.id(), KCMP_IO, 0, 0).await,
                kcmp(sys, pid, thread.id(), KCMP_IO, 0, 0).await,
            ];
            after.exit(0);
            got
        })
    });
    assert_eq!(got[..3], [0, 0, 0]);
    assert!(got[3..].iter().all(|&r| matches!(r, 1 | 2)), "{:?}", got);
    before.exit(0);
    caller.exit(0);
}

/// The undo list is made by the first `semop` with `SEM_UNDO`, even one
/// that fails on its set, and kept whatever becomes of the set, until the
/// process exits. A thread shares it; a child forked after has none, and
/// nor does a process whose `semop`s never asked for one.
#[test]
fn the_undo_list_is_made_by_the_first_sem_undo() {
    let caller = a_process(47_708, 0);
    let other = a_process(47_709, 0);
    let pid = caller.id();
    let tid = Thread::create_linux(&caller).unwrap().id();
    let before = a_child(&caller);
    let parent = caller.clone();
    let child = before.id();
    let got = on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let none = kcmp(sys, pid, child, KCMP_SYSVSEM, 0, 0).await;
            let id = call(sys, Sys::SEMGET, 0, 1, 0o600).await as usize;
            let plain = [0u16, 1, 0];
            let with_undo = [0u16, 1, 0x1000];
            let still_none = [
                call(sys, Sys::SEMOP, id, plain.as_ptr() as usize, 1).await,
                kcmp(sys, pid, child, KCMP_SYSVSEM, 0, 0).await,
            ];
            let made = call(sys, Sys::SEMOP, id, with_undo.as_ptr() as usize, 1).await;
            let after = a_child(&parent);
            // `exit_sem` takes the list away with the rest.
            let dying = a_child(&parent);
            dying.linux().semaphores_make_undo_list();
            let living = kcmp(sys, dying.id(), child, KCMP_SYSVSEM, 0, 0).await;
            dying.exit(0);
            let got = [
                kcmp(sys, pid, child, KCMP_SYSVSEM, 0, 0).await,
                kcmp(sys, pid, after.id(), KCMP_SYSVSEM, 0, 0).await,
                living,
                kcmp(sys, child, after.id(), KCMP_SYSVSEM, 0, 0).await,
                kcmp(sys, pid, tid, KCMP_SYSVSEM, 0, 0).await,
                kcmp(sys, dying.id(), child, KCMP_SYSVSEM, 0, 0).await,
                call(sys, Sys::SEMCTL, id, 0, 0).await,
                kcmp(sys, pid, child, KCMP_SYSVSEM, 0, 0).await,
            ];
            after.exit(0);
            (none, still_none, made, got)
        })
    });
    let (none, still_none, made, got) = got;
    assert_eq!((none, still_none, made), (0, [0, 0], 0));
    assert!(got[..3].iter().all(|&r| matches!(r, 1 | 2)), "{:?}", got);
    assert_eq!(got[3..7], [0, 0, 0, 0], "{:?}", got);
    assert_eq!(got[7], got[0], "after IPC_RMID");
    // A `semop` with `SEM_UNDO` on a set that is not there.
    let opid = other.id();
    let failed = on_a_thread(&other, move |sys| {
        Box::pin(async move {
            let with_undo = [0u16, 1, 0x1000];
            [
                call(sys, Sys::SEMOP, 999_999, with_undo.as_ptr() as usize, 1).await,
                kcmp(sys, opid, child, KCMP_SYSVSEM, 0, 0).await,
            ]
        })
    });
    assert_eq!(failed[0], EINVAL);
    assert!(matches!(failed[1], 1 | 2), "{:?}", failed);
    before.exit(0);
    other.exit(0);
    caller.exit(0);
}

/// `KCMP_EPOLL_TFD` compares a file of the first task with the one an epoll
/// of the second watches under a number, which is the same comparison
/// `KCMP_FILE` makes; and its errors in Linux's order.
#[test]
fn kcmp_epoll_tfd_is_the_file_an_epoll_watches() {
    let caller = a_process(47_710, 0);
    let pid = caller.id();
    let parent = caller.clone();
    let (same, errors, closed) = on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let (r, w) = pipe(sys).await;
            let d = call(sys, Sys::DUP, r, 0, 0).await as usize;
            // Forked with the pipe and before the epoll: `r` is the same
            // file in both, and `ep` is only the caller's.
            let child = a_child(&parent);
            let ep = call(sys, Sys::EPOLL_CREATE1, 0, 0, 0).await as usize;
            let event = EpollEvent { events: 1, data: 0 };
            let event = &event as *const EpollEvent as usize;
            for fd in [r, d] {
                let args = [ep, 1, fd, event, 0, 0];
                assert_eq!(by_number(sys, Sys::EPOLL_CTL as u32, args).await, 0);
            }
            let slot = |efd: usize, tfd: usize, toff: u32| KcmpEpollSlot {
                efd: efd as u32,
                tfd: tfd as u32,
                toff,
            };
            let (at_r, at_d) = (slot(ep, r, 0), slot(ep, d, 0));
            let at = |s: &KcmpEpollSlot| s as *const KcmpEpollSlot as usize;
            let same = [
                kcmp(sys, pid, pid, KCMP_EPOLL_TFD, r, at(&at_r)).await,
                kcmp(sys, pid, pid, KCMP_EPOLL_TFD, d, at(&at_r)).await,
                kcmp(sys, pid, pid, KCMP_EPOLL_TFD, r, at(&at_d)).await,
                kcmp(sys, child.id(), pid, KCMP_EPOLL_TFD, r, at(&at_r)).await,
                kcmp(sys, pid, pid, KCMP_EPOLL_TFD, w, at(&at_r)).await,
                kcmp(sys, pid, pid, KCMP_FILE, w, r).await,
            ];
            child.exit(0);
            let (unwatched, second) = (slot(ep, w, 0), slot(ep, r, 1));
            let (not_epoll, bad) = (slot(w, r, 0), slot(999, r, 0));
            let errors = [
                kcmp(sys, pid, pid, KCMP_EPOLL_TFD, r, at(&unwatched)).await,
                kcmp(sys, pid, pid, KCMP_EPOLL_TFD, r, at(&second)).await,
                kcmp(sys, pid, pid, KCMP_EPOLL_TFD, r, at(&not_epoll)).await,
                kcmp(sys, pid, pid, KCMP_EPOLL_TFD, r, at(&bad)).await,
                kcmp(sys, pid, pid, KCMP_EPOLL_TFD, 999, at(&at_r)).await,
                kcmp(sys, pid, pid, KCMP_EPOLL_TFD, 999, 0).await,
            ];
            // The entry lives as long as its file, under its old number.
            call(sys, Sys::CLOSE, r, 0, 0).await;
            let kept = kcmp(sys, pid, pid, KCMP_EPOLL_TFD, d, at(&at_r)).await;
            call(sys, Sys::CLOSE, d, 0, 0).await;
            let gone = kcmp(sys, pid, pid, KCMP_EPOLL_TFD, w, at(&at_r)).await;
            (same, errors, [kept, gone])
        })
    });
    assert_eq!(same[..4], [0, 0, 0, 0]);
    assert!(matches!(same[4], 1 | 2), "{:?}", same);
    assert_eq!(same[4], same[5], "not KCMP_FILE's order");
    assert_eq!(errors, [ENOENT, ENOENT, EINVAL, EBADF, EBADF, EFAULT]);
    assert_eq!(closed, [0, ENOENT]);
    caller.exit(0);
}

/// `PTRACE_MODE_READ_REALCREDS`, asked of both tasks after both are found
/// and before the type: another user's process is `EPERM`, the same user's
/// is not unless it made itself not dumpable (a zombie too), root reaches
/// anything, and a process always reaches itself.
#[test]
fn looking_at_a_task_is_the_ptrace_read_check() {
    let caller = a_process(47_711, 1000);
    let other = a_process(47_712, 1001);
    let same = a_process(47_713, 1000);
    let hardened = a_process(47_714, 1000);
    hardened.linux().set_dumpable(0);
    let root = a_process(47_715, 0);
    let dead = a_child(&caller);
    dead.linux().set_dumpable(0);
    dead.exit(0);
    let ids = [other.id(), same.id(), hardened.id(), dead.id()];
    let me = caller.id();
    let as_user = on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let [other, same, hardened, dead] = ids;
            [
                kcmp(sys, other, me, KCMP_VM, 0, 0).await,
                kcmp(sys, me, other, KCMP_VM, 0, 0).await,
                kcmp(sys, other, me, 99, 0, 0).await,
                kcmp(sys, 47_799, other, KCMP_VM, 0, 0).await,
                kcmp(sys, same, me, KCMP_VM, 0, 0).await,
                kcmp(sys, hardened, me, KCMP_VM, 0, 0).await,
                kcmp(sys, dead, me, KCMP_VM, 0, 0).await,
            ]
        })
    });
    assert_eq!(as_user[..4], [EPERM, EPERM, EPERM, ESRCH]);
    assert!(matches!(as_user[4], 1 | 2), "{:?}", as_user);
    assert_eq!(as_user[5..], [EPERM, EPERM]);
    let rid = root.id();
    let as_root = on_a_thread(&root, move |sys| {
        Box::pin(async move { kcmp(sys, ids[2], rid, KCMP_VM, 0, 0).await })
    });
    assert!(matches!(as_root, 1 | 2), "{}", as_root);
    let own = hardened.id();
    let itself = on_a_thread(&hardened, move |sys| {
        Box::pin(async move { kcmp(sys, own, own, KCMP_VM, 0, 0).await })
    });
    assert_eq!(itself, 0);
    for p in [caller, other, same, hardened, root] {
        p.exit(0);
    }
}

/// `struct kcmp_epoll_slot` comes from user space through a `UserInPtr`, and
/// its three fields are three `u32` in a row. `efd` and `tfd` are both file
/// descriptors, so swapping them does not fail cleanly: `kcmp` looks the epoll
/// up under the watched fd and answers about whatever it finds there, or says
/// `EINVAL` because that is not an epoll. The tests above cannot see it --
/// they build the slot field by field by name, so they read the same in
/// whichever order the three are declared.
#[test]
fn the_epoll_slot_is_linux_s_struct_kcmp_epoll_slot() {
    use core::mem::{align_of, offset_of, size_of};

    assert_eq!(size_of::<KcmpEpollSlot>(), 12);
    assert_eq!(align_of::<KcmpEpollSlot>(), 4);
    assert_eq!(offset_of!(KcmpEpollSlot, efd), 0);
    assert_eq!(offset_of!(KcmpEpollSlot, tfd), 4);
    assert_eq!(offset_of!(KcmpEpollSlot, toff), 8);
}
