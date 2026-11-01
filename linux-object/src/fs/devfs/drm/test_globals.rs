extern crate std;

use self::std::sync::atomic::{AtomicU64, Ordering};
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

/// Which acquisition of [`LOCK`] is the current one. It only ever goes up,
/// so a token taken before a helper was spawned proves not just that the
/// lock is held but that it is held by the SAME acquisition -- the one the
/// helper belongs to.
///
/// Without it a helper could be waved through by a stranger: if the test
/// that spawned it unwinds before the helper registers, `OWNERS` is empty
/// for a moment, another test takes the lock, and the late helper then sees
/// a non-empty `OWNERS` and writes into THAT test's globals with the
/// assertions none the wiser. Which is this whole file's defect, one level
/// up.
static ACQUISITION: AtomicU64 = AtomicU64::new(0);

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
        // The handover. The lock stops two tests from interleaving, but it
        // does not stop one from leaving a flag flipped for the next holder:
        // same defect, later clock, and the victim is again whoever runs
        // next. Checked here so the panic names the test that left it.
        //
        // Not while already unwinding: a panic in a drop during a panic
        // aborts the process and takes the real failure's report with it.
        // A test that has already failed has been reported.
        let dirty = super::output_state_left_dirty();
        owners().clear();
        if let Some(dirty) = dirty {
            if !self::std::thread::panicking() {
                panic!(
                    "the DRM test lock was released with the output state \
                     left dirty: {dirty}. It is process-wide, so the next \
                     test to take the lock inherits it and THAT test is the \
                     one the suite reports. Put it back before releasing -- \
                     `drm::reset_output_state_for_test()` does the whole lot \
                     -- or keep the lock until you do.",
                    dirty = dirty
                );
            }
        }
    }
}

pub(crate) fn lock() -> Held {
    // A test that panics while holding this poisons it. The poison is not
    // a failure for the tests that follow, so step over it -- the panicking
    // test has already been reported.
    let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    ACQUISITION.fetch_add(1, Ordering::SeqCst);
    let mut o = owners();
    o.clear();
    o.push(me());
    drop(o);
    Held(g)
}

/// The token for the acquisition in force, to hand to a helper before
/// spawning it. Taking one asserts that the caller is inside the section,
/// because a token from outside it names an acquisition that has nothing to
/// do with the caller.
#[track_caller]
pub(crate) fn acquisition() -> u64 {
    assert_held("drm::test_globals::ACQUISITION");
    ACQUISITION.load(Ordering::SeqCst)
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
/// It takes the `token` its parent read with [`acquisition`] and checks it
/// against the acquisition in force, which catches both ways a helper can be
/// outside the section it thinks it is in: nobody holds the lock at all, or
/// somebody else has taken it since -- a parent that unwound early, and a
/// stranger waved the late helper through.
pub(crate) struct Helper;

impl Drop for Helper {
    fn drop(&mut self) {
        // Its OWN registration, not every entry this thread has. They are
        // not the same when the holder is also the helper, which is what
        // `a_helper_of_the_acquisition_in_force_is_accepted` does: a
        // `retain` there took the holder out of its own section.
        let mine = me();
        let mut o = owners();
        if let Some(i) = o.iter().rposition(|t| *t == mine) {
            o.remove(i);
        }
    }
}

#[track_caller]
pub(crate) fn join_as_helper(token: u64) -> Helper {
    let mut o = owners();
    let now = ACQUISITION.load(Ordering::SeqCst);
    assert!(
        !o.is_empty() && now == token,
        "a helper thread joined the DRM critical section it is not in: it \
         carries acquisition {token} and the one in force is {now}, held by \
         {held} thread(s). Its parent left the section before this helper \
         registered, so whatever it writes lands in whichever test holds the \
         lock now. Keep the `Held` guard alive in the thread that spawned \
         this one, for as long as this helper runs.",
        token = token,
        now = now,
        held = o.len()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The handoff this file's own assertion used to let through: a helper
    /// whose parent left the section before it registered, with another test
    /// holding the lock by then. A non-empty `OWNERS` says only that SOMEBODY
    /// holds it, so the late helper was waved through by a stranger and wrote
    /// into that stranger's globals -- the very defect the assertions exist to
    /// catch, one level up.
    ///
    /// The two acquisitions here stand for the parent's and the stranger's;
    /// what matters is that the token names the first and the second is in
    /// force.
    #[test]
    #[should_panic(expected = "joined the DRM critical section it is not in")]
    fn a_helper_that_registers_after_its_parent_left_is_refused() {
        let token = {
            let _parent = lock();
            acquisition()
        };
        let _stranger = lock();
        let _late = join_as_helper(token);
    }

    /// And the token of the acquisition in force is accepted, so the refusal
    /// above is about the handoff and not about tokens in general.
    #[test]
    fn a_helper_of_the_acquisition_in_force_is_accepted() {
        let _holder = lock();
        let token = acquisition();
        let late = join_as_helper(token);
        assert!(held_by_me());
        drop(late);
        assert!(
            held_by_me(),
            "dropping the helper took the holder out of its own section"
        );
    }
}
