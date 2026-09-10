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

use core::sync::atomic::{AtomicU64, Ordering};

/// Above every number in the Linux x86_64 table (and the arch-specific ones
/// this tree adds); anything beyond is counted in the last slot.
pub const MAX_SYSCALL: usize = 512;

static COUNTS: [AtomicU64; MAX_SYSCALL] = [const { AtomicU64::new(0) }; MAX_SYSCALL];

/// How many recent calls the ring remembers.
pub const TRACE_LEN: usize = 256;

/// A ring of the most recent calls, as `(pid << 32) | num`, plus one past the
/// newest. Zero means "never written".
///
/// The histogram says what is being called a lot; this says what each process
/// did LAST. When a hang leaves every process asleep, that is the question
/// worth asking -- the tail names the syscall each one went to sleep in.
static TRACE: [AtomicU64; TRACE_LEN] = [const { AtomicU64::new(0) }; TRACE_LEN];
static TRACE_HEAD: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Count one call of syscall `num` by process `pid`.
#[inline]
pub fn record(pid: u64, num: u32) {
    let idx = (num as usize).min(MAX_SYSCALL - 1);
    COUNTS[idx].fetch_add(1, Ordering::Relaxed);
    let slot = TRACE_HEAD.fetch_add(1, Ordering::Relaxed) % TRACE_LEN;
    TRACE[slot].store(
        ((pid & 0xffff_ffff) << 32) | (num as u64 | 0x8000_0000),
        Ordering::Relaxed,
    );
}

/// The most recent calls, oldest first, as `(pid, syscall)`.
pub fn trace() -> alloc::vec::Vec<(u64, u32)> {
    let head = TRACE_HEAD.load(Ordering::Relaxed);
    let mut out = alloc::vec::Vec::with_capacity(TRACE_LEN);
    for i in 0..TRACE_LEN {
        let v = TRACE[(head + i) % TRACE_LEN].load(Ordering::Relaxed);
        if v != 0 {
            out.push((v >> 32, (v as u32) & 0x7fff_ffff));
        }
    }
    out
}

/// Every syscall seen at least once, as `(number, calls)`, busiest first.
pub fn snapshot() -> alloc::vec::Vec<(u32, u64)> {
    let mut v: alloc::vec::Vec<(u32, u64)> = COUNTS
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let n = c.load(Ordering::Relaxed);
            (n > 0).then_some((i as u32, n))
        })
        .collect();
    v.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    v
}
