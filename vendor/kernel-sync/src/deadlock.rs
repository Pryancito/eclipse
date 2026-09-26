//! Spinlock deadlock self-report, shared by every mutex flavor in this crate.
//!
//! A spinlock that spins "forever" (billions of PAUSEs, i.e. many seconds) is
//! a deadlock on an IRQ-off kernel: the machine freezes with no panic and no
//! console output — indistinguishable from a hard hang on a monitor-only box.
//! When a waiter crosses the threshold it calls the installed hook ONCE with
//! its `#[track_caller]` location, so the kernel can paint the stuck call site
//! somewhere lock-free (e.g. straight onto the framebuffer). The hook MUST NOT
//! take locks or allocate.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

static DEADLOCK_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Default threshold: ~8s of PAUSE iterations on current hardware. Normal
/// contention is orders of magnitude below this; only a genuine
/// deadlock/livelock crosses it.
pub const DEADLOCK_SPINS_DEFAULT: u64 = 1_000_000_000;

/// Live threshold, overridable at boot via [`set_deadlock_spins`].
///
/// A fixed count calibrated for real hardware is unreachable under emulation: a
/// billion spin iterations that take eight seconds natively take many minutes
/// under QEMU/TCG, so on an emulated run the detector effectively never fires
/// and a genuine deadlock is indistinguishable from a slow one. That cost a
/// real debugging cycle — a hang was read as "not a deadlock, no banner
/// appeared" when in truth the threshold had simply not been reached (and the
/// banner was framebuffer-only besides). Lower it with `DEADLOCKSPINS=<n>` on
/// the kernel command line when running emulated.
static DEADLOCK_SPINS_LIVE: AtomicU64 = AtomicU64::new(DEADLOCK_SPINS_DEFAULT);

/// Override the deadlock spin threshold. `0` restores the default.
pub fn set_deadlock_spins(spins: u64) {
    DEADLOCK_SPINS_LIVE.store(
        if spins == 0 {
            DEADLOCK_SPINS_DEFAULT
        } else {
            spins
        },
        Ordering::Relaxed,
    );
}

/// The current threshold. Read on the spin path, so kept to one relaxed load.
#[inline(always)]
pub(crate) fn deadlock_spins() -> u64 {
    DEADLOCK_SPINS_LIVE.load(Ordering::Relaxed)
}

/// Install the deadlock self-report hook (`file`, `line` of the stuck caller).
pub fn set_deadlock_hook(f: fn(&'static str, u32)) {
    DEADLOCK_HOOK.store(f as usize, Ordering::SeqCst);
}

#[inline(never)]
pub(crate) fn report_deadlock(file: &'static str, line: u32) {
    // Judged before the jump, like every other hook slot in the kernel: see
    // `fn_slot`. A banner that jumps into smash residue replaces a diagnosable
    // deadlock with an undiagnosable triple fault.
    let Some(h) = crate::fn_slot::live_fn(DEADLOCK_HOOK.load(Ordering::Relaxed)) else {
        return;
    };
    let f: fn(&'static str, u32) = unsafe { core::mem::transmute(h) };
    f(file, line);
}

/// Public variant for OTHER crates' spinlocks (e.g. the scheduler's
/// `spin::Mutex`-based runtime locks) to self-report through the same hook.
pub fn report_stuck(file: &'static str, line: u32) {
    report_deadlock(file, line);
}

static DEADLOCK_HOLDER_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Hook run periodically from inside spin loops, IRQs still disabled.
///
/// A CPU spinning on a ticket lock has interrupts off (`push_off` precedes the
/// spin), so it cannot take the TLB-shootdown IPI — and a peer performing a
/// shootdown spin-waits for its ack. Under parallel VM work that is a convoy:
/// the flusher holds the lock the spinners want, the spinners' silence burns
/// the flusher's whole ack budget, and 4 CPUs faulting in parallel measured
/// 80x SLOWER in absolute terms than 1. The hook lets a spinning waiter drain
/// its own shootdown queue (queue-only work: lock-free SPSC ring + `invlpg`,
/// no locks taken), turning the ack latency from "whenever the lock is
/// released" into "the next pump interval".
static SPIN_PUMP: AtomicUsize = AtomicUsize::new(0);

/// Install the spin-pump hook. MUST take no locks and never allocate.
pub fn set_spin_pump(f: fn()) {
    SPIN_PUMP.store(f as usize, Ordering::SeqCst);
}

#[inline]
pub(crate) fn spin_pump() {
    // Two relaxed loads more than the bare slot test it used to be, on a path
    // that runs once every few hundred spins -- and it is the hottest indirect
    // call in the kernel, made with interrupts off from inside every ticket
    // lock, so it is the last one that should be taken on trust.
    let Some(h) = crate::fn_slot::live_fn(SPIN_PUMP.load(Ordering::Relaxed)) else {
        return;
    };
    let f: fn() = unsafe { core::mem::transmute(h) };
    f();
}

/// Public variant for OTHER crates' IRQs-off spin loops to drain their OWN
/// pending TLB-shootdown queue while spinning, exactly as this crate's ticket
/// lock does at `set_spin_pump`.
///
/// The ones there are, which is a list worth keeping honest because a spinner
/// that is missing from it is invisible until a machine wedges: the
/// scheduler's `spin::Mutex`-based runtime locks in `diag_lock`; the RM's
/// `os_*_spinlock` glue; the IPI ring's own `MpscQueue::commit_entry`, where a
/// producer waits for a peer's earlier reservation; the fault handler's wait
/// for a peer's diagnosis, which is bounded at two whole seconds; and the
/// NMI RIP capture's wait for the peers to answer.
///
/// A CPU spinning with interrupts disabled is deaf to the shootdown IPI, so a
/// peer that spin-waits for its ack (while holding, say, the VMAR lock) wedges —
/// and every CPU queued behind that lock wedges with it. Only this crate's own
/// ticket lock pumped; a CPU parked in any other IRQs-off spinner was an ack
/// black hole. Callers should invoke it at a coarse cadence (e.g. every 512
/// spins); it is a single relaxed load when no pump is installed and one
/// queue-pointer compare when the queue is empty. Three relaxed loads when no
/// pump is installed, since the slot is judged before it is called (`fn_slot`).
/// Same contract as the hook:
/// takes no locks, never allocates.
#[inline]
pub fn pump() {
    spin_pump();
}

/// Install the holder-report hook: `(file_ptr, file_len, line, cpu)` of the
/// CURRENT HOLDER of a lock some CPU has been spinning on for ~8s. The
/// spinners a deadlock banner lists are usually innocent readers; this is the
/// line that names the culprit. The file travels as raw parts because it is
/// snapshotted from the lock's atomics, not a live `&'static str` — it still
/// points into an immortal `#[track_caller]` string. Same rules as the main
/// hook: MUST NOT take locks or allocate.
pub fn set_deadlock_holder_hook(f: fn(usize, usize, u32, u32)) {
    DEADLOCK_HOLDER_HOOK.store(f as usize, Ordering::SeqCst);
}

#[inline(never)]
pub(crate) fn report_deadlock_holder(file_ptr: usize, file_len: usize, line: u32, cpu: u32) {
    let Some(h) = crate::fn_slot::live_fn(DEADLOCK_HOLDER_HOOK.load(Ordering::Relaxed)) else {
        return;
    };
    let f: fn(usize, usize, u32, u32) = unsafe { core::mem::transmute(h) };
    f(file_ptr, file_len, line, cpu);
}

/// The one lock every test that installs this module's process-wide hooks
/// takes. It lives here, and not in each test module, because `rwlock.rs` and
/// `tests.rs` both arm the spin pump and the deadlock hook — two locks would
/// leave them overwriting each other's recorder. The suite runs with
/// `--test-threads=1` in CI and would never notice.
#[cfg(test)]
pub(crate) fn hook_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// The hook registry every spinner in the kernel depends on, which had no
/// tests of its own.
///
/// Four slots and one threshold, and all of it runs on the spin path with
/// interrupts off: the deadlock banner (the only thing that turns a silent
/// freeze into a named call site), the holder report (the only thing that names
/// the culprit rather than the innocent waiters), and the spin pump (the only
/// thing that keeps an IRQs-off spinner from being a TLB-shootdown ack black
/// hole). Every one of them is a word that is `transmute`d and **called**, and
/// the rule that word must pass — judged against the `.text` window before the
/// jump — was exercised only where `fn_slot` tests the predicate, never where
/// these four slots use it.
#[cfg(test)]
mod tests {
    use super::*;

    /// Everything here is process-wide: the four slots, the threshold, and the
    /// `.text` window that decides whether a slot may be called. Two locks,
    /// because two different sets of tests share each of them.
    struct Hooks(
        #[allow(dead_code)] std::sync::MutexGuard<'static, ()>,
        #[allow(dead_code)] std::sync::MutexGuard<'static, ()>,
    );

    fn hooks() -> Hooks {
        let held = hook_test_lock();
        let window = crate::fn_slot::window_test_lock();
        clear();
        Hooks(held, window)
    }

    impl Drop for Hooks {
        fn drop(&mut self) {
            clear();
        }
    }

    /// The resting state of the suite: no hook, no window, no threshold
    /// override. It has to be restored, and not only out of tidiness -- a
    /// window left published refuses every *host* function pointer, so the
    /// hooks the rest of this crate's tests install would silently stop being
    /// called and those tests would fail somewhere else entirely.
    fn clear() {
        DEADLOCK_HOOK.store(0, Ordering::SeqCst);
        DEADLOCK_HOLDER_HOOK.store(0, Ordering::SeqCst);
        SPIN_PUMP.store(0, Ordering::SeqCst);
        set_deadlock_spins(0);
        crate::fn_slot::clear_text_range_for_test();
        crate::fn_slot::set_counters_for_test(0, 0);
    }

    static BANNERS: AtomicUsize = AtomicUsize::new(0);
    static OTHER_BANNERS: AtomicUsize = AtomicUsize::new(0);
    static PUMPS: AtomicUsize = AtomicUsize::new(0);
    static HOLDERS: AtomicUsize = AtomicUsize::new(0);
    static LAST_FILE: AtomicUsize = AtomicUsize::new(0);
    static LAST_LINE: AtomicU64 = AtomicU64::new(0);
    static LAST_HOLDER: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

    fn record_banner(file: &'static str, line: u32) {
        LAST_FILE.store(file.as_ptr() as usize, Ordering::SeqCst);
        LAST_LINE.store(line as u64, Ordering::SeqCst);
        BANNERS.fetch_add(1, Ordering::SeqCst);
    }

    fn record_other_banner(_file: &'static str, _line: u32) {
        OTHER_BANNERS.fetch_add(1, Ordering::SeqCst);
    }

    fn record_pump() {
        PUMPS.fetch_add(1, Ordering::SeqCst);
    }

    fn record_holder(file_ptr: usize, file_len: usize, line: u32, cpu: u32) {
        LAST_HOLDER[0].store(file_ptr as u64, Ordering::SeqCst);
        LAST_HOLDER[1].store(file_len as u64, Ordering::SeqCst);
        LAST_HOLDER[2].store(line as u64, Ordering::SeqCst);
        LAST_HOLDER[3].store(cpu as u64, Ordering::SeqCst);
        HOLDERS.fetch_add(1, Ordering::SeqCst);
    }

    fn zero_counters() {
        BANNERS.store(0, Ordering::SeqCst);
        OTHER_BANNERS.store(0, Ordering::SeqCst);
        PUMPS.store(0, Ordering::SeqCst);
        HOLDERS.store(0, Ordering::SeqCst);
    }

    /// A `.text` window that contains no host code: the kernel's own image
    /// range, which every function in this test binary is outside of. Publishing
    /// it is how a test says "this slot is now residue, not code".
    const KERNEL_TEXT_LO: usize = 0xffff_ff00_0000_0000;
    const KERNEL_TEXT_HI: usize = 0xffff_ff00_0020_0000;

    fn publish_a_window_that_excludes_host_code() {
        assert!(
            crate::fn_slot::set_text_range(KERNEL_TEXT_LO, KERNEL_TEXT_HI),
            "the window has to be taken, or the test proves nothing"
        );
    }

    fn refusals() -> u32 {
        crate::fn_slot::slot_stats().0
    }

    // ── the threshold ───────────────────────────────────────────────────────

    #[test]
    fn the_threshold_is_the_one_the_boot_line_asked_for() {
        // `DEADLOCKSPINS=<n>` exists because the default is calibrated for real
        // hardware and is unreachable under TCG, where it turns every deadlock
        // into "no banner appeared, so it is not a deadlock".
        let _h = hooks();
        set_deadlock_spins(4096);
        assert_eq!(deadlock_spins(), 4096);
    }

    #[test]
    fn asking_for_zero_restores_the_eight_second_default() {
        // Zero is how the cmdline says "no override", so it must not mean "fire
        // the banner on the first spin" -- which would paint the framebuffer
        // over a machine under ordinary contention.
        let _h = hooks();
        set_deadlock_spins(1);
        set_deadlock_spins(0);
        assert_eq!(deadlock_spins(), 1_000_000_000);
    }

    // ── the banner ──────────────────────────────────────────────────────────

    #[test]
    fn the_banner_is_called_with_the_stuck_call_site() {
        let _h = hooks();
        zero_counters();
        set_deadlock_hook(record_banner);
        let file = "src/vm/vmar.rs";
        report_deadlock(file, 1234);
        assert!(BANNERS.load(Ordering::SeqCst) >= 1);
        assert_eq!(LAST_FILE.load(Ordering::SeqCst), file.as_ptr() as usize);
        assert_eq!(LAST_LINE.load(Ordering::SeqCst), 1234);
    }

    #[test]
    fn the_public_stuck_report_is_the_same_banner() {
        // Other crates' spinners (the scheduler's runtime locks, the RM glue,
        // the IPI ring) report through `report_stuck`. If it stopped reaching
        // the hook, every spinner outside this crate would freeze silently
        // again and nothing in this crate would notice.
        let _h = hooks();
        zero_counters();
        set_deadlock_hook(record_banner);
        report_stuck("src/ipi.rs", 77);
        assert!(BANNERS.load(Ordering::SeqCst) >= 1);
        assert_eq!(LAST_LINE.load(Ordering::SeqCst), 77);
    }

    #[test]
    fn the_hook_installed_last_is_the_one_that_is_called() {
        // The kernel installs the framebuffer banner once the console is up,
        // over whatever early stand-in was there.
        let _h = hooks();
        zero_counters();
        set_deadlock_hook(record_other_banner);
        set_deadlock_hook(record_banner);
        report_deadlock("src/late.rs", 9);
        assert!(BANNERS.load(Ordering::SeqCst) >= 1);
        assert_eq!(OTHER_BANNERS.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_banner_slot_that_is_not_text_is_not_jumped_into() {
        // The soft smash leaves plausible residue in exactly this kind of slot.
        // A banner that jumps into it replaces a diagnosable deadlock with an
        // undiagnosable triple fault -- on the machine that was mid-freeze.
        let _h = hooks();
        zero_counters();
        set_deadlock_hook(record_banner);
        publish_a_window_that_excludes_host_code();
        let before = refusals();
        report_deadlock("src/vmar.rs", 5);
        assert_eq!(BANNERS.load(Ordering::SeqCst), 0);
        assert!(refusals() > before, "and the refusal is counted");
    }

    // ── the spin pump ───────────────────────────────────────────────────────

    #[test]
    fn the_public_pump_is_the_hook_the_ticket_lock_pumps() {
        // Every IRQs-off spinner outside this crate pumps through here. A CPU
        // spinning with interrupts off cannot take the shootdown IPI, so one
        // that does not pump is an ack black hole and the peer waiting for it
        // wedges -- which is the convoy that measured 4 CPUs 80x slower than 1.
        let _h = hooks();
        zero_counters();
        set_spin_pump(record_pump);
        pump();
        assert!(PUMPS.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn a_pump_slot_that_is_not_text_is_not_jumped_into() {
        let _h = hooks();
        zero_counters();
        set_spin_pump(record_pump);
        publish_a_window_that_excludes_host_code();
        let before = refusals();
        pump();
        assert_eq!(PUMPS.load(Ordering::SeqCst), 0);
        assert!(refusals() > before);
    }

    // ── the holder report ───────────────────────────────────────────────────

    #[test]
    fn the_holder_report_carries_the_four_numbers_that_name_the_culprit() {
        // The spinners a banner lists are usually innocent; this is the line
        // that names the holder. The file travels as raw parts because it is
        // snapshotted out of the lock's atomics, so nothing but the order of
        // the arguments keeps the length from being printed as a line number.
        let _h = hooks();
        zero_counters();
        set_deadlock_holder_hook(record_holder);
        let file = "src/fs/fatfs.rs";
        report_deadlock_holder(file.as_ptr() as usize, file.len(), 4242, 3);
        assert!(HOLDERS.load(Ordering::SeqCst) >= 1);
        assert_eq!(LAST_HOLDER[0].load(Ordering::SeqCst), file.as_ptr() as u64);
        assert_eq!(LAST_HOLDER[1].load(Ordering::SeqCst), file.len() as u64);
        assert_eq!(LAST_HOLDER[2].load(Ordering::SeqCst), 4242);
        assert_eq!(LAST_HOLDER[3].load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_holder_slot_that_is_not_text_is_not_jumped_into() {
        let _h = hooks();
        zero_counters();
        set_deadlock_holder_hook(record_holder);
        publish_a_window_that_excludes_host_code();
        let before = refusals();
        report_deadlock_holder(0x1000, 4, 1, 0);
        assert_eq!(HOLDERS.load(Ordering::SeqCst), 0);
        assert!(refusals() > before);
    }

    // ── the resting state ───────────────────────────────────────────────────

    #[test]
    fn a_slot_nobody_installed_calls_nothing_and_is_not_a_refusal() {
        // Which is the state of all four on every boot until their owners
        // install them, so counting it as a refusal would make the boot report
        // claim the guard had rejected something.
        let _h = hooks();
        zero_counters();
        let before = refusals();
        report_deadlock("src/early.rs", 1);
        report_stuck("src/early.rs", 2);
        pump();
        report_deadlock_holder(0, 0, 0, 0);
        assert_eq!(BANNERS.load(Ordering::SeqCst), 0);
        assert_eq!(PUMPS.load(Ordering::SeqCst), 0);
        assert_eq!(HOLDERS.load(Ordering::SeqCst), 0);
        assert_eq!(refusals(), before, "an empty slot is not a rejected one");
    }

    #[test]
    fn the_three_slots_are_three_slots() {
        // One `AtomicUsize` per hook, and the banner, the holder report and the
        // pump have different signatures: a slot shared between two of them is
        // a call through the wrong prototype.
        let _h = hooks();
        zero_counters();
        set_spin_pump(record_pump);
        report_deadlock("src/a.rs", 1);
        report_deadlock_holder(0x2000, 2, 3, 4);
        assert_eq!(BANNERS.load(Ordering::SeqCst), 0);
        assert_eq!(HOLDERS.load(Ordering::SeqCst), 0);
        assert_eq!(PUMPS.load(Ordering::SeqCst), 0, "and nothing called it yet");
        pump();
        assert!(PUMPS.load(Ordering::SeqCst) >= 1);
    }
}
