//! Whose x87/SSE/AVX registers a CPU is holding.
//!
//! A thread's registers are saved into its [`FpState`](super::FpState) every
//! time it enters the kernel, and the kernel does not use them. So when the
//! thread goes back out and nothing else has been loaded on that CPU since --
//! every system call that does not end in a switch to another thread -- the
//! registers are already what the area says, and loading them again is work
//! for nothing: `XRSTOR` of a full AVX-512 area is the most expensive
//! instruction on the way out.
//!
//! To know, each save is given a stamp, never given twice on any CPU, that
//! goes both into the area it filled and into the CPU that made it. A way out
//! loads the area unless the two still agree:
//!
//! - another thread ran on this CPU: its own save changed the CPU's stamp;
//! - the thread ran on another CPU meanwhile: the save there changed the
//!   area's stamp, to one this CPU never gave;
//! - the area was written by hand (a debugger setting a register):
//!   [`as_bytes_mut`](super::FpState::as_bytes_mut) dropped its stamp;
//! - the area was copied (a fork, a context saved and put back whole): the
//!   copy carries the stamp of the save that made its bytes, and agrees with
//!   a CPU exactly when that CPU still holds those bytes.

// Hosted, the threads are the host's and so are their registers: only the
// tests use this.
#![cfg_attr(not(any(target_os = "none", target_os = "uefi")), allow(dead_code))]

/// The stamp of an area no save has filled: it agrees with no CPU.
pub(crate) const NO_STAMP: u64 = 0;

/// The stamps are a CPU number (of at most 256) above this many bits of
/// count: at a save every 100 ns a CPU takes two centuries to run out.
const COUNT_BITS: u32 = 56;

/// What one CPU knows of the registers it holds. One per CPU, in its
/// `CpuLocalRegion`, and only ever touched from that CPU.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub(crate) struct FpOwner {
    /// The stamp of the save these registers were last written out by, while
    /// they are still as that save found them; [`NO_STAMP`] before the first.
    loaded: u64,
    /// The stamp the next save here gets.
    next: u64,
}

impl FpOwner {
    /// The owner for the `nth` CPU to come up (from 0).
    pub(crate) const fn new(nth: u64) -> Self {
        FpOwner {
            loaded: NO_STAMP,
            // From 1: no CPU ever gives NO_STAMP.
            next: (nth << COUNT_BITS) + 1,
        }
    }

    /// The two words, as they sit in the `CpuLocalRegion`.
    pub(crate) const fn from_words(loaded: u64, next: u64) -> Self {
        FpOwner { loaded, next }
    }
    pub(crate) const fn words(self) -> (u64, u64) {
        (self.loaded, self.next)
    }

    /// Whether the registers still hold what the area stamped `stamp` says,
    /// so that loading it would change nothing.
    #[inline(always)]
    pub(crate) fn holds(&self, stamp: u64) -> bool {
        stamp != NO_STAMP && stamp == self.loaded
    }

    /// The registers have just been saved: the stamp for the area they went
    /// into, which this CPU now holds.
    #[inline(always)]
    pub(crate) fn saved(&mut self) -> u64 {
        let stamp = self.next;
        self.next += 1;
        self.loaded = stamp;
        stamp
    }
}

#[cfg(test)]
mod tests {
    use super::super::FpState;
    use super::*;

    /// One trip of a thread through user mode on `cpu`, as `run` makes it:
    /// whether the area had to be loaded on the way out.
    fn trip(cpu: &mut FpOwner, area: &mut FpState) -> bool {
        let loaded = !cpu.holds(area.stamp);
        area.stamp = cpu.saved();
        loaded
    }

    #[test]
    fn a_fresh_thread_is_loaded_and_then_is_not_while_it_runs_alone() {
        let mut cpu = FpOwner::new(0);
        let mut a = FpState::default();
        assert!(trip(&mut cpu, &mut a), "nothing was ever loaded for it");
        assert!(!trip(&mut cpu, &mut a));
        assert!(!trip(&mut cpu, &mut a));
    }

    #[test]
    fn two_threads_taking_turns_are_loaded_every_time() {
        let mut cpu = FpOwner::new(0);
        let (mut a, mut b) = (FpState::default(), FpState::default());
        for _ in 0..3 {
            assert!(trip(&mut cpu, &mut a));
            assert!(trip(&mut cpu, &mut b));
        }
        // And each alone again is not.
        assert!(!trip(&mut cpu, &mut b));
        assert!(trip(&mut cpu, &mut a));
        assert!(!trip(&mut cpu, &mut a));
    }

    #[test]
    fn a_thread_back_from_another_cpu_is_loaded_on_both() {
        let (mut one, mut two) = (FpOwner::new(0), FpOwner::new(1));
        let mut a = FpState::default();
        assert!(trip(&mut one, &mut a));
        // `one` still holds what `a` had then, and `a` has moved on.
        assert!(trip(&mut two, &mut a));
        assert!(!trip(&mut two, &mut a));
        assert!(
            trip(&mut one, &mut a),
            "what CPU one holds is two trips old"
        );
        assert!(trip(&mut two, &mut a));
    }

    #[test]
    fn the_stamps_of_two_cpus_never_meet() {
        // The same number of saves on each: were the stamps only counts, a
        // thread would look at home on a CPU it has never run on.
        let (mut one, mut two) = (FpOwner::new(0), FpOwner::new(1));
        let (mut a, mut b) = (FpState::default(), FpState::default());
        for _ in 0..5 {
            trip(&mut one, &mut a);
            trip(&mut two, &mut b);
        }
        assert!(trip(&mut two, &mut a));
        assert!(trip(&mut one, &mut b));
        assert_ne!(FpOwner::new(0).saved(), FpOwner::new(1).saved());
        assert_ne!(FpOwner::new(3).saved(), NO_STAMP);
    }

    #[test]
    fn an_area_written_by_hand_is_loaded() {
        let mut cpu = FpOwner::new(0);
        let mut a = FpState::default();
        trip(&mut cpu, &mut a);
        assert!(!trip(&mut cpu, &mut a));
        // A debugger sets a register: the CPU's copy is now the stale one.
        a.as_bytes_mut()[160] = 0x5a;
        assert!(trip(&mut cpu, &mut a));
        // Only reading it changes nothing.
        let _ = a.as_bytes();
        assert!(!trip(&mut cpu, &mut a));
    }

    #[test]
    fn a_copy_is_at_home_exactly_where_its_bytes_still_are() {
        let mut cpu = FpOwner::new(0);
        let mut parent = FpState::default();
        trip(&mut cpu, &mut parent);
        // A fork: the child's registers are the parent's, which the CPU
        // holds. Whichever of the two goes out first finds them there...
        let mut child = parent;
        assert!(!trip(&mut cpu, &mut child));
        // ...and the other, after it, does not.
        assert!(trip(&mut cpu, &mut parent));
        assert!(trip(&mut cpu, &mut child));
    }

    #[test]
    fn a_context_put_back_whole_is_loaded_once_the_thread_has_run_since() {
        let mut cpu = FpOwner::new(0);
        let mut a = FpState::default();
        trip(&mut cpu, &mut a);
        // A signal: the context is set aside, the handler runs and comes
        // back into the kernel (the save gives the area a new stamp), and
        // the old context is put back over it.
        let aside = a;
        assert!(!trip(&mut cpu, &mut a));
        a = aside;
        assert!(trip(&mut cpu, &mut a), "the registers are the handler's");
    }

    #[test]
    fn the_stamp_is_no_part_of_what_two_areas_hold() {
        let mut cpu = FpOwner::new(0);
        let mut a = FpState::default();
        trip(&mut cpu, &mut a);
        assert_eq!(a, FpState::default());
    }
}
