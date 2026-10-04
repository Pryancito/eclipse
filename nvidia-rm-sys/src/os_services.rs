//! Implementation of the small subset of NVIDIA's internal "OBJOS" service
//! surface that real vendored RM files (core/thread_state.c, the nvport
//! libraries, diagnostics/nvlog_printf.c) call directly -- distinct from
//! the `os-interface.h` ABI in `os_interface.rs`. Real signatures
//! transcribed from src/nvidia/generated/g_os_nvoc.h (MIT,
//! NVIDIA/open-gpu-kernel-modules); NVIDIA's own real implementation of
//! these lives in the Linux-specific arch/nvalloc/unix/src tree, which
//! Eclipse has no equivalent of, so each is backed by Eclipse's own
//! primitives (via `crate::hooks`) or a safe default, same convention as
//! `os_interface.rs`.
//!
//! A few functions that originally looked missing from this same header
//! (osGetMaximumCoreCount, osReadRegistryDword/String, g_pSys,
//! gpumgrGetCurrentGpuInstance, threadPriorityStateAlloc/Free,
//! rcdbAddAssertJournalRecWithLine) turned out to already have real
//! implementations in other vendored files once the full RM core linked
//! (os_init.c, system.c, gpu_mgr.c, locks_common.c, journal.c) --
//! confirmed by an actual `duplicate symbol` link error against a hand-
//! written stand-in, not assumed -- so they are deliberately NOT
//! duplicated here.
#![allow(non_snake_case)]

use crate::types::*;

#[no_mangle]
pub extern "C" fn osGetCurrentThread(handle: *mut NvU64) -> NV_STATUS {
    if handle.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    unsafe { *handle = 0 };
    NV_OK
}

#[no_mangle]
pub extern "C" fn osGetCurrentProcessorNumber() -> NvU32 {
    0
}

#[no_mangle]
pub extern "C" fn osGetCurrentProcessFlags() -> NvU32 {
    0
}

/// Microsecond busy-wait + TLB yield. This is the primitive
/// `eclipse_ce_wait_bounded` should poll with (e.g. `osDelayUs(10)` or
/// `osDelayUs(0)` / `osSchedule`) instead of `osDelay(1)`, which cannot
/// resolve finer than a millisecond even after the CE copy has finished.
#[no_mangle]
pub extern "C" fn osDelayUs(microseconds: NvU32) -> NV_STATUS {
    crate::hooks::delay_us_or_spin(microseconds);
    NV_OK
}

#[no_mangle]
pub extern "C" fn osGetMonotonicTimeNs() -> NvU64 {
    crate::hooks::clock_ns()
}

/// 580.178.04 retired `osGetTickResolution` in favour of this one, and
/// `timeoutSet` (gpu_timeout.c) uses it exactly the same way the old hook was
/// used: added to the requested timeout to pad the deadline out to the next
/// tick, so that a timeout started near the end of a tick does not fire early.
/// It is never a divisor, so any small non-zero value is safe. Same value as
/// the `osGetTickResolution` it replaces -- Linux's fallback for a
/// microsecond-granularity clock, NSEC_PER_USEC.
#[no_mangle]
pub extern "C" fn osGetMonotonicTickResolutionNs() -> NvU64 {
    // The same constant the fallback clock advances by, so the granularity
    // this crate reports is the granularity its clock actually has.
    crate::hooks::TICK_RESOLUTION_NS
}

// GPU_TIMEOUT_FLAGS_OSTIMER = NVBIT(3) (gpu_timeout.h). This MUST be set:
// the RM's timeout engine (_checkTimeout, gpu_timeout.c) starts every check
// at status = NV_OK and only ever returns NV_ERR_TIMEOUT from inside a branch
// gated on one of the timer-source flags (OSTIMER / OSDELAY / TMR). With flags
// = 0 the timeout is structurally disabled -- _checkTimeout returns NV_OK
// forever, so any gpuTimeoutCondWait whose condition never comes true (e.g.
// kgspExecuteSequencerCommand_TU102 polling BSI_SECURE_SCRATCH_14 for the SEC2
// GSP-RM resume handoff) spins the CPU with interrupts off and hard-hangs the
// whole box instead of timing out. The real Linux osGetTimeoutParams
// (arch/nvalloc/unix/src/os.c) returns GPU_TIMEOUT_FLAGS_OSTIMER, which routes
// the check to osGetCurrentTick (which Eclipse now backs with a real TSC
// clock), so this matches it.
const GPU_TIMEOUT_FLAGS_OSTIMER: NvU32 = 1 << 3;

#[no_mangle]
pub extern "C" fn osGetTimeoutParams(
    _gpu: *mut c_void,
    time_out_us: *mut NvU32,
    scale: *mut NvU32,
    flags: *mut NvU32,
) {
    unsafe {
        if !time_out_us.is_null() {
            // Real Linux graphics-mode default (os.c: 4 * 1000000). Long enough
            // not to trip on a healthy multi-second GSP bootstrap, short enough
            // that a genuinely stuck poll reports a clean NV_ERR_TIMEOUT.
            *time_out_us = 4_000_000;
        }
        if !scale.is_null() {
            *scale = 1;
        }
        if !flags.is_null() {
            *flags = GPU_TIMEOUT_FLAGS_OSTIMER;
        }
    }
}

#[no_mangle]
pub extern "C" fn osSchedule() -> NV_STATUS {
    // Yield-only: drain this CPU's TLB-shootdown queue without sleeping
    // a millisecond. Safe between CE completion polls when the copy is
    // already done and `osDelay(1)` would just waste frame time.
    lock::pump();
    NV_OK
}

#[no_mangle]
pub extern "C" fn osGetSystemTime(sec: *mut NvU32, usec: *mut NvU32) -> NV_STATUS {
    let ns = crate::hooks::clock_ns();
    unsafe {
        if !sec.is_null() {
            *sec = (ns / 1_000_000_000) as NvU32;
        }
        if !usec.is_null() {
            *usec = ((ns / 1_000) % 1_000_000) as NvU32;
        }
    }
    NV_OK
}

#[cfg(test)]
mod os_services_tests {
    use super::*;
    use crate::hooks::{swap_hooks, test_turnstile, KernelHooks, TICK_RESOLUTION_NS};
    use core::sync::atomic::{AtomicU64, Ordering};
    extern crate std;
    use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};

    /// A kernel that answers a clock we set and counts the microseconds it was
    /// asked to wait, so a test can tell "the delay reached the kernel" from
    /// "nobody waited".
    struct Fake;

    static NOW_NS: AtomicU64 = AtomicU64::new(0);
    static DELAYED_US: AtomicU64 = AtomicU64::new(0);

    impl KernelHooks for Fake {
        fn pci_config_read(&self, _h: usize, _o: u32, _l: u32) -> u32 {
            0
        }
        fn pci_config_write(&self, _h: usize, _o: u32, _l: u32, _v: u32) {}
        fn map_kernel_space(&self, _p: u64, _s: u64) -> u64 {
            0
        }
        fn unmap_kernel_space(&self, _v: u64, _s: u64) {}
        fn io_read(&self, _p: u32, _l: u32) -> u32 {
            0
        }
        fn io_write(&self, _p: u32, _l: u32, _v: u32) {}
        fn monotonic_time_ns(&self) -> u64 {
            NOW_NS.load(Ordering::SeqCst)
        }
        fn delay_us(&self, us: u32) {
            DELAYED_US.fetch_add(us as u64, Ordering::SeqCst);
        }
    }

    static FAKE: Fake = Fake;

    /// The hook slot is a process-global and `cargo test` runs in parallel, so
    /// one test at a time through the module's own turnstile, and put back
    /// whatever was installed.
    fn with_kernel<R>(now_ns: u64, body: impl FnOnce() -> R) -> R {
        let _guard = test_turnstile();
        let previous = swap_hooks(Some(&FAKE));
        NOW_NS.store(now_ns, Ordering::SeqCst);
        DELAYED_US.store(0, Ordering::SeqCst);
        let out = catch_unwind(AssertUnwindSafe(body));
        swap_hooks(previous);
        match out {
            Ok(v) => v,
            Err(p) => resume_unwind(p),
        }
    }

    fn with_no_kernel<R>(body: impl FnOnce() -> R) -> R {
        let _guard = test_turnstile();
        let previous = swap_hooks(None);
        let out = catch_unwind(AssertUnwindSafe(body));
        swap_hooks(previous);
        match out {
            Ok(v) => v,
            Err(p) => resume_unwind(p),
        }
    }

    // -----------------------------------------------------------------
    // osGetTimeoutParams. This is the one that hangs the machine.
    // -----------------------------------------------------------------

    /// GPU_TIMEOUT_FLAGS_OSTIMER must be set. `_checkTimeout` (gpu_timeout.c)
    /// only ever returns NV_ERR_TIMEOUT from inside a branch gated on a timer
    /// source flag, so with flags = 0 every `gpuTimeoutCondWait` whose
    /// condition never comes true spins with interrupts off instead of timing
    /// out -- a hard hang of the whole box.
    #[test]
    fn the_timeout_params_name_a_timer_source() {
        let mut time_out_us: NvU32 = 0xDEAD_BEEF;
        let mut scale: NvU32 = 0xDEAD_BEEF;
        let mut flags: NvU32 = 0xDEAD_BEEF;
        osGetTimeoutParams(
            core::ptr::null_mut(),
            &mut time_out_us,
            &mut scale,
            &mut flags,
        );
        assert_ne!(
            flags & GPU_TIMEOUT_FLAGS_OSTIMER,
            0,
            "no timer-source flag: the RM's timeout engine is structurally disabled, \
             so a stuck poll hangs the box instead of reporting NV_ERR_TIMEOUT"
        );
        // Real Linux graphics-mode default (os.c: 4 * 1000000).
        assert_eq!(time_out_us, 4_000_000);
        // The scale multiplies the timeout; 0 would make every deadline
        // already past and time out instantly.
        assert_eq!(scale, 1);
    }

    /// The RM calls it with any of the three out-pointers null (it asks only
    /// for what it needs); a write through a null one faults inside the RM.
    #[test]
    fn the_timeout_params_tolerate_each_pointer_being_null() {
        let mut only: NvU32 = 0;
        osGetTimeoutParams(
            core::ptr::null_mut(),
            &mut only,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        );
        assert_eq!(only, 4_000_000);
        only = 0;
        osGetTimeoutParams(
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            &mut only,
            core::ptr::null_mut(),
        );
        assert_eq!(only, 1);
        only = 0;
        osGetTimeoutParams(
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            &mut only,
        );
        assert_eq!(only & GPU_TIMEOUT_FLAGS_OSTIMER, GPU_TIMEOUT_FLAGS_OSTIMER);
        // All three null must not fault either.
        osGetTimeoutParams(
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        );
    }

    // -----------------------------------------------------------------
    // The clock the timeout engine is built out of.
    // -----------------------------------------------------------------

    /// `timeoutSet` pads the deadline out by one tick resolution so a timeout
    /// started near the end of a tick does not fire early. A resolution of 0
    /// pads by nothing, which is the early fire it exists to prevent, and this
    /// crate must report the granularity its own clock actually has.
    #[test]
    fn the_tick_resolution_is_the_one_the_clock_advances_by() {
        assert_ne!(osGetMonotonicTickResolutionNs(), 0);
        assert_eq!(osGetMonotonicTickResolutionNs(), TICK_RESOLUTION_NS);
    }

    /// With a kernel behind it the clock is the kernel's, not a constant of
    /// this crate's own.
    #[test]
    fn the_monotonic_clock_is_the_kernels() {
        with_kernel(777_000_000_000, || {
            assert_eq!(osGetMonotonicTimeNs(), 777_000_000_000);
        });
    }

    /// And with no kernel registered yet it still has to move: `_checkTimeout`
    /// compares against an absolute deadline, so a clock that answers the same
    /// number twice never expires.
    #[test]
    fn the_monotonic_clock_moves_with_no_kernel_behind_it() {
        with_no_kernel(|| {
            let first = osGetMonotonicTimeNs();
            let second = osGetMonotonicTimeNs();
            assert!(
                first < second,
                "a clock that answers {} twice is not a clock",
                first
            );
        });
    }

    /// `osGetSystemTime` splits the one nanosecond clock into whole seconds
    /// and the microseconds *within* that second: a usec field carrying the
    /// whole clock, or seconds rounded up, is a wall clock that disagrees with
    /// itself.
    #[test]
    fn the_system_time_splits_the_clock_into_seconds_and_microseconds() {
        with_kernel(1_234_000_567_891, || {
            let mut sec: NvU32 = 0xDEAD_BEEF;
            let mut usec: NvU32 = 0xDEAD_BEEF;
            assert_eq!(osGetSystemTime(&mut sec, &mut usec), NV_OK);
            assert_eq!(sec, 1_234);
            assert_eq!(usec, 567);
            assert!(usec < 1_000_000, "the microseconds must be within a second");
        });
        // A clock just short of the next second must not round its seconds up.
        with_kernel(1_999_999_999, || {
            let mut sec: NvU32 = 0;
            let mut usec: NvU32 = 0;
            assert_eq!(osGetSystemTime(&mut sec, &mut usec), NV_OK);
            assert_eq!(sec, 1);
            assert_eq!(usec, 999_999);
        });
    }

    /// Either out-pointer may be null.
    #[test]
    fn the_system_time_tolerates_each_pointer_being_null() {
        with_kernel(5_000_000_000, || {
            let mut sec: NvU32 = 0;
            assert_eq!(osGetSystemTime(&mut sec, core::ptr::null_mut()), NV_OK);
            assert_eq!(sec, 5);
            let mut usec: NvU32 = 0xDEAD_BEEF;
            assert_eq!(osGetSystemTime(core::ptr::null_mut(), &mut usec), NV_OK);
            assert_eq!(usec, 0);
            assert_eq!(
                osGetSystemTime(core::ptr::null_mut(), core::ptr::null_mut()),
                NV_OK
            );
        });
    }

    // -----------------------------------------------------------------
    // Waiting and yielding.
    // -----------------------------------------------------------------

    /// A microsecond wait has to reach the kernel's own wait, not be rounded
    /// to a millisecond tick: this is the primitive the CE-completion poll
    /// uses instead of `osDelay(1)`.
    #[test]
    fn a_microsecond_wait_reaches_the_kernel_unrounded() {
        with_kernel(0, || {
            assert_eq!(osDelayUs(10), NV_OK);
            assert_eq!(DELAYED_US.load(Ordering::SeqCst), 10);
            assert_eq!(osDelayUs(1), NV_OK);
            assert_eq!(DELAYED_US.load(Ordering::SeqCst), 11);
        });
    }

    /// `osDelayUs(0)` is the yield-only call: it must not hand a zero wait to
    /// the kernel (a kernel delay of 0 can still burn a tick).
    #[test]
    fn a_zero_microsecond_wait_is_a_yield_and_waits_on_nobody() {
        with_kernel(0, || {
            assert_eq!(osDelayUs(0), NV_OK);
            assert_eq!(DELAYED_US.load(Ordering::SeqCst), 0);
        });
    }

    /// With no kernel behind it the wait cannot actually happen, but it must
    /// still charge the time asked for to the fallback clock -- otherwise a
    /// caller polling against its own deadline loops against a frozen clock.
    #[test]
    fn a_wait_with_no_kernel_behind_it_still_moves_the_clock() {
        with_no_kernel(|| {
            let before = osGetMonotonicTimeNs();
            assert_eq!(osDelayUs(1_000), NV_OK);
            let after = osGetMonotonicTimeNs();
            assert!(
                after - before >= 1_000 * 1_000,
                "a 1000 us wait moved the clock only {} ns",
                after - before
            );
        });
    }

    /// Yield-only, and it says so.
    #[test]
    fn scheduling_away_succeeds() {
        assert_eq!(osSchedule(), NV_OK);
    }

    // -----------------------------------------------------------------
    // Thread and process identity.
    // -----------------------------------------------------------------

    /// The RM passes a handle to fill in; a null one is a caller bug and must
    /// come back as such rather than fault.
    #[test]
    fn the_current_thread_refuses_a_null_handle() {
        assert_eq!(
            osGetCurrentThread(core::ptr::null_mut()),
            NV_ERR_INVALID_ARGUMENT
        );
    }

    /// And with somewhere to write it answers, leaving a defined value behind
    /// (the RM compares the handle it gets with the one it stored).
    #[test]
    fn the_current_thread_answers_a_defined_handle() {
        let mut handle: NvU64 = 0xDEAD_BEEF_DEAD_BEEF;
        assert_eq!(osGetCurrentThread(&mut handle), NV_OK);
        assert_eq!(handle, 0);
    }

    /// Eclipse runs the RM on one processor and with no process flags; both
    /// answers index into RM tables, so they have to be in range, not garbage.
    #[test]
    fn the_processor_number_and_process_flags_are_in_range() {
        assert_eq!(osGetCurrentProcessorNumber(), 0);
        assert_eq!(osGetCurrentProcessFlags(), 0);
    }
}
