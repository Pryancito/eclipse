use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Scratch slot handed out when a queue's backing slice has been corrupted to
/// zero length. Never read for anything meaningful — it exists so `entry_at`
/// has something in-bounds to return on a path where panicking is fatal.
const FALLBACK_SLOT_LEN: usize = 64;
const FALLBACK_SLOT_ALIGN: usize = 16;
#[repr(C, align(16))]
struct FallbackSlot([u8; FALLBACK_SLOT_LEN]);
static mut FALLBACK_SLOT: FallbackSlot = FallbackSlot([0; FALLBACK_SLOT_LEN]);

/// One line naming a queue whose `size` and backing-slice length disagree.
/// Budgeted, allocation-free and lock-free: it runs on the IPI drain path with
/// interrupts already off.
#[cold]
#[inline(never)]
fn report_size_mismatch(len: usize, size: usize) {
    use core::sync::atomic::AtomicU32;
    static REPORTED: AtomicU32 = AtomicU32::new(0);
    if REPORTED.fetch_add(1, Ordering::Relaxed) >= 4 {
        return;
    }
    crate::console::serial_write_fmt_spin(format_args!(
        "\n[mpsc] CORRUPTED QUEUE: backing slice len={} but size={} — the ring's \
         fat pointer has been overwritten. Wrapping by the real length so the \
         IPI drain does not panic with locks held.\n",
        len, size,
    ));
}

/// How many turns of an IRQs-off spin go by between drains of this CPU's own
/// pending IPI work. `lock::pump`'s contract asks for a coarse cadence: the
/// call is a single relaxed load when no pump is installed and one
/// queue-pointer compare when the queue is empty, but it is not free, and a
/// wait this short usually ends before the first drain is due.
const PUMP_SPIN_INTERVAL: u32 = 512;

/// Drain this CPU's own pending IPI work while spinning. The host build has no
/// interrupts to be deaf to, so the test seam counts the calls instead --
/// which is the only way to see from a test that the spin pumps at all.
#[cfg(not(test))]
#[inline]
fn pump_while_spinning() {
    lock::pump();
}

#[cfg(test)]
fn pump_while_spinning() {
    PUMPS.with(|c| c.set(c.get() + 1));
}

#[cfg(test)]
std::thread_local! {
    /// Calls to [`pump_while_spinning`] **on this thread**. Per thread and not
    /// global on purpose: one host thread stands for one CPU, exactly as the
    /// rest of this crate's host seams do, so a test reads only its own spin
    /// and cannot be moved by whatever else the test binary is running.
    static PUMPS: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

/// How many times this thread drained while spinning in [`MpscQueue::commit_entry`].
#[cfg(test)]
fn pumps_on_this_thread() -> usize {
    PUMPS.with(|c| c.get())
}

/// Count one turn of the spin. The **pair** (turns, drains) is the rule this
/// loop obeys, and the rule is a ratio: a test that only counts the drains
/// cannot tell a coarse cadence from one that fires on every single turn, and
/// firing on every turn puts the drain back on the hot path this cadence
/// exists to keep it off. No-op outside the tests.
#[cfg(not(test))]
#[inline(always)]
fn note_spin() {}

#[cfg(test)]
fn note_spin() {
    SPINS.with(|c| c.set(c.get() + 1));
}

#[cfg(test)]
std::thread_local! {
    static SPINS: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

/// Turns this thread has spun in [`MpscQueue::commit_entry`].
#[cfg(test)]
fn spins_on_this_thread() -> usize {
    SPINS.with(|c| c.get())
}

/// Bounded multi-producer single-consumer ring.
///
/// Producers reserve a slot with [`alloc_entry`] (CAS on `phead`), write the
/// payload, then [`commit_entry`], which spins until `ptail == idx` and only
/// then publishes. Commits may finish out of order at the callers, but the
/// publish order stays strict — a later index never advances `ptail` past a
/// still-uncommitted predecessor.
///
/// The old implementation abandoned the commit after 100 spins (`return false`),
/// leaving a reserved-never-published slot that froze `ptail` forever and
/// forced the IPI path into perpetual overflow — a root cause of TLB-shootdown
/// starvation under SMP.
pub struct MpscQueue<'a, T: Copy> {
    pub size: usize,
    pub chead: AtomicUsize,
    pub phead: AtomicUsize,
    pub ptail: AtomicUsize,
    /// Safety:
    ///
    /// Access conflicts are avoided via atomic variables
    queue: UnsafeCell<&'a mut [T]>,
}

#[allow(unsafe_code)]
unsafe impl<'a, T: Copy> Sync for MpscQueue<'a, T> {}
#[allow(unsafe_code)]
unsafe impl<'a, T: Copy> Send for MpscQueue<'a, T> {}

impl<'a, T: Copy> MpscQueue<'a, T> {
    pub fn new(queue: &'a mut [T]) -> Self {
        assert!(!queue.is_empty(), "MpscQueue needs a non-empty buffer");
        // `entry_at` hands back `FALLBACK_SLOT` when the ring is found
        // corrupted, so it must be able to hold a `T`.
        assert!(
            core::mem::size_of::<T>() <= FALLBACK_SLOT_LEN
                && core::mem::align_of::<T>() <= FALLBACK_SLOT_ALIGN,
            "MpscQueue entry does not fit the corruption fallback slot"
        );
        Self {
            size: queue.len(),
            chead: AtomicUsize::new(0),
            phead: AtomicUsize::new(0),
            ptail: AtomicUsize::new(0),
            queue: UnsafeCell::new(queue),
        }
    }

    /// The slot `idx` maps to, or a scratch slot when the ring is no longer
    /// trustworthy.
    ///
    /// This is the TLB-shootdown drain path: `tlb_shootdown_ack_on` reaches it
    /// from `TicketMutex::lock`'s spin pump, i.e. from inside every contended
    /// lock acquire in the kernel, with that lock's interrupts already off. A
    /// panic there can never be contained by `oops` — it is guaranteed to be
    /// holding a lock — so it takes the machine down and buries whatever
    /// caused it. Hence no indexing that can panic, and no dereference of a
    /// pointer the queue cannot vouch for.
    ///
    /// `size` and the backing slice's length are both set once in `new` from
    /// one slice, so a disagreement is never a logic error here: it means a
    /// wild write has landed on the queue's own fat pointer. Both observed
    /// values were kernel addresses —
    ///
    /// ```text
    /// len=0xffffff00218688e0 (a coroutine stack)  size=0xffffff00006567c1
    /// ```
    ///
    /// — so *neither* bound may be used. An earlier version wrapped by `len`,
    /// which stops the panic but is worse than it: the slice claims that
    /// length, so `queue[idx % len]` reads and writes wherever the corrupted
    /// data pointer happens to aim. Hand back a scratch slot instead and say
    /// so once: the drain then no-ops on a dead queue rather than spreading
    /// the corruption it just detected.
    #[allow(clippy::mut_from_ref)]
    #[allow(unsafe_code)]
    pub fn entry_at(&self, idx: usize) -> &mut T {
        let queue = unsafe { &mut *self.queue.get() };
        let len = queue.len();
        if len != self.size || idx % len.max(1) >= len {
            report_size_mismatch(len, self.size);
            // SAFETY: `FALLBACK_SLOT` is a static byte array at least as large
            // as any `T` this queue is instantiated with (asserted in `new`),
            // and it is only ever reached on a queue already known corrupt.
            return unsafe { &mut *(&raw mut FALLBACK_SLOT).cast::<T>() };
        }
        &mut queue[idx % len]
    }

    pub fn chead(&self) -> usize {
        self.chead.load(Ordering::Acquire)
    }

    pub fn phead(&self) -> usize {
        self.phead.load(Ordering::Acquire)
    }

    pub fn ptail(&self) -> usize {
        self.ptail.load(Ordering::Acquire)
    }

    pub fn alloc_entry(&self) -> Option<usize> {
        loop {
            let chead = self.chead();
            let phead = self.phead();
            if phead.saturating_sub(chead) < self.size {
                if self
                    .phead
                    .compare_exchange(phead, phead + 1, Ordering::SeqCst, Ordering::Relaxed)
                    .is_ok()
                {
                    break Some(phead);
                }
            } else {
                // notify consumer ?
                break None;
            }
        }
    }

    /// Publish slot `idx`. Spins until `ptail == idx`, then advances `ptail`.
    ///
    /// Always returns `true`. Never abandons a reserved slot — the old
    /// 100-spin cap left holes that froze `ptail` permanently.
    ///
    /// The spin drains this CPU's own pending IPI work every
    /// [`PUMP_SPIN_INTERVAL`] turns, for the reason `lock::pump` gives.
    ///
    /// A producer gets here from `send_ipi`, which runs with whatever interrupt
    /// state its caller had -- and the callers that reach it in volume are
    /// page-table operations holding a `lock::Mutex`, whose `push_off` has
    /// already turned interrupts off. A CPU spinning here in that state cannot
    /// take the shootdown IPI, and a peer that spin-waits for its
    /// acknowledgement burns its whole budget: the `slow ack wait
    /// spins=16777216` wedge, which on riscv64 and aarch64 has no NMI rescue
    /// behind it. Where interrupts do happen to be on the drain is redundant
    /// and harmless -- the handler would have done it -- which is why the
    /// cadence, and not the interrupt state, is what this loop checks.
    ///
    /// This is one of the places `lock::pump`'s own list of IRQs-off spinners
    /// did not name, and it is the one inside the shootdown machinery itself.
    pub fn commit_entry(&self, idx: usize) -> bool {
        let mut spins: u32 = 0;
        while self.ptail() != idx {
            spins = spins.wrapping_add(1);
            note_spin();
            if spins.is_multiple_of(PUMP_SPIN_INTERVAL) {
                pump_while_spinning();
            }
            core::hint::spin_loop();
        }
        self.ptail.fetch_add(1, Ordering::SeqCst);
        true
    }

    pub fn consume_entrys(&self) -> Vec<(usize, T)> {
        let mut vec = Vec::new();
        let chead = self.chead();
        let ptail = self.ptail();
        for idx in chead..ptail {
            vec.push((idx, *self.entry_at(idx)));
        }
        self.chead.store(ptail, Ordering::Release);
        vec
    }

    /// Drop all pending entries without allocating (advance the consumer head to
    /// the producer tail). Used on the TLB-shootdown path, which runs while
    /// holding the page-table / VMAR spinlocks where a heap allocation
    /// (`consume_entrys`' `Vec`) would be both wasteful and a lock-ordering
    /// hazard. Returns `true` if any entry was discarded.
    pub fn discard_entrys(&self) -> bool {
        let ptail = self.ptail();
        let chead = self.chead.swap(ptail, Ordering::Release);
        chead != ptail
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn out_of_order_commit_publishes_in_index_order() {
        let buf = Box::leak(Box::new([0u32; 8]));
        let q = Arc::new(MpscQueue::new(buf));
        let a = q.alloc_entry().unwrap();
        let b = q.alloc_entry().unwrap();
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        *q.entry_at(a) = 10;
        *q.entry_at(b) = 20;
        // Commit B in a side thread — it blocks until A publishes slot 0.
        let q2 = q.clone();
        let t = thread::spawn(move || {
            assert!(q2.commit_entry(b));
        });
        assert_eq!(q.ptail(), 0);
        assert!(q.commit_entry(a));
        t.join().unwrap();
        assert_eq!(q.ptail(), 2);
        let got = q.consume_entrys();
        assert_eq!(got, vec![(0, 10), (1, 20)]);
    }

    #[test]
    fn a_producer_waiting_on_a_peer_drains_its_own_ipi_work() {
        // The wedge this guards: a producer reaches `commit_entry` from
        // `send_ipi` with interrupts off, so every turn it spins here is a
        // turn it cannot take a TLB-shootdown IPI, and the peer waiting for
        // its acknowledgement has a budget and, on riscv64 and aarch64, no
        // NMI rescue when that budget runs out.
        let buf = Box::leak(Box::new([0u32; 8]));
        let q = Arc::new(MpscQueue::new(buf));
        let a = q.alloc_entry().unwrap();
        let b = q.alloc_entry().unwrap();
        let q2 = q.clone();
        // The peer holds its earlier reservation long enough that the waiter
        // has to cross the pump interval many times over. A slower machine
        // only spins more, never fewer, so the bound holds either way.
        let t = thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            assert!(q2.commit_entry(a));
        });
        let (pumps_before, spins_before) = (pumps_on_this_thread(), spins_on_this_thread());
        assert!(q.commit_entry(b));
        let pumps = pumps_on_this_thread() - pumps_before;
        let spins = spins_on_this_thread() - spins_before;
        t.join().unwrap();
        assert!(
            pumps > 0,
            "a commit that waited 30 ms drained nothing in {} turns",
            spins,
        );
        // And the other side of the rule: the drain is a cadence, not a thing
        // this loop does on every turn. Asserting the ratio and not just the
        // count is what tells the two apart.
        assert!(
            pumps <= spins / PUMP_SPIN_INTERVAL as usize + 1,
            "drained {} times in {} turns, which is not a cadence of {}",
            pumps,
            spins,
            PUMP_SPIN_INTERVAL,
        );
        assert_eq!(q.ptail(), 2);
    }

    #[test]
    fn a_commit_that_does_not_wait_pumps_nothing() {
        // The other half of the cadence: the overwhelmingly common commit
        // publishes on its first look and must not pay for the drain.
        let buf = Box::leak(Box::new([0u32; 8]));
        let q = MpscQueue::new(buf);
        let a = q.alloc_entry().unwrap();
        let before = pumps_on_this_thread();
        assert!(q.commit_entry(a));
        assert_eq!(pumps_on_this_thread(), before);
    }

    #[test]
    fn the_first_drain_comes_due_a_whole_interval_in() {
        // The counter is bumped before the test, so turn 512 is the first
        // drain and not turn 0 -- a cadence that fired on the first turn
        // would put the drain on the path of every uncontended commit, which
        // is the one this must stay off.
        assert!(PUMP_SPIN_INTERVAL.is_multiple_of(PUMP_SPIN_INTERVAL));
        assert!(!1u32.is_multiple_of(PUMP_SPIN_INTERVAL));
    }

    #[test]
    fn commit_never_freezes_ptail_under_racing_producers() {
        const N: usize = 16;
        const ROUNDS: usize = 4_000;
        let buf = Box::leak(vec![0usize; N].into_boxed_slice());
        let queue = Arc::new(MpscQueue::new(buf));
        let stop = Arc::new(core::sync::atomic::AtomicBool::new(false));
        let producers = 8usize;

        let q_cons = queue.clone();
        let stop_cons = stop.clone();
        let consumer = thread::spawn(move || {
            while !stop_cons.load(Ordering::Relaxed) {
                let _ = q_cons.consume_entrys();
                thread::yield_now();
            }
            let _ = q_cons.consume_entrys();
        });

        let mut handles = Vec::new();
        for p in 0..producers {
            let q = queue.clone();
            handles.push(thread::spawn(move || {
                for i in 0..ROUNDS {
                    loop {
                        if let Some(idx) = q.alloc_entry() {
                            *q.entry_at(idx) = p * ROUNDS + i;
                            assert!(
                                q.commit_entry(idx),
                                "commit_entry must never abandon a reserved slot"
                            );
                            break;
                        }
                        thread::yield_now();
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        consumer.join().unwrap();

        assert_eq!(
            queue.ptail(),
            queue.phead(),
            "after all producers join, ptail must equal phead (no frozen hole)"
        );
        assert_eq!(queue.chead(), queue.ptail());
    }
}
