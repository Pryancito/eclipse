use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Bounded multi-producer single-consumer ring.
///
/// Producers reserve a slot with [`alloc_entry`] (CAS on `phead`), write the
/// payload, then [`commit_entry`]. Commits may complete out of order: each
/// finished slot sets a bit in [`ready`]; whoever sees the bit for the current
/// `ptail` advances the published tail through every contiguous ready run.
///
/// The old "spin 100 times then abandon" commit left a reserved-never-committed
/// hole that froze `ptail` forever and forced the IPI path into perpetual
/// overflow mode — a root cause of TLB-shootdown starvation under SMP.
pub struct MpscQueue<'a, T: Copy> {
    pub size: usize,
    pub chead: AtomicUsize,
    pub phead: AtomicUsize,
    pub ptail: AtomicUsize,
    /// Bit `(idx % size)` is set once the producer has written slot `idx` and
    /// is ready for `ptail` to publish past it. Cleared when `ptail` advances
    /// through that slot. `size` must be ≤ 64.
    ready: AtomicU64,
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
        assert!(
            !queue.is_empty() && queue.len() <= 64,
            "MpscQueue ready-bitmap needs 1..=64 slots"
        );
        Self {
            size: queue.len(),
            chead: AtomicUsize::new(0),
            phead: AtomicUsize::new(0),
            ptail: AtomicUsize::new(0),
            ready: AtomicU64::new(0),
            queue: UnsafeCell::new(queue),
        }
    }

    #[allow(clippy::mut_from_ref)]
    #[allow(unsafe_code)]
    pub fn entry_at(&self, idx: usize) -> &mut T {
        let queue = unsafe { &mut *self.queue.get() };
        &mut queue[idx % self.size]
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

    #[inline]
    fn ready_bit(idx: usize, size: usize) -> u64 {
        1u64 << (idx % size)
    }

    pub fn alloc_entry(&self) -> Option<usize> {
        loop {
            let chead = self.chead();
            let phead = self.phead();
            if phead - chead < self.size {
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

    /// Mark slot `idx` ready and advance `ptail` through every contiguous ready
    /// run starting at the current tail.
    ///
    /// Always returns `true`. A reserved slot is never left unpublished: even
    /// if this producer's predecessor has not finished yet, our ready bit stays
    /// set until that predecessor (or a helper) walks `ptail` through us.
    pub fn commit_entry(&self, idx: usize) -> bool {
        let size = self.size;
        let my_bit = Self::ready_bit(idx, size);
        self.ready.fetch_or(my_bit, Ordering::Release);

        // Help advance the published tail. Any committer may drive this loop;
        // CAS losers just re-read. Bounded by the ring depth so a torn view
        // cannot spin forever here.
        let mut guard = size + 2;
        while guard > 0 {
            guard -= 1;
            let tail = self.ptail.load(Ordering::Acquire);
            // Everything up to and including `idx` is already published.
            if tail > idx {
                return true;
            }
            let bit = Self::ready_bit(tail, size);
            let ready = self.ready.load(Ordering::Acquire);
            if ready & bit == 0 {
                // Hole at `tail`: a predecessor still writing. Our bit remains
                // set; that predecessor's commit (or a later helper) will walk
                // through us. Never abandon — that froze ptail forever.
                return true;
            }
            if self
                .ptail
                .compare_exchange(tail, tail + 1, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                self.ready.fetch_and(!bit, Ordering::Release);
            }
        }
        // Depth exceeded only if the CAS storm was extreme; the ready bit for
        // `idx` is still set, so the next committer will finish the advance.
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
        let mut buf = [0u32; 8];
        let q = MpscQueue::new(&mut buf);
        let a = q.alloc_entry().unwrap();
        let b = q.alloc_entry().unwrap();
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        *q.entry_at(a) = 10;
        *q.entry_at(b) = 20;
        // Commit B first — ptail must stay 0 until A commits.
        assert!(q.commit_entry(b));
        assert_eq!(q.ptail(), 0);
        assert!(q.commit_entry(a));
        assert_eq!(q.ptail(), 2);
        let got = q.consume_entrys();
        assert_eq!(got, vec![(0, 10), (1, 20)]);
    }

    #[test]
    fn commit_never_freezes_ptail_under_racing_producers() {
        const N: usize = 16;
        const ROUNDS: usize = 4_000;
        let buf = Box::leak(vec![0usize; N].into_boxed_slice());
        let queue = Arc::new(MpscQueue::new(buf));
        let stop = Arc::new(core::sync::atomic::AtomicBool::new(false));
        let producers = 8usize;

        // Single dedicated consumer — the real IPI path is single-consumer.
        let q_cons = queue.clone();
        let stop_cons = stop.clone();
        let consumer = thread::spawn(move || {
            while !stop_cons.load(Ordering::Relaxed) {
                let _ = q_cons.consume_entrys();
                thread::yield_now();
            }
            // Final drain.
            for _ in 0..64 {
                if q_cons.chead() == q_cons.phead() && q_cons.ptail() == q_cons.phead() {
                    break;
                }
                let t = q_cons.ptail();
                if t < q_cons.phead() {
                    let ready = q_cons.ready.load(Ordering::Acquire);
                    let bit = MpscQueue::<usize>::ready_bit(t, q_cons.size);
                    if ready & bit != 0
                        && q_cons
                            .ptail
                            .compare_exchange(t, t + 1, Ordering::SeqCst, Ordering::Relaxed)
                            .is_ok()
                    {
                        q_cons.ready.fetch_and(!bit, Ordering::Release);
                    }
                }
                let _ = q_cons.consume_entrys();
            }
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
        assert_eq!(queue.ready.load(Ordering::Acquire), 0);
    }
}
