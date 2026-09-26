//! Boot-time file-access recorder — "reverse-engineering" the desktop startup.
//!
//! The desktop compositor (labwc) reaches its first frame only after the
//! dynamic linker has mapped ~30-40 shared objects, xkbcommon has parsed the
//! keymap out of dozens of tiny `/usr/share/X11/xkb` files, and fontconfig /
//! theme / config files have been read — all of it, on a cold boot, a storm of
//! scattered 4 KiB demand-page reads over files that were never in the block
//! cache. To attack that we first have to SEE it.
//!
//! This module records, for exactly one target process (matched by `comm`,
//! selected at boot via `BOOTTRACE=<comm>` on the kernel command line), every
//! file it opens, with a monotonic timestamp and the open's result. The result
//! is surfaced as plain text at `/proc/bootprofile`: a timeline (which reveals
//! the phases — libraries first, then xkb, then fonts — and the stalls between
//! them) followed by a deduplicated, access-ordered *preload list*.
//!
//! That preload list is the point: it is the exact set of files, in the exact
//! order, that a later prefetch stage can stream into the block cache during
//! the idle window Eclipse already spends waiting for seatd and for
//! `/dev/input` to settle — so that when labwc actually starts, its working set
//! is already warm. The recorder here is deliberately the *first half* of that
//! system, not throwaway instrumentation.
//!
//! Cost when disabled (the default): one relaxed atomic load per `openat`.
//! Nothing else runs, nothing is allocated, until `BOOTTRACE=` arms it.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use lock::Mutex;

/// Master gate. Flipped true exactly once, by [`set_target`], when the boot
/// code sees `BOOTTRACE=<comm>`. Every hook checks this first and returns on a
/// single relaxed load when it is false.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// The `comm` we are hunting for. Read-only after [`set_target`]; guarded by a
/// mutex only because it is a `String`.
///
/// Matched against a process's `comm` case-insensitively, by
/// [`str::eq_ignore_ascii_case`] at compare time rather than by lower-casing
/// both sides: the value comes off a hand-typed kernel command line, and
/// `BOOTTRACE=LabWC` silently never arming is indistinguishable from the
/// recorder being broken -- `/proc/bootprofile` just says "no matching process
/// has opened a file yet" for the rest of the boot.
static TARGET: Mutex<Option<String>> = Mutex::new(None);

/// KoID of the process we locked onto (its `comm` first matched [`TARGET`]).
/// `0` until armed. Once non-zero, only that pid's opens are recorded, so the
/// trace is a clean single-process capture even though every process's opens
/// flow through [`record_open`].
static ARMED_PID: AtomicU64 = AtomicU64::new(0);

/// Nanosecond timestamp (uptime) at which we armed — the `+0.000` of the
/// timeline.
static T0_NS: AtomicU64 = AtomicU64::new(0);

/// Uptime nanoseconds at arm time, kept for the header only.
static T0_UPTIME_NS: AtomicU64 = AtomicU64::new(0);

/// One recorded event, timestamped at [`dt_us`] microseconds since [`T0_NS`].
enum Rec {
    /// An `open`/`openat`: `result >= 0` is the fd, `< 0` is `-errno`.
    Open {
        dt_us: u64,
        result: i32,
        path: String,
    },
    /// A syscall that took longer than [`SLOW_SYSCALL_US`] — the raw material
    /// for the gaps in the open timeline where NO file is touched (a blocking
    /// wait, or one very long call). `dur_us` is how long the call itself took;
    /// `detail` carries decoded args for the calls worth dissecting (mmap's
    /// len/prot/flags/fd — MAP_FIXED there means an unmap+TLB-shootdown).
    Slow {
        dt_us: u64,
        num: u32,
        dur_us: u64,
        detail: String,
    },
}

impl Rec {
    fn dt_us(&self) -> u64 {
        match self {
            Rec::Open { dt_us, .. } | Rec::Slow { dt_us, .. } => *dt_us,
        }
    }
}

/// Threshold above which a syscall is worth recording on its own. Chosen so the
/// multi-second desktop stalls (icon-theme init, first render) are captured
/// without flooding the ring with ordinary fast calls.
const SLOW_SYSCALL_US: u64 = 300_000;

/// Bounded so a long-lived compositor cannot grow the log without end: startup
/// is what we care about, and 8192 opens comfortably covers a full desktop
/// bring-up. Once full, further opens are counted (see [`DROPPED`]) but not
/// stored.
const MAX_RECS: usize = 8192;
static RECS: Mutex<Vec<Rec>> = Mutex::new(Vec::new());
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Arm the recorder for processes whose `comm` equals `target`. Called once
/// from boot when `BOOTTRACE=<comm>` is present. Idempotent.
pub fn set_target(target: &str) {
    let mut t = TARGET.lock();
    if t.is_none() {
        *t = Some(target.to_string());
        ENABLED.store(true, Ordering::Release);
    }
}

/// True once [`set_target`] has armed the recorder. One relaxed load; this is
/// the whole cost on the `openat` fast path when tracing is off.
#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

fn now_ns() -> u64 {
    kernel_hal::timer::timer_now().as_nanos() as u64
}

/// Record one `openat`/`open`. `comm` is computed lazily — the closure only
/// runs while we are still hunting for the target (before arming), never on the
/// steady-state path once locked onto a pid.
///
/// `result` is the syscall's return: `>= 0` an fd, `< 0` a `-errno`. Errors are
/// recorded on purpose: the dynamic linker's ENOENT probes across its library
/// search path are exactly what reveal how it hunts for each `.so`.
pub fn record_open(pid: u64, comm: impl FnOnce() -> String, path: &str, result: i32) {
    if !enabled() {
        return;
    }
    let armed = ARMED_PID.load(Ordering::Relaxed);
    if armed == 0 {
        // Still hunting: pay for the comm string and the compare.
        let target_matches = {
            let t = TARGET.lock();
            match t.as_deref() {
                Some(target) => comm().eq_ignore_ascii_case(target),
                None => false,
            }
        };
        if !target_matches {
            return;
        }
        // First matching open wins the arm. A concurrent opener of the same
        // comm (there is only one labwc, but be correct) either loses the CAS
        // and, unless it is the same pid, is dropped.
        match ARMED_PID.compare_exchange(0, pid, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => {
                let now = now_ns();
                T0_NS.store(now, Ordering::Relaxed);
                T0_UPTIME_NS.store(now, Ordering::Relaxed);
            }
            Err(winner) if winner != pid => return,
            Err(_) => {}
        }
    } else if armed != pid {
        return;
    }
    let dt_us = elapsed_us();
    push(Rec::Open {
        dt_us,
        result,
        path: path.to_string(),
    });
}

/// Record a syscall that took `dur_ns`, but only if it crossed
/// [`SLOW_SYSCALL_US`] and belongs to the armed process. Called from the
/// syscall dispatcher's per-call timing. Cheap and lazy: returns on the
/// enabled/armed/threshold checks before touching the lock.
pub fn record_syscall(pid: u64, num: u32, dur_ns: u64, detail: impl FnOnce() -> String) {
    if !enabled() || dur_ns / 1000 < SLOW_SYSCALL_US {
        return;
    }
    // `armed == 0` on its own, and not just `!= pid`: nothing has been locked
    // onto yet, so there is no timeline to place a record on. Today's koids
    // start at `1 << 32` so no caller can pass a pid of 0, which is the only
    // way the compare alone would let one through -- this says so rather than
    // resting on it, the way [`record_open`] already does.
    let armed = ARMED_PID.load(Ordering::Relaxed);
    if armed == 0 || armed != pid {
        return;
    }
    let dt_us = elapsed_us();
    push(Rec::Slow {
        dt_us,
        num,
        dur_us: dur_ns / 1000,
        detail: detail(),
    });
}

fn elapsed_us() -> u64 {
    let t0 = T0_NS.load(Ordering::Relaxed);
    if t0 == 0 {
        // Not published yet. Reachable: a second thread of the target process
        // sees `ARMED_PID` the instant the winning thread's
        // `compare_exchange` lands and can get here before its `T0_NS.store`.
        // Subtracting zero would stamp that record with the machine's whole
        // uptime in microseconds, putting it at the far end of a timeline whose
        // only purpose is reading where the time goes.
        return 0;
    }
    now_ns().saturating_sub(t0) / 1000
}

fn push(rec: Rec) {
    let mut recs = RECS.lock();
    if recs.len() >= MAX_RECS {
        DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    recs.push(rec);
}

/// The successful opens, deduplicated, in first-access order.
///
/// One implementation, because there were two: this list is both the tail of
/// `/proc/bootprofile` and the prefetch list the replay stage consumes, and the
/// two copies deduplicated by different means (`Vec::contains` over `&str` on
/// one side, `iter().any()` over `String` on the other) with nothing keeping
/// them in step.
///
/// The membership test is a `BTreeSet`, not a scan of what has been kept so far.
/// That scan was quadratic in the number of records, and [`MAX_RECS`] is 8192:
/// a full capture made one `cat /proc/bootprofile` do upwards of thirty million
/// string comparisons **holding [`RECS`]**, which is a spin lock -- so every
/// other cpu reaching the recorder spun behind it.
fn dedup_ok_opens(recs: &[Rec]) -> Vec<&str> {
    let mut seen = alloc::collections::BTreeSet::new();
    let mut out = Vec::new();
    for r in recs.iter() {
        if let Rec::Open { result, path, .. } = r {
            if *result >= 0 && seen.insert(path.as_str()) {
                out.push(path.as_str());
            }
        }
    }
    out
}

/// The deduplicated, access-ordered list of paths that were opened
/// SUCCESSFULLY by the traced process — the prefetch list for the replay stage.
/// Empty until a trace has been captured.
pub fn preload_list() -> Vec<String> {
    let recs = RECS.lock();
    dedup_ok_opens(&recs)
        .into_iter()
        .map(String::from)
        .collect()
}

/// Render `/proc/bootprofile`.
pub fn render() -> String {
    if !enabled() {
        return "boot-trace: disabled.\n\
                Enable by adding BOOTTRACE=<comm> to the kernel command line \
                (e.g. BOOTTRACE=labwc), reboot, then read /proc/bootprofile \
                after the desktop is up.\n"
            .to_string();
    }
    let target = TARGET.lock().clone().unwrap_or_default();
    let armed = ARMED_PID.load(Ordering::Relaxed);
    let mut out = String::new();

    if armed == 0 {
        let _ = writeln!(
            out,
            "eclipse boot-trace — target '{}': ARMED, no matching process has opened a file yet.",
            target
        );
        return out;
    }

    let recs = RECS.lock();
    let dropped = DROPPED.load(Ordering::Relaxed);
    let t0_uptime_ms = T0_UPTIME_NS.load(Ordering::Relaxed) / 1_000_000;
    let (mut ok, mut miss, mut slow) = (0u32, 0u32, 0u32);
    for r in recs.iter() {
        match r {
            Rec::Open { result, .. } if *result >= 0 => ok += 1,
            Rec::Open { .. } => miss += 1,
            Rec::Slow { .. } => slow += 1,
        }
    }
    let window_ms = recs
        .last()
        .map(|r| r.dt_us() as f64 / 1000.0)
        .unwrap_or(0.0);

    let _ = writeln!(
        out,
        "eclipse boot-trace — target '{}', pid {}",
        target, armed
    );
    let _ = writeln!(
        out,
        "armed at {}.{:03}s uptime; window {:.1}ms; {} opens ({} ok, {} miss), {} slow syscalls (>{}ms){}",
        t0_uptime_ms / 1000,
        t0_uptime_ms % 1000,
        window_ms,
        ok + miss,
        ok,
        miss,
        slow,
        SLOW_SYSCALL_US / 1000,
        if dropped > 0 {
            alloc::format!("; {dropped} DROPPED (log full at {MAX_RECS})")
        } else {
            String::new()
        }
    );
    let _ = writeln!(
        out,
        "a '*' marks a gap > 2ms since the previous event; SLOW lines are single \
         syscalls that themselves took >{}ms (the blocking waits inside the gaps \
         with no file activity — first render, icon-theme init, GPU waits):",
        SLOW_SYSCALL_US / 1000
    );
    let _ = writeln!(out, "\n   time(ms)   d(ms)  res  path / syscall");

    let mut prev_us = 0u64;
    for r in recs.iter() {
        let gap_us = r.dt_us().saturating_sub(prev_us);
        let stall = if gap_us > 2000 { '*' } else { ' ' };
        match r {
            Rec::Open {
                dt_us,
                result,
                path,
            } => {
                let res = if *result >= 0 {
                    "ok  ".to_string()
                } else {
                    alloc::format!("e{:<3}", -*result)
                };
                let _ = writeln!(
                    out,
                    "  {:>8.3} {:>7.2} {} {} {}",
                    *dt_us as f64 / 1000.0,
                    gap_us as f64 / 1000.0,
                    stall,
                    res,
                    path
                );
            }
            Rec::Slow {
                dt_us,
                num,
                dur_us,
                detail,
            } => {
                let _ = writeln!(
                    out,
                    "  {:>8.3} {:>7.2} {} SLOW {} took {:.1}ms{}{}",
                    *dt_us as f64 / 1000.0,
                    gap_us as f64 / 1000.0,
                    stall,
                    crate::perf::name_of(*num),
                    *dur_us as f64 / 1000.0,
                    if detail.is_empty() { "" } else { "  " },
                    detail,
                );
            }
        }
        prev_us = r.dt_us();
    }

    // The prefetch list: ok-only, deduped, in first-access order.
    let seen = dedup_ok_opens(&recs);
    let _ = writeln!(
        out,
        "\n--- preload list (ok opens, deduped, in access order): {} files ---",
        seen.len()
    );
    for p in &seen {
        let _ = writeln!(out, "{p}");
    }
    out
}

#[cfg(test)]
mod tests {
    //! Every piece of this recorder's state is a process-global `static`:
    //! `ENABLED`, `TARGET`, `ARMED_PID`, `T0_NS`, `RECS`, `DROPPED`. It is boot
    //! state, armed once and never disarmed, so there is no production route
    //! that clears it -- which means a test cannot have a recorder of its own.
    //!
    //! So every test in here takes [`alone_with_the_recorder`] on its first
    //! line, which both serialises and resets. A new test belongs in that queue
    //! whatever it asserts: without it two tests arm the same recorder with
    //! different targets and read each other's records.

    use super::*;
    use core::sync::atomic::AtomicBool;

    /// Hold the recorder's turnstile, with the state wiped, until the end of the
    /// test.
    ///
    /// The guard travels in a `let _alone = ...` binding so a failed assertion's
    /// unwind releases it; naming it `_` would drop it on the spot and serialise
    /// nothing.
    #[must_use = "bind it to `_alone`: a bare `_` releases the turnstile at once"]
    fn alone_with_the_recorder() -> spin::MutexGuard<'static, ()> {
        static GUARD: spin::Mutex<()> = spin::Mutex::new(());
        let guard = GUARD.lock();
        ENABLED.store(false, Ordering::Relaxed);
        *TARGET.lock() = None;
        ARMED_PID.store(0, Ordering::Relaxed);
        T0_NS.store(0, Ordering::Relaxed);
        T0_UPTIME_NS.store(0, Ordering::Relaxed);
        RECS.lock().clear();
        DROPPED.store(0, Ordering::Relaxed);
        guard
    }

    /// A pid in the range real ones come from: koids start at `1 << 32`.
    const PID: u64 = 1 << 32;
    const OTHER_PID: u64 = (1 << 32) + 1;

    fn opens() -> Vec<(i32, String)> {
        RECS.lock()
            .iter()
            .filter_map(|r| match r {
                Rec::Open { result, path, .. } => Some((*result, path.clone())),
                _ => None,
            })
            .collect()
    }

    fn slows() -> Vec<u32> {
        RECS.lock()
            .iter()
            .filter_map(|r| match r {
                Rec::Slow { num, .. } => Some(*num),
                _ => None,
            })
            .collect()
    }

    fn stamps() -> Vec<u64> {
        RECS.lock().iter().map(|r| r.dt_us()).collect()
    }

    /// One `record_open` from the target process, with `comm` reporting `comm`.
    fn open_as(pid: u64, comm: &str, path: &str, result: i32) {
        record_open(pid, || comm.to_string(), path, result);
    }

    #[test]
    /// Disabled is the default and it costs one relaxed load: nothing is
    /// recorded, and in particular the `comm` closure -- which allocates a
    /// `String` out of the process's execute path -- is never run.
    fn nothing_happens_until_a_target_is_set() {
        let _alone = alone_with_the_recorder();
        let asked = AtomicBool::new(false);
        record_open(
            PID,
            || {
                asked.store(true, Ordering::Relaxed);
                "labwc".to_string()
            },
            "/lib/libc.so",
            3,
        );
        record_syscall(PID, 9, SLOW_SYSCALL_US * 1000 * 2, || String::new());
        assert!(!enabled());
        assert!(opens().is_empty(), "recorded with no target set");
        assert!(slows().is_empty(), "recorded with no target set");
        assert!(
            !asked.load(Ordering::Relaxed),
            "paid for the comm string with tracing off"
        );
    }

    #[test]
    /// `BOOTTRACE=` is read once at boot, and a second pass must not move the
    /// target out from under a capture already running.
    fn set_target_is_idempotent() {
        let _alone = alone_with_the_recorder();
        set_target("labwc");
        set_target("something-else");
        assert_eq!(TARGET.lock().as_deref(), Some("labwc"));
        assert!(enabled());
    }

    #[test]
    /// **The target comes off a hand-typed kernel command line.** It used to be
    /// compared exactly, against a doc comment that said it was lower-cased at
    /// boot -- it was not -- so `BOOTTRACE=LabWC` armed nothing and
    /// `/proc/bootprofile` said "no matching process has opened a file yet" for
    /// the whole boot, which reads exactly like a broken recorder.
    fn a_target_typed_with_capitals_still_arms() {
        let _alone = alone_with_the_recorder();
        set_target("LabWC");
        open_as(PID, "labwc", "/lib/libc.so", 3);
        assert_eq!(ARMED_PID.load(Ordering::Relaxed), PID, "never armed");
        assert_eq!(opens(), vec![(3, "/lib/libc.so".to_string())]);
    }

    #[test]
    /// And a process that is not the target arms nothing, however many files it
    /// opens.
    fn another_process_arms_nothing() {
        let _alone = alone_with_the_recorder();
        set_target("labwc");
        open_as(OTHER_PID, "getty", "/etc/passwd", 3);
        open_as(OTHER_PID, "getty", "/etc/shadow", 3);
        assert_eq!(ARMED_PID.load(Ordering::Relaxed), 0);
        assert!(opens().is_empty());
    }

    #[test]
    /// The capture is one process. Every process's opens flow through here, so
    /// the pid filter is what keeps the timeline readable.
    fn once_armed_only_that_pid_is_recorded() {
        let _alone = alone_with_the_recorder();
        set_target("labwc");
        open_as(PID, "labwc", "/lib/libc.so", 3);
        open_as(OTHER_PID, "labwc", "/lib/libm.so", 3);
        open_as(OTHER_PID, "getty", "/etc/passwd", 3);
        open_as(PID, "labwc", "/lib/libz.so", 4);
        assert_eq!(
            opens(),
            vec![
                (3, "/lib/libc.so".to_string()),
                (4, "/lib/libz.so".to_string()),
            ],
            "a second process got into the capture"
        );
    }

    #[test]
    /// Once locked onto a pid the `comm` closure must not run again: it builds a
    /// `String` out of the process's execute path, and this is on the `openat`
    /// path of every process on the machine.
    fn the_comm_closure_stops_being_called_once_armed() {
        let _alone = alone_with_the_recorder();
        set_target("labwc");
        open_as(PID, "labwc", "/lib/libc.so", 3);

        let asked = AtomicBool::new(false);
        record_open(
            PID,
            || {
                asked.store(true, Ordering::Relaxed);
                "labwc".to_string()
            },
            "/lib/libm.so",
            3,
        );
        assert_eq!(opens().len(), 2);
        assert!(
            !asked.load(Ordering::Relaxed),
            "built the comm string again on the steady-state path"
        );
    }

    #[test]
    /// Failed opens are kept on purpose -- the linker's ENOENT probes across its
    /// search path are what show how it hunts -- so the timeline holds them and
    /// only the preload list drops them.
    fn a_failed_open_is_recorded_and_left_out_of_the_preload_list() {
        let _alone = alone_with_the_recorder();
        set_target("labwc");
        open_as(PID, "labwc", "/usr/lib/libfoo.so", -2);
        open_as(PID, "labwc", "/lib/libfoo.so", 3);
        assert_eq!(opens().len(), 2, "the ENOENT probe was dropped");
        assert_eq!(preload_list(), vec!["/lib/libfoo.so".to_string()]);
    }

    #[test]
    /// First-access order, and each path once: it is a prefetch list, and a file
    /// opened four times is one file to warm.
    fn the_preload_list_keeps_first_access_order_and_deduplicates() {
        let _alone = alone_with_the_recorder();
        set_target("labwc");
        for path in ["/b", "/a", "/b", "/c", "/a", "/b"] {
            open_as(PID, "labwc", path, 3);
        }
        assert_eq!(
            preload_list(),
            vec!["/b".to_string(), "/a".to_string(), "/c".to_string()]
        );
    }

    #[test]
    /// The same list, and one implementation. It was two, deduplicating by
    /// different means with nothing keeping them in step.
    fn the_rendered_preload_list_is_the_one_preload_list_returns() {
        let _alone = alone_with_the_recorder();
        set_target("labwc");
        for path in ["/x", "/y", "/x", "/z"] {
            open_as(PID, "labwc", path, 3);
        }
        open_as(PID, "labwc", "/missing", -2);

        let rendered = render();
        let tail = rendered
            .split("--- preload list")
            .nth(1)
            .expect("no preload list in the rendered profile");
        for p in preload_list() {
            assert!(tail.contains(&p), "{} missing from the rendered list", p);
        }
        assert!(
            !tail.contains("/missing"),
            "a failed open reached the rendered preload list"
        );
        assert!(tail.contains("3 files"), "wrong count: {}", tail);
    }

    #[test]
    /// Only calls over the threshold, so the ring is not flooded by ordinary
    /// fast ones.
    fn a_syscall_under_the_threshold_is_not_recorded() {
        let _alone = alone_with_the_recorder();
        set_target("labwc");
        open_as(PID, "labwc", "/lib/libc.so", 3);
        record_syscall(PID, 9, (SLOW_SYSCALL_US - 1) * 1000, || String::new());
        assert!(slows().is_empty(), "a fast call was recorded");
        record_syscall(PID, 9, SLOW_SYSCALL_US * 1000, || String::new());
        assert_eq!(slows(), vec![9], "a call at the threshold was dropped");
    }

    #[test]
    /// A slow call from a process that is not the armed one is not on this
    /// timeline. The guard is `armed == 0` as well as `armed != pid`, because
    /// `!= pid` alone lets a pid of 0 through onto a timeline that has no `t0`
    /// yet -- today's koids start at `1 << 32`, and the point is not to depend
    /// on that.
    fn a_slow_syscall_outside_the_capture_is_not_recorded() {
        let _alone = alone_with_the_recorder();
        set_target("labwc");
        let slow = SLOW_SYSCALL_US * 1000 * 2;
        record_syscall(0, 9, slow, || String::new());
        record_syscall(PID, 9, slow, || String::new());
        assert!(slows().is_empty(), "recorded before anything armed");

        open_as(PID, "labwc", "/lib/libc.so", 3);
        record_syscall(OTHER_PID, 10, slow, || String::new());
        assert!(slows().is_empty(), "recorded another process's stall");
        record_syscall(PID, 11, slow, || String::new());
        assert_eq!(slows(), vec![11]);
    }

    #[test]
    /// A record made before the arming thread has published `t0` lands at zero,
    /// not at the machine's whole uptime.
    ///
    /// Reachable: a second thread of the target process sees `ARMED_PID` the
    /// instant the winner's `compare_exchange` lands and can reach `elapsed_us`
    /// before its `T0_NS.store`. Stamping that record with the uptime in
    /// microseconds puts it at the far end of a timeline whose only job is
    /// showing where the time went.
    fn a_record_made_before_t0_is_published_lands_at_zero() {
        let _alone = alone_with_the_recorder();
        set_target("labwc");
        // Exactly the window: armed, t0 not stored yet.
        ARMED_PID.store(PID, Ordering::Relaxed);
        open_as(PID, "labwc", "/lib/libc.so", 3);
        assert_eq!(stamps(), vec![0], "stamped with the uptime instead of zero");
    }

    #[test]
    /// Bounded, and it says how much it threw away -- a truncated capture that
    /// looked complete would be read as "the desktop only opens 8192 files".
    fn the_log_stops_at_its_bound_and_counts_what_it_drops() {
        let _alone = alone_with_the_recorder();
        set_target("labwc");
        open_as(PID, "labwc", "/first", 3);
        for i in 0..MAX_RECS + 10 {
            open_as(PID, "labwc", &alloc::format!("/f{}", i), 3);
        }
        assert_eq!(RECS.lock().len(), MAX_RECS);
        assert_eq!(DROPPED.load(Ordering::Relaxed), 11);
        let rendered = render();
        assert!(rendered.contains("11 DROPPED"), "the drop went unsaid");
        // The oldest records are the ones kept: a startup trace is about the
        // beginning, so the bound drops the tail rather than the head.
        assert_eq!(opens()[0].1, "/first");
    }

    #[test]
    /// With nothing set, the profile says how to turn it on rather than looking
    /// empty.
    fn a_disabled_profile_says_how_to_enable_it() {
        let _alone = alone_with_the_recorder();
        let out = render();
        assert!(
            out.contains("BOOTTRACE="),
            "no hint of how to arm it: {}",
            out
        );
        assert!(!out.contains("preload list"));
    }

    #[test]
    /// Armed but not yet matched is its own answer, and not an empty timeline.
    fn an_armed_but_unmatched_profile_says_so() {
        let _alone = alone_with_the_recorder();
        set_target("labwc");
        let out = render();
        assert!(out.contains("no matching process"), "{}", out);
        assert!(out.contains("labwc"), "the target went unnamed: {}", out);
    }

    #[test]
    /// The header counts the opens that worked apart from the ones that did not,
    /// because the ENOENT probes are most of a linker trace and would otherwise
    /// read as files worth warming.
    fn the_header_counts_the_hits_and_the_misses_apart() {
        let _alone = alone_with_the_recorder();
        set_target("labwc");
        open_as(PID, "labwc", "/lib/libc.so", 3);
        open_as(PID, "labwc", "/usr/lib/nope.so", -2);
        open_as(PID, "labwc", "/usr/lib/nope2.so", -2);
        record_syscall(PID, 9, SLOW_SYSCALL_US * 1000 * 2, || String::new());
        let out = render();
        assert!(
            out.contains("3 opens (1 ok, 2 miss), 1 slow syscalls"),
            "{}",
            out
        );
    }
}
