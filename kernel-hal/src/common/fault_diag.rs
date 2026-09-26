//! Whose turn it is to diagnose a kernel page fault.
//!
//! A kernel #PF that cannot be resolved ends in a formatted dump, and the
//! formatting itself can fault again -- the heap or a vtable is already
//! smashed, or the graphic console was torn down mid-redraw. So the dump is
//! behind a latch, and the latch has to tell two cases apart that need
//! opposite answers:
//!
//! * the **same CPU re-entering** its own diagnosis: the first dump faulted,
//!   so anything more cascades. It has to stop.
//! * a **peer CPU faulting at the same time**: its own state is intact and its
//!   report is worth as much as the first one. It has to wait its turn, not
//!   stop.
//!
//! Treating a peer as a re-entry cost the whole machine once. The QEMU monitor
//! caught it mid-freeze with two CPUs parked in the re-entry spin, one halted,
//! and the other three stuck on ordinary locks those two were holding -- and
//! not one word on the serial console. That is why the latch remembers *which*
//! CPU holds it and not merely that it is held.
//!
//! The decision lives here, and not next to the printing in `zCore`, because
//! `zCore`'s `[[bin]]` carries `test = false`: no test of that crate has ever
//! run. Here the host suite can drive the latch from several threads.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use core::time::Duration;

use crate::config::MAX_CORE_NUM;

/// The value [`FaultDiagLatch`] stores when nobody holds it.
///
/// Out of range for every per-CPU array, like `lock::NO_CPU`, so it cannot be
/// mistaken for a CPU.
const NOBODY: usize = usize::MAX;

/// The seam a test uses to stand in the middle of [`FaultDiagLatch::release`].
///
/// The order of the two stores in there is a rule about *writes*, and no
/// single-threaded test can see it: whichever order they are in, the state
/// afterwards is the same. A hook run between them is a peer arriving in
/// exactly the window the order exists to close.
#[cfg(not(test))]
#[inline(always)]
fn release_midpoint(_latch: &FaultDiagLatch) {}

#[cfg(test)]
fn release_midpoint(latch: &FaultDiagLatch) {
    if let Some(f) = RELEASE_MIDPOINT.with(|c| c.get()) {
        f(latch);
    }
}

#[cfg(test)]
std::thread_local! {
    static RELEASE_MIDPOINT: core::cell::Cell<Option<fn(&FaultDiagLatch)>> =
        const { core::cell::Cell::new(None) };
}

/// What a CPU asking to diagnose is told.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum DiagTurn {
    /// Go ahead: this CPU holds the latch and must [`FaultDiagLatch::release`]
    /// it, unless it is about to halt.
    Mine,
    /// This very CPU already holds the latch, so the diagnosis it is running
    /// has itself faulted. Anything more cascades: stop here.
    ReEntry,
    /// Another CPU holds it. Wait for it (see
    /// [`FaultDiagLatch::wait_for_peer`]) and report afterwards.
    PeerBusy,
}

/// The latch itself.
pub struct FaultDiagLatch {
    held: AtomicBool,
    /// Which CPU holds it, or [`NOBODY`].
    cpu: AtomicUsize,
}

impl Default for FaultDiagLatch {
    fn default() -> Self {
        Self::new()
    }
}

impl FaultDiagLatch {
    pub const fn new() -> Self {
        Self {
            held: AtomicBool::new(false),
            cpu: AtomicUsize::new(NOBODY),
        }
    }

    /// Whether an id may be compared with another CPU's at all.
    ///
    /// `cpu_id()` answers `lock::NO_CPU` -- 255, one past every per-CPU array
    /// -- for a core SMP bring-up never gave a dense id, and it answers it for
    /// **every** such core. Two of them faulting at once would read the same
    /// number and each be told it was re-entering its own diagnosis, so both
    /// would stop: the freeze in the module docs, reached from the other side.
    ///
    /// And an id-less core is exactly the one that gets here. A CPU whose GS
    /// or `TPIDR_EL1` lies about who it is has no believable id
    /// (`lock::accepts_published` refuses it), and a GS that lies is what
    /// manufactures the wild writes this fault is the tail end of -- the
    /// `[cpuid-bogus]` line printed a few lines further down says so.
    ///
    /// So an id that names no CPU never matches. The cost of being wrong here
    /// is not symmetric: a peer mistaken for a re-entry stops a healthy CPU
    /// dead, while a re-entry mistaken for a peer waits its turn and prints
    /// one more line.
    fn is_an_identity(id: usize) -> bool {
        id < MAX_CORE_NUM
    }

    /// Ask for the latch on behalf of CPU `me`.
    pub fn take(&self, me: usize) -> DiagTurn {
        if !self.held.swap(true, Ordering::SeqCst) {
            self.cpu.store(me, Ordering::SeqCst);
            return DiagTurn::Mine;
        }
        let holder = self.cpu.load(Ordering::SeqCst);
        if Self::is_an_identity(me) && holder == me {
            DiagTurn::ReEntry
        } else {
            DiagTurn::PeerBusy
        }
    }

    /// Release a latch taken by [`take`](Self::take).
    ///
    /// The holder is forgotten **before** the flag, and that order is the
    /// whole of it: with the flag cleared first, a peer can take the latch and
    /// publish its own id in the window, and the `NOBODY` written afterwards
    /// lands on top of it. The latch would then be held by a CPU it does not
    /// name, and the real holder's own re-entry would read `NOBODY`, be told
    /// it was a peer, and spin for the whole budget instead of stopping.
    pub fn release(&self) {
        self.cpu.store(NOBODY, Ordering::SeqCst);
        release_midpoint(self);
        self.held.store(false, Ordering::SeqCst);
    }

    /// Whether anyone is diagnosing right now.
    pub fn busy(&self) -> bool {
        self.held.load(Ordering::SeqCst)
    }

    /// The CPU diagnosing right now, if the latch is held and names one.
    pub fn holder(&self) -> Option<usize> {
        if !self.busy() {
            return None;
        }
        let who = self.cpu.load(Ordering::SeqCst);
        Self::is_an_identity(who).then_some(who)
    }

    /// Wait for a peer's diagnosis to finish, and say whether it did.
    ///
    /// Bounded, because a peer that halts mid-diagnosis would otherwise park
    /// this CPU for ever; after the budget it goes ahead regardless, since by
    /// then the alternative is a silent freeze.
    ///
    /// `pump` is called on every turn of the wait and must be lock-free and
    /// allocation-free: this runs from a fault with interrupts off, and the
    /// budget is a very long time to be deaf to a TLB-shootdown IPI. A peer
    /// spinning for this CPU's acknowledgement has a budget of its own, and
    /// when it runs out on riscv64 or aarch64 there is no NMI rescue behind
    /// it -- a fault the machine was about to survive takes it down anyway,
    /// from a CPU that had nothing to do with it.
    pub fn wait_for_peer(
        &self,
        budget: Duration,
        mut now: impl FnMut() -> Duration,
        mut pump: impl FnMut(),
    ) -> bool {
        let deadline = now() + budget;
        while self.busy() {
            if now() >= deadline {
                return false;
            }
            pump();
            core::hint::spin_loop();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;

    const CPU0: usize = 0;
    const CPU1: usize = 1;
    /// What `cpu_id()` answers for every core bring-up never gave a dense
    /// logical id: `lock::NO_CPU`.
    const NO_CPU: usize = u8::MAX as usize;

    #[test]
    fn the_first_cpu_to_ask_gets_to_report() {
        let l = FaultDiagLatch::new();
        assert!(!l.busy());
        assert_eq!(l.holder(), None);
        assert_eq!(l.take(CPU0), DiagTurn::Mine);
        assert!(l.busy());
        assert_eq!(l.holder(), Some(CPU0));
    }

    #[test]
    fn the_same_cpu_asking_again_is_a_re_entry() {
        // Its own diagnosis faulted, so anything more cascades -- that was the
        // infinite `[KERNEL PAGE FAULT]` scroll.
        let l = FaultDiagLatch::new();
        assert_eq!(l.take(CPU0), DiagTurn::Mine);
        assert_eq!(l.take(CPU0), DiagTurn::ReEntry);
        assert_eq!(l.take(CPU0), DiagTurn::ReEntry);
    }

    #[test]
    fn another_cpu_asking_is_not_a_re_entry() {
        // Its own state is intact and its report is worth as much as the first
        // one. Told it was re-entering, it stops -- and that froze the machine.
        let l = FaultDiagLatch::new();
        assert_eq!(l.take(CPU0), DiagTurn::Mine);
        assert_eq!(l.take(CPU1), DiagTurn::PeerBusy);
    }

    #[test]
    fn two_cores_with_no_logical_id_are_not_the_same_core() {
        // `cpu_id()` answers `NO_CPU` for EVERY core bring-up never gave a
        // dense id, so comparing two of them for identity reads as one CPU and
        // the second is told to stop. And an id-less core is exactly the one
        // that gets here: a CPU whose GS lies about who it is has no
        // believable id, and a GS that lies is what manufactures the writes
        // this fault is the tail end of.
        let l = FaultDiagLatch::new();
        assert_eq!(l.take(NO_CPU), DiagTurn::Mine);
        assert_eq!(
            l.take(NO_CPU),
            DiagTurn::PeerBusy,
            "an id that names no CPU was matched against another one"
        );
        // ...and it does not claim to be the holder either.
        assert_eq!(l.holder(), None);
        assert!(l.busy(), "the latch is held all the same");
    }

    #[test]
    fn an_id_past_the_per_cpu_arrays_is_never_an_identity() {
        // The rule is the one the rest of the tree is written to: an id no
        // per-CPU array can hold is not a CPU. `NO_CPU` is only the one that
        // happens to arrive.
        let l = FaultDiagLatch::new();
        assert_eq!(l.take(MAX_CORE_NUM), DiagTurn::Mine);
        assert_eq!(l.take(MAX_CORE_NUM), DiagTurn::PeerBusy);
        // The last id that IS one still works.
        let l = FaultDiagLatch::new();
        assert_eq!(l.take(MAX_CORE_NUM - 1), DiagTurn::Mine);
        assert_eq!(l.take(MAX_CORE_NUM - 1), DiagTurn::ReEntry);
    }

    #[test]
    fn releasing_hands_the_latch_to_the_next_cpu() {
        let l = FaultDiagLatch::new();
        assert_eq!(l.take(CPU0), DiagTurn::Mine);
        l.release();
        assert!(!l.busy());
        assert_eq!(l.holder(), None);
        assert_eq!(l.take(CPU1), DiagTurn::Mine);
        assert_eq!(l.holder(), Some(CPU1));
    }

    std::thread_local! {
        static MIDPOINT_TURN: Cell<Option<DiagTurn>> = const { Cell::new(None) };
    }

    #[test]
    fn nothing_can_slip_into_the_middle_of_a_release() {
        // The order of the two stores in `release` is a rule about writes, so
        // a peer has to arrive in the window it exists to close. With the flag
        // cleared first, this peer takes the latch and publishes its own id,
        // and the `NOBODY` written afterwards lands on top of it: the latch
        // ends up held by a CPU it does not name, and the real holder's
        // re-entry is told it is a peer and spins for the whole budget
        // instead of stopping.
        let l = FaultDiagLatch::new();
        assert_eq!(l.take(CPU0), DiagTurn::Mine);
        MIDPOINT_TURN.with(|c| c.set(None));
        RELEASE_MIDPOINT.with(|c| {
            c.set(Some(|l: &FaultDiagLatch| {
                MIDPOINT_TURN.with(|t| t.set(Some(l.take(CPU1))));
            }))
        });
        l.release();
        RELEASE_MIDPOINT.with(|c| c.set(None));
        assert_eq!(
            MIDPOINT_TURN.with(|c| c.get()),
            Some(DiagTurn::PeerBusy),
            "a peer got the latch while it was still being let go"
        );
        // ...and the release left it free and nameless, not held by a CPU it
        // cannot name.
        assert!(!l.busy());
        assert_eq!(l.holder(), None);
    }

    #[test]
    fn the_holder_is_forgotten_before_the_latch_is_dropped() {
        // With the flag cleared first, a peer takes the latch and publishes
        // its own id in the window, and the `NOBODY` written afterwards lands
        // on top of it: the latch is then held by a CPU it does not name, and
        // the real holder's re-entry is told it is a peer and spins for the
        // whole budget instead of stopping.
        let l = FaultDiagLatch::new();
        assert_eq!(l.take(CPU0), DiagTurn::Mine);
        l.release();
        assert_eq!(
            l.take(CPU1),
            DiagTurn::Mine,
            "the latch was not free after being released"
        );
        assert_eq!(
            l.holder(),
            Some(CPU1),
            "the release overwrote the new holder"
        );
        assert_eq!(l.take(CPU0), DiagTurn::PeerBusy);
        assert_eq!(l.take(CPU1), DiagTurn::ReEntry);
    }

    // ── the bounded wait ─────────────────────────────────────────────────

    /// A clock that moves one millisecond every time it is read.
    fn ticking_clock() -> impl FnMut() -> Duration {
        let mut ms = 0u64;
        move || {
            ms += 1;
            Duration::from_millis(ms)
        }
    }

    #[test]
    fn a_wait_that_ends_says_the_peer_finished() {
        let l = FaultDiagLatch::new();
        assert_eq!(l.take(CPU0), DiagTurn::Mine);
        // The peer lets go on the third turn of the loop.
        let turns = Cell::new(0);
        let finished = l.wait_for_peer(Duration::from_secs(2), ticking_clock(), || {
            turns.set(turns.get() + 1);
            if turns.get() == 3 {
                l.release();
            }
        });
        assert!(finished);
        assert_eq!(turns.get(), 3);
    }

    #[test]
    fn a_peer_that_never_finishes_does_not_park_this_cpu_for_ever() {
        // A peer that halts mid-diagnosis holds the latch until the machine
        // is reset. After the budget this CPU reports anyway, because by then
        // the alternative is the silent freeze.
        let l = FaultDiagLatch::new();
        assert_eq!(l.take(CPU0), DiagTurn::Mine);
        let turns = Cell::new(0);
        let finished = l.wait_for_peer(Duration::from_millis(5), ticking_clock(), || {
            turns.set(turns.get() + 1);
        });
        assert!(!finished);
        assert!(l.busy(), "the peer still holds it");
        assert!(turns.get() > 0, "and the wait was a wait, not a return");
    }

    #[test]
    fn a_latch_nobody_holds_is_not_waited_on_at_all() {
        let l = FaultDiagLatch::new();
        let turns = Cell::new(0);
        let finished = l.wait_for_peer(Duration::from_secs(2), ticking_clock(), || {
            turns.set(turns.get() + 1);
        });
        assert!(finished);
        assert_eq!(turns.get(), 0);
    }

    #[test]
    fn the_wait_drains_its_own_ipi_queue_on_every_turn() {
        // Not a detail: this runs from a fault with interrupts off, and the
        // budget is a very long time to be deaf to a TLB-shootdown IPI. A peer
        // spinning for this CPU's acknowledgement runs out of budget, and on
        // riscv64 and aarch64 there is no NMI rescue behind it.
        let l = FaultDiagLatch::new();
        assert_eq!(l.take(CPU0), DiagTurn::Mine);
        let pumps = Cell::new(0);
        let reads = Cell::new(0);
        let mut ms = 0u64;
        l.wait_for_peer(
            Duration::from_millis(4),
            || {
                reads.set(reads.get() + 1);
                ms += 1;
                Duration::from_millis(ms)
            },
            || pumps.set(pumps.get() + 1),
        );
        assert!(pumps.get() > 0);
        // One pump per turn of the loop: the deadline is read once up front
        // and once per turn.
        assert_eq!(pumps.get(), reads.get() - 2);
    }
}
