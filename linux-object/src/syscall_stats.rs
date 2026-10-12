//! Per-syscall call counters, exposed as `/proc/syscalls`.
//!
//! A process that burns system time without making progress says nothing about
//! itself in a log: the syscalls it spins on are implemented ones returning
//! promptly, so there is no error to print. Counting them is the only way to
//! see the loop. Firefox's socket process sat at 83% system time on this
//! kernel with no other symptom, and this is what it took to name the call.
//!
//! One relaxed increment per syscall, and nothing else on the hot path.
//! Entries are raw x86_64 syscall numbers (`asm/unistd_64.h`); reading them
//! twice and subtracting is what turns them into a profile.
//!
//! That sentence used to be false in the way that matters. Beside the
//! histogram there is a table naming each process's most recent call, and it
//! was a ring with a **single global head**: one `AtomicUsize`, one
//! cacheline, `fetch_add`-ed by every thread on the machine on every syscall,
//! unconditionally. `record` benched at 13 ns alone and **313 ns with three
//! other threads recording other syscall numbers** -- a 24x blow-up whose
//! only shared word was that head. It is gone: the table is now indexed by
//! pid, which is both lock-free in the sense the paragraph above claims and a
//! better answer to the question the table exists for. See
//! [`benches`][self::benches] for the rows and
//! [`TRACE`] for why the index is the right one.

use core::sync::atomic::{AtomicU64, Ordering};

/// Above every number in the Linux x86_64 table (and the arch-specific ones
/// this tree adds); anything beyond is counted in the last slot.
pub const MAX_SYSCALL: usize = 512;

/// How many copies of the histogram there are: one per possible CPU.
///
/// The histogram is the one thing here that **cannot** be indexed by process
/// -- a system-wide count is the thing it is -- so the only way to stop CPUs
/// serialising on `COUNTS[num]` is to give each its own row and add them up
/// when somebody reads the file. The cost is `MAX_SYSCALL * 8 * BANKS` =
/// **256 KiB of `.bss`**, against a kernel whose `.bss` is already measured
/// in hundreds of megabytes, and 32768 relaxed loads on a `/proc/syscalls`
/// read, which happens when a human asks.
///
/// One per possible CPU and not a smaller number on purpose: a bank shared
/// by two CPUs is the same contention at a quarter of the memory, and
/// picking which CPUs share it is a question with no good answer that would
/// be asked again every time the core count changed.
const COUNT_BANKS: usize = kernel_hal::config::MAX_CORE_NUM;

/// `& (COUNT_BANKS - 1)` is the index, so the count has to be a power of two.
/// Checked where a mistake cannot ship.
const _: () = assert!(COUNT_BANKS.is_power_of_two());

/// All banks off, which is what `SYSCALLBANKS=off` leaves behind: every CPU
/// masks to bank 0 and the table behaves exactly as it did before the banks
/// existed. A mask rather than a flag so the hot path is one load and an
/// `and` either way, with no branch to predict -- and so the A/B Moebius
/// runs on hardware measures the banks and not a branch.
static BANK_MASK: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(COUNT_BANKS - 1);

/// Turn the per-CPU banks off (`SYSCALLBANKS=off`), or back on. Boot knob.
pub fn set_banks_enabled(on: bool) {
    BANK_MASK.store(if on { COUNT_BANKS - 1 } else { 0 }, Ordering::Relaxed);
}

/// Whether the per-CPU banks are on.
pub fn banks_enabled() -> bool {
    BANK_MASK.load(Ordering::Relaxed) != 0
}

/// Which copy of the histogram a given CPU counts into.
///
/// `cpu_id()` is a `u8`, so it can name a CPU past the array; the mask is
/// what bounds it, and it is the same mask the knob turns off.
///
/// Takes the CPU rather than asking, so the mask has a test: on the host
/// `kernel_hal::cpu::cpu_id()` is a constant 0 -- libos has one logical CPU
/// -- so a [`count_bank`] that dropped the mask entirely would answer 0 here
/// either way, and nothing in a `cargo test` could tell.
#[inline]
fn bank_of_cpu(cpu: u8) -> usize {
    (cpu as usize) & BANK_MASK.load(Ordering::Relaxed)
}

/// Which copy of the histogram this CPU counts into.
#[inline]
fn count_bank() -> usize {
    bank_of_cpu(kernel_hal::cpu::cpu_id())
}

static COUNTS: [[AtomicU64; MAX_SYSCALL]; COUNT_BANKS] =
    [const { [const { AtomicU64::new(0) }; MAX_SYSCALL] }; COUNT_BANKS];

/// How many processes' last calls the table remembers.
pub const TRACE_LEN: usize = 256;

/// Each process's most recent call, as `(pid << 32) | num`, in a slot chosen
/// by the pid. Zero means "never written".
///
/// The histogram says what is being called a lot; this says what each process
/// did LAST. When a hang leaves every process asleep, that is the question
/// worth asking -- this names the syscall each one went to sleep in.
///
/// **Indexed by pid, and that is the fix as well as the point.** This was a
/// ring ordered by a single global head, and ordering it cost one
/// machine-wide `fetch_add` per syscall (see this module's documentation for
/// the 13 ns / 313 ns pair). The order was never what the table was for, and
/// buying it was worse than useless: a ring is filled by whoever is *busiest*,
/// so one spinning process could own all 256 slots and hide every sleeping
/// one -- the exact case the paragraph above says this exists for. A slot per
/// pid answers the question with one relaxed store, keeps the quiet processes
/// visible, and shares a line only with the threads of its own process.
///
/// Two pids whose slots collide overwrite each other; with 256 slots that
/// starts at a few dozen live processes, and the loser is a process whose
/// last call is then simply absent rather than wrong.
static TRACE: [AtomicU64; TRACE_LEN] = [const { AtomicU64::new(0) }; TRACE_LEN];

/// Which slot a pid's last call lives in.
///
/// Pids here are Zircon KoIDs, which are allocated densely from a counter, so
/// the low bits alone would be a decent spread -- but they are handed out to
/// every kernel object, not just processes, so a run of live processes can
/// share low bits more than it looks. One multiply of the 64-bit
/// Fibonacci-hashing constant mixes the high bits down before the mask, which
/// costs one `imul` and removes the question.
#[inline]
fn trace_slot(pid: u64) -> usize {
    const MIX: u64 = 0x9e37_79b9_7f4a_7c15;
    (pid.wrapping_mul(MIX) >> 56) as usize % TRACE_LEN
}

/// Count one call of syscall `num` by process `pid`.
#[inline]
pub fn record(pid: u64, num: u32) {
    record_in_bank(count_bank(), pid, num)
}

/// The body of [`record`], with the bank handed in.
///
/// Split out so the benches can measure what two CPUs counting the same
/// syscall cost **against** two CPUs counting it into their own banks. On
/// the host `kernel_hal::cpu::cpu_id()` is a constant 0 -- libos has one
/// logical CPU -- so every thread of a `cargo bench` would land in bank 0
/// and the rows would show the contention this change is meant to remove.
/// The seam is what makes the claim measurable here; `SYSCALLBANKS=off` is
/// what makes it measurable on real hardware.
#[inline]
fn record_in_bank(bank: usize, pid: u64, num: u32) {
    let idx = (num as usize).min(MAX_SYSCALL - 1);
    COUNTS[bank & (COUNT_BANKS - 1)][idx].fetch_add(1, Ordering::Relaxed);
    TRACE[trace_slot(pid)].store(
        ((pid & 0xffff_ffff) << 32) | (num as u64 | 0x8000_0000),
        Ordering::Relaxed,
    );
}

/// How many times syscall `num` has been counted **in one bank**.
///
/// Test-only: production has no reason to look at a single CPU's row, and
/// the sum is what the file prints. It exists because the totals cannot see
/// a bank index that is merely *wrong*: a mask that folded bank 63 onto bank
/// 0 would keep every total exact and quietly put two CPUs back on one
/// cacheline, which is the whole thing this table is cut up to avoid.
#[cfg(test)]
fn calls_in_bank(bank: usize, num: u32) -> u64 {
    let idx = (num as usize).min(MAX_SYSCALL - 1);
    COUNTS[bank][idx].load(Ordering::Relaxed)
}

/// How many times syscall `num` has been called, across every bank.
pub fn calls_of(num: u32) -> u64 {
    let idx = (num as usize).min(MAX_SYSCALL - 1);
    COUNTS
        .iter()
        .map(|bank| bank[idx].load(Ordering::Relaxed))
        .sum()
}

/// Each process's most recent call, as `(pid, syscall)`.
pub fn trace() -> alloc::vec::Vec<(u64, u32)> {
    let mut out = alloc::vec::Vec::with_capacity(TRACE_LEN);
    for slot in TRACE.iter() {
        let v = slot.load(Ordering::Relaxed);
        if v != 0 {
            out.push((v >> 32, (v as u32) & 0x7fff_ffff));
        }
    }
    out
}

/// Every syscall seen at least once, as `(number, calls)`, busiest first.
pub fn snapshot() -> alloc::vec::Vec<(u32, u64)> {
    // Summed across the banks. Not a consistent snapshot of anything: a CPU
    // can count into a bank already added while another is still being read,
    // so a number can be short by a few calls. That was already true of a
    // single row read while the machine ran, and the file is read twice and
    // subtracted anyway.
    let mut v: alloc::vec::Vec<(u32, u64)> = (0..MAX_SYSCALL)
        .filter_map(|i| {
            let n: u64 = COUNTS
                .iter()
                .map(|bank| bank[i].load(Ordering::Relaxed))
                .sum();
            (n > 0).then_some((i as u32, n))
        })
        .collect();
    v.sort_unstable_by_key(|a| core::cmp::Reverse(a.1));
    v
}

#[cfg(test)]
mod tests {
    //! The per-process last-call table, and the property the ring it
    //! replaced got wrong.

    use super::*;
    use alloc::vec::Vec;

    /// Pids the `benches` module in this file records under. They write to
    /// the same static table, `cargo test` runs a `#[bench]` once each, and
    /// the suite runs in parallel -- so a test pid that lands in one of their
    /// slots can have its entry overwritten mid-test. The tests pick around
    /// them rather than hoping.
    const BENCH_PIDS: &[u64] = &[1, 900, 901, 902];

    /// `n` pids whose slots are distinct and clear of the bench pids.
    ///
    /// Bounded, and that is not decoration: the first version searched `while
    /// out.len() < n`, and a mutation that made every pid hash to the same
    /// slot turned it into an **infinite loop** -- the mutant was caught, but
    /// as a hang rather than as a failure, which in CI is a job that burns
    /// its whole timeout and names nothing. A helper that cannot find what it
    /// needs says so.
    fn unclaimed_pids(n: usize) -> Vec<u64> {
        let mut taken: Vec<usize> = BENCH_PIDS.iter().map(|p| trace_slot(*p)).collect();
        let mut out = Vec::new();
        for pid in 500_000u64..500_000 + (TRACE_LEN as u64 * 64) {
            if out.len() == n {
                return out;
            }
            let slot = trace_slot(pid);
            if !taken.contains(&slot) {
                taken.push(slot);
                out.push(pid);
            }
        }
        assert_eq!(
            out.len(),
            n,
            "no {} pids with distinct slots in a range of {}: the slot mix \
             is collapsing everything onto a few slots",
            n,
            TRACE_LEN * 64
        );
        out
    }

    fn last_call_of(pid: u64) -> Option<u32> {
        trace().into_iter().find(|(p, _)| *p == pid).map(|(_, n)| n)
    }

    /// A process's entry is its **latest** call, not a row per call: the
    /// table holds one slot per process and a later syscall replaces the
    /// earlier one.
    #[test]
    fn a_process_entry_is_its_most_recent_call() {
        let pid = unclaimed_pids(1)[0];
        record(pid, 0);
        assert_eq!(last_call_of(pid), Some(0));
        record(pid, 202);
        assert_eq!(last_call_of(pid), Some(202));
        // And the count of entries for it stays one: a ring would have two.
        assert_eq!(trace().into_iter().filter(|(p, _)| *p == pid).count(), 1);
    }

    /// **The property the ring got wrong.** A process spinning on syscalls
    /// must not push a quiet process's last call out of the table.
    ///
    /// With a single global head, the slots went to whoever was busiest, so
    /// one process making `TRACE_LEN` calls owned the whole table -- and the
    /// table's entire reason to exist is to name what the *sleeping*
    /// processes are asleep in, which is to say the quiet ones. Indexing by
    /// pid is what makes this hold.
    #[test]
    fn a_busy_process_does_not_evict_a_quiet_one() {
        let pids = unclaimed_pids(2);
        let (quiet, busy) = (pids[0], pids[1]);
        record(quiet, 23);
        // Four times the table's length, which under the old ring was four
        // full laps of it.
        for _ in 0..(TRACE_LEN * 4) {
            record(busy, 0);
        }
        assert_eq!(
            last_call_of(quiet),
            Some(23),
            "the quiet process's last call was evicted by the busy one"
        );
        assert_eq!(last_call_of(busy), Some(0));
    }

    /// Dense pids land in different slots. Zircon hands KoIDs out of one
    /// counter to every kernel object, so a run of live processes is a run of
    /// *sparse* numbers out of a dense sequence; the mix is what stops the
    /// table from degenerating into a few used slots.
    #[test]
    fn a_run_of_pids_spreads_across_the_table() {
        let mut slots: Vec<usize> = (1_000u64..1_000 + TRACE_LEN as u64)
            .map(trace_slot)
            .collect();
        slots.sort_unstable();
        slots.dedup();
        // A perfect hash would give TRACE_LEN distinct slots out of TRACE_LEN
        // pids; a hash is not perfect, and the birthday bound puts the
        // expectation near 0.63 of them. Well above half is the honest
        // assertion, and it is far enough above "a handful" to fail loudly if
        // the mix is dropped (the low bits of a dense run would give a
        // perfect spread, and the TOP bits of one would give exactly one
        // slot, which is the failure this guards).
        assert!(
            slots.len() > TRACE_LEN / 2,
            "{} distinct slots for {} consecutive pids",
            slots.len(),
            TRACE_LEN
        );
    }

    /// Every slot is reachable: a hash that could not produce some index
    /// would shrink the table silently.
    #[test]
    fn every_slot_can_be_reached() {
        let mut seen = alloc::vec![false; TRACE_LEN];
        for pid in 0..200_000u64 {
            seen[trace_slot(pid)] = true;
        }
        assert!(seen.iter().all(|s| *s), "some slot is unreachable");
    }

    /// A slot nothing has written is not an entry.
    ///
    /// The table is 256 slots and a live machine has fewer processes than
    /// that, so most of it is zero at any moment; `trace` has to tell an
    /// untouched slot from a real one. It does it by the marker bit the
    /// packing sets, which is why a recorded call for pid 0 -- syscall 0 of
    /// process 0, every field zero -- is still visible. Without the check,
    /// `/proc/syscalls` would print `0/0` two hundred times.
    #[test]
    fn an_untouched_slot_is_not_reported_but_a_zero_entry_is() {
        assert!(
            !trace().iter().any(|(pid, _)| *pid == 0),
            "an unwritten slot is being reported as process 0"
        );
        record(0, 0);
        assert_eq!(
            trace().into_iter().filter(|(pid, _)| *pid == 0).count(),
            1,
            "a recorded call for pid 0 is indistinguishable from an empty slot"
        );
    }

    /// The histogram still counts, and counts the call it was given.
    ///
    /// Syscall 500 is used by nothing else in this crate -- not by the rows
    /// in `benches` below, which use 0 and the 80..=112 range -- so the delta
    /// over this test is this test's own. Asked as a delta and not as an
    /// absolute, because `COUNTS` is a static that outlives one test.
    #[test]
    fn a_recorded_call_reaches_the_histogram() {
        const MINE: u32 = 500;
        let before = calls_of(MINE);
        let pid = unclaimed_pids(1)[0];
        for _ in 0..7 {
            record(pid, MINE);
        }
        assert_eq!(calls_of(MINE) - before, 7);
        assert_eq!(
            snapshot()
                .into_iter()
                .find(|(n, _)| *n == MINE)
                .map(|(_, c)| c),
            Some(before + 7)
        );
    }

    /// Every bank a CPU can name is one that exists, and the mask is what
    /// makes that true. `cpu_id()` is a `u8`, so it can say 200.
    #[test]
    fn every_bank_a_cpu_can_name_is_inside_the_array() {
        let was = banks_enabled();
        set_banks_enabled(true);
        for cpu in [0u8, 1, 63, 64, 200, 255] {
            let bank = bank_of_cpu(cpu);
            assert!(bank < COUNT_BANKS, "cpu {} named bank {}", cpu, bank);
        }
        assert_eq!(1usize << COUNT_BANKS.trailing_zeros(), COUNT_BANKS);
        // And the first `COUNT_BANKS` CPUs each get one of their own, which
        // is the point: a mask that collapsed two of them would keep every
        // total exact and put two CPUs back on one cacheline.
        let banks: alloc::collections::BTreeSet<usize> =
            (0..COUNT_BANKS).map(|c| bank_of_cpu(c as u8)).collect();
        assert_eq!(
            banks.len(),
            COUNT_BANKS,
            "{} CPUs share {} banks between them",
            COUNT_BANKS,
            banks.len()
        );
        set_banks_enabled(was);
    }

    /// Distinct bank arguments have to reach distinct rows. The totals
    /// cannot see this: a bank index that folds 63 onto 0 keeps every sum
    /// exact and silently undoes the change.
    #[test]
    fn distinct_banks_land_in_distinct_rows() {
        const MINE: u32 = 503;
        let pid = unclaimed_pids(1)[0];
        let before: alloc::vec::Vec<u64> =
            (0..COUNT_BANKS).map(|b| calls_in_bank(b, MINE)).collect();
        // The last bank especially: an off-by-one in the mask takes that one.
        for bank in [0, 1, COUNT_BANKS / 2, COUNT_BANKS - 1] {
            record_in_bank(bank, pid, MINE);
            assert_eq!(
                calls_in_bank(bank, MINE) - before[bank],
                1,
                "a call for bank {} did not land in bank {}",
                bank,
                bank
            );
        }
    }

    /// The whole point: a call counted into any bank is in the total. A bank
    /// the reader forgets is a syscall that stops being counted, which looks
    /// exactly like a process that stopped making it.
    #[test]
    fn a_call_in_any_bank_is_in_the_total() {
        const MINE: u32 = 501;
        let before = calls_of(MINE);
        let pid = unclaimed_pids(1)[0];
        // The first, the last, and one in the middle: a reader that walked
        // only the first bank, or stopped one short, fails here.
        for bank in [0, COUNT_BANKS / 2, COUNT_BANKS - 1] {
            record_in_bank(bank, pid, MINE);
        }
        assert_eq!(
            calls_of(MINE) - before,
            3,
            "a bank is not being added up: counted {} of 3",
            calls_of(MINE) - before
        );
        assert_eq!(
            snapshot()
                .into_iter()
                .find(|(n, _)| *n == MINE)
                .map(|(_, c)| c),
            Some(before + 3),
            "/proc/syscalls disagrees with calls_of"
        );
    }

    /// A bank index past the array is masked rather than panicking. The
    /// production caller cannot produce one today (`count_bank` masks), but
    /// the body takes a number and the mask is the only thing between it and
    /// a kernel panic on a `/proc` write away.
    #[test]
    fn a_bank_past_the_array_is_masked_and_not_a_panic() {
        const MINE: u32 = 502;
        let before = calls_of(MINE);
        let pid = unclaimed_pids(1)[0];
        record_in_bank(COUNT_BANKS, pid, MINE);
        record_in_bank(usize::MAX, pid, MINE);
        assert_eq!(calls_of(MINE) - before, 2);
    }

    /// The knob: off is the old behaviour (every CPU on bank 0), on is one
    /// bank per CPU. Both are a mask, so neither costs a branch.
    #[test]
    fn the_knob_collapses_every_cpu_onto_one_bank_and_back() {
        let was = banks_enabled();
        set_banks_enabled(false);
        assert!(!banks_enabled());
        for cpu in [0u8, 1, 7, 63, 255] {
            assert_eq!(
                bank_of_cpu(cpu),
                0,
                "with the banks off cpu {} still counts into a bank of its own",
                cpu
            );
        }
        set_banks_enabled(true);
        assert!(banks_enabled());
        assert_eq!(bank_of_cpu(7), 7, "the mask did not come back");
        assert_eq!(count_bank(), bank_of_cpu(kernel_hal::cpu::cpu_id()));
        set_banks_enabled(was);
    }

    /// A syscall number past the table is counted in the last slot rather
    /// than indexing out of bounds -- the `min` in `record`.
    #[test]
    fn a_syscall_number_past_the_table_lands_in_the_last_slot() {
        let last = (MAX_SYSCALL - 1) as u32;
        let before = calls_of(last);
        let pid = unclaimed_pids(1)[0];
        record(pid, u32::MAX);
        assert_eq!(calls_of(last) - before, 1);
        // And the entry keeps the number it was given, masked the way the
        // packing does: the pid must still be readable beside it.
        assert_eq!(
            trace().into_iter().find(|(p, _)| *p == pid).map(|(p, _)| p),
            Some(pid)
        );
    }
}

#[cfg(test)]
mod benches {
    //! What counting a syscall costs, and what it costs when every CPU is
    //! counting one.
    //!
    //! ## Why there are three rows and not one
    //!
    //! `record` does two atomic operations: the histogram slot, and the
    //! per-process last-call entry. One is shared by every CPU making the
    //! *same* syscall; the other only by the threads of one process. It used
    //! to do a third -- a `fetch_add` on a single global ring head, by every
    //! thread on the machine, on every syscall -- and a single-threaded row
    //! cannot see what that costs, because an uncontended `lock xadd` on a
    //! line this core already owns is a handful of cycles. So the rows come
    //! in three and the differences between them are the measurement:
    //!
    //! 1. alone -- the figure a one-core bench reports;
    //! 2. while other threads count the **same** syscall number, which
    //!    shares `COUNTS[num]`;
    //! 3. while other threads count **different** syscall numbers, which now
    //!    shares nothing at all.
    //!
    //! ## What they said, before and after
    //!
    //! | row | with the global head | indexed by pid |
    //! |---|---|---|
    //! | alone | 13.03 ns | **8.50 ns** |
    //! | others, different syscalls | 313.41 ns | **8.52 ns** |
    //! | others, same syscall | 422.07 ns | **87.09 ns** |
    //!
    //! Row 2 is the one that mattered: nothing was shared there except the
    //! head, and it cost 300 ns -- more than an order of magnitude above
    //! everything else on the syscall entry path put together (the hunter
    //! gate is 5.95 ns, the two clock reads about 22). It is now within noise
    //! of the uncontended figure, which is what "shares nothing" should read
    //! like. The head is gone; see [`TRACE`].
    //!
    //! Row 3 was the histogram, and it is the one this file carries now.
    //! Four threads making the same syscall serialised on `COUNTS[num]`.
    //! It cannot be indexed by process -- a system-wide count is the thing
    //! it is -- so it is indexed by **CPU**, one copy of the histogram each,
    //! added up when somebody reads `/proc/syscalls`.
    //!
    //! ## Deciding it was worth 256 KiB
    //!
    //! The saturation figure is the one that comes out of this module by
    //! default, and it is the one that could not decide anything: 87 ns, and
    //! re-run it says 100 with a band of 68. So the row was taken at each
    //! thread count instead:
    //!
    //! | threads on the same syscall | ns |
    //! |---|---|
    //! | 1 (alone) | 8.50 |
    //! | 2 | **32.59** |
    //! | 3 | 50.28 |
    //! | 4 | 84-100 |
    //!
    //! **Two** is the row that decided it: +24 ns per syscall on a two-core
    //! machine, which is the same order as the two clock reads the previous
    //! change took off this path, and it is stable (+/- 8) where the
    //! saturation row is not. A four-point curve costs three bench passes
    //! and turns "87 ns under saturation, worth arguing about" into a
    //! decision.
    //!
    //! ## What the banks cost and buy
    //!
    //! ```text
    //! record_alone                                              8.81
    //! record_while_others_count_different_syscalls              8.56
    //! record_while_others_count_the_same_syscall               84.15
    //! ..._the_same_syscall_in_their_own_banks                   8.56   <--
    //! count_bank_of_this_cpu                                    0.94
    //! record_shaped_control                                     0.62
    //! ```
    //!
    //! The pair is the claim: the same four threads on the same syscall
    //! number, differing in nothing but which bank they count into, and the
    //! own-banks row lands on `record_alone`. The price is `count_bank`,
    //! 0.94 against a 0.62 control, so about **0.3 ns on every syscall**
    //! whether anything is contending or not.
    //!
    //! `cpu_id()` is a constant 0 on this host -- libos has one logical CPU
    //! -- so the bank has to be handed in for the rows to differ at all.
    //! That is what [`record_in_bank`] is for, and `SYSCALLBANKS=off` is the
    //! same A/B on real hardware, where the CPU ids are real.
    //!
    //! **Read the contended rows with the container in mind.** This host has
    //! 4 cores and is shared, the background threads are neither pinned nor
    //! guaranteed a core, and they spin on `record` with no work in between,
    //! which no real workload does. So a contended figure here is a
    //! saturation point, not a prediction: what the rows establish is the
    //! *shape* -- which word is shared by what -- and that is a property of
    //! the code rather than of this machine.

    use super::*;
    use test::{black_box, Bencher};

    extern crate std;
    use self::std::sync::atomic::AtomicBool;
    use self::std::sync::Arc;
    use self::std::thread;
    use self::std::vec::Vec;

    /// `read` on x86_64: the syscall number a desktop makes most of.
    const ORDINARY: u32 = 0;

    /// How many threads stand in for the other CPUs. One less than this
    /// host's cores, so the measuring thread has one.
    const OTHERS: usize = 3;

    /// Runs `b.iter` with `OTHERS` threads calling `record` in a loop,
    /// numbering their calls with `other_num`.
    fn with_background(b: &mut Bencher, other_num: impl Fn(usize) -> u32 + Send + Sync + 'static) {
        with_background_in_banks(b, other_num, |_| 0)
    }

    /// The same, with each thread's histogram bank handed in too.
    ///
    /// Two knobs because the two questions are separate: which *syscall
    /// number* the other threads count (which cacheline inside a bank), and
    /// which *bank* they count it into (which copy of the histogram). The
    /// second is what [`COUNT_BANKS`] exists for, and on this host it has to
    /// be handed in: libos has one logical CPU, so `count_bank()` answers 0
    /// for every thread and `record` alone could never show the difference.
    fn with_background_in_banks(
        b: &mut Bencher,
        other_num: impl Fn(usize) -> u32 + Send + Sync + 'static,
        other_bank: impl Fn(usize) -> usize + Send + Sync + 'static,
    ) {
        let stop = Arc::new(AtomicBool::new(false));
        let other_num = Arc::new(other_num);
        let other_bank = Arc::new(other_bank);
        let mut threads = Vec::with_capacity(OTHERS);
        for i in 0..OTHERS {
            let stop = Arc::clone(&stop);
            let other_num = Arc::clone(&other_num);
            let other_bank = Arc::clone(&other_bank);
            threads.push(thread::spawn(move || {
                let num = other_num(i);
                let bank = other_bank(i);
                // A plain spin on `record`: the point is the cacheline
                // traffic, so there is nothing to yield to.
                while !stop.load(Ordering::Relaxed) {
                    for _ in 0..1024 {
                        record_in_bank(bank, 900 + i as u64, num);
                    }
                }
            }));
        }
        b.iter(|| record_in_bank(black_box(0), black_box(1), black_box(ORDINARY)));
        stop.store(true, Ordering::Relaxed);
        for t in threads {
            let _ = t.join();
        }
    }

    /// The two atomics with nobody else on the machine. This is the figure a
    /// single-threaded bench reports, and the one to distrust.
    #[bench]
    fn record_alone(b: &mut Bencher) {
        b.iter(|| record(black_box(1), black_box(ORDINARY)));
    }

    /// Other threads counting the **same** syscall, so `COUNTS[0]` is shared
    /// -- four threads doing `read`, which is a Wayland session. This is the
    /// contention that is left, and it is the histogram's, which is the one
    /// piece of this that cannot be indexed by process.
    #[bench]
    fn record_while_others_count_the_same_syscall(b: &mut Bencher) {
        with_background(b, |_| ORDINARY);
    }

    /// Other threads counting **different** syscalls, on different
    /// cachelines, under different pids: nothing is shared with the
    /// measuring thread. It was 313 ns with the global ring head and should
    /// now sit near `record_alone` -- that gap closing is the whole of the
    /// change this file carries.
    #[bench]
    fn record_while_others_count_different_syscalls(b: &mut Bencher) {
        // Numbers far enough apart to be on different cachelines: eight
        // `AtomicU64`s to a 64-byte line, so stride by 16.
        with_background(b, |i| 64 + (i as u32 + 1) * 16);
    }

    /// **The pair that justifies the banks.** The same four threads on the
    /// same syscall number as the row above, except each counts into a bank
    /// of its own, which is what a real machine does once the histogram is
    /// per-CPU. Everything else about the row is identical, so the
    /// difference between the two is the contention and nothing else.
    #[bench]
    fn record_while_others_count_the_same_syscall_in_their_own_banks(b: &mut Bencher) {
        with_background_in_banks(b, |_| ORDINARY, |i| i + 1);
    }

    /// And what `count_bank` itself costs: one relaxed load of a read-mostly
    /// word and an `and`. It is on every syscall whether or not anything is
    /// contending, so it is the price of the rows above and belongs beside
    /// them.
    #[bench]
    fn count_bank_of_this_cpu(b: &mut Bencher) {
        b.iter(|| black_box(count_bank()));
    }

    /// The pid-to-slot mix on its own: one multiply and a shift, which is
    /// what replaced the global `fetch_add`.
    #[bench]
    fn trace_slot_of_a_pid(b: &mut Bencher) {
        b.iter(|| black_box(trace_slot(black_box(4_242))));
    }

    /// A control of `record`'s shape: two machine words in, nothing out, no
    /// atomics. Without it the rows above would be read against zero.
    #[bench]
    fn record_shaped_control(b: &mut Bencher) {
        #[inline(never)]
        fn counts_nothing(_pid: u64, _num: u32) {}
        b.iter(|| counts_nothing(black_box(1), black_box(ORDINARY)));
    }
}
