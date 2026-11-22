//! What the kernel does when its own heap runs out: Linux's `out_of_memory()`,
//! as far as this allocator lets it go.
//!
//! In Linux a failed kernel allocation is not an oops. Direct reclaim runs
//! first; when it frees nothing the OOM killer picks the process with the
//! worst badness and kills it, the victim may dig into the memory reserves to
//! get out, and the allocation is retried. Only `vm.panic_on_oom`, or a
//! machine with nothing left to kill, ends in a panic.
//!
//! Here the order is the same, with one difference forced by `GlobalAlloc`:
//! the allocation that fails may hold any kernel lock, so the OOM killer
//! (which walks the process tree and allocates) cannot run inside it. So:
//!
//! 1. the allocator grows the heap, drains its caches and runs the shrinkers
//!    (`memory_x86_64.rs`), as before;
//! 2. if that is not enough, it gives the **emergency reserve** back to the
//!    buddy and retries: the allocation succeeds, the kernel keeps going, and
//!    a kill is requested here;
//! 3. `reclaimd` takes the request on its next pass (at most 200 ms later)
//!    and runs `kill_worst`, which kills by badness -- the process holding
//!    the most memory, not whichever one happened to allocate;
//! 4. once the heap has room again, `reclaimd` re-arms the reserve;
//! 5. only if the reserve is already spent does the allocation fail. That
//!    reaches the oops path labelled `kernel OOM` (not `kernel panic`), and
//!    is contained like any other oops when it can be.
//!
//! `PANICONOOM=1` on the command line is `vm.panic_on_oom=1`: no reserve, no
//! containment, the machine stops on the first kernel OOM with its report on
//! the screen. Off by default, as in Linux.

// Most of this is reached only from the bare x86_64 allocator and from
// `reclaimd`; a libos or zircon build compiles it and uses part of it.
#![allow(dead_code)]

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// `PANICONOOM=1`.
static PANIC_ON_OOM: AtomicBool = AtomicBool::new(false);
/// A kill the allocator asked for and `reclaimd` has not served yet.
static KILL_PENDING: AtomicBool = AtomicBool::new(false);
/// Kills asked for since boot.
static KILLS_REQUESTED: AtomicUsize = AtomicUsize::new(0);
/// `cpu + 1` of the CPU whose panic is a kernel OOM (set by the
/// `#[alloc_error_handler]` right before it panics), or 0.
static OOM_PANIC_CPU: AtomicUsize = AtomicUsize::new(0);
/// Kernel OOMs that reached the oops path, contained or not.
static OOM_PANICS: AtomicUsize = AtomicUsize::new(0);

/// Enable `PANICONOOM=1`.
pub fn set_panic_on_oom(enabled: bool) {
    PANIC_ON_OOM.store(enabled, Ordering::SeqCst);
}

/// Whether `PANICONOOM=1` is on.
pub fn panic_on_oom() -> bool {
    PANIC_ON_OOM.load(Ordering::Relaxed)
}

/// Ask `reclaimd` to run the OOM killer. Lock-free and allocation-free: this
/// is called from inside the allocator.
pub fn request_kill() {
    KILL_PENDING.store(true, Ordering::Release);
    KILLS_REQUESTED.fetch_add(1, Ordering::Relaxed);
}

/// Take the pending kill request, if there is one.
pub fn take_kill_request() -> bool {
    KILL_PENDING.swap(false, Ordering::AcqRel)
}

/// Whether a kill has been asked for and not yet served.
pub fn kill_pending() -> bool {
    KILL_PENDING.load(Ordering::Acquire)
}

/// Kills asked for since boot.
pub fn kills_requested() -> usize {
    KILLS_REQUESTED.load(Ordering::Relaxed)
}

/// Mark the coming panic on `cpu` as a kernel OOM.
pub fn note_oom_panic(cpu: usize) {
    OOM_PANICS.fetch_add(1, Ordering::Relaxed);
    OOM_PANIC_CPU.store(cpu + 1, Ordering::Release);
}

/// Whether the panic in flight on `cpu` is a kernel OOM. Clears the mark,
/// so a later, unrelated panic on the same CPU is not mislabelled.
pub fn take_oom_panic(cpu: usize) -> bool {
    OOM_PANIC_CPU
        .compare_exchange(cpu + 1, 0, Ordering::AcqRel, Ordering::Relaxed)
        .is_ok()
}

/// Kernel OOMs that reached the oops path since boot.
pub fn oom_panics() -> usize {
    OOM_PANICS.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_oom_mark_belongs_to_its_cpu_and_is_taken_once() {
        note_oom_panic(3);
        assert!(!take_oom_panic(2), "another CPU's panic is not this OOM");
        assert!(take_oom_panic(3));
        assert!(!take_oom_panic(3), "the mark is taken with the label");
    }

    #[test]
    fn a_kill_request_is_served_once() {
        let before = kills_requested();
        request_kill();
        request_kill();
        assert!(kill_pending());
        assert!(take_kill_request());
        assert!(!take_kill_request());
        assert_eq!(kills_requested(), before + 2);
    }
}
