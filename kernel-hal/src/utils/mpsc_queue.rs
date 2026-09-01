use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

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
        Self {
            size: queue.len(),
            chead: AtomicUsize::new(0),
            phead: AtomicUsize::new(0),
            ptail: AtomicUsize::new(0),
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
    pub fn commit_entry(&self, idx: usize) -> bool {
        while self.ptail() != idx {
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
