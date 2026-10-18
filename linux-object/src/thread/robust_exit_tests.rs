//! What a dying thread does with the robust locks it still holds, and which
//! queue it wakes.
//!
//! A robust lock exists for one situation: its owner dies holding it and
//! somebody else is waiting, typically in ANOTHER process, on a
//! `PTHREAD_PROCESS_SHARED` mutex in a `MAP_SHARED` page. Linux marks the
//! word `FUTEX_OWNER_DIED` and wakes the waiter with shared flags
//! (`handle_futex_death`), so the waiter's `pthread_mutex_lock` returns
//! `EOWNERDEAD` instead of sleeping forever.
//!
//! The wake used to go to the dead thread's PER-PROCESS futex table, which is
//! not where a waiter without `FUTEX_PRIVATE_FLAG` on a shared page sleeps:
//! that one is on the shared futex keyed by object and offset. The word was
//! marked and the waiter never woke.
//!
//! The walk is driven here through [`release_robust_locks_of`], which takes a
//! plain `Thread`; [`CurrentThreadExt::release_robust_locks`] is that call on
//! the current thread, and the run loop makes it on every way a thread dies.

extern crate std;

use super::*;
use crate::process::{LinuxProcess, ProcessExt};
use alloc::boxed::Box;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU32, AtomicUsize};
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// The bits of a robust lock word (`<linux/futex.h>`).
const FUTEX_WAITERS: u32 = 0x8000_0000;
const FUTEX_OWNER_DIED: u32 = 0x4000_0000;

/// Where the pieces of the list sit, as byte offsets from its head: the head
/// itself (`next`, `futex_offset`, `list_op_pending`), one node, and the lock
/// word `FUTEX_OFFSET` bytes past that node.
const NODE: usize = 0x40;
const FUTEX_OFFSET: usize = 0x10;
const WORD: usize = NODE + FUTEX_OFFSET;

fn noop_waker() -> Waker {
    fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(core::ptr::null(), &VTABLE)
    }
    fn nop(_: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, nop, nop, nop);
    unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
}

fn a_linux_process(name: &str) -> Arc<Process> {
    Process::create_with_ext(&ROOT_JOB, name, LinuxProcess::new(RamFS::new(), 0)).unwrap()
}

/// Lay out a robust list holding one lock owned by `tid` with a waiter
/// recorded on it, and register it on `thread`.
///
/// The list is written at `at` and its pointers name `seen_at`, the address
/// the thread itself sees it at: the two differ for a shared page, which the
/// test writes through the frame and the thread reads through its mapping.
///
/// # Safety
/// `at..at + WORD + 4` must be writable memory of this test.
unsafe fn hold_one_robust_lock(thread: &Thread, at: usize, seen_at: usize, tid: u32) {
    let word = |off: usize| &*((at + off) as *const AtomicUsize);
    // The head: `next` points at the node, and nothing is pending.
    word(0).store(seen_at + NODE, Ordering::SeqCst);
    word(8).store(FUTEX_OFFSET, Ordering::SeqCst);
    word(16).store(0, Ordering::SeqCst);
    // The node: the last one, so its `next` comes back to the head.
    word(NODE).store(seen_at, Ordering::SeqCst);
    let lock = &*((at + WORD) as *const AtomicU32);
    lock.store(tid | FUTEX_WAITERS, Ordering::SeqCst);
    thread.set_robust_list(seen_at.into(), core::mem::size_of::<RobustList>());
}

fn lock_word(base: usize) -> u32 {
    unsafe { (*((base + WORD) as *const AtomicU32)).load(Ordering::SeqCst) }
}

#[test]
fn a_shared_lock_wakes_the_queue_a_shared_waiter_sleeps_on() {
    let _serialised = shared_futex::serialised_for_test();
    let owner = a_linux_process("robust-shared");
    let dying = Thread::create_linux(&owner).unwrap();
    let tid = dying.id() as u32;

    // One `MAP_SHARED | MAP_ANONYMOUS` page holding the list and the lock,
    // the way a `PTHREAD_PROCESS_SHARED` robust mutex lives.
    let vmar = owner.vmar();
    let (_vmo, user, frame) = shared_futex::map_shared_page_for_test(&vmar);
    unsafe { hold_one_robust_lock(&dying, frame, user, tid) };

    // A `FUTEX_WAIT` without the private flag on that word -- glibc's for
    // every robust mutex, musl's for a process-shared one -- sleeps here,
    // and so does one from any other process that maps the page.
    let queue = shared_futex::shared_futex_at(&vmar, user + WORD)
        .expect("a word in a shared page has a shared futex");
    let mut wait = Box::pin(queue.wait((tid | FUTEX_WAITERS) as i32));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    assert!(Pin::new(&mut wait).poll(&mut cx).is_pending());

    release_robust_locks_of(&dying);

    assert_eq!(
        lock_word(frame),
        FUTEX_WAITERS | FUTEX_OWNER_DIED,
        "the word is marked the way the libcs read it: owner gone, waiter kept"
    );
    assert!(
        matches!(Pin::new(&mut wait).poll(&mut cx), Poll::Ready(Ok(()))),
        "the waiter was never woken: the wake went to a queue it was not on, \
         and its `pthread_mutex_lock` sleeps forever"
    );
    // See `map_shared_page_for_test` for why the mapping is never undone.
    core::mem::forget(owner);
}

#[test]
fn a_private_lock_still_wakes_the_process_own_queue() {
    let owner = a_linux_process("robust-private");
    let dying = Thread::create_linux(&owner).unwrap();
    let tid = dying.id() as u32;
    // Memory no shared object backs: the per-process table is its home.
    let base = Box::leak(Box::new([0usize; 16])).as_ptr() as usize;
    unsafe { hold_one_robust_lock(&dying, base, base, tid) };

    let queue = owner.linux().get_futex(base + WORD).unwrap();
    let mut wait = Box::pin(queue.wait((tid | FUTEX_WAITERS) as i32));
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    assert!(Pin::new(&mut wait).poll(&mut cx).is_pending());

    release_robust_locks_of(&dying);

    assert_eq!(lock_word(base), FUTEX_WAITERS | FUTEX_OWNER_DIED);
    assert!(
        matches!(Pin::new(&mut wait).poll(&mut cx), Poll::Ready(Ok(()))),
        "a lock no other process can see is woken on the process's own table"
    );
}

#[test]
fn the_list_is_released_once_however_many_death_paths_reach_it() {
    let owner = a_linux_process("robust-once");
    let dying = Thread::create_linux(&owner).unwrap();
    let tid = dying.id() as u32;
    let base = Box::leak(Box::new([0usize; 16])).as_ptr() as usize;
    unsafe { hold_one_robust_lock(&dying, base, base, tid) };

    release_robust_locks_of(&dying);
    {
        let linux = dying.lock_linux();
        assert_eq!(
            (linux.robust_list.as_addr(), linux.robust_list_len),
            (0, 0),
            "the list is taken, as Linux's `futex_cleanup` clears it"
        );
    }
    // `exit(2)` releases and then falls out of the run loop, which releases
    // again. Put the word back to this thread's own tid: a list that was
    // only read, not taken, would be walked a second time and mark it again.
    let as_it_was = tid | FUTEX_WAITERS;
    unsafe { (*((base + WORD) as *const AtomicU32)).store(as_it_was, Ordering::SeqCst) };
    release_robust_locks_of(&dying);
    assert_eq!(
        lock_word(base),
        as_it_was,
        "the second death path walked the list again"
    );
}
