//! `flock(2)`: BSD advisory whole-file locks.
//!
//! One global table keyed by the file's `(dev, inode)` identity holds, per
//! file, the open file descriptions that lock it and whether each holds it
//! exclusively. The owner of a flock is the **open file description**, not
//! the process (flock(2)): descriptors from `dup`, from `fork` and from an fd
//! passed over a unix socket share the one lock, any of them may release it,
//! and it goes away when the last of them is closed. The description here is
//! the `Arc<File>`, so its address is the owner id and `File`'s drop is the
//! release of the last close.
//!
//! `sys_flock` used to validate the request and answer 0 without taking
//! anything: two processes asking `LOCK_EX` on the same file both got it, so
//! everything that serialises itself with a lock file ran concurrently --
//! `flock(1)` in scripts, dpkg's and apt's frontend locks, fontconfig's cache
//! writers, `xdg-desktop-portal`, and every `LOCK_EX | LOCK_NB` "is another
//! instance running?" probe, which never found one.

use crate::error::LxError;
use crate::fs::record_lock::FileKey;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, Poll, Waker};
use core::time::Duration;
use hashbrown::HashMap;
use lazy_static::lazy_static;
use lock::Mutex;

/// The open file description that holds a lock: the address of its `File`.
pub type Owner = usize;

/// One held lock.
#[derive(Debug, Clone, Copy)]
struct Held {
    owner: Owner,
    exclusive: bool,
}

struct Table {
    locks: HashMap<FileKey, Vec<Held>>,
    /// Bumped whenever a lock is released or converted, which is when a
    /// blocked `flock` may get its turn.
    generation: u64,
    waiters: Vec<(u64, Waker)>,
    next_waiter: u64,
}

impl Table {
    /// A lock went away: those parked on the table get a look.
    fn touch(&mut self) {
        self.generation += 1;
        for (_, waker) in self.waiters.drain(..) {
            waker.wake();
        }
    }
}

lazy_static! {
    static ref FLOCKS: Mutex<Table> = Mutex::new(Table {
        locks: HashMap::new(),
        generation: 0,
        waiters: Vec::new(),
        next_waiter: 0,
    });
}

/// How many locks the table holds, kept outside its lock so that the drop
/// of a `File` (every close of every file) costs one load when nothing is
/// locked, which is nearly always.
static HELD_COUNT: AtomicUsize = AtomicUsize::new(0);

/// How often a parked `flock` looks for a signal while nothing changes.
const FLOCK_INTERRUPT_CHECK_TICK_MS: u64 = 100;

/// A lock held by another description that `exclusive` cannot share: any
/// other lock when asking for exclusive, an exclusive one when asking for
/// shared (`flock_locks_conflict`).
fn conflicts(held: &Held, owner: Owner, exclusive: bool) -> bool {
    held.owner != owner && (held.exclusive || exclusive)
}

/// Take `key` for `owner`, shared or exclusive, as `flock_lock_inode` does:
///
/// * a lock the owner already holds of the same kind is left as it is;
/// * a lock of the other kind is a conversion: the old lock is dropped
///   first, and only then is the new one tried, so a conversion that has to
///   wait holds nothing meanwhile (flock(2): "a conversion is not guaranteed
///   to be atomic; the existing lock is first removed");
/// * a conflicting lock of another description makes the attempt fail with
///   the table's generation, for [`wait_for_change`] to park on.
pub fn try_lock(key: FileKey, owner: Owner, exclusive: bool) -> Result<(), u64> {
    let mut table = FLOCKS.lock();
    let mut released = false;
    {
        let locks = table.locks.entry(key).or_default();
        if let Some(at) = locks.iter().position(|h| h.owner == owner) {
            if locks[at].exclusive == exclusive {
                return Ok(());
            }
            locks.swap_remove(at);
            HELD_COUNT.fetch_sub(1, Ordering::Relaxed);
            released = true;
        }
    }
    if released {
        table.touch();
    }
    let generation = table.generation;
    let locks = table.locks.entry(key).or_default();
    if locks.iter().any(|h| conflicts(h, owner, exclusive)) {
        if locks.is_empty() {
            table.locks.remove(&key);
        }
        return Err(generation);
    }
    locks.push(Held { owner, exclusive });
    HELD_COUNT.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// `LOCK_UN`: release `owner`'s lock on `key`, if it holds one.
pub fn unlock(key: FileKey, owner: Owner) {
    let mut table = FLOCKS.lock();
    let mut released = false;
    if let Some(locks) = table.locks.get_mut(&key) {
        let before = locks.len();
        locks.retain(|h| h.owner != owner);
        if locks.len() != before {
            HELD_COUNT.fetch_sub(1, Ordering::Relaxed);
            released = true;
        }
        if locks.is_empty() {
            table.locks.remove(&key);
        }
    }
    if released {
        table.touch();
    }
}

/// The last close of a description: every lock it holds goes. Called from
/// `File`'s drop, so it is cheap when nothing at all is locked.
pub fn release_owner(owner: Owner) {
    if HELD_COUNT.load(Ordering::Relaxed) == 0 {
        return;
    }
    let mut table = FLOCKS.lock();
    let mut released = 0;
    table.locks.retain(|_, locks| {
        let before = locks.len();
        locks.retain(|h| h.owner != owner);
        released += before - locks.len();
        !locks.is_empty()
    });
    if released > 0 {
        HELD_COUNT.fetch_sub(released, Ordering::Relaxed);
        table.touch();
    }
}

/// Who holds `key`: `(owner, exclusive)` per lock, in no particular order.
pub fn holders(key: FileKey) -> Vec<(Owner, bool)> {
    FLOCKS
        .lock()
        .locks
        .get(&key)
        .map(|locks| locks.iter().map(|h| (h.owner, h.exclusive)).collect())
        .unwrap_or_default()
}

/// Resolve once a lock has been released or converted since the attempt that
/// saw generation `since`; the caller then tries again. `EINTR` on a signal.
pub fn wait_for_change(since: u64) -> impl Future<Output = Result<(), LxError>> {
    ChangeFuture {
        since,
        sub_id: None,
        timer: None,
    }
}

/// How many waiters are parked on the table (tests).
#[cfg(test)]
fn waiter_count() -> usize {
    FLOCKS.lock().waiters.len()
}

struct ChangeFuture {
    since: u64,
    sub_id: Option<u64>,
    /// Backstop: the table wakes this for a release, and for nothing that
    /// happens to the waiter, so a tick is what lets a signal kill a
    /// `flock` parked behind a lock nobody drops.
    timer: Option<kernel_hal::timer_waker::TimerWakerSlot>,
}

impl ChangeFuture {
    fn unsubscribe(&mut self) {
        if let Some(id) = self.sub_id.take() {
            FLOCKS.lock().waiters.retain(|(i, _)| *i != id);
        }
        kernel_hal::timer_waker::kill_timer_waker(&mut self.timer);
    }
}

impl Drop for ChangeFuture {
    fn drop(&mut self) {
        self.unsubscribe();
    }
}

impl Future for ChangeFuture {
    type Output = Result<(), LxError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.as_mut().get_mut();
        {
            let mut table = FLOCKS.lock();
            if table.generation != this.since {
                drop(table);
                this.unsubscribe();
                return Poll::Ready(Ok(()));
            }
            // Park: register (or refresh) the waker before letting go of
            // the lock, so a release right after cannot miss it.
            match this.sub_id {
                Some(id) => {
                    if let Some(slot) = table.waiters.iter_mut().find(|(i, _)| *i == id) {
                        slot.1 = cx.waker().clone();
                    } else {
                        table.waiters.push((id, cx.waker().clone()));
                    }
                }
                None => {
                    let id = table.next_waiter;
                    table.next_waiter += 1;
                    table.waiters.push((id, cx.waker().clone()));
                    this.sub_id = Some(id);
                }
            }
        }
        if let Err(err) = crate::process::check_signals() {
            this.unsubscribe();
            return Poll::Ready(Err(err));
        }
        let deadline =
            kernel_hal::timer::deadline_after(Duration::from_millis(FLOCK_INTERRUPT_CHECK_TICK_MS));
        kernel_hal::timer_waker::ensure_timer_waker(&mut this.timer, deadline, cx);
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    //! `flock(2)` on the table, with descriptions named by number, and on
    //! real `File`s for the release at the last close. Every file key here is
    //! its own, but a key only keeps the `locks` map apart: the generation and
    //! the waiter list are ONE table for the binary, so every test in here
    //! takes [`alone_on_the_table`] on its first line. A new test belongs in
    //! that queue too, whatever it asserts -- see the note there.
    extern crate std;
    use super::*;
    use crate::fs::{File, OpenFlags};
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::sync::Arc;
    use alloc::task::Wake;
    use core::sync::atomic::AtomicUsize;
    use rcore_fs::vfs::{FileSystem, FileType};
    use rcore_fs_ramfs::RamFS;

    fn key(n: usize) -> FileKey {
        (0xf10c, n)
    }

    /// A waker that counts how often it was woken, so a wait that should
    /// resolve is polled by hand and a wait that should not is not hung on.
    struct CountWaker(AtomicUsize);
    impl Wake for CountWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn poll_once<F: Future + Unpin>(f: &mut F, waker: &Arc<CountWaker>) -> Poll<F::Output> {
        let waker: Waker = waker.clone().into();
        let mut cx = Context::from_waker(&waker);
        Pin::new(f).poll(&mut cx)
    }

    /// Hold the flock tests' turnstile until the end of the test.
    ///
    /// The per-test file key looked like enough isolation, and it is not:
    /// `Table::touch` bumps ONE `generation` for the whole table and drains
    /// EVERY parked waker, whichever file the released lock was on. So any
    /// test that unlocks -- which is nearly all of them -- can, from another
    /// harness thread:
    ///
    /// * wake and unpark a waiter another test had just parked, which is how
    ///   `waiter_count()` came back one short and how the wake counter came
    ///   back non-zero for a take that woke nobody;
    /// * move the generation between a failed `try_lock` and the
    ///   `wait_for_change` built from the number it returned, so the wait
    ///   resolves at once instead of parking.
    ///
    /// `a_dropped_wait_takes_its_waker_with_it` failed two runs in three on
    /// master because of it. Serialising is the fix rather than loosening the
    /// assertions: the exact waiter count IS the property -- a cancelled
    /// `flock` that leaves its waker behind leaks one per cancelled wait --
    /// and so is "a take wakes nobody".
    ///
    /// The guard travels in a `let _alone = ...` binding so it is released by
    /// the unwind of a failed assertion; naming it `_` instead would drop it
    /// on the spot and serialise nothing.
    #[must_use = "bind it to `_alone`: a bare `_` releases the turnstile at once"]
    fn alone_on_the_table() -> spin::MutexGuard<'static, ()> {
        static GUARD: spin::Mutex<()> = spin::Mutex::new(());
        GUARD.lock()
    }

    #[test]
    fn two_exclusive_locks_from_two_descriptions_do_not_both_succeed() {
        let _alone = alone_on_the_table();
        // What `flock -n` and dpkg rely on, and what used to be answered 0
        // for both callers.
        let k = key(1);
        assert_eq!(try_lock(k, 100, true), Ok(()));
        assert!(
            try_lock(k, 101, true).is_err(),
            "the second LOCK_EX went in"
        );
        assert!(
            try_lock(k, 101, false).is_err(),
            "a LOCK_SH went in beside a LOCK_EX"
        );
        unlock(k, 100);
        assert_eq!(try_lock(k, 101, true), Ok(()));
        unlock(k, 101);
    }

    #[test]
    fn shared_locks_coexist_and_an_exclusive_one_waits_for_all_of_them() {
        let _alone = alone_on_the_table();
        let k = key(2);
        assert_eq!(try_lock(k, 200, false), Ok(()));
        assert_eq!(try_lock(k, 201, false), Ok(()));
        assert!(try_lock(k, 202, true).is_err());
        unlock(k, 200);
        assert!(try_lock(k, 202, true).is_err(), "one reader is still there");
        unlock(k, 201);
        assert_eq!(try_lock(k, 202, true), Ok(()));
        unlock(k, 202);
    }

    #[test]
    fn a_lock_on_one_file_says_nothing_about_another() {
        let _alone = alone_on_the_table();
        assert_eq!(try_lock(key(3), 300, true), Ok(()));
        assert_eq!(try_lock(key(4), 301, true), Ok(()));
        unlock(key(3), 300);
        unlock(key(4), 301);
    }

    #[test]
    fn the_same_description_locking_again_is_a_no_op_and_dup_shares_it() {
        let _alone = alone_on_the_table();
        // `dup`, `fork`: the same description, so the same owner id. Asking
        // for what it already holds changes nothing, and a release through
        // any of the descriptors is the release.
        let k = key(5);
        assert_eq!(try_lock(k, 500, true), Ok(()));
        assert_eq!(try_lock(k, 500, true), Ok(()));
        assert_eq!(holders(k), alloc::vec![(500, true)], "the lock was doubled");
        unlock(k, 500);
        assert!(holders(k).is_empty());
    }

    #[test]
    fn a_conversion_drops_the_old_lock_first_and_waits_holding_nothing() {
        let _alone = alone_on_the_table();
        // `flock_lock_inode`: the existing lock of the same description is
        // deleted before the new type is tried, so a reader that wants to
        // become the writer while another reader is there loses its read
        // lock while it waits, and the other reader may take the write lock
        // in the meantime -- what flock(2) documents.
        let k = key(6);
        assert_eq!(try_lock(k, 600, false), Ok(()));
        assert_eq!(try_lock(k, 601, false), Ok(()));
        assert!(try_lock(k, 600, true).is_err());
        assert_eq!(
            holders(k),
            alloc::vec![(601, false)],
            "600 kept its shared lock"
        );
        assert_eq!(
            try_lock(k, 601, true),
            Ok(()),
            "the other reader can convert"
        );
        unlock(k, 601);
        // And a conversion with nobody in the way is one lock, of the new kind.
        assert_eq!(try_lock(k, 600, false), Ok(()));
        assert_eq!(try_lock(k, 600, true), Ok(()));
        assert_eq!(holders(k), alloc::vec![(600, true)]);
        unlock(k, 600);
    }

    #[test]
    fn unlocking_what_is_not_held_is_nothing() {
        let _alone = alone_on_the_table();
        let k = key(7);
        unlock(k, 700);
        assert!(holders(k).is_empty());
        assert_eq!(try_lock(k, 701, true), Ok(()));
        unlock(k, 700);
        assert_eq!(
            holders(k),
            alloc::vec![(701, true)],
            "someone else's lock went"
        );
        unlock(k, 701);
    }

    #[test]
    fn the_last_close_of_a_description_releases_its_locks_on_every_file() {
        let _alone = alone_on_the_table();
        let k1 = key(8);
        let k2 = key(9);
        assert_eq!(try_lock(k1, 800, true), Ok(()));
        assert_eq!(try_lock(k2, 800, false), Ok(()));
        assert_eq!(try_lock(k2, 801, false), Ok(()));
        release_owner(800);
        assert!(holders(k1).is_empty());
        assert_eq!(
            holders(k2),
            alloc::vec![(801, false)],
            "another owner's lock went"
        );
        assert_eq!(try_lock(k1, 802, true), Ok(()));
        unlock(k1, 802);
        unlock(k2, 801);
    }

    #[test]
    fn dropping_the_file_is_the_last_close() {
        let _alone = alone_on_the_table();
        // The syscall names the description by the `File`'s address, and
        // nothing else knows when the last descriptor to it is gone: the
        // drop has to be the release, or a process that took `LOCK_EX` and
        // exited (or just closed the fd) would hold the file for ever.
        let fs = RamFS::new();
        let inode = fs
            .root_inode()
            .create("locked", FileType::File, 0o644)
            .unwrap();
        core::mem::forget(fs);
        let file = File::new(inode.clone(), OpenFlags::RDWR, String::from("/locked"));
        let owner = Arc::as_ptr(&file) as usize;
        let k = key(10);
        assert_eq!(try_lock(k, owner, true), Ok(()));
        assert!(try_lock(k, 1000, true).is_err());
        drop(file);
        assert!(holders(k).is_empty(), "the close did not release the lock");
        assert_eq!(try_lock(k, 1000, true), Ok(()));
        unlock(k, 1000);
    }

    #[test]
    fn a_release_wakes_the_parked_waiter_and_a_stale_generation_does_not_park() {
        let _alone = alone_on_the_table();
        let k = key(11);
        assert_eq!(try_lock(k, 1100, true), Ok(()));
        let since = try_lock(k, 1101, true).unwrap_err();
        let waker = Arc::new(CountWaker(AtomicUsize::new(0)));
        let mut wait = Box::pin(wait_for_change(since));
        let parked = waiter_count();
        assert!(poll_once(&mut wait, &waker).is_pending());
        assert_eq!(waiter_count(), parked + 1);

        unlock(k, 1100);

        assert!(
            waker.0.load(Ordering::SeqCst) >= 1,
            "the release woke nobody"
        );
        assert!(matches!(poll_once(&mut wait, &waker), Poll::Ready(Ok(()))));
        assert_eq!(waiter_count(), parked, "the waiter stayed parked");
        assert_eq!(try_lock(k, 1101, true), Ok(()));
        unlock(k, 1101);

        // A change between the failed attempt and the wait: no parking.
        let mut stale = Box::pin(wait_for_change(since));
        assert!(matches!(poll_once(&mut stale, &waker), Poll::Ready(Ok(()))));
    }

    #[test]
    fn a_dropped_wait_takes_its_waker_with_it() {
        let _alone = alone_on_the_table();
        let k = key(12);
        assert_eq!(try_lock(k, 1200, true), Ok(()));
        let since = try_lock(k, 1201, true).unwrap_err();
        let waker = Arc::new(CountWaker(AtomicUsize::new(0)));
        let parked = waiter_count();
        let mut wait = Box::pin(wait_for_change(since));
        assert!(poll_once(&mut wait, &waker).is_pending());
        assert_eq!(waiter_count(), parked + 1);
        drop(wait);
        assert_eq!(waiter_count(), parked, "the cancelled flock left its waker");
        unlock(k, 1200);
    }

    #[test]
    fn taking_a_lock_does_not_wake_anyone_but_releasing_does() {
        let _alone = alone_on_the_table();
        // A parked writer has nothing to gain from one more reader arriving;
        // a spurious wake would have it re-scan for nothing.
        let k = key(13);
        assert_eq!(try_lock(k, 1300, false), Ok(()));
        let since = try_lock(k, 1301, true).unwrap_err();
        let waker = Arc::new(CountWaker(AtomicUsize::new(0)));
        let mut wait = Box::pin(wait_for_change(since));
        assert!(poll_once(&mut wait, &waker).is_pending());
        assert_eq!(try_lock(k, 1302, false), Ok(()));
        assert_eq!(waker.0.load(Ordering::SeqCst), 0);
        assert!(poll_once(&mut wait, &waker).is_pending());
        unlock(k, 1300);
        unlock(k, 1302);
        assert!(matches!(poll_once(&mut wait, &waker), Poll::Ready(Ok(()))));
        assert_eq!(try_lock(k, 1301, true), Ok(()));
        unlock(k, 1301);
    }

    /// One generation for the whole table means a release on ANY file wakes
    /// every parked `flock`, including those waiting on a file that did not
    /// change. They re-read the table, find their own lock still held and park
    /// again, so the answer stays right -- `sys_flock` loops on `try_lock` --
    /// but N blocked `flock`s cost N wakes per unrelated release. Worth
    /// pinning: it is the behaviour every waiter-count assertion in here
    /// depends on, and it is exactly what the per-file `locks` map does NOT
    /// give you.
    #[test]
    fn a_release_on_one_file_wakes_the_flocks_parked_on_another() {
        let _alone = alone_on_the_table();
        let mine = key(14);
        let other = key(15);
        assert_eq!(try_lock(mine, 1400, true), Ok(()));
        assert_eq!(try_lock(other, 1500, true), Ok(()));
        let since = try_lock(mine, 1401, true).unwrap_err();
        let waker = Arc::new(CountWaker(AtomicUsize::new(0)));
        let mut wait = Box::pin(wait_for_change(since));
        assert!(poll_once(&mut wait, &waker).is_pending());

        // Nothing at all happened to `mine`.
        unlock(other, 1500);

        assert_eq!(
            waker.0.load(Ordering::SeqCst),
            1,
            "a release elsewhere left the waiter asleep"
        );
        assert_eq!(
            waiter_count(),
            0,
            "the wake did not take the waker off the table"
        );
        assert!(matches!(poll_once(&mut wait, &waker), Poll::Ready(Ok(()))));
        // And the lock it actually wanted is still held by 1400.
        assert!(try_lock(mine, 1401, true).is_err());
        unlock(mine, 1400);
    }

    /// A waiter is charged once, not once per poll. The future carries its
    /// `sub_id` and refreshes the waker in place on a second poll; pushing
    /// again instead would leak one entry per poll, and a parked `flock` is
    /// re-polled every 100 ms by its own interrupt-check timer.
    #[test]
    fn polling_the_same_wait_twice_parks_one_waiter_not_two() {
        let _alone = alone_on_the_table();
        let k = key(16);
        assert_eq!(try_lock(k, 1600, true), Ok(()));
        let since = try_lock(k, 1601, true).unwrap_err();
        let first = Arc::new(CountWaker(AtomicUsize::new(0)));
        let second = Arc::new(CountWaker(AtomicUsize::new(0)));
        let mut wait = Box::pin(wait_for_change(since));
        assert!(poll_once(&mut wait, &first).is_pending());
        assert_eq!(waiter_count(), 1);
        assert!(poll_once(&mut wait, &second).is_pending());
        assert_eq!(waiter_count(), 1, "the second poll parked a second waker");

        // The waker on the table is the one from the LAST poll, which is why
        // refreshing it matters: a future moved to another task has to be woken
        // through that task's waker, not the one that polled it first.
        unlock(k, 1600);
        assert_eq!(first.0.load(Ordering::SeqCst), 0);
        assert_eq!(second.0.load(Ordering::SeqCst), 1);
        assert_eq!(waiter_count(), 0);
    }

    /// A wait built on a generation the table has already left never parks, so
    /// a release landing between the failed `try_lock` and the wait cannot be
    /// missed. This is the other half of that: a wait built on the CURRENT
    /// generation does park. The two together say the decision is the
    /// generation, and not always-park or never-park.
    #[test]
    fn a_wait_parks_on_the_current_generation_and_not_on_an_older_one() {
        let _alone = alone_on_the_table();
        let k = key(17);
        assert_eq!(try_lock(k, 1700, true), Ok(()));
        let since = try_lock(k, 1701, true).unwrap_err();
        let waker = Arc::new(CountWaker(AtomicUsize::new(0)));

        let mut now = Box::pin(wait_for_change(since));
        assert!(poll_once(&mut now, &waker).is_pending());
        assert_eq!(waiter_count(), 1);
        drop(now);

        let mut older = Box::pin(wait_for_change(since.wrapping_sub(1)));
        assert!(matches!(poll_once(&mut older, &waker), Poll::Ready(Ok(()))));
        assert_eq!(waiter_count(), 0, "a stale wait parked anyway");
        unlock(k, 1700);
    }
}
