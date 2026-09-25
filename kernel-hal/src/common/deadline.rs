//! The timer heap, and the arithmetic that decides when a CPU's timer fires
//! next.
//!
//! Every sleep, `poll`/`select` timeout, socket retransmit and scheduled
//! wakeup in the system goes through here. Timers used to expire only on the
//! 250 Hz scheduler tick, which put a 4 ms floor under all of them; now the
//! deadline is programmed into the CPU's own timer, so a 1 ms sleep takes
//! about 1 ms. The direction is deliberately one-way — this only ever makes a
//! timer fire *sooner* than the scheduler tick would have, never later —
//! because the previous attempt at the opposite (stretching the period on an
//! idle CPU) left halted CPUs with timers that never expired, which killed
//! keyboard and mouse input.
//!
//! All of it lived in `bare/timer.rs`, which only a bare build compiles, and
//! the arithmetic half of it behind a further `cfg(target_arch = "x86_64")`.
//! Nothing that could report a mistake in it ever compiled a line, and its
//! failures are the quiet kind: a timer that fires late, or a CPU that
//! reprograms its timer four thousand times a second.

use alloc::boxed::Box;
use alloc::collections::BinaryHeap;
use alloc::vec::Vec;
use core::cmp::Ordering as CmpOrdering;
use core::convert::TryFrom;
use core::time::Duration;

/// What [`TimerHeap::next`] reports when no timer is registered, and what the
/// per-CPU fast path compares against to skip the heap lock entirely.
pub const NO_DEADLINE: u64 = u64::MAX;

/// The time a counter running at `hz` needs to advance `ticks`.
///
/// Seconds first and the remainder second, because the obvious spelling
/// (`ticks * 1_000_000_000 / hz`) overflows a `u64` after about 18 seconds of
/// a 1 GHz counter. `ticks % hz` is smaller than `hz`, so the remainder's
/// multiply is safe for any rate a real counter has.
///
/// The spelling this replaces dodged the overflow by rounding the rate down to
/// whole megahertz first, and that is where the precision went: a K210's
/// 7.8 MHz became 7, so every timestamp the kernel took ran **10% fast**, and
/// a board with a timebase under 1 MHz — a 32768 Hz one, say — rounded to zero
/// and divided by it inside the clock read.
///
/// A rate of zero still has no answer here, and a zero-length duration is the
/// one that does least damage: it reads as "no time has passed", where the
/// division would take the machine down inside the timer path.
pub fn ticks_to_duration(ticks: u64, hz: u64) -> Duration {
    if hz == 0 {
        return Duration::ZERO;
    }
    let rem = ticks % hz;
    let nanos = if hz <= u64::MAX / 1_000_000_000 {
        rem * 1_000_000_000 / hz
    } else {
        // Above ~18 GHz the multiply would overflow, and a counter that fast
        // cannot be read to nanosecond precision anyway. Scale the rate down
        // instead of the answer.
        rem / (hz / 1_000_000_000)
    };
    Duration::new(ticks / hz, nanos as u32)
}

/// How many ticks of a counter running at `hz` make one period of a `per_sec`
/// hertz tick.
///
/// Never zero. A period of zero ticks does not mean "no tick", it means the
/// timer is already due the instant it is armed — an interrupt that re-arms
/// itself as fast as the CPU can take it, with no cycles left over for the
/// thing the tick was supposed to schedule. That is what a timebase rounded
/// down to zero megahertz used to produce.
pub fn ticks_per_period(hz: u64, per_sec: u64) -> u64 {
    if per_sec == 0 {
        return hz.max(1);
    }
    (hz / per_sec).max(1)
}

/// A `Duration` as whole nanoseconds, saturating.
///
/// The saturation lands on [`NO_DEADLINE`], so a deadline further out than a
/// `u64` of nanoseconds can hold — about 584 years — reads as "no timer
/// registered". That is the right answer for the only thing it can be: a
/// wait that will not end within the life of the machine.
pub fn duration_to_ns(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(NO_DEADLINE)
}

/// Whether the earliest pending deadline has arrived.
///
/// The per-CPU tick asks this before taking the heap lock at all, which is
/// what keeps 250 Hz times however many CPUs off a single mutex.
pub fn is_due(now_ns: u64, next_ns: u64) -> bool {
    now_ns >= next_ns
}

/// How long to program a CPU's timer for, to serve a deadline at `target_ns`.
///
/// `None` means leave it alone: it is already set to fire at least this soon.
/// The answer is bounded below by `min_arm_ns`, which bounds the worst-case
/// interrupt rate the way Linux's `min_delta_ns` does, and above by the
/// scheduler tick, because preemption and the per-tick housekeeping have to
/// keep running whatever the timer heap wants.
///
/// `armed_ns` of `0` means this CPU has not been armed by this mechanism yet
/// and is treated as infinitely far away, so the first arm always takes.
pub fn arm_span(
    now_ns: u64,
    target_ns: u64,
    tick_due_ns: u64,
    armed_ns: u64,
    tick_ns: u64,
    min_arm_ns: u64,
) -> Option<u64> {
    // Before this CPU's first tick there is no recorded due time; a full
    // period from now still bounds the clamp below.
    let tick_due = if tick_due_ns == 0 {
        now_ns.saturating_add(tick_ns)
    } else {
        tick_due_ns
    };
    let target = target_ns.min(tick_due);
    // Hysteresis: reprogram only when it buys a whole `min_arm_ns`. Without
    // it a stream of timers with slightly-decreasing deadlines — a busy
    // poll/select loop, socket retransmits — reprograms on every `timer_set`,
    // and each reprogram restarts the countdown.
    if armed_ns != 0 && target.saturating_add(min_arm_ns) >= armed_ns {
        return None;
    }
    // `max` on the upper bound, not a bare `clamp`: `clamp` panics when its
    // bounds cross, and these two are separate constants — one a property of
    // the interrupt controller, the other of the scheduler — with nothing
    // tying them together. A tick rate above 5 kHz would have made every arm
    // panic, inside the timer interrupt.
    Some(
        target
            .saturating_sub(now_ns)
            .clamp(min_arm_ns, tick_ns.max(min_arm_ns)),
    )
}

/// How long to program a CPU's timer for, at the end of a scheduler tick.
///
/// `None` means there is nothing to bring the timer forward for, and the
/// freshly restored full-rate period stands.
///
/// A deadline already at or behind `now_ns` gets `None` too, and that is the
/// point: it is about to be drained by this very tick, so arming for it buys
/// nothing — and when the tick *cannot* drain it, which is what happens once
/// the smash detector makes a tick skip every indirect call, arming for a
/// deadline that stays in the past re-arms at the floor on every tick
/// forever. That is a five-kilohertz interrupt storm on a CPU that is
/// supposed to be limping along so the fault can be reported.
pub fn rearm_span(
    now_ns: u64,
    next_ns: u64,
    tick_due_ns: u64,
    tick_ns: u64,
    min_arm_ns: u64,
) -> Option<u64> {
    if next_ns == NO_DEADLINE || next_ns <= now_ns {
        return None;
    }
    arm_span(
        now_ns,
        next_ns,
        tick_due_ns,
        tick_due_ns,
        tick_ns,
        min_arm_ns,
    )
}

/// A now-relative nanosecond span in timer counts, at `hz` counts a second.
///
/// Never zero — a count of zero stops the timer outright — and never past
/// what the count register can hold.
pub fn counts_for(hz: u64, ns: u64) -> u32 {
    // A plain product: two `u64`s cannot overflow a `u128`, so the saturation
    // that used to be written here was a branch nothing could take.
    let counts = (hz as u128) * (ns as u128) / 1_000_000_000;
    counts.clamp(1, u32::MAX as u128) as u32
}

/// The counts in one scheduler tick, at `hz` counts a second.
pub fn counts_per_tick(hz: u64, ticks_per_sec: u64) -> u32 {
    if ticks_per_sec == 0 {
        return u32::MAX;
    }
    (hz / ticks_per_sec).clamp(1, u32::MAX as u64) as u32
}

/// A pending timer: its absolute deadline and the callback to run.
///
/// Ordered so the [`BinaryHeap`] (a max-heap) yields the *earliest* deadline
/// first — `cmp` is reversed on `deadline`.
struct TimerEvent {
    deadline: Duration,
    callback: Box<dyn FnOnce(Duration) + Send + Sync + 'static>,
}

impl PartialEq for TimerEvent {
    fn eq(&self, other: &Self) -> bool {
        self.deadline.eq(&other.deadline)
    }
}
impl Eq for TimerEvent {}
impl PartialOrd for TimerEvent {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}
impl Ord for TimerEvent {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        // Reverse so the min-deadline is at the top of the max-heap.
        other.deadline.cmp(&self.deadline)
    }
}

/// A minimal timer heap.
///
/// Unlike `naive_timer::Timer`, whose `expire()` runs every due callback
/// *inline while the heap is borrowed*, this type separates draining from
/// invoking: [`Self::drain_expired`] pops the due events and returns their
/// callbacks so the caller can drop the heap lock BEFORE running them. That
/// is essential — timer callbacks re-arm periodic timers by calling
/// `timer_set`, which re-locks the heap; running them under the lock is a
/// same-CPU re-entrant self-deadlock.
#[derive(Default)]
pub struct TimerHeap {
    events: BinaryHeap<TimerEvent>,
}

impl TimerHeap {
    pub fn add(
        &mut self,
        deadline: Duration,
        callback: Box<dyn FnOnce(Duration) + Send + Sync + 'static>,
    ) {
        self.events.push(TimerEvent { deadline, callback });
    }

    /// Deadline of the earliest pending timer, if any.
    pub fn next(&self) -> Option<Duration> {
        self.events.peek().map(|e| e.deadline)
    }

    /// The earliest pending deadline in nanoseconds, or [`NO_DEADLINE`].
    ///
    /// This is what gets republished for the other CPUs' fast paths, so the
    /// "nothing pending" case has to be the same number they compare against.
    pub fn next_ns(&self) -> u64 {
        self.next().map(duration_to_ns).unwrap_or(NO_DEADLINE)
    }

    /// How many timers are pending.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Pop every event whose deadline is `<= now`, returning their callbacks.
    pub fn drain_expired(
        &mut self,
        now: Duration,
    ) -> Vec<Box<dyn FnOnce(Duration) + Send + Sync + 'static>> {
        let mut ready = Vec::new();
        while let Some(t) = self.events.peek() {
            if t.deadline > now {
                break;
            }
            ready.push(self.events.pop().unwrap().callback);
        }
        ready
    }
}

/// The counter-to-clock conversion was written once per architecture and only
/// x86_64's had been fixed, in a comment that lists what the other two still
/// did: a `count * 1_000_000_000` that overflows, and a rate rounded down to
/// whole megahertz. These pin the rates real boards actually report.
#[cfg(test)]
mod tick_rate_tests {
    use super::*;

    /// QEMU `virt`, riscv64.
    const QEMU_RISCV: u64 = 10_000_000;
    /// QEMU `virt`, aarch64 — the generic timer's CNTFRQ.
    const QEMU_ARM: u64 = 62_500_000;
    /// Kendryte K210: 7.8 MHz, which is not a whole number of megahertz.
    const K210: u64 = 7_800_000;
    /// A timebase below one megahertz, which is what rounding to megahertz
    /// turned into zero.
    const RTC_32K: u64 = 32_768;

    #[test]
    fn a_counter_is_read_exactly_at_the_rates_boards_report() {
        assert_eq!(
            ticks_to_duration(QEMU_RISCV, QEMU_RISCV),
            Duration::from_secs(1)
        );
        assert_eq!(
            ticks_to_duration(QEMU_ARM, QEMU_ARM),
            Duration::from_secs(1)
        );
        assert_eq!(ticks_to_duration(K210, K210), Duration::from_secs(1));
        assert_eq!(ticks_to_duration(RTC_32K, RTC_32K), Duration::from_secs(1));
        assert_eq!(
            ticks_to_duration(QEMU_RISCV / 1000, QEMU_RISCV),
            Duration::from_millis(1)
        );
    }

    #[test]
    fn a_rate_that_is_not_a_whole_number_of_megahertz_keeps_its_precision() {
        // Rounding 7.8 MHz down to 7 is how the previous spelling read the
        // K210's counter, and 7.8/7 is 11% — every timestamp the kernel took
        // on that board ran fast by a ninth.
        let one_second = ticks_to_duration(K210, K210);
        let as_if_rounded = ticks_to_duration(K210, 7_000_000);
        assert_eq!(one_second, Duration::from_secs(1));
        assert!(
            as_if_rounded > Duration::from_millis(1100),
            "the rounded rate is meant to be visibly wrong here: {:?}",
            as_if_rounded
        );
    }

    #[test]
    fn a_rate_below_a_megahertz_still_tells_the_time() {
        // Rounded to megahertz this rate is zero, and zero is what the clock
        // read divided by.
        assert_eq!(
            ticks_to_duration(RTC_32K * 5, RTC_32K),
            Duration::from_secs(5)
        );
        assert_eq!(
            ticks_to_duration(RTC_32K / 2, RTC_32K),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn the_clock_does_not_wrap_five_minutes_into_the_boot() {
        // `count * 1_000_000_000` overflows a u64 at 18_446_744_073 counts.
        // At QEMU's 62.5 MHz that is 295 seconds, and the old spelling turned
        // the next count after it into a reading in the past.
        let overflows_at = u64::MAX / 1_000_000_000;
        let before = ticks_to_duration(overflows_at, QEMU_ARM);
        let after = ticks_to_duration(overflows_at + 1, QEMU_ARM);
        assert!(
            after > before,
            "the monotonic clock went backwards at {} counts: {:?} then {:?}",
            overflows_at,
            before,
            after
        );
        assert_eq!(before.as_secs(), 295);
    }

    #[test]
    fn the_clock_still_counts_after_a_century() {
        // A day, a year and a century at QEMU's aarch64 rate, to say the
        // arithmetic has no second cliff further out.
        for secs in [86_400u64, 31_536_000, 3_153_600_000] {
            assert_eq!(
                ticks_to_duration(secs * QEMU_ARM, QEMU_ARM),
                Duration::from_secs(secs)
            );
        }
    }

    #[test]
    fn a_counter_that_reports_no_rate_does_not_divide_by_it() {
        // Firmware that leaves CNTFRQ_EL0 at zero, or a device tree with no
        // `timebase-frequency`: the answer is wrong either way, and the one
        // that does not panic inside the clock read is the one to give.
        assert_eq!(ticks_to_duration(12_345, 0), Duration::ZERO);
    }

    #[test]
    fn a_tick_period_is_never_zero_counts() {
        assert_eq!(ticks_per_period(QEMU_RISCV, 250), 40_000);
        assert_eq!(ticks_per_period(QEMU_ARM, 250), 250_000);
        assert_eq!(
            ticks_per_period(0, 250),
            1,
            "a period of zero counts is a timer already due when it is armed, \
             which re-arms itself as fast as the CPU can take it"
        );
        assert_eq!(ticks_per_period(100, 250), 1, "a rate slower than the tick");
        assert_eq!(ticks_per_period(QEMU_ARM, 0), QEMU_ARM);
    }

    #[test]
    fn a_rate_too_high_to_express_in_nanoseconds_is_still_monotone() {
        // Above ~18 GHz the remainder's multiply would overflow. No counter
        // runs that fast, but the function is the one three architectures
        // share and it must not wrap for any of them.
        let hz = 40_000_000_000u64;
        // One nanosecond is 40 counts at this rate, and the fraction still has
        // to be worth reading: a branch that gave up and answered whole
        // seconds would make every sub-second deadline fire at the second.
        assert_eq!(ticks_to_duration(hz + 40, hz), Duration::new(1, 1));
        assert_eq!(ticks_to_duration(hz + 40_000, hz), Duration::new(1, 1_000));
        assert!(ticks_to_duration(hz * 2, hz) > ticks_to_duration(hz + 40, hz));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicU64, Ordering};

    const MS: u64 = 1_000_000;
    /// The scheduler tick, 4 ms at 250 Hz.
    const TICK: u64 = 4 * MS;
    /// The floor on how short a span may be programmed, 200 µs.
    const FLOOR: u64 = 200_000;
    const NOW: u64 = 1_000 * MS;

    fn marker(bits: &Arc<AtomicU64>, bit: u64) -> Box<dyn FnOnce(Duration) + Send + Sync> {
        let bits = bits.clone();
        Box::new(move |_| {
            bits.fetch_or(1 << bit, Ordering::Relaxed);
        })
    }

    // ── the sentinel ────────────────────────────────────────────────────────

    #[test]
    fn a_duration_becomes_whole_nanoseconds() {
        assert_eq!(duration_to_ns(Duration::from_millis(4)), TICK);
        assert_eq!(duration_to_ns(Duration::ZERO), 0);
        assert_eq!(duration_to_ns(Duration::from_nanos(1)), 1);
    }

    #[test]
    fn a_deadline_beyond_the_counter_reads_as_no_deadline_at_all() {
        // The saturation and the "nothing pending" marker are the same
        // number, and they have to be: a wait longer than five centuries is
        // the one case where those two answers mean the same thing.
        assert_eq!(duration_to_ns(Duration::MAX), NO_DEADLINE);
        assert!(!is_due(u64::MAX - 1, NO_DEADLINE));
    }

    #[test]
    fn a_deadline_is_due_the_instant_it_arrives_and_not_before() {
        assert!(!is_due(NOW - 1, NOW));
        assert!(is_due(NOW, NOW));
        assert!(is_due(NOW + 1, NOW));
    }

    // ── arming for a deadline ───────────────────────────────────────────────

    #[test]
    fn a_deadline_inside_the_tick_is_armed_for_its_own_distance() {
        assert_eq!(
            arm_span(NOW, NOW + MS, NOW + TICK, 0, TICK, FLOOR),
            Some(MS)
        );
    }

    #[test]
    fn a_deadline_past_the_tick_is_cut_back_to_the_tick() {
        // Preemption and the per-tick housekeeping have to keep running
        // whatever the timer heap wants.
        assert_eq!(
            arm_span(NOW, NOW + 100 * MS, NOW + TICK, 0, TICK, FLOOR),
            Some(TICK)
        );
    }

    #[test]
    fn a_deadline_nearer_than_the_floor_is_armed_at_the_floor() {
        // Chasing it would push this CPU's interrupt rate past five kilohertz
        // for no gain, which is what Linux's `min_delta_ns` is for.
        assert_eq!(
            arm_span(NOW, NOW + 1_000, NOW + TICK, 0, TICK, FLOOR),
            Some(FLOOR)
        );
        assert_eq!(arm_span(NOW, NOW, NOW + TICK, 0, TICK, FLOOR), Some(FLOOR));
    }

    #[test]
    fn a_deadline_already_past_is_served_as_soon_as_the_floor_allows() {
        // From `timer_set` this is a caller asking for a callback it already
        // wanted, so the answer is "as soon as possible", not "never".
        assert_eq!(
            arm_span(NOW, NOW - 50 * MS, NOW + TICK, 0, TICK, FLOOR),
            Some(FLOOR)
        );
    }

    #[test]
    fn a_cpu_that_has_never_been_armed_always_arms() {
        // Zero is the "not yet armed by this mechanism" marker and has to
        // read as infinitely far away, or the first arm would never take.
        assert_eq!(
            arm_span(NOW, NOW + MS, NOW + TICK, 0, TICK, FLOOR),
            Some(MS)
        );
    }

    #[test]
    fn an_arm_that_buys_less_than_the_floor_is_not_worth_reprogramming() {
        // A poll/select loop sets timers whose deadlines creep down by
        // microseconds; each reprogram restarts the countdown and costs an
        // MMIO write.
        let armed = NOW + MS;
        assert_eq!(
            arm_span(NOW, NOW + MS - FLOOR / 2, NOW + TICK, armed, TICK, FLOOR),
            None
        );
        assert_eq!(
            arm_span(NOW, NOW + MS, NOW + TICK, armed, TICK, FLOOR),
            None
        );
        assert_eq!(
            arm_span(NOW, NOW + MS + MS, NOW + TICK, armed, TICK, FLOOR),
            None,
            "a later deadline never pushes the timer out"
        );
        assert_eq!(
            arm_span(NOW, NOW + MS - FLOOR, NOW + TICK, armed, TICK, FLOOR),
            None,
            "exactly the floor is not more than the floor"
        );
    }

    #[test]
    fn an_arm_that_buys_more_than_the_floor_does_reprogram() {
        let armed = NOW + MS;
        assert_eq!(
            arm_span(NOW, NOW + MS - FLOOR - 1, NOW + TICK, armed, TICK, FLOOR),
            Some(MS - FLOOR - 1)
        );
    }

    #[test]
    fn before_a_cpus_first_tick_a_whole_period_bounds_the_arm() {
        // There is no recorded due time yet, and without a bound the clamp
        // below would have nothing to cut an over-long deadline back to.
        assert_eq!(arm_span(NOW, NOW + 100 * MS, 0, 0, TICK, FLOOR), Some(TICK));
        assert_eq!(arm_span(NOW, NOW + MS, 0, 0, TICK, FLOOR), Some(MS));
    }

    #[test]
    fn a_tick_shorter_than_the_floor_does_not_panic() {
        // The two bounds are separate constants — one a property of the
        // interrupt controller, the other of the scheduler — with nothing
        // tying them together, and `clamp` panics when its bounds cross. A
        // tick rate above five kilohertz would have panicked inside the timer
        // interrupt, on every arm.
        let tiny = FLOOR / 2;
        assert_eq!(
            arm_span(NOW, NOW + MS, NOW + tiny, 0, tiny, FLOOR),
            Some(FLOOR)
        );
        assert_eq!(arm_span(NOW, NOW, 0, 0, tiny, FLOOR), Some(FLOOR));
    }

    #[test]
    fn a_deadline_at_the_far_end_of_the_counter_still_arms_within_the_tick() {
        assert_eq!(
            arm_span(NOW, NO_DEADLINE, NOW + TICK, 0, TICK, FLOOR),
            Some(TICK)
        );
        // And the fallback path, where the tick due time has to be computed.
        assert_eq!(
            arm_span(u64::MAX - 1, NO_DEADLINE, 0, 0, TICK, FLOOR),
            Some(FLOOR)
        );
        // A CPU already armed at the far end: the hysteresis adds the floor to
        // a target that is already the largest number there is.
        assert_eq!(
            arm_span(u64::MAX - 1, NO_DEADLINE, u64::MAX, u64::MAX, TICK, FLOOR),
            None
        );
    }

    // ── re-arming at the end of a tick ──────────────────────────────────────

    #[test]
    fn nothing_pending_leaves_the_restored_period_alone() {
        assert_eq!(rearm_span(NOW, NO_DEADLINE, NOW + TICK, TICK, FLOOR), None);
    }

    #[test]
    fn a_deadline_already_due_is_not_re_armed_for() {
        // It is about to be drained by this very tick, so arming buys
        // nothing. And when the tick *cannot* drain it — which is what
        // happens once the smash detector makes a tick skip every indirect
        // call — a deadline that stays in the past re-armed at the floor on
        // every tick, forever: a five-kilohertz interrupt storm on a CPU that
        // is meant to be limping along so the fault can be reported.
        assert_eq!(rearm_span(NOW, NOW - MS, NOW + TICK, TICK, FLOOR), None);
        assert_eq!(rearm_span(NOW, NOW, NOW + TICK, TICK, FLOOR), None);
    }

    #[test]
    fn a_deadline_inside_the_tick_brings_the_timer_forward() {
        assert_eq!(rearm_span(NOW, NOW + MS, NOW + TICK, TICK, FLOOR), Some(MS));
    }

    #[test]
    fn a_deadline_at_or_past_the_tick_needs_no_re_arm() {
        // The tick already fires then; the period was just restored.
        assert_eq!(rearm_span(NOW, NOW + TICK, NOW + TICK, TICK, FLOOR), None);
        assert_eq!(
            rearm_span(NOW, NOW + 10 * TICK, NOW + TICK, TICK, FLOOR),
            None
        );
    }

    // ── nanoseconds into timer counts ───────────────────────────────────────

    #[test]
    fn a_span_becomes_counts_at_the_measured_rate() {
        assert_eq!(counts_for(1_000_000_000, MS), 1_000_000);
        assert_eq!(counts_for(100_000_000, TICK), 400_000);
        // A rate past what a 32-bit count can hold is ordinary — a 5 GHz TSC
        // is the fallback rate when the LAPIC has not been calibrated.
        assert_eq!(counts_for(5_000_000_000, MS), 5_000_000);
    }

    #[test]
    fn a_count_is_never_zero_because_zero_stops_the_timer() {
        assert_eq!(counts_for(1_000_000_000, 0), 1);
        assert_eq!(counts_for(0, MS), 1);
        assert_eq!(counts_per_tick(0, 250), 1);
    }

    #[test]
    fn a_count_never_overflows_the_register() {
        assert_eq!(counts_for(u64::MAX, u64::MAX), u32::MAX);
        assert_eq!(counts_per_tick(u64::MAX, 1), u32::MAX);
        assert_eq!(counts_per_tick(1_000_000_000, 0), u32::MAX);
    }

    #[test]
    fn a_whole_tick_of_nanoseconds_is_a_whole_tick_of_counts() {
        // The two are computed by different routes and programmed into the
        // same register; a machine whose scheduler tick and deadline arms
        // disagreed about how long 4 ms is would drift one against the other.
        for hz in [25_000_000u64, 100_000_000, 1_000_000_000, 3_000_000_000] {
            assert_eq!(counts_for(hz, TICK), counts_per_tick(hz, 250), "hz={hz}");
        }
    }

    // ── the heap ────────────────────────────────────────────────────────────

    #[test]
    fn the_earliest_deadline_comes_out_first_however_they_went_in() {
        let bits = Arc::new(AtomicU64::new(0));
        let mut h = TimerHeap::default();
        h.add(Duration::from_millis(30), marker(&bits, 2));
        h.add(Duration::from_millis(10), marker(&bits, 0));
        h.add(Duration::from_millis(20), marker(&bits, 1));
        assert_eq!(h.next(), Some(Duration::from_millis(10)));
        assert_eq!(h.len(), 3);
        for cb in h.drain_expired(Duration::from_millis(25)) {
            cb(Duration::ZERO);
        }
        assert_eq!(bits.load(Ordering::Relaxed), 0b011);
        assert_eq!(h.next(), Some(Duration::from_millis(30)));
    }

    #[test]
    fn a_deadline_exactly_now_is_expired_and_the_next_one_is_not() {
        let bits = Arc::new(AtomicU64::new(0));
        let mut h = TimerHeap::default();
        h.add(Duration::from_millis(10), marker(&bits, 0));
        h.add(Duration::from_nanos(10 * MS + 1), marker(&bits, 1));
        let due = h.drain_expired(Duration::from_millis(10));
        assert_eq!(due.len(), 1);
        for cb in due {
            cb(Duration::ZERO);
        }
        assert_eq!(bits.load(Ordering::Relaxed), 0b001);
    }

    #[test]
    fn two_timers_on_the_same_deadline_both_come_out() {
        let bits = Arc::new(AtomicU64::new(0));
        let mut h = TimerHeap::default();
        h.add(Duration::from_millis(10), marker(&bits, 0));
        h.add(Duration::from_millis(10), marker(&bits, 1));
        for cb in h.drain_expired(Duration::from_millis(10)) {
            cb(Duration::ZERO);
        }
        assert_eq!(bits.load(Ordering::Relaxed), 0b011);
        assert!(h.is_empty());
    }

    #[test]
    fn an_empty_heap_publishes_the_no_deadline_marker() {
        let mut h = TimerHeap::default();
        assert_eq!(h.next_ns(), NO_DEADLINE);
        assert!(h.drain_expired(Duration::from_secs(1)).is_empty());
        assert_eq!(h.next_ns(), NO_DEADLINE);
    }

    #[test]
    fn draining_republishes_the_new_earliest_deadline() {
        // What the other CPUs' fast paths read, so it has to be the state
        // after the drain and not before it.
        let bits = Arc::new(AtomicU64::new(0));
        let mut h = TimerHeap::default();
        h.add(Duration::from_millis(10), marker(&bits, 0));
        h.add(Duration::from_millis(40), marker(&bits, 1));
        assert_eq!(h.next_ns(), 10 * MS);
        drop(h.drain_expired(Duration::from_millis(10)));
        assert_eq!(h.next_ns(), 40 * MS);
    }

    #[test]
    fn a_tick_that_expires_nothing_leaves_every_timer_where_it_was() {
        let bits = Arc::new(AtomicU64::new(0));
        let mut h = TimerHeap::default();
        h.add(Duration::from_millis(10), marker(&bits, 0));
        assert!(h.drain_expired(Duration::from_millis(9)).is_empty());
        assert_eq!(h.len(), 1);
        assert_eq!(bits.load(Ordering::Relaxed), 0);
    }
}
