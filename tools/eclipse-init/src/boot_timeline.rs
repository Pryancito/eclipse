//! The boot timeline: every line init prints is stamped with its offset
//! from PID 1's first statement, and the boot's waits are timed and printed
//! worst-first once it is over. Out of `main.rs`, which had grown past the
//! tree's ceiling (`cargo test -p xtask tamano`).

use super::*;

// ── The boot timeline ──────────────────────────────────────────────────────
//
// "Can the boot be made faster?" cannot be answered by reading the code: the
// expensive parts of this init are not the ones that compute, they are the ones
// that WAIT -- a `wait_socket =` for a daemon that is still linking itself, a
// `wait_path =` for device nodes the kernel has not enumerated yet, the settle
// window after them, a `oneshot` that runs a shell script. None of those were
// measured, and none of them are visible in a console log, because until now
// init's lines carried no time at all: a reader could see the ORDER of the boot
// and had no way to see its SHAPE.
//
// So: every line init prints is stamped with its offset from PID 1's first
// statement, every wait is timed, and at the end of the boot the stretches are
// printed worst-first with what they do not account for. The cost is one
// `Instant::now()` per wait and a `Vec` of a few dozen short strings.

/// When PID 1 started: the `+0.000` of every stamped line and of the timeline.
///
/// `OnceLock` and not a plain `static mut`: set once at the top of `main`, read
/// from `log`, which runs on every line including ones printed from inside a
/// `catch_unwind`. A read before it is set (the multi-call helper path, which
/// never gets as far as `main`'s PID 1 setup) prints an unstamped line rather
/// than a wrong one.
pub(crate) static BOOT_T0: OnceLock<Instant> = OnceLock::new();

/// One measured stretch of the boot: a wait, or a step that runs to completion.
///
/// Only LEAVES are recorded -- a gate, a settle window, a oneshot's run -- never
/// a wrapper around several of them. The whole point of the table is that the
/// rows add up to something less than the total and the difference is real
/// unaccounted time; nesting one row inside another would let the rows sum past
/// the boot and make `rest` meaningless.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BootStep {
    /// What was waited for, as the row should read.
    pub(crate) what: String,
    /// When it started, since init's first statement.
    pub(crate) at: Duration,
    /// How long it took.
    pub(crate) took: Duration,
}

/// The stretches recorded so far. Rendered once, at the end of the boot.
pub(crate) static BOOT_STEPS: Mutex<Vec<BootStep>> = Mutex::new(Vec::new());

/// Set once the timeline has been rendered. From then on nothing is recorded:
/// a `respawn` that crashes an hour later goes through the same gates as it did
/// at boot ([`start_service`] serves both), and those waits are not boot --
/// recording them would grow this vector for the life of the machine and
/// rewrite the history of a boot that is already over.
pub(crate) static BOOT_RECORDED: AtomicBool = AtomicBool::new(false);

/// Hard cap on recorded stretches, so a boot that somehow loops through
/// [`start_service`] many times before the timeline is rendered cannot grow
/// this without bound. Generous: a full desktop boot records about thirty.
pub(crate) const BOOT_STEPS_MAX: usize = 512;

/// Stretches shorter than this are left out of the table and land in `rest`.
/// A boot has a long tail of sub-millisecond steps, and a table that lists
/// them is one nobody reads to the end.
pub(crate) const BOOT_STEP_FLOOR: Duration = Duration::from_millis(5);

/// Where the timeline is left for later reading. The console scrolls, and the
/// question "where did my boot go" is usually asked on a machine that is
/// already up. `/run` is a tmpfs init has just mounted.
pub(crate) const BOOT_TIMELINE_PATH: &str = "/run/eclipse-boot-timeline";

/// Time since PID 1's first statement, or `None` before `main` set the origin.
pub(crate) fn since_boot() -> Option<Duration> {
    BOOT_T0.get().map(Instant::elapsed)
}

/// Run `f`, record how long it took as one row of the timeline, and answer its
/// value.
pub(crate) fn timed<T>(what: impl Into<String>, f: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let value = f();
    note_step(what, start.elapsed());
    value
}

/// Record one stretch of the boot.
pub(crate) fn note_step(what: impl Into<String>, took: Duration) {
    if BOOT_RECORDED.load(Ordering::SeqCst) {
        return;
    }
    let ended_at = since_boot().unwrap_or_default();
    // Poisoned is not a reason to lose the measurement: `unwind` means a panic
    // anywhere in a start can poison this, and a timeline with a hole in it is
    // worse than one taken from a boot that had a bug in it.
    let mut steps = BOOT_STEPS.lock().unwrap_or_else(|p| p.into_inner());
    if steps.len() >= BOOT_STEPS_MAX {
        return;
    }
    steps.push(BootStep {
        what: what.into(),
        at: ended_at.saturating_sub(took),
        took,
    });
}

/// Render the timeline, stop recording, and leave it in
/// [`BOOT_TIMELINE_PATH`]. Called once, at the end of the start loop.
pub(crate) fn report_boot_timeline() {
    let total = since_boot().unwrap_or_default();
    let steps = {
        let steps = BOOT_STEPS.lock().unwrap_or_else(|p| p.into_inner());
        steps.clone()
    };
    // After the render, not before it: a step still in flight when this runs
    // (there is none today, but `start_service` is called under `guard`) would
    // otherwise be dropped silently.
    BOOT_RECORDED.store(true, Ordering::SeqCst);
    let text = render_boot_timeline(&steps, total);
    for line in text.lines() {
        log(line);
    }
    let _ = fs::write(BOOT_TIMELINE_PATH, &text);
}

/// The boot timeline as plain text: the stretches that cost something, worst
/// first, and the time no row accounts for.
///
/// Worst-first and not chronological, because the table answers one question --
/// what is there to cut -- and the console lines above it, now stamped, already
/// give the order. `rest` is everything the rows do not cover: the forks and
/// execs, the sub-[`BOOT_STEP_FLOOR`] tail, and anything that was never timed,
/// which is the number that says whether this instrumentation is still missing
/// something.
pub(crate) fn render_boot_timeline(steps: &[BootStep], total: Duration) -> String {
    let named: Duration = steps.iter().map(|s| s.took).sum();
    let mut out = String::new();
    out.push_str(&format!(
        "boot timeline: {} to the supervision loop, {} of it waiting\n",
        secs(total),
        secs(named.min(total))
    ));
    let mut rows: Vec<&BootStep> = steps.iter().filter(|s| s.took >= BOOT_STEP_FLOOR).collect();
    // Longest first; ties broken by when they happened, so a table of equal
    // rows is stable and reads in boot order.
    rows.sort_by(|a, b| b.took.cmp(&a.took).then(a.at.cmp(&b.at)));
    for step in &rows {
        out.push_str(&format!(
            "  {:>8}  {:>3}%  at {:>8}  {}\n",
            secs(step.took),
            percent_of(step.took, total),
            secs(step.at),
            step.what
        ));
    }
    // `saturating_sub`: `total` is read after the steps, so it cannot be the
    // smaller of the two today -- but a future caller that reads it first would
    // otherwise get a row claiming the rest of the boot took 584 million years.
    let rest = total.saturating_sub(named);
    out.push_str(&format!(
        "  {:>8}  {:>3}%  {}\n",
        secs(rest),
        percent_of(rest, total),
        "          everything not timed above (forks, execs, short steps)"
    ));
    out
}

/// Seconds with three decimals: `  1.234s`. Milliseconds would need a second
/// unit for the long waits, and a boot is read in seconds.
pub(crate) fn secs(d: Duration) -> String {
    format!("{:.3}s", d.as_secs_f64())
}

/// `part` as a whole percentage of `total`, saturating at 100 and answering 0
/// for a zero total (a timeline rendered before the clock moved).
pub(crate) fn percent_of(part: Duration, total: Duration) -> u64 {
    let total = total.as_micros();
    if total == 0 {
        return 0;
    }
    ((part.as_micros() * 100) / total).min(100) as u64
}
