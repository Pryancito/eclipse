use crate::waker_page::{WakerPage, WakerRef, WAKER_PAGE_SIZE};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use bit_iter::BitIter;
use core::ops::{Coroutine, CoroutineState};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use spin::{Mutex, MutexGuard};
use unicycle::pin_slab::PinSlab;
use {
    alloc::boxed::Box,
    core::future::Future,
    core::pin::Pin,
    core::task::{Context, Poll},
};

use core::fmt::{Debug, Formatter, Result};

// #[allow(unused)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    // BLOCKED,
    RUNNABLE,
    RUNNING,
}

pub struct Task {
    id: usize,
    future: Mutex<Pin<Box<dyn Future<Output = ()> + Send>>>,
    inner: Mutex<TaskInner>,
    finish: Arc<AtomicBool>,
    /// Optional CPU affinity mask: bit `i` set means the task may run on the
    /// logical CPU `i`. `None` means "run anywhere" (no restriction, no extra
    /// allocation). The mask lives behind an `Arc<AtomicU64>` so a thread can
    /// change its own affinity at runtime (`sched_setaffinity`) and the
    /// scheduler observes the new value on the next placement/steal decision.
    affinity: Option<Arc<AtomicU64>>,
    /// The task's one shared waker, built at insertion. Every poll previously
    /// constructed two fresh `WakerRef`s (four `Arc` refcount RMWs) plus an
    /// `Arc::new` heap allocation; reusing a single allocation removes all of
    /// that from the hot poll path, and makes `Waker::will_wake` comparisons
    /// meaningful (same data pointer across polls), which keeps waker
    /// registries (net RX, sleep slots) deduplicated.
    waker: spin::Once<Arc<WakerRef>>,
}

struct TaskInner {
    priority: usize,
    state: TaskState,
    intr_enable: bool,
}

impl core::fmt::Debug for Task {
    fn fmt(&self, f: &mut Formatter) -> Result {
        let inner = crate::diag::diag_lock(&self.inner);
        let mut f = f.debug_struct("X86PTE");
        f.field("priority", &inner.priority);
        f.field("state", &inner.state);
        f.field("intr_enable", &inner.intr_enable);
        f.finish()
    }
}

fn alloc_id() -> usize {
    static TASK_ID: AtomicUsize = AtomicUsize::new(1);
    TASK_ID.fetch_add(1, Ordering::SeqCst)
}

impl Task {
    pub fn new(
        future: impl Future<Output = ()> + Send + 'static,
        priority: usize,
        affinity: Option<Arc<AtomicU64>>,
    ) -> Self {
        Self {
            id: alloc_id(),
            future: Mutex::new(Box::pin(future)),
            inner: Mutex::new(TaskInner {
                priority,
                state: TaskState::RUNNABLE,
                intr_enable: false,
            }),
            finish: Arc::new(AtomicBool::new(false)),
            affinity,
            waker: spin::Once::new(),
        }
    }

    /// The task's shared waker. Set exactly once by `FutureCollection::insert`
    /// right after the slab slot and waker page exist; every later access is a
    /// plain acquire load.
    pub fn waker(&self) -> &Arc<WakerRef> {
        self.waker.get().expect("task waker not initialized")
    }

    /// Whether this task is allowed to be polled on the given logical CPU.
    ///
    /// A task with no affinity mask runs anywhere. CPU ids `>= 64` are always
    /// allowed (the mask only tracks the first 64 logical CPUs, which matches
    /// `MAX_CORE_NUM`).
    pub fn allowed_on(&self, cpu: usize) -> bool {
        match &self.affinity {
            None => true,
            Some(mask) => cpu >= 64 || (mask.load(Ordering::Relaxed) >> cpu) & 1 != 0,
        }
    }

    /// The task's affinity mask, or `None` when it may run anywhere.
    pub fn affinity_mask(&self) -> Option<u64> {
        self.affinity.as_ref().map(|m| m.load(Ordering::Relaxed))
    }
    pub fn poll(&self, cx: &mut Context) -> Poll<()> {
        // Never poll a task whose future already completed. `finish` is set by
        // `drop_by_ref` the instant a poll returns Ready, BEFORE the generator
        // removes the slab slot and frees the future Box. A stale waker (a
        // net-RX / IoMultiplexWait timer still holding a clone) that re-notifies
        // in that window could otherwise get the slot handed out and re-polled,
        // running the completed future again — its captured state
        // (interest_list buffer, process path) is being torn down, so the
        // re-poll dereferenced freed+poisoned heap (the intermittent
        // sys_epoll_pwait #GP with 0xa5.. in the faulting register). `self` is
        // the Arc<Task> the executor holds, so reading `finish` here is always
        // safe.
        if self.finish.load(Ordering::Relaxed) {
            return Poll::Ready(());
        }
        let mut f = crate::diag::diag_lock(&self.future);
        // (Deliberately no vtable-sniffing "corruption check" here: reading a
        // trait object's {data, vtable} fields via transmute_copy assumes a
        // layout Rust doesn't guarantee, and on master this exact pattern
        // (bbddbb56) caused false positives that silently completed healthy
        // tasks -- 6,500-30,000 busy-polls/s and a deterministic labwc crash
        // at ~35s, fixed by removing it entirely (PR #759). Don't reintroduce
        // it via a future merge from master.)
        f.as_mut().poll(cx)
    }

    /// Retire a future that was interrupted **in the middle of a poll** and
    /// must never be entered again.
    ///
    /// Used by the kernel's panic-containment path (`zcore::oops`): a poll that
    /// panicked left its coroutine stack half-unwound, so the generator's saved
    /// state no longer describes the values it holds — resuming it could
    /// re-run the faulting code, and *dropping* it could double-drop whatever
    /// the aborted poll had already moved out. Neither is acceptable, so the
    /// future is swapped out for an inert `Pending` and the original is
    /// deliberately leaked. One dead future's worth of memory, per contained
    /// fault, is the price of not halting the machine.
    ///
    /// Setting `finish` first means a late waker that gets this task handed out
    /// again finds `poll` returning `Ready` immediately, so the swapped-in
    /// `Pending` is never actually polled either.
    ///
    /// Returns `false` if the future lock could not be reclaimed, which leaves
    /// the task exactly as it was — the caller must then fall back to halting.
    ///
    /// # Safety
    ///
    /// Only the CPU that was polling this task may call this, and only while
    /// that poll is being abandoned: it force-releases the future lock that the
    /// abandoned poll still holds.
    pub unsafe fn abandon(&self) -> bool {
        // The panicking poll holds `future`'s lock and will never release it.
        self.future.force_unlock();
        let Some(mut slot) = self.future.try_lock() else {
            // Someone else took it between the unlock and here — do not touch
            // a future another CPU may be polling.
            return false;
        };
        self.finish.store(true, Ordering::SeqCst);
        let dead = core::mem::replace(
            &mut *slot,
            Box::pin(core::future::pending::<()>()) as Pin<Box<dyn Future<Output = ()> + Send>>,
        );
        core::mem::forget(dead);
        true
    }

    pub fn id(&self) -> usize {
        self.id
    }
}

pub struct FutureCollection {
    pub slab: PinSlab<Arc<Task>>,
    // pub vec: VecDeque<Key>,
    pub pages: Vec<Arc<WakerPage>>,
    pub priority: usize,
    /// Logical CPU that owns this collection; stamped into every `WakerPage`
    /// so a cross-CPU wake knows which CPU to kick with a reschedule IPI.
    pub cpu_id: u8,
}

impl FutureCollection {
    pub fn new(priority: usize, cpu_id: u8) -> Self {
        Self {
            slab: PinSlab::new(),
            // vec: VecDeque::new(),
            pages: vec![],
            priority,
            cpu_id,
        }
    }
    /// Our pages hold 64 contiguous future wakers, so we can do simple arithmetic to access the
    /// correct page as well as the index within page.
    /// Given the `key` representing a future, return a reference to that page, `Arc<WakerPage>`. And
    /// the index _within_ that page (usize).
    pub fn page(&self, key: Key) -> (&Arc<WakerPage>, usize) {
        let (_, page_idx, subpage_idx) = unpack_key(key);
        (&self.pages[page_idx], subpage_idx)
    }

    /// Insert a future into our scheduler returning an integer key representing this future. This
    /// key is used to index into the slab for accessing the future.
    pub fn insert<F: Future<Output = ()> + 'static + Send>(
        &mut self,
        future: F,
        affinity: Option<Arc<AtomicU64>>,
    ) -> Key {
        let key = self
            .slab
            .insert(Arc::new(Task::new(future, self.priority, affinity)));
        // Add a new page to hold this future's status if the current page is filled.
        while key >= self.pages.len() * WAKER_PAGE_SIZE {
            self.pages.push(WakerPage::new(self.cpu_id));
        }
        let (page, subpage_idx) = self.page(key);
        page.initialize(subpage_idx);
        // Build the task's one shared waker now that its page slot exists.
        // (Clone the page Arc first so the `pages` borrow ends before the
        // mutable `slab` access below.)
        let page = page.clone();
        let task = self.slab.get(key).unwrap();
        let waker = Arc::new(page.make_waker(subpage_idx, &task.finish));
        task.waker.call_once(|| waker);
        // self.vec.push_back(key);
        key
    }

    pub fn remove(&mut self, key: Key) {
        let (page, subpage_idx) = self.page(key);
        page.clear(subpage_idx);
        self.slab.remove(unmask_priority(key));
    }
}

pub struct TaskCollection {
    cpu_id: u8, // Just for debug, not used
    future_collections: Vec<Mutex<FutureCollection>>,
    pub task_num: AtomicUsize,
    generator: Option<Mutex<Pin<Box<dyn Coroutine<Yield = Option<Key>, Return = ()>>>>>,
}

/// One line naming a collection whose generator field is gone.
///
/// `new` fills that field before the `Arc` is published and nothing ever
/// clears it, so `None` cannot be a logic error — it means the
/// `TaskCollection` itself has been overwritten. The `unwrap()` that used to
/// stand in both takers turned that into
///
///   panic at task_collection.rs:340: called `Option::unwrap()` on a `None`
///   value
///
/// inside the executor, on a CPU already holding scheduler locks — which
/// `oops` can never contain, so it took the machine down and buried the
/// corruption that caused it. Seen alongside two other cores panicking on the
/// same boot, both of which WERE contained and survived.
///
/// A collection with no generator simply has no tasks to hand out, so the
/// takers report "nothing ready", the executor parks, and the evidence reaches
/// the log instead of the floor. One-shot: a parked queue is re-polled forever.
#[cold]
#[inline(never)]
fn report_missing_generator(cpu_id: u8) {
    static REPORTED: AtomicBool = AtomicBool::new(false);
    if REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }
    error!(
        "[sched] CORRUPTED TaskCollection: cpu {} has no task generator — the \
         collection has been overwritten. Parking this queue instead of \
         panicking inside the executor.",
        cpu_id,
    );
}

impl TaskCollection {
    pub fn new(cpu_id: u8) -> Arc<Self> {
        let mut task_collection = Arc::new(TaskCollection {
            cpu_id,
            future_collections: Vec::with_capacity(MAX_PRIORITY),
            task_num: AtomicUsize::new(0),
            generator: None,
        });
        // SAFETY: no other Arc or Weak pointers
        let tc_clone = task_collection.clone();
        let tc = unsafe { Arc::get_mut_unchecked(&mut task_collection) };
        for priority in 0..MAX_PRIORITY {
            tc.future_collections
                .push(Mutex::new(FutureCollection::new(priority, cpu_id)));
        }
        tc.generator = Some(Mutex::new(Box::pin(TaskCollection::generator(tc_clone))));
        task_collection
    }

    /// 插入一个Future, 其优先级为 DEFAULT_PRIORITY
    pub fn add_task<F: Future<Output = ()> + 'static + Send>(
        &self,
        future: F,
        affinity: Option<Arc<AtomicU64>>,
    ) -> usize {
        self.priority_add_task(DEFAULT_PRIORITY, future, affinity)
    }

    /// remove the task correponding to the key.
    pub fn remove_task(&self, key: Key) {
        let mut inner = self.get_mut_inner(key >> PRIORITY_SHIFT);
        inner.remove(unmask_priority(key));
        self.task_num.fetch_sub(1, Ordering::Relaxed);
    }

    fn priority_add_task<F: Future<Output = ()> + 'static + Send>(
        &self,
        priority: usize,
        future: F,
        affinity: Option<Arc<AtomicU64>>,
    ) -> Key {
        debug_assert!(priority == DEFAULT_PRIORITY);
        let key =
            crate::diag::diag_lock(&self.future_collections[priority]).insert(future, affinity);
        debug_assert!(key < TASK_NUM_PER_PRIORITY);
        self.task_num.fetch_add(1, Ordering::Relaxed);
        key | (priority << PRIORITY_SHIFT)
    }

    fn get_mut_inner(&self, priority: usize) -> MutexGuard<'_, FutureCollection> {
        crate::diag::diag_lock(&self.future_collections[priority])
    }

    pub fn task_num(&self) -> usize {
        self.task_num.load(Ordering::Relaxed)
    }

    /// Diagnostics: `(task_num, notified_bits, dropped_bits, borrowed_bits)`
    /// summed across all waker pages. Used by the executor's hang detector to
    /// tell a lost wake (tasks present, notified == 0) from a take_task bug
    /// (notified > 0 yet nothing is polled).
    pub fn debug_pending(&self) -> (usize, u32, u32, u32) {
        let (mut n, mut d, mut b) = (0u32, 0u32, 0u32);
        for fc in &self.future_collections {
            let inner = crate::diag::diag_lock(fc);
            for page in &inner.pages {
                let (pn, pd, pb) = page.peek();
                n += pn.count_ones();
                d += pd.count_ones();
                b += pb.count_ones();
            }
        }
        (self.task_num(), n, d, b)
    }

    /// Non-blocking take for the work-stealing path: if the generator is busy
    /// (the owning CPU is inside its own `take_task`, possibly parked there by
    /// a timer preemption), give up instead of spinning. A thief that spins
    /// here while holding the victim's runtime lock deadlocks against the
    /// victim's timer IRQ (see `steal_task_from_other_cpu`).
    pub fn try_take_task(&self) -> Option<(Key, Arc<Task>, Arc<WakerRef>)> {
        // See `report_missing_generator`: never `unwrap` here.
        let Some(generator) = self.generator.as_ref() else {
            report_missing_generator(self.cpu_id);
            return None;
        };
        let mut generator = generator.try_lock()?;
        self.resume_generator(&mut generator)
    }

    pub fn take_task(&self) -> Option<(Key, Arc<Task>, Arc<WakerRef>)> {
        // See `report_missing_generator`: never `unwrap` here.
        let Some(generator) = self.generator.as_ref() else {
            report_missing_generator(self.cpu_id);
            return None;
        };
        let mut generator = crate::diag::diag_lock(generator);
        self.resume_generator(&mut generator)
    }

    /// Whether any task on this queue has a published (pending) wake. Pre-halt
    /// recheck for the executor: `try_lock` so a peer mid-insert makes us
    /// conservatively report "ready" instead of spinning — the caller simply
    /// skips the halt and re-runs `take_task`.
    pub fn has_ready(&self) -> bool {
        let cpu = crate::arch::cpu_id() as usize;
        self.future_collections.iter().any(|fc| {
            match fc.try_lock() {
                Some(mut inner) => {
                    for page_idx in 0..inner.pages.len() {
                        let page = &inner.pages[page_idx];
                        let (notified, dropped, borrowed) = page.peek();
                        let runnable = notified & !dropped & !borrowed;
                        if runnable != 0 {
                            for subpage_idx in BitIter::from(runnable) {
                                let key = pack_key(DEFAULT_PRIORITY, page_idx, subpage_idx);
                                let allowed = inner
                                    .slab
                                    .get(unmask_priority(key))
                                    .map(|task| task.allowed_on(cpu))
                                    .unwrap_or(true);
                                if allowed {
                                    return true;
                                }
                            }
                        }
                    }
                    false
                }
                // Keep this `true`: a peer mid-insert/mid-drain holds the lock, and
                // reporting `false` here would let a CPU halt through a wake it could
                // not yet observe, with no timer backstop in the executor's idle path.
                // (master's 1b1d289b/bbddbb56 shipped this as `false` and reintroduced
                // exactly that lost-wake class of bug; reverted there by PR #759 -- do
                // not let a future merge from master bring it back here either.)
                None => true,
            }
        })
    }

    /// Number of tasks on this queue that are *runnable right now* (a wake is
    /// published and no executor holds them).
    ///
    /// This is the load figure the scheduler actually wants. [`task_num`] counts
    /// every task the collection owns, including the ones parked on a `Pending`
    /// future — a CPU hosting fifty sleeping daemons looked fifty times more
    /// loaded than a CPU spinning two CPU-bound threads, so placement pushed new
    /// work *onto* the busy CPU and work stealing probed the idle one first.
    ///
    /// `try_lock`: this runs on the placement and idle/steal paths, which must
    /// never spin on a peer's collection. A momentarily locked collection is
    /// reported as `None` and simply skipped by the caller.
    ///
    /// [`task_num`]: Self::task_num
    pub fn ready_num(&self) -> Option<usize> {
        let inner = self.future_collections[DEFAULT_PRIORITY].try_lock()?;
        Some(
            inner
                .pages
                .iter()
                .map(|p| {
                    let (notified, dropped, borrowed) = p.peek();
                    (notified & !dropped & !borrowed).count_ones() as usize
                })
                .sum(),
        )
    }

    /// Load figure for spawn PLACEMENT — includes the task being polled right
    /// now (`borrowed`), unlike [`ready_num`].
    ///
    /// `ready_num` deliberately excludes `borrowed` because a borrowed task is
    /// checked out to an executor and cannot be stolen; for choosing a steal
    /// target that is correct. For placement it is exactly wrong: a CPU pegged
    /// running a CPU-bound hog has that hog `borrowed`, so `ready_num` reports
    /// it as 0 — indistinguishable from a truly idle CPU. Under 2N hogs on N
    /// CPUs the fork-storm placement scan then stacks new hogs onto whichever
    /// CPUs happen to be mid-poll (load 0) and never rebalances, so one CPU ends
    /// up with 4-5 hogs at 1/5 share each while another runs one at full speed —
    /// the measured 4.46x max/min unfairness. Counting `borrowed` as load makes
    /// a busy CPU advertise load >= 1, so hogs spread ~evenly. Reads the same
    /// page bits, adds no lock, and touches only the (cold) placement path.
    pub fn placement_load(&self) -> Option<usize> {
        let inner = self.future_collections[DEFAULT_PRIORITY].try_lock()?;
        Some(
            inner
                .pages
                .iter()
                .map(|p| {
                    let (notified, dropped, borrowed) = p.peek();
                    ((notified | borrowed) & !dropped).count_ones() as usize
                })
                .sum(),
        )
    }

    #[allow(clippy::type_complexity)]
    fn resume_generator(
        &self,
        generator: &mut Pin<Box<dyn Coroutine<Yield = Option<Key>, Return = ()>>>,
    ) -> Option<(Key, Arc<Task>, Arc<WakerRef>)> {
        // A yielded key can already be dead by the time we look it up: the
        // generator publishes it from the page bitmap, and only THEN do we take
        // the collection lock, so a concurrent `remove_task` on another CPU fits
        // in between. That is not a rare race -- it is exactly what oops
        // containment does ("kernel coroutine retired") -- and the `unwrap()`
        // that used to be here turned every one of those into
        //
        //   panic at task_collection.rs:450: called `Option::unwrap()` on a
        //   `None` value
        //
        // inside the executor, on a CPU already holding scheduler locks. So a
        // fault the kernel had just successfully CONTAINED took the machine
        // down anyway, one line after it reported "contained ... the rest of
        // the system carries on".
        //
        // A dead key means "this slot is gone", so skip it and ask the
        // generator for the next one. `remove` clears the page bit before
        // dropping the slab entry, so a removed key is not yielded again and
        // the retry terminates; the bound is belt-and-braces against a bitmap
        // that disagrees with the slab.
        const MAX_STALE_KEYS: usize = 64;
        for _ in 0..MAX_STALE_KEYS {
            match generator.as_mut().resume(()) {
                CoroutineState::Yielded(Some(key)) => {
                    let (priority, _page_idx, _subpage_idx) = unpack_key(key);
                    let mut inner = self.get_mut_inner(priority);
                    let Some(task) = inner.slab.get(unmask_priority(key)) else {
                        drop(inner);
                        continue;
                    };
                    let task = task.clone();
                    // The task's shared waker doubles as the borrow/drop handle,
                    // so the hot path no longer builds fresh `WakerRef`s (and an
                    // `Arc::new`) on every single poll.
                    let waker = task.waker().clone();
                    return Some((key, task, waker));
                }
                CoroutineState::Yielded(None) => return None,
                _ => panic!("unexpected value from resume"),
            }
        }
        None
    }

    pub fn generator(self: Arc<Self>) -> impl Coroutine<Yield = Option<Key>, Return = ()> {
        #[coroutine]
        static move || {
            loop {
                let priority = DEFAULT_PRIORITY;
                loop {
                    let mut found_key: Option<Key> = None;
                    let mut inner = self.get_mut_inner(priority);
                    // Pass 1 — urgent external wakes. Must finish before any
                    // voluntary yield on *any* page is promoted, or a hog that
                    // just preempted for a sleeper can re-steal the CPU from a
                    // lower-index page while the sleeper sits notified on a
                    // higher one.
                    for page_idx in 0..inner.pages.len() {
                        let page = &inner.pages[page_idx];
                        // `pending` is this page's snapshot, minus whatever has
                        // already been handed out. It must go back into the
                        // page across every yield: see `park_notified`.
                        let mut pending = page.take_notified();
                        let dropped = page.take_dropped();
                        if pending != 0 {
                            let cpu = crate::arch::cpu_id() as usize;
                            while pending != 0 {
                                let subpage_idx = pending.trailing_zeros() as usize;
                                pending &= pending - 1;
                                let key = pack_key(priority, page_idx, subpage_idx);
                                let allowed = inner
                                    .slab
                                    .get(unmask_priority(key))
                                    .map(|task| task.allowed_on(cpu))
                                    .unwrap_or(true);
                                if !allowed {
                                    inner.pages[page_idx].notify(subpage_idx);
                                    let mask = inner
                                        .slab
                                        .get(unmask_priority(key))
                                        .and_then(|task| task.affinity_mask())
                                        .unwrap_or(u64::MAX);
                                    crate::runtime::kick_for_affinity(mask, cpu);
                                    continue;
                                }
                                found_key = Some(key);
                                inner.pages[page_idx].mark_borrowed(subpage_idx, true);
                                inner.pages[page_idx].park_notified(pending);
                                drop(inner);
                                yield found_key;
                                inner = self.get_mut_inner(priority);
                                pending = inner.pages[page_idx].reclaim_notified(pending);
                            }
                        }
                        if dropped != 0 {
                            for subpage_idx in BitIter::from(dropped) {
                                let key = pack_key(priority, page_idx, subpage_idx);
                                self.task_num.fetch_sub(1, Ordering::Relaxed);
                                inner.remove(key);
                            }
                        }
                    }
                    // Pass 2 — voluntary yields, only when nothing urgent remains.
                    if found_key.is_none() {
                        for page_idx in 0..inner.pages.len() {
                            let mut pending = inner.pages[page_idx].take_yielded();
                            if pending == 0 {
                                continue;
                            }
                            let cpu = crate::arch::cpu_id() as usize;
                            while pending != 0 {
                                let subpage_idx = pending.trailing_zeros() as usize;
                                pending &= pending - 1;
                                let key = pack_key(priority, page_idx, subpage_idx);
                                let allowed = inner
                                    .slab
                                    .get(unmask_priority(key))
                                    .map(|task| task.allowed_on(cpu))
                                    .unwrap_or(true);
                                if !allowed {
                                    inner.pages[page_idx].mark_yielded(subpage_idx);
                                    let mask = inner
                                        .slab
                                        .get(unmask_priority(key))
                                        .and_then(|task| task.affinity_mask())
                                        .unwrap_or(u64::MAX);
                                    crate::runtime::kick_for_affinity(mask, cpu);
                                    continue;
                                }
                                found_key = Some(key);
                                inner.pages[page_idx].mark_borrowed(subpage_idx, true);
                                inner.pages[page_idx].park_yielded(pending);
                                drop(inner);
                                yield found_key;
                                inner = self.get_mut_inner(priority);
                                pending = inner.pages[page_idx].reclaim_yielded(pending);
                            }
                        }
                    }
                    if found_key.is_none() {
                        break;
                    }
                }
                yield None;
            }
        }
    }
}

pub use key::*;

/// A task key is one `usize` with three fields, from the top down:
///
/// ```text
///   63      58 57                        6 5        0
///  +----------+---------------------------+----------+
///  | priority |        page number        | subpage  |
///  +----------+---------------------------+----------+
/// ```
///
/// Each field is given once here, as a shift and a mask, and every function
/// below is written from them. They used not to be: `pack_key` wrote five
/// priority bits at 58, `unmask_priority` cleared five bits at 58, and
/// `unpack_key` cleared five bits off the *top of the word* — one bit too
/// high. The lowest priority bit therefore stayed inside the page number, so
/// every odd priority unpacked to page `1 << 52`, and `FutureCollection::page`
/// indexes `pages` with that, inside the generator, on a CPU holding the
/// collection lock. Only priority 4 is ever used today (`priority_add_task`
/// asserts it), which is even, so the tree has never hit it — but a scheduler
/// that grows a second priority class hits it on the first task.
pub mod key {
    pub type Key = usize;
    pub const PRIORITY_SHIFT: usize = 58;
    pub const TASK_NUM_PER_PRIORITY: usize = 1 << PRIORITY_SHIFT;
    pub const MAX_PRIORITY: usize = 1 << 5;
    /// The priority field, five bits wide: the one width all three functions
    /// have to agree on.
    pub const PRIORITY_MASK: usize = MAX_PRIORITY - 1;
    pub const DEFAULT_PRIORITY: usize = 4;

    pub const PAGE_INDEX_SHIFT: usize = 6;
    /// A subpage index names one of a [`WakerPage`]'s 64 slots.
    ///
    /// [`WakerPage`]: crate::waker_page::WakerPage
    pub const SUBPAGE_MASK: usize = (1 << PAGE_INDEX_SHIFT) - 1;
    /// What is left between the two: bits 6..=57.
    pub const PAGE_INDEX_MASK: usize = (1 << (PRIORITY_SHIFT - PAGE_INDEX_SHIFT)) - 1;

    pub fn unpack_key(key: Key) -> (usize, usize, usize) {
        let subpage_idx = key & SUBPAGE_MASK;
        let page_idx = (key >> PAGE_INDEX_SHIFT) & PAGE_INDEX_MASK;
        let priority = (key >> PRIORITY_SHIFT) & PRIORITY_MASK;
        (priority, page_idx, subpage_idx)
    }

    pub fn pack_key(priority: usize, page_idx: usize, subpage_idx: usize) -> Key {
        debug_assert!(priority <= PRIORITY_MASK);
        debug_assert!(page_idx <= PAGE_INDEX_MASK);
        debug_assert!(subpage_idx <= SUBPAGE_MASK);
        (priority << PRIORITY_SHIFT) | (page_idx << PAGE_INDEX_SHIFT) | subpage_idx
    }

    pub fn unmask_priority(key: Key) -> usize {
        key & !(PRIORITY_MASK << PRIORITY_SHIFT)
    }
}

/// The scheduler's task key: one `usize` carrying three fields, packed by one
/// function, taken apart by a second and masked by a third — and none of the
/// three had a test.
///
/// The key is not bookkeeping. `FutureCollection::page` indexes `pages` with
/// the page number this unpacks, inside the generator, on a CPU already
/// holding the collection lock — which is the one place this file's own
/// comments say twice must never panic, because `oops` cannot contain a fault
/// taken with a scheduler lock held.
#[cfg(test)]
mod key_tests {
    use super::key::*;

    /// Page numbers worth trying: the first few, a carry across the subpage
    /// field, and the largest one the field can hold.
    fn pages() -> [usize; 6] {
        [0, 1, 2, 63, 1_000, PAGE_INDEX_MASK]
    }

    #[test]
    fn every_key_survives_the_round_trip() {
        for priority in 0..MAX_PRIORITY {
            for page_idx in pages() {
                for subpage_idx in [0usize, 1, 31, 63] {
                    let key = pack_key(priority, page_idx, subpage_idx);
                    assert_eq!(
                        unpack_key(key),
                        (priority, page_idx, subpage_idx),
                        "priority {} page {} subpage {}",
                        priority,
                        page_idx,
                        subpage_idx
                    );
                }
            }
        }
    }

    #[test]
    fn the_priority_field_is_the_same_width_in_every_function() {
        // `pack_key` writes it, `unpack_key` reads it and `unmask_priority`
        // clears it, and they have to agree on where it ends. They did not:
        // `unpack_key` cleared five bits off the TOP of the word for the page
        // number, where the field it had to clear starts five bits lower —
        // so the lowest priority bit stayed in, and every odd priority came
        // back with a page number of 2^52. `pages[4_503_599_627_370_496]` is
        // the panic described above.
        for priority in 0..MAX_PRIORITY {
            let key = pack_key(priority, 7, 9);
            assert_eq!(
                unmask_priority(key),
                pack_key(0, 7, 9),
                "priority {} left something behind",
                priority
            );
            assert_eq!(
                unpack_key(key).1,
                7,
                "priority {} leaked into the page",
                priority
            );
        }
    }

    #[test]
    fn the_three_fields_do_not_overlap() {
        // Each field alone, read back with the other two at zero.
        assert_eq!(
            unpack_key(pack_key(MAX_PRIORITY - 1, 0, 0)).0,
            MAX_PRIORITY - 1
        );
        assert_eq!(
            unpack_key(pack_key(0, PAGE_INDEX_MASK, 0)).1,
            PAGE_INDEX_MASK
        );
        assert_eq!(unpack_key(pack_key(0, 0, 63)).2, 63);
        // And the boundaries: the largest page number must not reach the
        // priority field, and the largest subpage index must not reach the
        // page number.
        assert_eq!(
            unpack_key(pack_key(0, PAGE_INDEX_MASK, 63)),
            (0, PAGE_INDEX_MASK, 63)
        );
        assert_eq!(pack_key(0, PAGE_INDEX_MASK, 63) >> PRIORITY_SHIFT, 0);
    }

    #[test]
    fn no_key_can_name_a_priority_the_collection_does_not_have() {
        // `TaskCollection::remove_task` reads this priority and hands it to
        // `get_mut_inner`, which indexes `future_collections` — a `Vec` with
        // `MAX_PRIORITY` entries — with no bounds check of its own. `pack_key`
        // never sets bit 63, but a key that reaches there is not always one
        // this module packed: the generator builds keys from a page bitmap,
        // and this file's comments record twice what a bitmap that disagrees
        // with the slab costs. The field is five bits wide, so read five.
        for key in [usize::MAX, 1 << 63, (1 << 63) | (7 << PRIORITY_SHIFT)] {
            let (priority, page_idx, subpage_idx) = unpack_key(key);
            assert!(
                priority < MAX_PRIORITY,
                "key {:#x} named priority {}",
                key,
                priority
            );
            assert!(page_idx <= PAGE_INDEX_MASK);
            assert!(subpage_idx <= SUBPAGE_MASK);
        }
    }

    #[test]
    fn a_page_holds_exactly_the_slots_a_waker_page_has() {
        // `FutureCollection::insert` grows `pages` by `WAKER_PAGE_SIZE` at a
        // time and then indexes the new page with the subpage field, so the
        // field has to be exactly as wide as a page is long.
        assert_eq!(1 << PAGE_INDEX_SHIFT, crate::waker_page::WAKER_PAGE_SIZE);
    }
}

/// One CPU's run queue: what it will hand its executor, and what it refuses to.
///
/// This is the cross-CPU half of the scheduler. A collection belongs to one
/// logical CPU, its generator is resumed both by that CPU's own executor
/// (`take_task`) and by thieves from other CPUs (`try_take_task`), and the
/// three load figures it publishes are what placement and work stealing steer
/// by. 799 lines of it, and the only tests were of the key packing.
///
/// The host `cpu_id()` is hardwired to 0, so "this CPU" is always CPU 0 here
/// and a mask that excludes it is how a task pinned elsewhere is spelled.
#[cfg(test)]
mod collection_tests {
    use super::*;
    use alloc::sync::Arc;
    use alloc::vec::Vec;

    fn pending() -> impl Future<Output = ()> + Send + 'static {
        core::future::pending::<()>()
    }

    /// A mask that allows CPU 1 and not CPU 0, i.e. not us.
    fn pinned_elsewhere() -> Option<Arc<AtomicU64>> {
        Some(Arc::new(AtomicU64::new(1 << 1)))
    }

    /// Drain the queue, returning the keys in the order they were handed out.
    /// Releases each borrow, as a Pending poll would.
    fn drain(tc: &TaskCollection) -> Vec<Key> {
        let mut keys = Vec::new();
        while let Some((key, _task, waker)) = tc.take_task() {
            keys.push(key);
            waker.mark_borrowed(false);
        }
        keys
    }

    // ── what the queue hands out ───────────────────────────────────────────

    #[test]
    fn a_task_is_handed_out_once_and_not_again_until_it_is_given_back() {
        let tc = TaskCollection::new(0);
        let key = tc.add_task(pending(), None);
        let (got, _task, waker) = tc.take_task().expect("a new task is runnable");
        assert_eq!(got, key);
        // Checked out to an executor: every reader of the page has to agree
        // there is nothing here, or a second CPU polls the same future.
        assert!(tc.take_task().is_none(), "handed out while borrowed");
        assert!(!tc.has_ready());
        assert_eq!(tc.ready_num(), Some(0));
        waker.mark_borrowed(false);
        // A Pending poll gives the borrow back but publishes no new wake, so
        // the task waits for one rather than spinning on this CPU.
        assert!(tc.take_task().is_none(), "a Pending poll re-queued itself");
    }

    #[test]
    fn a_task_nobody_has_woken_yet_is_still_polled_once() {
        // `insert` publishes the slot as notified precisely so the future gets
        // to run its first line; without it a spawn never starts.
        let tc = TaskCollection::new(0);
        tc.add_task(pending(), None);
        assert!(tc.has_ready());
        assert_eq!(tc.ready_num(), Some(1));
        assert_eq!(drain(&tc).len(), 1);
    }

    #[test]
    fn every_task_across_two_pages_is_handed_out_exactly_once() {
        // A page holds 64, so the 65th forces a second one and the generator
        // has to walk them both.
        let tc = TaskCollection::new(0);
        let keys: Vec<Key> = (0..65).map(|_| tc.add_task(pending(), None)).collect();
        assert_eq!(unpack_key(keys[64]), (DEFAULT_PRIORITY, 1, 0));
        assert_eq!(tc.task_num(), 65);

        let mut handed = drain(&tc);
        handed.sort_unstable();
        let mut expected = keys.clone();
        expected.sort_unstable();
        assert_eq!(handed, expected, "the second page was skipped or doubled");
    }

    #[test]
    fn an_urgent_wake_is_handed_out_before_a_voluntary_yield() {
        // The two lanes are the interactivity fix: a CPU-bound hog that yields
        // must not race an externally woken task for the next poll slot.
        let tc = TaskCollection::new(0);
        let yielder = tc.add_task(pending(), None);
        let woken = tc.add_task(pending(), None);
        drain(&tc);

        let (py, sy) = {
            let (_, p, s) = unpack_key(yielder);
            (p, s)
        };
        let (pw, sw) = {
            let (_, p, s) = unpack_key(woken);
            (p, s)
        };
        {
            let inner = tc.get_mut_inner(DEFAULT_PRIORITY);
            inner.pages[py].mark_yielded(sy);
            inner.pages[pw].notify(sw);
        }
        assert_eq!(drain(&tc), alloc::vec![woken, yielder]);
    }

    // ── what it refuses ────────────────────────────────────────────────────

    #[test]
    fn a_task_pinned_to_another_cpu_is_refused_and_kept() {
        let tc = TaskCollection::new(0);
        tc.add_task(pending(), pinned_elsewhere());
        assert!(
            tc.take_task().is_none(),
            "handed this CPU a task it is not allowed to run"
        );
        // Refused, not consumed: the wake has to survive for the CPU that can
        // run it, or the thread never starts.
        assert_eq!(tc.task_num(), 1);
        assert_eq!(tc.ready_num(), Some(1), "the wake was swallowed");
        assert!(tc.take_task().is_none());
        assert_eq!(tc.ready_num(), Some(1), "a second pass swallowed it");
    }

    #[test]
    fn a_task_pinned_elsewhere_is_not_work_this_cpu_can_halt_through() {
        // `has_ready` is the pre-halt recheck. Answering "yes" for a task this
        // CPU may not touch makes it skip the halt, find nothing, and go round
        // again until some other CPU steals the task.
        let tc = TaskCollection::new(0);
        tc.add_task(pending(), pinned_elsewhere());
        assert!(!tc.has_ready());
    }

    #[test]
    fn widening_a_mask_at_runtime_releases_the_task_on_the_next_pass() {
        // `sched_setaffinity` writes through the shared mask; the scheduler is
        // meant to observe it on the next placement or steal decision rather
        // than at the next spawn.
        let mask = Arc::new(AtomicU64::new(1 << 1));
        let tc = TaskCollection::new(0);
        let key = tc.add_task(pending(), Some(mask.clone()));
        assert!(tc.take_task().is_none());
        mask.store(1 << 0 | 1 << 1, Ordering::Relaxed);
        assert_eq!(drain(&tc), alloc::vec![key]);
    }

    #[test]
    fn a_mask_that_cannot_name_this_cpu_does_not_refuse_it() {
        // The mask is 64 bits wide and `MAX_CORE_NUM` is 64, so a cpu id past
        // the end is a bug elsewhere; refusing it here would strand the task
        // on a queue nothing can drain.
        let task = Task::new(pending(), DEFAULT_PRIORITY, pinned_elsewhere());
        assert!(!task.allowed_on(0));
        assert!(task.allowed_on(1));
        assert!(task.allowed_on(64), "a shift of 64 is not a refusal");
        assert!(task.allowed_on(usize::MAX));
        // And no mask at all means anywhere.
        let free = Task::new(pending(), DEFAULT_PRIORITY, None);
        assert!(free.allowed_on(0) && free.allowed_on(63) && free.allowed_on(usize::MAX));
    }

    // ── finishing ──────────────────────────────────────────────────────────

    #[test]
    fn a_completed_task_is_removed_and_counted_down_once() {
        let tc = TaskCollection::new(0);
        tc.add_task(pending(), None);
        let (_key, _task, waker) = tc.take_task().unwrap();
        // The executor publishes `dropped` and deliberately leaves the borrow
        // set, so the generator's own remove() is what clears the slot.
        waker.drop_by_ref();
        assert!(tc.take_task().is_none());
        assert_eq!(tc.task_num(), 0, "the count did not follow the removal");
        assert_eq!(tc.debug_pending(), (0, 0, 0, 0), "a lane was left dirty");
    }

    #[test]
    fn a_completed_task_does_not_take_its_neighbours_with_it() {
        let tc = TaskCollection::new(0);
        let dying = tc.add_task(pending(), None);
        let living = tc.add_task(pending(), None);
        let mut wakers = Vec::new();
        while let Some((key, _t, w)) = tc.take_task() {
            if key == dying {
                w.drop_by_ref();
            } else {
                wakers.push((key, w));
            }
        }
        for (_k, w) in &wakers {
            w.mark_borrowed(false);
        }
        assert!(tc.take_task().is_none());
        assert_eq!(tc.task_num(), 1);
        // The survivor's slot is still its own, and a wake still reaches it.
        let (_, p, s) = unpack_key(living);
        tc.get_mut_inner(DEFAULT_PRIORITY).pages[p].notify(s);
        assert_eq!(drain(&tc), alloc::vec![living]);
    }

    #[test]
    fn a_task_that_finishes_before_it_is_ever_polled_is_still_reclaimed() {
        // A thread killed between spawn and its first poll: the slot carries
        // `notified` from `initialize` and `dropped` from the kill.
        let tc = TaskCollection::new(0);
        let key = tc.add_task(pending(), None);
        let (_, p, s) = unpack_key(key);
        tc.get_mut_inner(DEFAULT_PRIORITY).pages[p].mark_dropped(s);
        assert!(tc.take_task().is_none(), "polled a task that was retired");
        assert_eq!(tc.task_num(), 0);
    }

    // ── the three load figures, which are three on purpose ─────────────────

    #[test]
    fn a_cpu_mid_poll_advertises_load_for_placement_but_offers_nothing_to_steal() {
        // `ready_num` picks steal targets: a borrowed task cannot be stolen,
        // so it is not load. `placement_load` picks where a spawn lands: a CPU
        // pegged running a hog has that hog borrowed, and reporting 0 there
        // stacked new hogs onto the busiest cores (the measured 4.46x
        // unfairness). Same bits, two questions, two answers.
        let tc = TaskCollection::new(0);
        tc.add_task(pending(), None);
        tc.add_task(pending(), None);
        assert_eq!(tc.ready_num(), Some(2));
        assert_eq!(tc.placement_load(), Some(2));

        let (_k, _t, waker) = tc.take_task().unwrap();
        assert_eq!(tc.ready_num(), Some(1), "a borrowed task was offered");
        assert_eq!(tc.placement_load(), Some(2), "a busy CPU looked idle");
        waker.mark_borrowed(false);
        assert_eq!(tc.ready_num(), Some(1));
    }

    #[test]
    fn a_sleeping_task_is_not_load_at_all() {
        // `task_num` counts everything the collection owns; the load figures
        // count what is runnable. A CPU hosting fifty sleeping daemons looked
        // fifty times busier than one spinning two threads, so placement
        // pushed work onto the busy CPU and stealing probed the idle one.
        let tc = TaskCollection::new(0);
        for _ in 0..4 {
            tc.add_task(pending(), None);
        }
        drain(&tc);
        assert_eq!(tc.task_num(), 4);
        assert_eq!(tc.ready_num(), Some(0));
        assert_eq!(tc.placement_load(), Some(0));
    }

    #[test]
    fn a_collection_somebody_else_is_holding_is_skipped_not_waited_on() {
        // Both load figures run on the placement and idle/steal paths, which
        // must never spin on a peer's collection; `None` means "skip me this
        // pass". `has_ready` answers the opposite way on purpose: it is a
        // pre-halt recheck, and a peer mid-insert holds the lock, so "yes"
        // makes this CPU look again instead of halting through a wake it
        // could not observe.
        let tc = TaskCollection::new(0);
        tc.add_task(pending(), None);
        let held = tc.get_mut_inner(DEFAULT_PRIORITY);
        assert_eq!(tc.ready_num(), None);
        assert_eq!(tc.placement_load(), None);
        assert!(
            tc.has_ready(),
            "a locked collection must not let a CPU halt"
        );
        drop(held);
        assert_eq!(tc.ready_num(), Some(1));
    }

    #[test]
    fn a_cpu_with_a_backlog_does_not_advertise_itself_as_empty_while_it_polls() {
        // The generator empties a page's whole lane in one swap and hands out
        // one task per resume, so the rest used to live only in a local of a
        // suspended coroutine. Every figure a peer reads — which tasks can be
        // stolen, how loaded this CPU is, whether it may halt — reads the
        // page. A CPU with nine tasks queued therefore told every thief it had
        // nothing, and told spawn placement it was the emptiest CPU on the
        // machine, for as long as it was polling the one it handed out.
        let tc = TaskCollection::new(0);
        for _ in 0..10 {
            tc.add_task(pending(), None);
        }
        let (_k, _t, waker) = tc.take_task().unwrap();
        assert_eq!(tc.ready_num(), Some(9), "a thief was shown an empty queue");
        assert_eq!(
            tc.placement_load(),
            Some(10),
            "a backlogged CPU looked idle"
        );
        assert!(tc.has_ready());
        assert_eq!(tc.debug_pending().1, 9);
        waker.mark_borrowed(false);
        // And the backlog is still every one of them, handed out once each.
        assert_eq!(drain(&tc).len(), 9);
    }

    #[test]
    fn a_backlog_of_voluntary_yields_stays_visible_too() {
        // The low-priority lane is emptied by the same one-swap-then-yield, so
        // it loses its backlog the same way. A CPU running three CPU-bound
        // threads that are taking turns has two of them queued here at any
        // moment, and that is exactly the CPU a thief should be probing.
        let tc = TaskCollection::new(0);
        let keys: Vec<Key> = (0..3).map(|_| tc.add_task(pending(), None)).collect();
        drain(&tc);
        {
            let inner = tc.get_mut_inner(DEFAULT_PRIORITY);
            for key in &keys {
                let (_, p, sp) = unpack_key(*key);
                inner.pages[p].mark_yielded(sp);
            }
        }
        assert_eq!(tc.ready_num(), Some(3));

        let (_k, _t, waker) = tc.take_task().unwrap();
        assert_eq!(
            tc.ready_num(),
            Some(2),
            "the yielded backlog went invisible"
        );
        assert!(tc.has_ready());
        waker.mark_borrowed(false);
        assert_eq!(drain(&tc).len(), 2);
    }

    #[test]
    fn a_task_that_finishes_while_the_rest_wait_does_not_come_back() {
        // The parked set is reclaimed under the same rule the lane itself
        // applies: a slot retired while its wake was parked is gone, not
        // handed to an executor that would poll a completed future.
        let tc = TaskCollection::new(0);
        let keys: Vec<Key> = (0..3).map(|_| tc.add_task(pending(), None)).collect();
        let (first, _t, waker) = tc.take_task().unwrap();
        // Retire one of the two still parked, from "another CPU".
        let doomed = *keys.iter().find(|k| **k != first).unwrap();
        let (_, p, sp) = unpack_key(doomed);
        tc.get_mut_inner(DEFAULT_PRIORITY).pages[p].mark_dropped(sp);
        waker.mark_borrowed(false);

        let handed = drain(&tc);
        assert!(!handed.contains(&doomed), "polled a task that was retired");
        assert_eq!(
            handed.len(),
            1,
            "the third task was lost with the retired one"
        );
        // Three went in, one was retired: the one handed out is still alive.
        assert_eq!(tc.task_num(), 2);
    }

    #[test]
    fn a_wake_that_arrives_while_the_rest_wait_is_not_swallowed() {
        // Reclaiming is masked to exactly what was parked, so a wake published
        // by another CPU in that window stays in the lane for the next pass
        // instead of being taken out by a snapshot that predates it.
        let tc = TaskCollection::new(0);
        let parked = tc.add_task(pending(), None);
        let handed = tc.add_task(pending(), None);
        let latecomer = tc.add_task(pending(), None);
        // Put the latecomer to sleep so only the other two are runnable: take
        // the whole lane and park back everything but its bit.
        let (_, lp, ls) = unpack_key(latecomer);
        {
            let inner = tc.get_mut_inner(DEFAULT_PRIORITY);
            let lane = inner.pages[lp].take_notified();
            inner.pages[lp].park_notified(lane & !(1u64 << ls));
        }
        assert_eq!(tc.ready_num(), Some(2));

        let (first, _t, waker) = tc.take_task().unwrap();
        assert!(first == parked || first == handed);
        // Now wake it, mid-poll.
        tc.get_mut_inner(DEFAULT_PRIORITY).pages[lp].notify(ls);
        waker.mark_borrowed(false);

        let mut rest = drain(&tc);
        rest.sort_unstable();
        let mut expected: Vec<Key> = alloc::vec![parked, handed, latecomer]
            .into_iter()
            .filter(|k| *k != first)
            .collect();
        expected.sort_unstable();
        assert_eq!(rest, expected, "a wake was lost or doubled");
    }

    // ── the thief's entry point ────────────────────────────────────────────

    #[test]
    fn a_thief_gives_up_rather_than_spinning_on_a_busy_generator() {
        // The AB-BA this avoids: the thief holds the victim's runtime lock and
        // spins on its generator; the victim's executor holds that generator
        // and is interrupted by a timer whose `sched_yield` spins on the
        // runtime lock with interrupts off. Neither can move.
        let tc = TaskCollection::new(0);
        tc.add_task(pending(), None);
        let busy = tc.generator.as_ref().unwrap().lock();
        assert!(tc.try_take_task().is_none(), "the thief waited");
        drop(busy);
        assert!(tc.try_take_task().is_some());
    }

    #[test]
    fn a_collection_with_no_generator_parks_instead_of_panicking() {
        // A `None` here cannot be a logic error — `new` fills the field before
        // the Arc is published and nothing clears it — so it means the
        // collection has been overwritten. The `unwrap` that used to stand
        // here turned that into a panic inside the executor, on a CPU already
        // holding scheduler locks, which `oops` can never contain: it took the
        // machine down and buried the corruption that caused it.
        let mut tc = TaskCollection::new(0);
        {
            let tc = unsafe { Arc::get_mut_unchecked(&mut tc) };
            tc.generator = None;
        }
        assert!(tc.take_task().is_none());
        assert!(tc.try_take_task().is_none());
    }

    // ── diagnostics ────────────────────────────────────────────────────────

    #[test]
    fn the_hang_detector_can_tell_a_lost_wake_from_a_queue_that_will_not_give() {
        // `debug_pending` exists to separate the two: tasks present with
        // nothing notified is a lost wake; notified bits with nothing being
        // polled is a take_task bug.
        let tc = TaskCollection::new(0);
        tc.add_task(pending(), None);
        tc.add_task(pending(), None);
        assert_eq!(tc.debug_pending(), (2, 2, 0, 0));

        let (_k, _t, waker) = tc.take_task().unwrap();
        assert_eq!(tc.debug_pending(), (2, 1, 0, 1), "the borrow went unseen");
        waker.drop_by_ref();
        assert_eq!(tc.debug_pending(), (2, 1, 1, 1));
    }
}
