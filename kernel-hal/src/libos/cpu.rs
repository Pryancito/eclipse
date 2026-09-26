//! CPU information.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::config::MAX_CORE_NUM;

/// One bit per slot, and the bitmap is a `u64`.
const _: () = assert!(
    MAX_CORE_NUM <= u64::BITS as usize,
    "the slot bitmap is one u64: widen it before raising MAX_CORE_NUM"
);

/// The slot handed out when a thread's own slot cannot be reached or none is
/// free. Never handed out by [`SlotMap::claim`], so an ordinary thread never
/// lands on it, and two threads only ever share it in those two cases.
const SHARED_SLOT: u8 = (MAX_CORE_NUM - 1) as u8;

/// A bitmap of logical cpu slots, one bit per slot.
///
/// A hosted kernel has as many "cpus" as it has threads running at once, so
/// slots are claimed on a thread's first [`cpu_id`] and given back when it
/// exits. [`SHARED_SLOT`] is deliberately kept out of the map: it is the one
/// slot threads may end up sharing, so it must never be handed to anybody as
/// if it were theirs alone.
struct SlotMap {
    in_use: AtomicU64,
}

impl SlotMap {
    const fn new() -> Self {
        Self {
            in_use: AtomicU64::new(0),
        }
    }

    /// Take the lowest free slot, or `None` when every slot below
    /// [`SHARED_SLOT`] is taken.
    fn claim(&self) -> Option<u8> {
        let mut seen = self.in_use.load(Ordering::Relaxed);
        loop {
            let free = (!seen).trailing_zeros();
            if free >= SHARED_SLOT as u32 {
                return None;
            }
            match self.in_use.compare_exchange_weak(
                seen,
                seen | (1u64 << free),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(free as u8),
                Err(now) => seen = now,
            }
        }
    }

    /// Give a slot back. [`SHARED_SLOT`] was never in the map, and more than
    /// one thread may be on it, so releasing it has to do nothing at all --
    /// clearing its bit would be handing it to the next caller as an exclusive
    /// slot while its sharers are still using it.
    fn release(&self, slot: u8) {
        if slot != SHARED_SLOT {
            self.in_use.fetch_and(!(1u64 << slot), Ordering::AcqRel);
        }
    }
}

static SLOTS: SlotMap = SlotMap::new();

/// How many times a thread asked for its id and could not be told (see
/// [`SHARED_SLOT`]). Non-zero means some per-cpu state was shared; it is worth
/// knowing, so it is counted rather than ignored.
static UNRESOLVED: AtomicUsize = AtomicUsize::new(0);

/// Runs inside the [`OWN_SLOT`] initializer, so it must not log: see
/// [`warn_shared_slot`].
fn claim_slot() -> u8 {
    SLOTS.claim().unwrap_or_else(|| {
        // More live threads than the kernel has cpus. Nothing here can make
        // that safe; the warning goes out from `cpu_id` instead.
        UNRESOLVED.fetch_add(1, Ordering::Relaxed);
        SHARED_SLOT
    })
}

/// Whether the warning below has already gone out.
static WARNED: AtomicBool = AtomicBool::new(false);

/// Say once that per-cpu state is being shared.
///
/// **Called from [`cpu_id`] and never from the [`OWN_SLOT`] initializer**, which
/// is the point of it existing at all. `SimpleLogger::log` in `zCore` stamps
/// every line with `cpu_id()`, so a `warn!` raised while `OWN_SLOT` was still
/// initializing would re-enter that same initializer on that same thread --
/// which does not return, it runs the initializer again. The warning is wanted
/// exactly when the map is full, that is when the machine is already in
/// trouble, so it must not be the thing that finishes it off. Raised after
/// `try_with` has answered, the logger's own `cpu_id()` resolves normally and
/// the flag stops the second line.
#[cold]
fn warn_shared_slot() {
    if WARNED.load(Ordering::Relaxed) || WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    warn!(
        "cpu_id: mas de {} hilos vivos a la vez; se comparte la ranura {}, y con ella el \
         estado por-CPU",
        MAX_CORE_NUM - 1,
        SHARED_SLOT
    );
}

/// This thread's slot, released when the thread exits.
struct OwnSlot(u8);

impl Drop for OwnSlot {
    fn drop(&mut self) {
        SLOTS.release(self.0);
    }
}

std::thread_local! {
    static OWN_SLOT: OwnSlot = OwnSlot(claim_slot());
}

/// How many times [`cpu_id`] could not name the calling thread's own slot.
///
/// Zero in a healthy run, and a number worth having when it is not: every count
/// is a thread that shared per-cpu state with another. Not something to assert
/// on -- a hosted kernel with more live threads than `MAX_CORE_NUM - 1` really
/// is sharing, and that is a limit rather than a bug -- so it is reported in the
/// failure message of the test that proves the ids are distinct.
pub fn unresolved_cpu_ids() -> usize {
    UNRESOLVED.load(Ordering::Relaxed)
}

hal_fn_impl! {
    impl mod crate::hal_fn::cpu {
        /// A dense logical cpu id for this host thread, stable for its life.
        ///
        /// **Not** the host thread id. Every per-cpu array in the kernel
        /// indexes by `cpu_id()` and rests its `unsafe impl Sync` on "each cpu
        /// only ever touches its own slot". This used to be
        /// `std::thread::current().id().as_u64().get() as u8`, which is
        /// neither dense nor bounded: one test binary starts hundreds of
        /// threads, the cast wraps past 255, and every consumer then clamps
        /// with `.min(N - 1)` -- so most threads shared ONE slot. That is not
        /// a cosmetic difference: `zircon-object`'s VMO deferred-drop stash is
        /// a `Vec` behind an `UnsafeCell` with no lock, justified by exactly
        /// that assumption, and Rust's own check caught the consequence --
        /// `hint::assert_unchecked` violated inside `Vec::pop`, from
        /// `StashDrain::drop`, about once in eight runs of the
        /// `zircon-object` suite.
        ///
        /// So each thread claims a slot out of a bitmap and gives it back when
        /// it exits: what has to fit under `MAX_CORE_NUM` is how many threads
        /// run at once, not how many the process has ever started.
        fn cpu_id() -> u8 {
            // `try_with`, not `with`: a thread that is already tearing down
            // its thread-locals can still reach here while dropping a kernel
            // object, and `with` panics then.
            let slot = OWN_SLOT.try_with(|s| s.0).unwrap_or_else(|_| {
                UNRESOLVED.fetch_add(1, Ordering::Relaxed);
                SHARED_SLOT
            });
            // Only the fallback lands here, since `claim` never hands the last
            // slot out. Cold, and past the initializer, so the logger may ask
            // for its own `cpu_id()` from inside it.
            if slot == SHARED_SLOT {
                warn_shared_slot();
            }
            slot
        }

        fn cpu_brand() -> alloc::string::String {
            alloc::string::String::from("Host CPU")
        }

        fn cpu_count() -> u8 {
            std::thread::available_parallelism()
                .map(|n| n.get() as u8)
                .unwrap_or(1)
        }

        fn reset() -> ! {
            info!("shutdown...");
            std::process::exit(0);
        }
    }
}

#[cfg(test)]
mod tests {
    //! Two kinds of test, and the split matters.
    //!
    //! [`SlotMap`] is tested on a map of the test's own, so the assertions can
    //! be exact -- "this claim returns slot 1" is only true of a map nobody else
    //! is using. [`cpu_id`] is tested on the real, process-wide map, so only
    //! what is true of it GLOBALLY can be asserted: that live threads do not
    //! share an id, that an id does not move under a thread, and that every id
    //! is a valid index. Those three are precisely what the 166 call sites
    //! around the kernel rest on, and the exact numbers are not, since every
    //! other test in this binary is holding slots at the same time.

    use super::*;
    use core::sync::atomic::AtomicBool;
    use std::sync::{Arc, Barrier, Mutex};

    /// Start and join enough throwaway threads that the host's own thread-id
    /// counter is well past `MAX_CORE_NUM`.
    ///
    /// **This is the precondition under which the old `cpu_id()` broke**, and
    /// without it a regression test here proves nothing: while the host's thread
    /// ids happen to be small numbers, `id as u8` is accidentally dense and
    /// distinct too, and a clamp to `MAX_CORE_NUM - 1` never bites. A mutation
    /// run is what said so -- putting the old body back left every test in this
    /// module green until this churn went in front of the two that care.
    ///
    /// Joining each thread means its slot comes straight back, so the real map
    /// is no fuller afterwards than it was before.
    fn past_the_first_sixty_four_threads() {
        for _ in 0..MAX_CORE_NUM * 2 {
            std::thread::spawn(cpu_id).join().unwrap();
        }
    }

    #[test]
    /// Lowest first, and a released slot is the next one out. Densely packed is
    /// the whole point: the id indexes fixed-size per-cpu arrays.
    fn slots_are_handed_out_lowest_first_and_come_back() {
        let map = SlotMap::new();
        assert_eq!(map.claim(), Some(0));
        assert_eq!(map.claim(), Some(1));
        assert_eq!(map.claim(), Some(2));

        map.release(1);
        assert_eq!(map.claim(), Some(1), "the freed slot was not reused");
        assert_eq!(map.claim(), Some(3));
    }

    #[test]
    /// The map holds every slot but the last, and the last is never handed out.
    /// If it were, a thread would be given the fallback slot as if it owned it,
    /// which is the one way per-cpu state can be shared without anybody
    /// noticing.
    fn the_shared_slot_is_never_handed_out() {
        let map = SlotMap::new();
        let mut seen = Vec::new();
        while let Some(slot) = map.claim() {
            assert_ne!(slot, SHARED_SLOT, "the fallback slot was handed out");
            seen.push(slot);
        }
        assert_eq!(seen.len(), SHARED_SLOT as usize);
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            SHARED_SLOT as usize,
            "a slot was handed out twice"
        );
    }

    #[test]
    /// And its bit is never set in the map, which is what makes it safe for two
    /// threads to be on it at once: neither of them can hand it to a third by
    /// exiting. That invariant is why `release` can ignore it -- and the guard
    /// there is kept anyway, so that a `claim` which one day did hand out the
    /// last slot could not become quietly unsafe.
    fn the_fallback_slot_never_enters_the_map() {
        let map = SlotMap::new();
        while map.claim().is_some() {}
        assert_eq!(
            map.in_use.load(Ordering::Relaxed) >> SHARED_SLOT,
            0,
            "the map claimed the fallback slot"
        );
        map.release(SHARED_SLOT);
        assert_eq!(
            map.claim(),
            None,
            "releasing the fallback slot freed a slot"
        );
    }

    #[test]
    /// The claim is a CAS loop, and the one thing it must never do is hand the
    /// same bit to two threads. Checked on a map of this test's own, with a
    /// flag per slot that is set while the slot is held.
    fn two_threads_never_hold_the_same_slot_at_once() {
        struct Shared {
            map: SlotMap,
            held: Vec<AtomicBool>,
        }
        let shared = Arc::new(Shared {
            map: SlotMap::new(),
            held: (0..MAX_CORE_NUM).map(|_| AtomicBool::new(false)).collect(),
        });
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let shared = shared.clone();
                std::thread::spawn(move || {
                    for _ in 0..2000 {
                        let slot = shared
                            .map
                            .claim()
                            .expect("eight threads, sixty-three slots");
                        assert!(
                            !shared.held[slot as usize].swap(true, Ordering::AcqRel),
                            "slot {} was handed out while another thread held it",
                            slot
                        );
                        shared.held[slot as usize].store(false, Ordering::Release);
                        shared.map.release(slot);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
    }

    #[test]
    /// The property the kernel actually rests on: no two threads that are alive
    /// at the same moment answer the same `cpu_id()`.
    ///
    /// This is what the old implementation broke. It returned
    /// `std::thread::current().id().as_u64().get() as u8`, and every consumer
    /// clamps that with `.min(MAX_CORE_NUM - 1)`, so in a test binary that has
    /// started a few hundred threads most of them shared slot 63 -- including
    /// `zircon-object`'s deferred-drop stash, a `Vec` behind an `UnsafeCell`
    /// with no lock whose `unsafe impl Sync` says in as many words that each cpu
    /// only ever touches its own slot.
    ///
    /// The barrier is the test: the ids have to be distinct WHILE every thread
    /// still holds its slot. Eight threads, not sixty-three, because the rest of
    /// the binary is holding slots of its own and exhausting the map is a
    /// different test's job.
    fn live_threads_never_share_a_cpu_id() {
        const THREADS: usize = 8;
        past_the_first_sixty_four_threads();
        let ready = Arc::new(Barrier::new(THREADS));
        let ids = Arc::new(Mutex::new(Vec::new()));
        let threads: Vec<_> = (0..THREADS)
            .map(|_| {
                let (ready, ids) = (ready.clone(), ids.clone());
                std::thread::spawn(move || {
                    ids.lock().unwrap().push(cpu_id());
                    // Nobody leaves until everybody has an id, so every id
                    // below was held at the same instant as all the others.
                    ready.wait();
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }

        let ids = core::mem::take(&mut *ids.lock().unwrap());
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            ids.len(),
            "two live threads got the same cpu id: {:?} (sin resolver: {})",
            ids,
            unresolved_cpu_ids()
        );
    }

    #[test]
    /// Every id indexes a `MAX_CORE_NUM`-sized array somewhere, so an id that
    /// does not fit is an out-of-bounds write waiting to happen. The old cast to
    /// `u8` was unbounded in exactly this way, and the clamping at the call
    /// sites is what turned it into sharing instead of a panic.
    fn a_cpu_id_is_always_a_valid_index() {
        past_the_first_sixty_four_threads();
        for _ in 0..16 {
            let id = std::thread::spawn(|| cpu_id()).join().unwrap();
            assert!(
                (id as usize) < MAX_CORE_NUM,
                "cpu id {} does not index a per-cpu array of {}",
                id,
                MAX_CORE_NUM
            );
        }
        assert!((cpu_id() as usize) < MAX_CORE_NUM);
    }

    #[test]
    /// And it does not move under a thread. Per-cpu state is read back by the
    /// same thread that wrote it -- a stash pushed under one id and drained
    /// under another is the bug this whole file is about.
    fn a_cpu_id_is_stable_for_the_life_of_its_thread() {
        std::thread::spawn(|| {
            let first = cpu_id();
            for _ in 0..1000 {
                assert_eq!(cpu_id(), first, "the cpu id moved under its own thread");
            }
            std::thread::yield_now();
            assert_eq!(cpu_id(), first, "the cpu id moved across a yield");
        })
        .join()
        .unwrap();
    }

    #[test]
    /// `cpu_id()` has to answer from inside a thread-local destructor, because a
    /// kernel object dropped as a thread tears down asks for it -- and `with`
    /// panics once the thread's own thread-locals are gone, which is why this
    /// reads `try_with`.
    ///
    /// Destructors run in reverse order of registration, so `LATE` is touched
    /// FIRST and `OWN_SLOT` second: that has `OWN_SLOT` destroyed before `LATE`,
    /// which is the order in which the question gets asked of a slot that is
    /// already gone. With `with` in there the destructor panics, and a panic in
    /// a thread-local destructor takes the process with it rather than failing
    /// one test.
    fn a_cpu_id_asked_for_during_teardown_still_answers() {
        struct AsksOnDrop;
        impl Drop for AsksOnDrop {
            fn drop(&mut self) {
                assert!(
                    (cpu_id() as usize) < MAX_CORE_NUM,
                    "no usable cpu id during thread teardown"
                );
            }
        }
        std::thread_local! {
            static LATE: AsksOnDrop = const { AsksOnDrop };
        }
        std::thread::spawn(|| {
            LATE.with(|_| ());
            let _ = cpu_id();
        })
        .join()
        .unwrap();
    }

    #[test]
    /// A thread that has exited gives its slot back, so what has to fit under
    /// `MAX_CORE_NUM` is how many threads are alive at once and not how many the
    /// process has ever started. Far more than sixty-three threads, one at a
    /// time: the old code wrapped its `u8` and this could not have passed.
    fn a_slot_comes_back_when_its_thread_exits() {
        for _ in 0..200 {
            let id = std::thread::spawn(|| cpu_id()).join().unwrap();
            assert!((id as usize) < MAX_CORE_NUM);
        }
        // Slots given back are slots reused, so the map cannot have filled: a
        // thread starting now still gets one of its own.
        let ready = Arc::new(Barrier::new(2));
        let mine = cpu_id();
        let other = {
            let ready = ready.clone();
            std::thread::spawn(move || {
                let id = cpu_id();
                ready.wait();
                id
            })
        };
        ready.wait();
        let other = other.join().unwrap();
        assert_ne!(
            other, mine,
            "two hundred threads later the map had no slot left to give"
        );
    }
}
