//! Registration point for kernel-specific operations the vendored NVIDIA RM
//! core needs that this crate cannot implement on its own: PCI config space
//! access, MMIO mapping, legacy I/O port access, and time. `nvidia-rm-sys`
//! must not depend back on `drivers` (which depends on it), so `drivers`
//! registers real implementations via `register_hooks()` during GPU driver
//! init instead. Until registered, every hook answers with a safe default so
//! an unwired build still links and runs (as the smoke test already does).
//!
//! "Safe default" is not the same thing for every hook. A PCI config read
//! answers all ones, because that is what an absent device reads as on the
//! bus. A clock is the opposite case: there is no value a clock can return
//! twice and still be a clock, which is why the time hook has a fallback that
//! *advances* rather than a constant -- see [`clock_ns`].
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
// The crate is `no_std`; the turnstile below and the tests need the host's
// `Mutex` and `catch_unwind`, which only exist in a test build.
#[cfg(test)]
extern crate std;

pub trait KernelHooks: Sync {
    /// Read `len` (1/2/4) bytes at PCI config offset `offset` for the
    /// device identified by `pci_handle` (whatever `pci_init_handle`
    /// returned, passed back opaquely -- see os_interface::os_pci_init_handle).
    fn pci_config_read(&self, pci_handle: usize, offset: u32, len: u32) -> u32;
    fn pci_config_write(&self, pci_handle: usize, offset: u32, len: u32, value: u32);
    /// Map `size` bytes of physical memory starting at `phys` into kernel
    /// address space, returning the virtual address (0 on failure).
    fn map_kernel_space(&self, phys: u64, size: u64) -> u64;
    fn unmap_kernel_space(&self, virt: u64, size: u64);
    fn io_read(&self, port: u32, len: u32) -> u32;
    fn io_write(&self, port: u32, len: u32, value: u32);
    /// Monotonic time since some fixed point, in nanoseconds.
    fn monotonic_time_ns(&self) -> u64;
    /// Busy-wait for approximately `us` microseconds.
    fn delay_us(&self, us: u32);
}

/// The registered hooks, held as a **thin** pointer to the (fat) trait-object
/// reference, so reading them is one atomic load.
///
/// This was a `lock::Mutex<Option<&'static dyn KernelHooks>>`, and
/// `with_hooks` took that lock on **every** callback: every PCI config read,
/// every MMIO map, every port access, every clock read and every delay the RM
/// asks for. `lock::Mutex` is a spinlock that **disables interrupts** for as
/// long as it is held, so the RM's hottest paths -- the register polls it runs
/// hundreds of thousands of times per GSP boot -- each opened and closed an
/// interrupts-off critical section, to read a pointer that `register_hooks`
/// writes once at init and nothing ever clears. An `Acquire` load answers the
/// same question and touches neither the interrupt flag nor a lock.
static HOOKS: AtomicPtr<&'static dyn KernelHooks> = AtomicPtr::new(core::ptr::null_mut());

/// Box a trait-object reference and hand back the thin pointer to it.
///
/// One fat pointer's worth of heap, leaked on purpose: the hooks themselves
/// are already `&'static`, so there is nothing here whose lifetime could end.
fn leak_thin(hooks: &'static dyn KernelHooks) -> *mut &'static dyn KernelHooks {
    let slot: &'static &'static dyn KernelHooks =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(hooks));
    slot as *const &'static dyn KernelHooks as *mut &'static dyn KernelHooks
}

/// Read back whatever [`leak_thin`] stored, or `None` for the null slot.
fn read_thin(p: *mut &'static dyn KernelHooks) -> Option<&'static dyn KernelHooks> {
    if p.is_null() {
        return None;
    }
    // `p` is either null or a leaked `&'static &'static dyn KernelHooks`
    // published with `Release`, so the pointee outlives the program.
    Some(*unsafe { &*p })
}

/// Called once by `drivers` during NVIDIA GPU init with real
/// implementations backed by Eclipse's PCI/MMIO/timer primitives.
pub fn register_hooks(hooks: &'static dyn KernelHooks) {
    HOOKS.store(leak_thin(hooks), Ordering::Release);
}

/// Test-only: put `hooks` in the slot and hand back whatever was there.
///
/// `HOOKS` is a process-global that `drivers` fills once during GPU init and
/// nothing ever clears, so a test that installs a fake has to put the previous
/// value back or it leaks into every test that runs after it. This lives here
/// rather than in the tests because `HOOKS` is private and stays private.
#[cfg(test)]
pub(crate) fn swap_hooks(
    hooks: Option<&'static dyn KernelHooks>,
) -> Option<&'static dyn KernelHooks> {
    let next = match hooks {
        Some(h) => leak_thin(h),
        None => core::ptr::null_mut(),
    };
    read_thin(HOOKS.swap(next, Ordering::AcqRel))
}

/// The registered hooks, or `None` in a build that never registered any.
pub(crate) fn hooks() -> Option<&'static dyn KernelHooks> {
    read_thin(HOOKS.load(Ordering::Acquire))
}

/// Run `f` against the registered hooks, or answer `default` if there are
/// none. `default` is eager: a caller whose fallback has to be computed (the
/// clock) reads [`hooks`] directly instead.
pub(crate) fn with_hooks<R>(default: R, f: impl FnOnce(&dyn KernelHooks) -> R) -> R {
    match hooks() {
        Some(h) => f(h),
        None => default,
    }
}

/// The granularity both `osGetMonotonicTickResolutionNs` and
/// `osGetTickResolution` report to the RM, and therefore the step
/// [`clock_ns`]'s fallback advances by: a finer one would be a granularity the
/// clock does not have, and `gpu_timeout.c` pads every deadline with it.
pub(crate) const TICK_RESOLUTION_NS: u64 = 1_000;

/// The clock used only while no timer hook is registered. See [`clock_ns`].
static FALLBACK_NS: AtomicU64 = AtomicU64::new(0);

/// The one monotonic nanosecond clock this crate reads, and the only thing
/// standing between an unwired build and a hang of the whole machine.
///
/// The RM's timeout engine is built entirely out of this clock.
/// `gpu_timeout.c`'s `timeoutSet` stores an **absolute** deadline:
///
/// ```text
///     timeInNs = osGetMonotonicTimeNs();
///     pTimeout->timeout = timeInNs + timeoutNs;
/// ```
///
/// and `_checkTimeout` is the only thing that ever returns `NV_ERR_TIMEOUT`:
///
/// ```text
///     timeInNs = osGetMonotonicTimeNs();
///     if (timeInNs >= pTimeout->timeout) status = NV_ERR_TIMEOUT;
/// ```
///
/// A clock that answers the same number twice makes that comparison false
/// **forever**: every `gpuTimeoutCondWait` whose condition never comes true
/// then spins with interrupts off and hard-hangs the box instead of reporting
/// a clean timeout. That is the exact hang `osGetTimeoutParams` sets
/// `GPU_TIMEOUT_FLAGS_OSTIMER` to avoid, and the exact thing
/// `osGetCurrentTick`'s own comment forbids -- "a constant would make lock
/// acquisition either time out instantly or never". Every reader of the clock
/// nevertheless spelled its fallback `with_hooks(0, |h| h.monotonic_time_ns())`,
/// nine separate times, so an unwired build handed the RM a constant 0.
///
/// The fallback cannot be a real clock -- there is no timer to read -- but it
/// can advance, and advancing is the whole of what the RM needs from it. So
/// reading it costs one tick, and [`delay_us_or_spin`] charges it for a wait
/// it could not actually perform. A four-second RM timeout then expires after
/// a bounded number of polls instead of never.
pub(crate) fn clock_ns() -> u64 {
    match hooks() {
        Some(h) => h.monotonic_time_ns(),
        None => FALLBACK_NS.fetch_add(TICK_RESOLUTION_NS, Ordering::Relaxed) + TICK_RESOLUTION_NS,
    }
}

/// Test-only: the one turnstile for everything in this module's statics.
///
/// `HOOKS` and `FALLBACK_NS` are process-globals, and `cargo test` runs test
/// functions in parallel (the `Unit Test` job is a bare `cargo test
/// --no-fail-fast`, with no `--test-threads=1`). Any test in any module that
/// installs a fake hook has to hold THIS lock, not a turnstile of its own, or
/// two modules' tests will trade the slot out from under each other. It lives
/// here because the statics do.
#[cfg(test)]
pub(crate) fn test_turnstile() -> std::sync::MutexGuard<'static, ()> {
    static TURNSTILE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A test that panicked while holding it poisoned it; the next test still
    // needs the slot, and the save/restore below puts the globals right.
    TURNSTILE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Test-only: swap the fallback clock, returning what it held.
///
/// `FALLBACK_NS` is the second process-global behind [`test_turnstile`], and it
/// is private to this module, so a test harness in another module had no way to
/// put it back -- any test that let the fallback clock advance leaked that into
/// whichever test ran next. This is how such a harness saves, zeroes and
/// restores it, the same way `hook_tests`'s own does.
#[cfg(test)]
pub(crate) fn test_swap_fallback_ns(value: u64) -> u64 {
    FALLBACK_NS.swap(value, Ordering::SeqCst)
}

/// Busy-wait `us` microseconds, pumping TLB shootdowns.
///
/// Prefer this (via `osDelayUs` / `osDelayNs`) over `osDelay(1)` for
/// CE-completion polls: a finished copy otherwise still burns a full
/// millisecond tick. `us == 0` is a yield-only call (`osSchedule`).
pub(crate) fn delay_us_or_spin(us: u32) {
    if us == 0 {
        lock::pump();
        return;
    }
    if let Some(h) = hooks() {
        h.delay_us(us);
        return;
    }
    // No timer hook (early boot, or a build that never registers one): we
    // cannot actually wait, so at least do not also claim that no time passed.
    // Charging the requested wait to the fallback clock is what lets a caller
    // polling against its own deadline reach it, instead of looping against a
    // frozen clock -- see `clock_ns`. Then still yield, so a caller spinning on
    // CE progress cannot starve TLB shootdowns.
    FALLBACK_NS.fetch_add(us as u64 * 1_000, Ordering::Relaxed);
    lock::pump();
}

#[cfg(test)]
mod hook_tests {
    use super::*;
    use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};

    /// A hook set that answers a fixed clock and counts the waits it was asked
    /// to perform, so a test can tell "the kernel did it" from "nobody did".
    struct Fake;

    static NOW_NS: AtomicU64 = AtomicU64::new(0);
    static DELAYED_US: AtomicU64 = AtomicU64::new(0);
    /// Set by `reentrant_read`: what the clock read from *inside* a hook.
    static SEEN_FROM_INSIDE: AtomicU64 = AtomicU64::new(0);
    static REENTRANT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

    impl KernelHooks for Fake {
        fn pci_config_read(&self, _h: usize, _o: u32, _l: u32) -> u32 {
            0x5A5A_5A5A
        }
        fn pci_config_write(&self, _h: usize, _o: u32, _l: u32, _v: u32) {}
        fn map_kernel_space(&self, _p: u64, _s: u64) -> u64 {
            0x1000
        }
        fn unmap_kernel_space(&self, _v: u64, _s: u64) {}
        fn io_read(&self, _p: u32, _l: u32) -> u32 {
            0x5A
        }
        fn io_write(&self, _p: u32, _l: u32, _v: u32) {}
        fn monotonic_time_ns(&self) -> u64 {
            NOW_NS.load(Ordering::SeqCst)
        }
        fn delay_us(&self, us: u32) {
            DELAYED_US.fetch_add(us as u64, Ordering::SeqCst);
            // Read the hooks again from inside a hook. With the slot behind a
            // mutex held across the callback this is a self-deadlock; behind an
            // atomic load it is just a load.
            if REENTRANT.load(Ordering::SeqCst) {
                // Deliberately back through `with_hooks`, not `hooks()`: that
                // is the call `os_get_cpu_frequency` makes, and the one a
                // guard held across the callback would deadlock against.
                let inner = with_hooks(0u64, |h| h.monotonic_time_ns());
                SEEN_FROM_INSIDE.store(inner, Ordering::SeqCst);
            }
        }
    }

    static FAKE: Fake = Fake;

    /// `HOOKS` and the fallback clock are process-globals, so one test at a
    /// time, and put back what was there.
    fn with_globals<R>(installed: Option<&'static dyn KernelHooks>, body: impl FnOnce() -> R) -> R {
        let _guard = test_turnstile();
        let previous = swap_hooks(installed);
        let saved_fallback = FALLBACK_NS.swap(0, Ordering::SeqCst);
        NOW_NS.store(0, Ordering::SeqCst);
        DELAYED_US.store(0, Ordering::SeqCst);
        SEEN_FROM_INSIDE.store(0, Ordering::SeqCst);
        REENTRANT.store(false, Ordering::SeqCst);
        let out = catch_unwind(AssertUnwindSafe(body));
        REENTRANT.store(false, Ordering::SeqCst);
        FALLBACK_NS.store(saved_fallback, Ordering::SeqCst);
        swap_hooks(previous);
        match out {
            Ok(v) => v,
            Err(p) => resume_unwind(p),
        }
    }

    fn with_kernel<R>(body: impl FnOnce() -> R) -> R {
        with_globals(Some(&FAKE), body)
    }
    fn with_no_kernel<R>(body: impl FnOnce() -> R) -> R {
        with_globals(None, body)
    }

    // -----------------------------------------------------------------
    // The clock. This is the one that hangs the machine.
    // -----------------------------------------------------------------

    /// The whole point: with no timer hook the clock used to answer 0 every
    /// time, and a clock that answers the same number twice is not a clock.
    #[test]
    fn a_clock_with_no_kernel_behind_it_still_moves() {
        with_no_kernel(|| {
            let first = clock_ns();
            let second = clock_ns();
            let third = clock_ns();
            assert!(
                first < second && second < third,
                "the fallback clock must advance, got {}, {}, {}",
                first,
                second,
                third
            );
        });
    }

    /// `gpu_timeout.c` exactly: `timeoutSet` stores `now + timeout` and
    /// `_checkTimeout` fires when `now >= that`. Against a frozen clock the
    /// comparison is false forever and `gpuTimeoutCondWait` spins with
    /// interrupts off until someone power-cycles the box. It must expire.
    #[test]
    fn an_rm_timeout_expires_even_with_no_kernel_behind_the_clock() {
        with_no_kernel(|| {
            // osGetTimeoutParams' real default, padded the way timeoutSet pads
            // it: 4 s in ns plus one tick resolution.
            let deadline = clock_ns() + 4_000_000 * 1_000 + TICK_RESOLUTION_NS;
            // A poll loop asking for 10 us a turn, which is what
            // `eclipse_ce_wait_bounded` is told to do.
            let mut polls: u64 = 0;
            while clock_ns() < deadline {
                delay_us_or_spin(10);
                polls += 1;
                assert!(polls < 10_000_000, "the timeout never expired");
            }
            assert!(polls > 0, "the deadline was already past before polling");
        });
    }

    /// The same deadline with no delay hook *and* no delay call: a caller that
    /// only re-reads the clock. Nothing but the step [`clock_ns`] takes moves
    /// it, so this is what says that step is big enough to matter -- shrink
    /// `TICK_RESOLUTION_NS` to a nanosecond and a four-second deadline needs
    /// four billion reads, which is a hang wearing a different hat.
    #[test]
    fn a_poll_that_only_reads_the_clock_still_reaches_its_deadline() {
        with_no_kernel(|| {
            let deadline = clock_ns() + 4_000_000 * 1_000 + TICK_RESOLUTION_NS;
            let mut reads: u64 = 0;
            while clock_ns() < deadline {
                reads += 1;
                assert!(
                    reads < 8_000_000,
                    "the deadline was still {} ns away after {} reads",
                    deadline - clock_ns(),
                    reads
                );
            }
        });
    }

    /// The step the fallback clock takes is also the resolution this crate
    /// reports to `gpu_timeout.c`, which pads every deadline with it. Three
    /// spellings answer that question -- #1527 already found two of them
    /// disagreeing by a factor of a thousand -- so they come off one constant
    /// now, and this is what holds them there.
    #[test]
    fn the_step_the_clock_takes_is_the_resolution_the_rm_is_told() {
        assert_eq!(
            crate::os_services::osGetMonotonicTickResolutionNs(),
            TICK_RESOLUTION_NS
        );
        assert_eq!(
            crate::os_interface::os_get_monotonic_tick_resolution_ns(),
            TICK_RESOLUTION_NS
        );
        assert_eq!(
            crate::os_boundary::osGetTickResolution(),
            TICK_RESOLUTION_NS
        );
    }

    /// ...and the fallback must not leak into a build that has a real timer:
    /// there, two reads of a clock that has not moved are equal.
    #[test]
    fn the_clock_answers_from_the_kernel_when_there_is_one() {
        with_kernel(|| {
            NOW_NS.store(7_000_000, Ordering::SeqCst);
            assert_eq!(clock_ns(), 7_000_000);
            assert_eq!(clock_ns(), 7_000_000, "and does not drift on its own");
            NOW_NS.store(9_000_000, Ordering::SeqCst);
            assert_eq!(clock_ns(), 9_000_000, "and follows the kernel's clock");
        });
    }

    /// Nine call sites each spelled the clock's fallback themselves, which is
    /// how they could disagree. They read one clock now, so they agree.
    #[test]
    fn every_spelling_of_the_clock_reads_the_same_one() {
        with_kernel(|| {
            NOW_NS.store(4_200_000_042, Ordering::SeqCst);
            assert_eq!(
                crate::os_interface::os_get_monotonic_time_ns(),
                4_200_000_042
            );
            assert_eq!(
                crate::os_interface::os_get_monotonic_time_ns_hr(),
                4_200_000_042
            );
            assert_eq!(crate::os_services::osGetMonotonicTimeNs(), 4_200_000_042);
            let mut tick = 0u64;
            crate::os_boundary::osGetCurrentTick(&mut tick);
            assert_eq!(tick, 4_200_000_042);
            // The microsecond spelling is the same clock divided, not another.
            assert_eq!(crate::os_boundary::osGetTimestamp(), 4_200_000);
        });
    }

    // -----------------------------------------------------------------
    // Waiting, when there is nothing to wait with.
    // -----------------------------------------------------------------

    /// With no timer hook the wait cannot happen. It must not then also report
    /// that no time passed, or the caller's own deadline never arrives.
    #[test]
    fn a_wait_that_could_not_happen_still_charges_the_clock() {
        with_no_kernel(|| {
            let before = clock_ns();
            delay_us_or_spin(1_000);
            let after = clock_ns();
            assert!(
                after - before >= 1_000 * 1_000,
                "asked to wait 1000 us, clock moved {} ns",
                after - before
            );
        });
    }

    /// And when the kernel really did wait, the fallback stays out of it.
    #[test]
    fn a_wait_the_kernel_performed_does_not_charge_the_fallback() {
        with_kernel(|| {
            let fallback_before = FALLBACK_NS.load(Ordering::SeqCst);
            delay_us_or_spin(1_000);
            assert_eq!(
                DELAYED_US.load(Ordering::SeqCst),
                1_000,
                "the kernel waited"
            );
            assert_eq!(
                FALLBACK_NS.load(Ordering::SeqCst),
                fallback_before,
                "and nothing was charged to the fallback clock"
            );
        });
    }

    /// `osDelayUs(0)` / `osSchedule` is a yield, not a wait, so it buys no time.
    #[test]
    fn a_yield_is_not_a_wait_and_buys_no_time() {
        with_no_kernel(|| {
            let before = FALLBACK_NS.load(Ordering::SeqCst);
            delay_us_or_spin(0);
            assert_eq!(FALLBACK_NS.load(Ordering::SeqCst), before);
        });
    }

    // -----------------------------------------------------------------
    // The slot itself.
    // -----------------------------------------------------------------

    /// What a build registers is what it reads back.
    #[test]
    fn the_hooks_a_build_registers_are_the_ones_it_reads_back() {
        with_kernel(|| {
            let h = hooks().expect("registered");
            assert_eq!(h.pci_config_read(0, 0, 4), 0x5A5A_5A5A);
            assert_eq!(h.io_read(0, 1), 0x5A);
        });
    }

    /// `drivers` calls `register_hooks`; `swap_hooks` is only for tests. A test
    /// that exercises the test-only setter does not exercise what the driver
    /// does, so this one goes through the real call. (Third time that rule has
    /// caught a weak test in this crate.)
    #[test]
    fn the_call_the_driver_actually_makes_installs_the_hooks() {
        let _guard = test_turnstile();
        let previous = swap_hooks(None);
        assert!(hooks().is_none(), "the slot starts empty");
        register_hooks(&FAKE);
        let installed = hooks().expect("register_hooks installed them");
        assert_eq!(installed.pci_config_read(0, 0, 4), 0x5A5A_5A5A);
        swap_hooks(previous);
    }

    /// And a build that never registered any says so, rather than handing out
    /// a hook that is not there.
    #[test]
    fn a_build_with_no_kernel_reads_no_hooks() {
        with_no_kernel(|| {
            assert!(hooks().is_none());
        });
    }

    /// `with_hooks`' default is for the unwired case only.
    #[test]
    fn a_default_answers_only_when_there_is_no_kernel() {
        with_no_kernel(|| {
            assert_eq!(
                with_hooks(0xFFFF_FFFFu32, |h| h.pci_config_read(0, 0, 4)),
                0xFFFF_FFFF
            );
        });
        with_kernel(|| {
            assert_eq!(
                with_hooks(0xFFFF_FFFFu32, |h| h.pci_config_read(0, 0, 4)),
                0x5A5A_5A5A
            );
        });
    }

    /// Every other test's save/restore rests on this, so it gets its own.
    #[test]
    fn putting_the_previous_hooks_back_really_puts_them_back() {
        let _guard = test_turnstile();
        let outermost = swap_hooks(Some(&FAKE));
        assert!(hooks().is_some());
        let one = swap_hooks(None);
        assert!(one.is_some(), "the swap hands back what was there");
        assert!(hooks().is_none());
        swap_hooks(one);
        assert!(hooks().is_some(), "and putting it back restores it");
        swap_hooks(outermost);
    }

    /// Reading the hooks from inside a hook, through `with_hooks` both times --
    /// which is the shape `os_get_cpu_frequency` already has, since it
    /// calibrates with `with_hooks((), |h| h.delay_us(10_000))`.
    ///
    /// The slot used to be a `lock::Mutex`, and matching on `*HOOKS.lock()`
    /// directly -- the obvious way to write it -- holds the guard across the
    /// callback and self-deadlocks solid here, with interrupts off. The old
    /// code dodged that by copying the value out and dropping the guard first,
    /// and said in a comment that the lock gave no warning if a caller ever
    /// nested. An atomic load cannot be nested wrongly at all.
    #[test]
    fn reading_the_hooks_from_inside_a_hook_is_not_a_deadlock() {
        with_kernel(|| {
            NOW_NS.store(1_234_000, Ordering::SeqCst);
            REENTRANT.store(true, Ordering::SeqCst);
            with_hooks((), |h| h.delay_us(5));
            assert_eq!(
                SEEN_FROM_INSIDE.load(Ordering::SeqCst),
                1_234_000,
                "the inner read saw the same clock"
            );
        });
    }
}
