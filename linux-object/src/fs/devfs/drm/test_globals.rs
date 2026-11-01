extern crate std;

use self::std::sync::{Mutex, MutexGuard};
use self::std::thread::ThreadId;
use alloc::vec::Vec;

/// `DRM_STATE`, the GEM table, `gem_mmap` and the CRTC's current fb are
/// process-wide, and cargo runs a crate's tests in threads. Every test
/// module that touches them takes this first, so one test's framebuffers
/// are not another's.
///
/// Without it the suite failed about one run in two with a different test
/// each time -- `crtc_fb` still holding a neighbour's framebuffer, or an
/// `ADDFB2` refused because a neighbour had filled the table. That reads
/// as a real regression and is not one, which is the worst kind of noise
/// to leave in a suite people are meant to trust.
static LOCK: Mutex<()> = Mutex::new(());

/// The threads allowed inside the critical section: the one that took
/// [`lock`], plus any helper it spawned that said so with [`join_as_helper`].
///
/// This is Linux's `lockdep_assert_held`, and it is here because the lock
/// only works if EVERY toucher takes it. One test writing a process-wide
/// global without it fails a DIFFERENT, innocent test in another thread, and
/// the suite reports the victim: the defect is invisible where it is and
/// loud where it is not. [`assert_held`] turns that into a panic on the
/// offender's own thread, which libtest names after the offending test.
static OWNERS: Mutex<Vec<ThreadId>> = Mutex::new(Vec::new());

fn owners() -> MutexGuard<'static, Vec<ThreadId>> {
    OWNERS.lock().unwrap_or_else(|e| e.into_inner())
}

fn me() -> ThreadId {
    self::std::thread::current().id()
}

/// The guard. It empties [`OWNERS`] before releasing [`LOCK`], so the window
/// where the two disagree reads as "held by nobody", never as "held by a
/// thread that has moved on".
pub(crate) struct Held(#[allow(dead_code)] MutexGuard<'static, ()>);

impl Drop for Held {
    fn drop(&mut self) {
        owners().clear();
    }
}

pub(crate) fn lock() -> Held {
    // A test that panics while holding this poisons it. The poison is not
    // a failure for the tests that follow, so step over it -- the panicking
    // test has already been reported.
    let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut o = owners();
    o.clear();
    o.push(me());
    drop(o);
    Held(g)
}

/// Says that this thread is a helper working inside the critical section of
/// the thread that spawned it, which is the shape of every test here that
/// races production code against itself: the holder parks in a wait and a
/// helper turns the CRTC off, or completes a flip, under it.
///
/// The lock is a plain `std::sync::Mutex`, so the helper cannot take it --
/// its holder is still in the section and the mutex is not reentrant -- and
/// it must not: the point of the helper is to run WHILE the holder waits.
/// So it registers instead, and [`assert_held`] accepts it.
///
/// It asserts that somebody holds the lock, because a helper whose parent
/// has already left is not inside anything and its writes land wherever the
/// next test is.
pub(crate) struct Helper;

impl Drop for Helper {
    fn drop(&mut self) {
        let mine = me();
        owners().retain(|t| *t != mine);
    }
}

#[track_caller]
pub(crate) fn join_as_helper() -> Helper {
    let mut o = owners();
    assert!(
        !o.is_empty(),
        "a helper thread joined the DRM critical section while nobody held \
         it: whatever it writes lands in whichever test runs next. Keep the \
         `Held` guard alive in the thread that spawned this one, for as long \
         as this helper runs."
    );
    o.push(me());
    Helper
}

/// Whether this thread may touch the globals: it took [`lock`], or it said
/// with [`join_as_helper`] that it works inside the holder's section.
pub(crate) fn held_by_me() -> bool {
    let mine = me();
    owners().contains(&mine)
}

/// Panic unless this thread may write, naming the global it was about to
/// write. The panic lands on the offender's own thread, so libtest reports
/// the offending test and not its victim.
#[track_caller]
pub(crate) fn assert_held(global: &str) {
    assert!(
        held_by_me(),
        "`{global}` was written without `drm::test_globals::lock()`. It is \
         process-wide, so the write lands in whichever test is running in \
         another thread, and THAT test is the one the suite reports. Take \
         the lock for as long as the write has to stand, or, in a helper \
         thread the holder spawned, call `join_as_helper` first.",
        global = global
    );
}
