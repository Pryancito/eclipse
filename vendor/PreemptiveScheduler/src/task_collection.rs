use crate::waker_page::{WakerPage, WakerRef, WAKER_PAGE_SIZE};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use bit_iter::BitIter;
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
    /// `finish` is left clear. It is the same `AtomicBool` as
    /// [`WakerRef::dropped`](crate::waker_page::WakerRef), and `drop_by_ref`
    /// only calls `mark_dropped` on the false→true edge. Storing it here made
    /// that edge a no-op: the page bit stayed clear, the generator never
    /// reaped the slab slot, and `borrowed` stuck at 1. The caller publishes
    /// both by calling `drop_by_ref` after this returns. The borrow bit is
    /// still set, so the task cannot be handed out in between, and once
    /// `drop_by_ref` has run a late waker finds `poll` returning `Ready`.
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
    /// How many tasks in `slab` carry a CPU affinity mask.
    ///
    /// Zero is the answer on most machines -- only `spawn_with_affinity` /
    /// `sched_setaffinity` ever sets one -- and it is what lets
    /// [`TaskCollection::ready_num_for`] and [`TaskCollection::has_ready`]
    /// answer from the page bitmaps alone. Without it both walk the runnable
    /// bits one at a time and do a `PinSlab` lookup per bit just to ask
    /// `allowed_on`, on the idle-steal scan (every victim, every pass) and on
    /// the pre-halt recheck (every trip into `hlt`). With no affine task on
    /// the queue the answer cannot differ from the popcount, because
    /// `allowed_on` is unconditionally `true` for a task with no mask.
    pub affine: usize,
}

impl FutureCollection {
    pub fn new(priority: usize, cpu_id: u8) -> Self {
        Self {
            slab: PinSlab::new(),
            // vec: VecDeque::new(),
            pages: vec![],
            priority,
            cpu_id,
            affine: 0,
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
        let affine = affinity.is_some();
        let key = self
            .slab
            .insert(Arc::new(Task::new(future, self.priority, affinity)));
        if affine {
            self.affine += 1;
        }
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

    pub fn remove(&mut self, key: Key) -> bool {
        let slab_key = unmask_priority(key);
        let Some(task) = self.slab.get(slab_key) else {
            return false;
        };
        task.waker().retire_slot();
        if task.affinity.is_some() {
            self.affine = self.affine.saturating_sub(1);
        }
        self.slab.remove(slab_key);
        true
    }
}

pub struct TaskCollection {
    cpu_id: u8,
    future_collections: Vec<Mutex<FutureCollection>>,
    pub task_num: AtomicUsize,
    generator: Option<Mutex<SchedulingCursor>>,
}

const URGENT_QUOTA: usize = 8;

#[derive(Default)]
struct SchedulingCursor {
    urgent_next: usize,
    yielded_next: usize,
    urgent_turns: usize,
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

/// A collection whose `future_collections` is no longer the vector `new` built.
///
/// `new` creates exactly `MAX_PRIORITY` entries before the `Arc` is published,
/// and nothing in the crate ever pushes, pops, truncates or replaces that
/// vector again -- `no_key_can_name_a_priority_the_collection_does_not_have`
/// pins the other half, that no key can name an index outside it. So a length
/// that is not `MAX_PRIORITY` is not a logic error here: the `Vec` header has
/// been written over, exactly as [`report_missing_generator`] describes for the
/// generator field beside it.
///
/// Indexing it anyway produced
///
///   index out of bounds: the len is 0 but the index is 4
///
/// on one CPU and `the len is 4 but the index is 4` on another in the same
/// photograph, which names neither the structure nor the CPU and reads as a
/// scheduler bug rather than as memory corruption. Say what it is.
/// The same verdict, for the callers that cannot carry a `None`.
#[cold]
#[inline(never)]
fn overwritten_collection(cpu_id: u8, len: usize, priority: usize) -> ! {
    if len != MAX_PRIORITY {
        panic!(
            "[sched] CORRUPTED TaskCollection on cpu {}: future_collections has \
             len {} where new() built {} and nothing ever resizes it -- the Vec \
             header has been overwritten (asked for priority {}). The cpu_id \
             above is read from the same wrecked struct.",
            cpu_id, len, MAX_PRIORITY, priority,
        );
    }
    panic!(
        "[sched] TaskCollection on cpu {}: priority {} is outside the {} queues \
         the collection has -- a key named a priority this module cannot pack.",
        cpu_id, priority, MAX_PRIORITY,
    );
}

#[cold]
#[inline(never)]
fn report_overwritten_collection(cpu_id: u8, len: usize) {
    static REPORTED: AtomicBool = AtomicBool::new(false);
    if REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }
    error!(
        "[sched] CORRUPTED TaskCollection: future_collections has len {} where \
         new() built {} and nothing ever resizes it -- the Vec header has been \
         overwritten. The cpu_id field of the same struct reads {}, which is \
         part of the same wreckage and not to be trusted.",
        len, MAX_PRIORITY, cpu_id,
    );
}

impl TaskCollection {
    /// The queue for `priority`, or `None` when this collection has been
    /// overwritten.
    ///
    /// Every caller already has to cope with "no queue right now" -- the
    /// takers park, the load figures are `Option` -- so a corrupted collection
    /// is reported once by name and then behaves like an empty one, instead of
    /// taking the machine down from inside the scheduler with locks held, where
    /// `oops` can never contain it. That is the same trade [`TaskCollection`]
    /// already makes for a missing generator.
    fn queue(&self, priority: usize) -> Option<&Mutex<FutureCollection>> {
        let len = self.future_collections.len();
        if len != MAX_PRIORITY {
            report_overwritten_collection(self.cpu_id, len);
            return None;
        }
        self.future_collections.get(priority)
    }

    pub fn new(cpu_id: u8) -> Arc<Self> {
        Arc::new(TaskCollection {
            cpu_id,
            future_collections: (0..MAX_PRIORITY)
                .map(|priority| Mutex::new(FutureCollection::new(priority, cpu_id)))
                .collect(),
            task_num: AtomicUsize::new(0),
            generator: Some(Mutex::new(SchedulingCursor::default())),
        })
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
        // `unpack_key`, not a bare `key >> PRIORITY_SHIFT`: the priority field
        // is five bits and the shift leaves six, so bit 63 of a key would
        // name a priority up to 63 -- outside the `MAX_PRIORITY` queues the
        // collection has, which `lock_queue` can only answer with the
        // "overwritten collection" panic, inside the scheduler. Every other
        // reader of a key in this file goes through `unpack_key`; this was the
        // one that wrote the field width out by hand, and got it wrong.
        let (priority, _page_idx, _subpage_idx) = unpack_key(key);
        let mut inner = self.get_mut_inner(priority);
        if inner.remove(unmask_priority(key)) {
            self.task_num.fetch_sub(1, Ordering::Relaxed);
        }
    }

    fn priority_add_task<F: Future<Output = ()> + 'static + Send>(
        &self,
        priority: usize,
        future: F,
        affinity: Option<Arc<AtomicU64>>,
    ) -> Key {
        debug_assert!(priority == DEFAULT_PRIORITY);
        let mut inner = self.lock_queue(priority);
        let key = inner.insert(future, affinity);
        debug_assert!(key < TASK_NUM_PER_PRIORITY);
        self.task_num.fetch_add(1, Ordering::Relaxed);
        key | (priority << PRIORITY_SHIFT)
    }

    pub(crate) fn get_mut_inner(&self, priority: usize) -> MutexGuard<'_, FutureCollection> {
        self.lock_queue(priority)
    }

    /// `queue`, locked, for the paths that have nowhere to put a `None`.
    ///
    /// They still get a sentence instead of `index out of bounds: the len is 0
    /// but the index is 4`, which names neither this structure nor the CPU and
    /// reads as an off-by-one in the scheduler rather than as the memory
    /// corruption it is.
    fn lock_queue(&self, priority: usize) -> MutexGuard<'_, FutureCollection> {
        match self.queue(priority) {
            Some(q) => crate::diag::diag_lock(q),
            None => overwritten_collection(self.cpu_id, self.future_collections.len(), priority),
        }
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

    /// Non-blocking take for work stealing. Neither the scheduling cursor nor
    /// the queue may be waited on while holding a victim's runtime lock.
    pub fn try_take_task(&self) -> Option<(Key, Arc<Task>, Arc<WakerRef>)> {
        // See `report_missing_generator`: never `unwrap` here.
        let Some(generator) = self.generator.as_ref() else {
            report_missing_generator(self.cpu_id);
            return None;
        };
        let mut generator = generator.try_lock()?;
        let mut inner = self.queue(DEFAULT_PRIORITY)?.try_lock()?;
        self.schedule(&mut generator, &mut inner)
    }

    pub fn take_task(&self) -> Option<(Key, Arc<Task>, Arc<WakerRef>)> {
        // See `report_missing_generator`: never `unwrap` here.
        let Some(generator) = self.generator.as_ref() else {
            report_missing_generator(self.cpu_id);
            return None;
        };
        let mut generator = crate::diag::diag_lock(generator);
        let mut inner = self.lock_queue(DEFAULT_PRIORITY);
        self.schedule(&mut generator, &mut inner)
    }

    /// Whether any task on this queue has a published (pending) wake. Pre-halt
    /// recheck for the executor: `try_lock` so a peer mid-insert makes us
    /// conservatively report "ready" instead of spinning — the caller simply
    /// skips the halt and re-runs `take_task`.
    ///
    /// Only [`DEFAULT_PRIORITY`] is consulted. Every insert goes through
    /// `add_task`, which always lands there; the other 31 queues stay empty
    /// and `try_lock`ing them on the way into `hlt` was pure cache traffic.
    pub fn has_ready(&self) -> bool {
        let cpu = crate::arch::cpu_id() as usize;
        // A smashed header means we can no longer see the queue. Don't halt:
        // a wake may be sitting where we can no longer name it.
        let Some(fc) = self.queue(DEFAULT_PRIORITY) else {
            return true;
        };
        match fc.try_lock() {
            Some(mut inner) => {
                // No task on this queue has an affinity mask, so `allowed_on`
                // is `true` for every one of them and the bitmaps are the
                // whole answer -- no `PinSlab` lookup per runnable bit on the
                // way into `hlt`. See `FutureCollection::affine`.
                if inner.affine == 0 {
                    return inner.pages.iter().any(|page| {
                        let (notified, dropped, borrowed) = page.peek();
                        notified & !dropped & !borrowed != 0
                    });
                }
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
        let inner = self.queue(DEFAULT_PRIORITY)?.try_lock()?;
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

    /// Runnable tasks on this queue that [`Task::allowed_on`] permits for `cpu`.
    ///
    /// Used by work-stealing to rank victims: a collection full of tasks pinned
    /// elsewhere looks "rich" to [`ready_num`] but has nothing the thief can
    /// take — probing it first just burns `try_lock`s and fires affinity kicks.
    /// Same `try_lock` discipline as [`ready_num`]: `None` means skip this pass.
    pub fn ready_num_for(&self, cpu: usize) -> Option<usize> {
        let mut inner = self.queue(DEFAULT_PRIORITY)?.try_lock()?;
        // Nothing here is pinned anywhere, so every runnable task is one the
        // thief may take and the popcount is exact -- the steal scan ranks
        // every victim on every idle pass, and walking the bits to ask
        // `allowed_on` of a task that has no mask is a `PinSlab` lookup for a
        // `true` nobody can change. See `FutureCollection::affine`.
        if inner.affine == 0 {
            return Some(
                inner
                    .pages
                    .iter()
                    .map(|p| {
                        let (notified, dropped, borrowed) = p.peek();
                        (notified & !dropped & !borrowed).count_ones() as usize
                    })
                    .sum(),
            );
        }
        let mut n = 0usize;
        for page_idx in 0..inner.pages.len() {
            let (notified, dropped, borrowed) = inner.pages[page_idx].peek();
            let runnable = notified & !dropped & !borrowed;
            if runnable == 0 {
                continue;
            }
            for subpage_idx in BitIter::from(runnable) {
                let key = pack_key(DEFAULT_PRIORITY, page_idx, subpage_idx);
                let allowed = inner
                    .slab
                    .get(unmask_priority(key))
                    .map(|task| task.allowed_on(cpu))
                    .unwrap_or(true);
                if allowed {
                    n += 1;
                }
            }
        }
        Some(n)
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
        let inner = self.queue(DEFAULT_PRIORITY)?.try_lock()?;
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

    /// Hand out one task under both locks; no keys, CPU identities, retirement
    /// snapshots, or owning references survive between calls.
    fn schedule(
        &self,
        cursor: &mut SchedulingCursor,
        inner: &mut FutureCollection,
    ) -> Option<(Key, Arc<Task>, Arc<WakerRef>)> {
        for page_idx in 0..inner.pages.len() {
            let dropped = inner.pages[page_idx].take_dropped();
            for subpage_idx in BitIter::from(dropped) {
                if inner.remove(pack_key(DEFAULT_PRIORITY, page_idx, subpage_idx)) {
                    self.task_num.fetch_sub(1, Ordering::Relaxed);
                }
            }
        }

        let cpu = crate::arch::cpu_id() as usize;
        let mut urgent = None;
        let mut yielded = None;
        let mut stranded = false;
        let mut kicked_masks = Vec::new();
        for page_idx in 0..inner.pages.len() {
            let (notified, voluntary, dropped, borrowed) = inner.pages[page_idx].peek_lanes();
            let runnable = (notified | voluntary) & !dropped & !borrowed;
            for subpage_idx in BitIter::from(runnable) {
                let slot = page_idx * WAKER_PAGE_SIZE + subpage_idx;
                let Some(task) = inner.slab.get(slot) else {
                    inner.pages[page_idx].clear(subpage_idx);
                    continue;
                };
                if !task.allowed_on(self.cpu_id as usize) {
                    stranded = true;
                    if let Some(mask) = task.affinity_mask() {
                        if !kicked_masks.contains(&mask) {
                            kicked_masks.push(mask);
                        }
                    }
                }
                if !task.allowed_on(cpu) {
                    continue;
                }
                let bit = 1u64 << subpage_idx;
                if notified & bit != 0 {
                    Self::consider_slot(&mut urgent, slot, cursor.urgent_next);
                } else if voluntary & bit != 0 {
                    Self::consider_slot(&mut yielded, slot, cursor.yielded_next);
                }
            }
        }
        // The destination must see the victim hint before it consumes the IPI.
        if stranded {
            crate::runtime::note_stranded(self.cpu_id as usize);
        } else {
            crate::runtime::note_not_stranded(self.cpu_id as usize);
        }
        for mask in kicked_masks {
            crate::runtime::kick_for_affinity(mask, cpu);
        }

        let serve_yielded =
            yielded.is_some() && (urgent.is_none() || cursor.urgent_turns >= URGENT_QUOTA);
        let slot = if serve_yielded { yielded } else { urgent }?;
        let page_idx = slot / WAKER_PAGE_SIZE;
        let subpage_idx = slot % WAKER_PAGE_SIZE;
        let bit = 1u64 << subpage_idx;
        let page = &inner.pages[page_idx];
        let claimed = if serve_yielded {
            page.reclaim_yielded(bit)
        } else {
            page.reclaim_notified(bit)
        };
        if claimed == 0 {
            return None;
        }
        page.mark_borrowed(subpage_idx, true);
        if serve_yielded {
            cursor.yielded_next = slot + 1;
            cursor.urgent_turns = 0;
        } else {
            cursor.urgent_next = slot + 1;
            cursor.urgent_turns = cursor.urgent_turns.saturating_add(1).min(URGENT_QUOTA);
        }
        let task = inner.slab.get(slot)?.clone();
        let waker = task.waker().clone();
        Some((
            pack_key(DEFAULT_PRIORITY, page_idx, subpage_idx),
            task,
            waker,
        ))
    }

    fn consider_slot(best: &mut Option<usize>, slot: usize, next: usize) {
        if best.is_none_or(|previous| (slot < next, slot) < (previous < next, previous)) {
            *best = Some(slot);
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

    #[test]
    fn the_last_external_owner_releases_the_collection() {
        let tc = TaskCollection::new(0);
        let weak = Arc::downgrade(&tc);
        drop(tc);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn a_peer_handout_uses_the_peers_cpu_not_the_previous_taker() {
        let _g = crate::runtime::resched_test_lock();
        let tc = TaskCollection::new(0);
        let first = tc.add_task(pending(), None);
        tc.add_task(pending(), Some(Arc::new(AtomicU64::new(1))));
        let shared = tc.add_task(pending(), None);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let peer_tc = tc.clone();
        let peer_barrier = barrier.clone();
        let peer = std::thread::spawn(move || {
            let old = crate::arch::set_cpu_id_for_test(1);
            peer_barrier.wait();
            assert_eq!(peer_tc.ready_num_for(1), Some(1));
            let (key, task, waker) = peer_tc.try_take_task().unwrap();
            assert_eq!(key, shared);
            assert!(task.allowed_on(1));
            waker.mark_borrowed(false);
            crate::arch::set_cpu_id_for_test(old);
        });
        let (key, _, waker) = tc.take_task().unwrap();
        assert_eq!(key, first);
        waker.mark_borrowed(false);
        barrier.wait();
        peer.join().unwrap();
    }

    #[test]
    fn continuous_urgent_work_gives_each_voluntary_yielder_bounded_service() {
        let _g = crate::runtime::resched_test_lock();
        let tc = TaskCollection::new(0);
        let hog = tc.add_task(pending(), None);
        let a = tc.add_task(pending(), None);
        let b = tc.add_task(pending(), None);
        drain(&tc);
        let page = tc.get_mut_inner(DEFAULT_PRIORITY).pages[0].clone();
        page.notify(unpack_key(hog).2);
        page.mark_yielded(unpack_key(a).2);
        page.mark_yielded(unpack_key(b).2);
        let mut seen = [false; 2];
        for _ in 0..2 * (URGENT_QUOTA + 1) {
            let (key, _, waker) = tc.take_task().unwrap();
            if key == hog {
                waker.wake_by_ref();
                waker.mark_borrowed(false);
            } else {
                seen[usize::from(key == b)] = true;
                cede(&waker);
            }
        }
        assert_eq!(seen, [true, true]);
    }

    #[test]
    fn a_new_urgent_wake_precedes_the_remaining_voluntary_backlog() {
        let tc = TaskCollection::new(0);
        let a = tc.add_task(pending(), None);
        let b = tc.add_task(pending(), None);
        let urgent = tc.add_task(pending(), None);
        drain(&tc);
        let page = tc.get_mut_inner(DEFAULT_PRIORITY).pages[0].clone();
        page.mark_yielded(unpack_key(a).2);
        page.mark_yielded(unpack_key(b).2);
        let (key, _, waker) = tc.take_task().unwrap();
        assert_eq!(key, a);
        waker.mark_borrowed(false);
        page.notify(unpack_key(urgent).2);
        assert_eq!(tc.take_task().unwrap().0, urgent);
    }

    #[test]
    fn retirement_is_immediate_and_duplicate_removal_does_not_remove_a_replacement() {
        let tc = TaskCollection::new(0);
        let dying = tc.add_task(pending(), None);
        tc.add_task(pending(), None);
        tc.add_task(pending(), None);
        let (key, _, waker) = tc.take_task().unwrap();
        assert_eq!(key, dying);
        waker.drop_by_ref();
        let (_, _, neighbor) = tc.take_task().unwrap();
        neighbor.mark_borrowed(false);
        assert_eq!(tc.task_num(), 2);
        tc.remove_task(dying);
        assert_eq!(tc.task_num(), 2);
        let replacement = tc.add_task(pending(), None);
        assert_eq!(replacement, dying);
        assert!(drain(&tc).contains(&replacement));
        assert_eq!(tc.task_num(), 3);
    }

    #[test]
    fn removing_the_same_slot_twice_never_underflows_the_count() {
        let tc = TaskCollection::new(0);
        let key = tc.add_task(pending(), None);
        tc.remove_task(key);
        tc.remove_task(key);
        assert_eq!(tc.task_num(), 0);
        assert_eq!(tc.debug_pending(), (0, 0, 0, 0));
    }

    #[test]
    fn old_checkout_handles_cannot_release_or_retire_a_reused_slot() {
        let _g = crate::runtime::resched_test_lock();
        let tc = TaskCollection::new(0);
        let old_key = tc.add_task(pending(), None);
        let (_, _, old) = tc.take_task().unwrap();
        tc.remove_task(old_key);
        let new_key = tc.add_task(pending(), None);
        assert_eq!(old_key, new_key);
        let (_, _, new) = tc.take_task().unwrap();
        old.mark_borrowed(false);
        old.drop_by_ref();
        assert_eq!(tc.debug_pending(), (1, 0, 0, 1));
        assert!(tc.take_task().is_none());
        new.mark_borrowed(false);
        new.wake_by_ref();
        assert_eq!(tc.take_task().unwrap().0, new_key);
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn an_affinity_forward_publishes_the_victim_hint_before_kicking_the_destination() {
        static PUBLISHED_BEFORE_KICK: AtomicBool = AtomicBool::new(false);
        fn record(cpu: usize) {
            if cpu == 2 {
                PUBLISHED_BEFORE_KICK.store(
                    crate::runtime::stranded_mask_for_test() & 1 != 0,
                    Ordering::SeqCst,
                );
            }
        }
        let _g = crate::runtime::resched_test_lock();
        let previous = crate::arch::set_cpu_id_for_test(1);
        let ready = crate::runtime::set_executor_ready_mask_for_test(0b111);
        crate::runtime::clear_stranded_for_test();
        crate::runtime::clear_need_resched(2);
        crate::runtime::set_cpu_sleeping(2, false);
        crate::runtime::set_resched_ipi_sender(record);
        PUBLISHED_BEFORE_KICK.store(false, Ordering::SeqCst);
        let tc = TaskCollection::new(0);
        tc.add_task(pending(), Some(Arc::new(AtomicU64::new(1 << 2))));
        tc.add_task(pending(), None);
        let (_, _, waker) = tc.try_take_task().unwrap();
        assert_eq!(crate::runtime::stranded_mask_for_test() & 0b11, 1);
        assert!(PUBLISHED_BEFORE_KICK.load(Ordering::SeqCst));
        waker.mark_borrowed(false);
        crate::runtime::clear_need_resched(2);
        crate::runtime::set_executor_ready_mask_for_test(ready);
        crate::arch::set_cpu_id_for_test(previous);
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

    // ── can a task that ceded the CPU be held off forever? ────────────────

    /// Hand a task back as a voluntary `sched_yield(2)` does: self-wake from
    /// inside its own poll, with the voluntary marker up.
    fn cede(waker: &Arc<WakerRef>) {
        crate::runtime::begin_voluntary_yield(Arc::as_ptr(waker) as usize);
        waker.wake_by_ref();
        crate::runtime::end_voluntary_yield();
        waker.mark_borrowed(false);
    }

    /// Hand a task back having been woken from outside, as a sleeper's timer
    /// does.
    fn notified_back(waker: &Arc<WakerRef>) {
        waker.wake_by_ref();
        waker.mark_borrowed(false);
    }

    /// Two threads that do nothing but `sched_yield(2)` on one CPU share it
    /// evenly, which is what the lane is supposed to do and what the bench said
    /// it was not doing (547.763 turns against **0 in twenty seconds**).
    ///
    /// Here they get 20 and 20. So the lane rotates, and the starvation the
    /// bench measured is not the lane order -- it is the next test.
    #[test]
    fn two_threads_that_only_yield_share_the_cpu_evenly() {
        let _g = crate::runtime::resched_test_lock();
        let tc = TaskCollection::new(0);
        let (a, b) = (tc.add_task(pending(), None), tc.add_task(pending(), None));
        let (mut n_a, mut n_b) = (0, 0);
        for _ in 0..40 {
            let Some((key, _t, waker)) = tc.take_task() else {
                break;
            };
            if key == a {
                n_a += 1;
            } else if key == b {
                n_b += 1;
            }
            cede(&waker);
        }
        assert_eq!((n_a, n_b), (20, 20), "el carril de cesion no rota");
    }

    /// **A cession made from a poll that never returns is lost for good.**
    ///
    /// This is the `sched_yield()` that does not come back, and it is a property
    /// of the queue, not of placement: `take_yielded` defers a yielded bit whose
    /// slot is still *borrowed* -- correctly, because the task already owns a CPU
    /// -- and re-publishes it. Nothing ever looks at it again until the borrow is
    /// released. A borrow is released when the poll returns, and a poll parked by
    /// a mid-poll preemption lives on in a weak executor, which used to be
    /// resumed only when the run queue drained. A twin in a tight yield loop
    /// guarantees it never drains.
    ///
    /// Measured here: the stuck task is handed out **once** and then never
    /// again, while its twin takes 39 of the 40 turns; release the borrow and it
    /// comes straight back. That last part is what says the fix belongs in who
    /// resumes the frame, not in the lane: the lane never lost the bit.
    #[test]
    fn a_cession_from_a_poll_that_never_returns_waits_for_the_borrow() {
        let _g = crate::runtime::resched_test_lock();
        let tc = TaskCollection::new(0);
        let (a, _b) = (tc.add_task(pending(), None), tc.add_task(pending(), None));
        let mut stuck = None;
        let (mut n_a, mut n_b) = (0, 0);
        for round in 0..40 {
            let Some((key, _t, waker)) = tc.take_task() else {
                break;
            };
            if key == a {
                n_a += 1;
            } else {
                n_b += 1;
            }
            if key == a && round == 0 {
                // Cede from inside the poll, exactly as `YieldFuture` does, and
                // then never return from it: the borrow stays.
                crate::runtime::begin_voluntary_yield(Arc::as_ptr(&waker) as usize);
                waker.wake_by_ref();
                crate::runtime::end_voluntary_yield();
                stuck = Some(waker);
                continue;
            }
            cede(&waker);
        }
        assert_eq!(
            (n_a, n_b),
            (1, 39),
            "el que cedio desde un poll vivo tendria que quedarse fuera"
        );

        // Release the borrow -- which is what resuming the weak executor does --
        // and the task is served again at once.
        let waker = stuck.expect("la tarea atascada");
        waker.mark_borrowed(false);
        let mut back = 0;
        for _ in 0..10 {
            let Some((key, _t, waker)) = tc.take_task() else {
                break;
            };
            if key == a {
                back += 1;
            }
            cede(&waker);
        }
        assert_eq!(back, 5, "soltar el prestamo la devuelve a la rotacion");
    }

    /// The `--yieldstall` shape of `eclipse-bench` #1690, as the queue sees it:
    /// two threads in a tight `sched_yield(2)` loop and a third waking every
    /// 200 us, all on one CPU. On Linux that sustains ~1M yields/s.
    ///
    /// The answer this pins down is that **the two yielders do get the CPU**.
    /// Pass 2 is skipped only in an iteration whose pass-1 snapshot was
    /// non-empty, and `found_key` is reset at the top of each iteration — so a
    /// sleeper that is actually asleep between its wakes leaves iterations in
    /// which pass 1 comes up empty, and those are pass 2's. The strict priority
    /// between the lanes is not by itself unbounded starvation.
    ///
    /// This matters because it is the hypothesis for the `sched_yield()` that
    /// did not return for 40 minutes, and it says the lane order alone does not
    /// explain it. Worth keeping as the thing a future change must not break:
    /// it is the ONLY bound the yielded lane has, and it rests entirely on the
    /// notified lane going empty.
    #[test]
    fn two_tight_yielders_still_get_the_cpu_between_a_sleepers_wakes() {
        let _g = crate::runtime::resched_test_lock();
        let tc = TaskCollection::new(0);
        let (y1, y2, sleeper) = (
            tc.add_task(pending(), None),
            tc.add_task(pending(), None),
            tc.add_task(pending(), None),
        );
        assert_ne!(y1, y2);

        let mut handed = Vec::new();
        // The sleeper is awake for one hand-out, then asleep: it publishes no
        // wake until the next 200 us tick. Everything else on the CPU is a
        // yielder re-ceding immediately.
        for round in 0..30 {
            let Some((key, _t, waker)) = tc.take_task() else {
                break;
            };
            handed.push(key);
            if key == sleeper {
                // Asleep now; nothing re-publishes it until its timer. Model
                // the tick landing every tenth round.
                waker.mark_borrowed(false);
                if round % 10 == 9 {
                    notified_back(&waker);
                }
            } else {
                cede(&waker);
            }
        }

        let yields = handed.iter().filter(|k| **k == y1 || **k == y2).count();
        assert!(
            yields >= 20,
            "the tight yielders were starved by one sleeper: {:?}",
            handed
        );
        assert!(handed.contains(&y1) && handed.contains(&y2));
    }

    // ── a collection whose Vec header was written over ─────────────────────

    /// A healthy collection answers for every priority it was built with, and
    /// for none above them.
    #[test]
    fn a_healthy_collection_has_a_queue_for_every_priority_and_no_more() {
        let tc = TaskCollection::new(0);
        assert_eq!(tc.future_collections.len(), MAX_PRIORITY);
        assert!(tc.queue(0).is_some());
        assert!(tc.queue(DEFAULT_PRIORITY).is_some());
        assert!(tc.queue(MAX_PRIORITY - 1).is_some());
        assert!(tc.queue(MAX_PRIORITY).is_none());
    }

    /// The shape the photograph of 2026-10-04 showed: `future_collections` with
    /// a length `new` never built, on two CPUs with two different lengths in
    /// the same crash. Nothing in this crate resizes that vector, so the only
    /// way to get here is a `Vec` header written over -- and the queue has to
    /// read as empty rather than as an index panic inside the scheduler, which
    /// `oops` can never contain because the CPU is holding scheduler locks.
    #[test]
    fn a_collection_whose_vec_header_was_overwritten_reads_as_empty() {
        let mut tc = TaskCollection::new(0);
        // SAFETY: this `Arc` has no other strong or weak holder in the test.
        let inner = Arc::get_mut(&mut tc).unwrap();
        inner.future_collections.truncate(4);
        assert!(inner.queue(DEFAULT_PRIORITY).is_none());
        // And the load figures, which the placement and steal paths read off a
        // peer's collection, skip it instead of taking the machine down.
        assert_eq!(inner.ready_num(), None);
        assert_eq!(inner.placement_load(), None);
        inner.future_collections.clear();
        assert!(inner.queue(DEFAULT_PRIORITY).is_none());
        assert_eq!(inner.ready_num(), None);
    }

    /// The paths with nowhere to put a `None` still say what happened instead
    /// of `index out of bounds: the len is 0 but the index is 4`, which names
    /// neither the structure nor the CPU.
    #[test]
    #[should_panic(expected = "CORRUPTED TaskCollection on cpu 7")]
    fn the_panic_that_is_left_names_the_corruption_and_not_an_index() {
        let mut tc = TaskCollection::new(7);
        // SAFETY: this `Arc` has no other strong or weak holder in the test.
        let inner = Arc::get_mut(&mut tc).unwrap();
        inner.future_collections.clear();
        let _ = inner.get_mut_inner(DEFAULT_PRIORITY);
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

    /// A refusal in the hand-out pass has to reach `runtime::STRANDED`, or the
    /// rescue never runs and the task never runs either.
    ///
    /// The owner refuses it on every pass and kicks an allowed CPU, but the
    /// kick is only read by the idle steal -- so an allowed CPU with work of
    /// its own never comes. Measured: a thread pinned to a CPU it was not born
    /// on got **one** timeslice in twenty seconds while its peer got 544.590.
    #[test]
    fn refusing_a_task_pinned_elsewhere_says_so_where_a_rescuer_reads_it() {
        let _g = crate::runtime::resched_test_lock();
        crate::runtime::clear_stranded_for_test();
        let tc = TaskCollection::new(0);
        tc.add_task(pending(), pinned_elsewhere());

        // The pass refuses it (CPU 0 is not in the mask) and hands out nothing.
        assert!(tc.take_task().is_none());
        assert_eq!(
            crate::runtime::stranded_mask_for_test() & 1,
            1,
            "nobody will ever come for this task"
        );
    }

    /// `sched_yield` parks the task in the yielded lane. Moving its affinity
    /// off this CPU then has to set the same mark: pass 1 sees nothing to
    /// refuse and used to clear `STRANDED` before pass 2 ran.
    #[test]
    fn refusing_a_yielded_task_pinned_elsewhere_still_says_so() {
        let _g = crate::runtime::resched_test_lock();
        crate::runtime::clear_stranded_for_test();
        let tc = TaskCollection::new(0);
        let key = tc.add_task(pending(), pinned_elsewhere());
        assert!(tc.take_task().is_none());
        crate::runtime::clear_stranded_for_test();
        {
            let inner = tc.get_mut_inner(DEFAULT_PRIORITY);
            let (_, page_idx, subpage_idx) = unpack_key(key);
            // The notified refusal re-published the bit. A yield leaves it
            // only in the other lane.
            let _ = inner.pages[page_idx].take_notified();
            inner.pages[page_idx].mark_yielded(subpage_idx);
        }
        assert!(tc.take_task().is_none(), "ran a task pinned elsewhere");
        assert_eq!(
            crate::runtime::stranded_mask_for_test() & 1,
            1,
            "a yielded-lane refusal cleared the mark a rescuer reads"
        );
    }

    /// Panic containment retires the future without storing `finish` first.
    /// That flag is the waker's `dropped` bit; storing it first made
    /// `drop_by_ref` skip `mark_dropped`, and the slot never left the slab.
    #[test]
    fn abandoning_a_poll_still_reaps_the_slab_slot() {
        let tc = TaskCollection::new(0);
        tc.add_task(pending(), None);
        let (_key, task, waker) = tc.take_task().expect("a new task is runnable");
        assert_eq!(tc.task_num(), 1);
        // `abandon` force-unlocks the future lock the in-flight poll still
        // holds. Nothing has polled here, so take the lock and leak the
        // guard: that is the state a panic inside `Task::poll` leaves behind.
        let guard = task.future.try_lock().expect("future lock");
        core::mem::forget(guard);
        assert!(unsafe { task.abandon() });
        waker.drop_by_ref();
        assert!(
            tc.take_task().is_none(),
            "a retired task was handed out again"
        );
        assert_eq!(tc.task_num(), 0, "the slab slot was not reaped");
        assert_eq!(tc.ready_num(), Some(0));
    }

    /// And the bit comes back down, or every CPU that once held an affine task
    /// keeps paying for a rescue scan on every rebalance tick forever.
    #[test]
    fn a_pass_that_refuses_nothing_takes_the_mark_back_off() {
        let _g = crate::runtime::resched_test_lock();
        crate::runtime::clear_stranded_for_test();
        let tc = TaskCollection::new(0);
        let pinned = tc.add_task(pending(), pinned_elsewhere());
        assert!(tc.take_task().is_none());
        assert_eq!(crate::runtime::stranded_mask_for_test() & 1, 1);

        // The task goes away (it migrated, or it exited). The next clean pass
        // has nothing to refuse.
        tc.remove_task(pinned);
        let _ = tc.take_task();
        assert_eq!(
            crate::runtime::stranded_mask_for_test() & 1,
            0,
            "the mark outlived the task that earned it"
        );
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
    fn many_tasks_pinned_to_the_same_cpu_cost_one_kick_per_pass() {
        // Each refused task used to call `kick_for_affinity` on its own; with
        // the kick ranking runtimes by load, that is a `try_lock` walk per
        // task for an IPI that `request_resched` coalesces anyway.
        let _g = crate::runtime::resched_test_lock();
        let saved = crate::runtime::set_executor_ready_mask_for_test(0b11);
        // A request left pending for CPU 1 by an earlier test would coalesce
        // ours away and make the count below read 0 for the wrong reason.
        crate::runtime::clear_need_resched(1);
        let (req0, _, _) = crate::runtime::wakeup_preempt_stats();
        let tc = TaskCollection::new(0);
        for _ in 0..8 {
            tc.add_task(pending(), pinned_elsewhere());
        }
        assert!(tc.take_task().is_none());
        let (req1, _, _) = crate::runtime::wakeup_preempt_stats();
        crate::runtime::clear_need_resched(1);
        crate::runtime::set_executor_ready_mask_for_test(saved);
        assert_eq!(
            req1 - req0,
            1,
            "eight pinned tasks raised {} kicks",
            req1 - req0
        );
        // All eight are still there for CPU 1.
        assert_eq!(tc.ready_num_for(1), Some(8));
    }

    #[test]
    fn ready_num_for_hides_tasks_the_thief_cannot_run() {
        // Steal ranks victims by load. A queue of foreign-affinity tasks must
        // not look stealable to a CPU that cannot poll them, or every idle
        // core piles onto the same useless victim.
        let tc = TaskCollection::new(0);
        tc.add_task(pending(), pinned_elsewhere());
        tc.add_task(pending(), None);
        assert_eq!(tc.ready_num(), Some(2));
        assert_eq!(
            tc.ready_num_for(0),
            Some(1),
            "CPU 0 saw a pinned-elsewhere task"
        );
        assert_eq!(tc.ready_num_for(1), Some(2), "CPU 1 can run both");
    }

    /// The counter that decides whether the affinity-free fast path runs.
    ///
    /// `ready_num_for` and `has_ready` answer straight from the page bitmaps
    /// when no task on the queue carries a mask, and the generator skips its
    /// per-bit slab lookup on the same condition. If the count ever disagreed
    /// with the slab, the fast path would be *wrong* rather than slow: a queue
    /// holding a pinned task would advertise it to a CPU the mask forbids, and
    /// the thief would take it and poll it there. So the count has to follow
    /// every insert and every removal, including the removals the generator
    /// does under its own lock.
    #[test]
    fn the_affinity_free_fast_path_only_runs_when_the_queue_really_is_free_of_masks() {
        let tc = TaskCollection::new(0);
        let plain = tc.add_task(pending(), None);
        assert_eq!(tc.get_mut_inner(DEFAULT_PRIORITY).affine, 0);
        // The two agree with the slow walk while nothing is pinned.
        assert_eq!(tc.ready_num_for(0), tc.ready_num());
        assert!(tc.has_ready());

        let pinned = tc.add_task(pending(), pinned_elsewhere());
        assert_eq!(tc.get_mut_inner(DEFAULT_PRIORITY).affine, 1);
        // With a mask on the queue the fast path must NOT run: CPU 0 can see
        // one of the two tasks, not both.
        assert_eq!(tc.ready_num(), Some(2));
        assert_eq!(tc.ready_num_for(0), Some(1));

        // Retiring the pinned one puts the queue back in the state the fast
        // path is allowed in -- and `remove` is reached through the
        // generator's dropped branch, which is the removal that matters.
        let (_, p, sub) = unpack_key(pinned);
        tc.get_mut_inner(DEFAULT_PRIORITY).pages[p].mark_dropped(sub);
        while tc.take_task().is_some() {}
        assert_eq!(tc.get_mut_inner(DEFAULT_PRIORITY).affine, 0);
        assert_eq!(tc.task_num(), 1);
        assert_eq!(tc.ready_num_for(0), tc.ready_num());

        // And a queue whose only task is pinned elsewhere still reads as empty
        // for us with the counter back in play.
        tc.remove_task(plain);
        tc.add_task(pending(), pinned_elsewhere());
        assert_eq!(tc.get_mut_inner(DEFAULT_PRIORITY).affine, 1);
        assert_eq!(tc.ready_num_for(0), Some(0));
        assert!(!tc.has_ready(), "a pinned-elsewhere task kept CPU 0 awake");
    }

    /// `remove_task` reads the priority field, not six bits off the top of the
    /// word.
    ///
    /// The field is five bits wide (`PRIORITY_MASK`); a bare
    /// `key >> PRIORITY_SHIFT` leaves six, so bit 63 of a key would name a
    /// priority up to 63 -- outside the `MAX_PRIORITY` queues the collection
    /// has, and `lock_queue` can only answer that with the "overwritten
    /// collection" panic, raised inside the scheduler with locks held.
    #[test]
    fn removing_a_task_names_the_same_priority_the_key_was_packed_with() {
        let tc = TaskCollection::new(0);
        let key = tc.add_task(pending(), None);
        assert_eq!(tc.task_num(), 1);
        tc.remove_task(key);
        assert_eq!(tc.task_num(), 0);
        assert!(!tc.has_ready());
        // Every priority the field can hold unpacks to a queue that exists,
        // whatever the highest bit of the word is doing.
        for priority in 0..MAX_PRIORITY {
            let key = pack_key(priority, 7, 3);
            assert_eq!(unpack_key(key).0, priority);
            assert!(tc.queue(unpack_key(key).0).is_some());
        }
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
    fn a_thief_also_skips_a_busy_queue_when_the_scheduling_cursor_is_free() {
        let tc = TaskCollection::new(0);
        tc.add_task(pending(), None);
        let busy = tc.get_mut_inner(DEFAULT_PRIORITY);
        assert!(tc.try_take_task().is_none());
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
            let tc = Arc::get_mut(&mut tc).unwrap();
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
