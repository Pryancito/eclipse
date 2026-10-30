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

/// Make the next [`Task::abandon`] lose the race for the future lock.
///
/// Between the `force_unlock` and the `try_lock` of that function there is a
/// window in which another CPU can take the lock, and the whole point of its
/// `false` return is that window. No test can hit it from outside: forcing it
/// with a real peer thread would put two writers inside the same critical
/// section, which is the very thing the lock is there to stop, so the race is
/// injected here instead -- the same way `waker_page::steal_next_reclaim`
/// injects the lost bit claim. One-shot, per thread, and in a build that is
/// not the test build `abandon_lock_stolen` is `false` and the branch folds
/// away.
#[cfg(test)]
pub(crate) fn steal_next_abandon_lock() {
    ABANDON_LOCK_STOLEN.with(|f| f.set(true));
}

#[cfg(test)]
std::thread_local! {
    static ABANDON_LOCK_STOLEN: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

// Una sola funcion con el cuerpo bajo `cfg` y no dos funciones: dos cuerpos
// `#[cfg]` son dos juegos de lineas de los que la medida de cobertura solo
// puede ejecutar uno, y el del build que no es de test no lo alcanza nada
// publico. Asi la unica linea que no se puede cubrir es el `false`.
#[inline(always)]
fn abandon_lock_stolen() -> bool {
    #[cfg(not(test))]
    return false;
    #[cfg(test)]
    ABANDON_LOCK_STOLEN.with(|f| f.replace(false))
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
        // `drop_by_ref` the instant a poll returns Ready, BEFORE the cursor
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
    /// panicked left its coroutine stack half-unwound, so the future's saved
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
    /// that edge a no-op: the page bit stayed clear, the scheduler never
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
        let reclaimed = if abandon_lock_stolen() {
            None
        } else {
            self.future.try_lock()
        };
        let Some(mut slot) = reclaimed else {
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

    /// Deja el cerrojo del futuro tomado y sin dueño, que es el estado exacto
    /// que deja un panico dentro de [`Task::poll`].
    ///
    /// Costura de test para los del ejecutor
    /// (`executor::metadata_tests`): ahi no se puede entrar en un poll de
    /// verdad -- `Executor::run` necesita una pila de corrutina y un `cr3` --
    /// y sin el cerrojo huerfano `abandon` no tiene nada que forzar, asi que
    /// `Executor::abandon_current_task` se probaria contra un estado que el
    /// camino del panico nunca ve. El campo es privado del modulo, que es por
    /// lo que esto vive aqui y no alli.
    #[cfg(test)]
    pub(crate) fn leak_future_lock_for_test(&self) {
        core::mem::forget(self.future.try_lock().expect("future lock"));
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
        if affine {
            // So the two scans below can tell this slot apart from the ones
            // that may run anywhere, without fetching the task. See
            // `WakerPage::affine_bits`.
            page.mark_affine(subpage_idx);
        }
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
                    if runnable == 0 {
                        continue;
                    }
                    // A runnable slot whose task carries no mask is allowed
                    // here, full stop -- `allowed_on` is unconditionally
                    // `true` for a task with no mask, which is the whole of
                    // the `affine == 0` shortcut above narrowed to one slot.
                    // So one `and` answers for every such slot on the page
                    // and only the masked ones are worth a `PinSlab` lookup.
                    // Both lanes are read under this collection's lock, which
                    // `insert` also needs, so the two cannot disagree.
                    let affine = page.affine_bits();
                    if runnable & !affine != 0 {
                        return true;
                    }
                    for subpage_idx in BitIter::from(runnable & affine) {
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
            // Same split as `has_ready`, except that this figure is a count
            // and not a question, so it has no early exit and the popcount is
            // the whole of the saving: unmasked runnable slots are allowed for
            // every thief and are counted without being fetched.
            let affine = inner.pages[page_idx].affine_bits();
            n += (runnable & !affine).count_ones() as usize;
            for subpage_idx in BitIter::from(runnable & affine) {
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

        let (slot, serve_yielded) = if inner.affine == 0 {
            crate::runtime::note_not_stranded(self.cpu_id as usize);
            if cursor.urgent_turns >= URGENT_QUOTA {
                Self::bitmap_candidate(inner, cursor.yielded_next, true)
                    .map(|slot| (slot, true))
                    .or_else(|| {
                        Self::bitmap_candidate(inner, cursor.urgent_next, false)
                            .map(|slot| (slot, false))
                    })
            } else {
                Self::bitmap_candidate(inner, cursor.urgent_next, false)
                    .map(|slot| (slot, false))
                    .or_else(|| {
                        Self::bitmap_candidate(inner, cursor.yielded_next, true)
                            .map(|slot| (slot, true))
                    })
            }?
        } else {
            let cpu = crate::arch::cpu_id() as usize;
            let mut urgent = None;
            let mut yielded = None;
            let mut stranded = false;
            let mut kick_mask = 0u64;
            for page_idx in 0..inner.pages.len() {
                let (notified, voluntary, dropped, borrowed) = inner.pages[page_idx].peek_lanes();
                // Like the masks in `consider_bits`, this one is an economy
                // and not the rule: everything it feeds is filtered again by
                // `& notified` or `& voluntary` below, and whatever comes out
                // of all that is still only a CANDIDATE -- the claim at the
                // end of this function asks `reclaim_*`, which is what
                // actually refuses a dropped or borrowed slot. So a mutation
                // of `runnable` survives the suite because it changes how
                // many slots are looked at and nothing else. Worth knowing
                // before reading a surviving mutant here as a missing test:
                // the ones that are NOT economies are the lane each slot is
                // claimed FROM (a slot claimed from the wrong lane is never
                // handed out again) and the quota comparison, and both have
                // their own test.
                let runnable = (notified | voluntary) & !dropped & !borrowed;
                if runnable == 0 {
                    continue;
                }
                // The same split `has_ready` and `ready_num_for` have had
                // since #1791, which this branch never got: `affine` is the
                // lane of slots whose task carries a mask, so a runnable slot
                // OUTSIDE it is allowed on every CPU, cannot be stranded and
                // contributes nothing to the kick mask. One `and` answers for
                // all of them, and only the masked ones are worth a `PinSlab`
                // lookup and an `Arc` deref.
                //
                // It is not a micro-optimisation: this branch runs on every
                // `schedule` as soon as ONE task anywhere in the collection is
                // pinned (`inner.affine != 0`), and it ran a lookup per
                // runnable bit. A pinned pipe round trip measured 992-1105 us
                // against 451 unpinned, with nothing else differing.
                //
                // Both lanes are read under this collection's lock, which
                // `insert` also needs, so the two cannot disagree.
                let affine = inner.pages[page_idx].affine_bits();
                let free = runnable & !affine;
                Self::consider_bits(&mut urgent, free & notified, page_idx, cursor.urgent_next);
                Self::consider_bits(
                    &mut yielded,
                    free & voluntary & !notified,
                    page_idx,
                    cursor.yielded_next,
                );
                for subpage_idx in BitIter::from(runnable & affine) {
                    let slot = page_idx * WAKER_PAGE_SIZE + subpage_idx;
                    let Some(task) = inner.slab.get(slot) else {
                        inner.pages[page_idx].clear(subpage_idx);
                        continue;
                    };
                    let mask = task.affinity_mask().unwrap_or(u64::MAX);
                    if self.cpu_id < 64 && mask & (1u64 << self.cpu_id) == 0 {
                        stranded = true;
                        kick_mask |= mask;
                    }
                    if cpu < 64 && mask & (1u64 << cpu) == 0 {
                        continue;
                    }
                    let bit = 1u64 << subpage_idx;
                    // `bit` esta en `runnable`, que es `(notified | voluntary)`
                    // menos lo muerto y lo prestado: o esta en un carril o
                    // esta en el otro, y no hay tercer caso. Escrito como un
                    // `else if` quedaba una rama que ninguna entrada podia
                    // tomar; el invariante se dice y se comprueba.
                    if notified & bit != 0 {
                        Self::consider_slot(&mut urgent, slot, cursor.urgent_next);
                    } else {
                        debug_assert!(
                            voluntary & bit != 0,
                            "un hueco listo que no esta en ningun carril"
                        );
                        Self::consider_slot(&mut yielded, slot, cursor.yielded_next);
                    }
                }
            }
            // Publish before forwarding. One destination is enough: it can take
            // an allowed task, and its victim scan forwards remaining masks to
            // another CPU (the kick picker excludes the current taker).
            if stranded {
                crate::runtime::note_stranded(self.cpu_id as usize);
            } else {
                crate::runtime::note_not_stranded(self.cpu_id as usize);
            }
            if kick_mask != 0 {
                crate::runtime::kick_for_affinity(kick_mask, cpu);
            }
            let serve_yielded =
                yielded.is_some() && (urgent.is_none() || cursor.urgent_turns >= URGENT_QUOTA);
            (
                (if serve_yielded { yielded } else { urgent })?,
                serve_yielded,
            )
        };
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
        // The claim end of the wake-to-run latency probe. Only the urgent lane
        // is measured: a voluntary yield is a task giving up a CPU it had, so
        // the interval between its self-wake and its next poll is not a wait
        // for a CPU. The swap is unconditional anyway -- a yielded slot can
        // carry a stamp from an earlier external wake, and leaving it there
        // would date the NEXT claim from a wake two turns ago.
        let stamp = page.take_wake_stamp(subpage_idx);
        if !serve_yielded {
            crate::runtime::note_wake_serviced(stamp);
        }
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

    /// [`consider_slot`] for a whole page of candidates at once, in constant
    /// time instead of one call per set bit.
    ///
    /// Removing the `PinSlab` lookup from the unmasked slots is only half the
    /// saving: the walk itself is the other half, and on a queue of a thousand
    /// tasks it is the larger one. The ordering `consider_slot` implements
    /// needs at most two bits of any page -- the lowest at or after `next`
    /// (which wins outright) and the lowest overall (which is the wrap-around
    /// answer) -- so a mask and two `trailing_zeros` replace the walk. This is
    /// the same reasoning as [`Self::bitmap_candidate`], which is why the
    /// no-affinity path never had to walk bits either.
    ///
    /// The ordering itself lives in `consider_slot`, which re-reads `next` for
    /// every candidate it is given. So the masks here are an economy and not a
    /// correctness argument: proposing a candidate this function could have
    /// filtered out cannot change the answer, only the work. That is worth
    /// knowing before trusting a mutation that survives in the masks.
    ///
    /// [`consider_slot`]: Self::consider_slot
    fn consider_bits(best: &mut Option<usize>, bits: u64, page_idx: usize, next: usize) {
        if bits == 0 {
            return;
        }
        let base = page_idx * WAKER_PAGE_SIZE;
        // Bits of this page whose slot is at or after `next`. Wholly before
        // it, none qualify; wholly after, all do.
        let at_or_after = if next <= base {
            bits
        } else if next >= base + WAKER_PAGE_SIZE {
            0
        } else {
            bits & (u64::MAX << (next - base))
        };
        if at_or_after != 0 {
            Self::consider_slot(best, base + at_or_after.trailing_zeros() as usize, next);
        }
        // And the lowest bit of the page, which is the answer once the cursor
        // has to wrap. Harmless when it is the same slot as above.
        Self::consider_slot(best, base + bits.trailing_zeros() as usize, next);
    }

    fn consider_slot(best: &mut Option<usize>, slot: usize, next: usize) {
        if best.is_none_or(|previous| (slot < next, slot) < (previous < next, previous)) {
            *best = Some(slot);
        }
    }

    fn bitmap_candidate(inner: &mut FutureCollection, next: usize, yielded: bool) -> Option<usize> {
        let page_count = inner.pages.len();
        if page_count == 0 {
            return None;
        }
        let first_page = (next / WAKER_PAGE_SIZE) % page_count;
        let first_bit = next % WAKER_PAGE_SIZE;
        for step in 0..=page_count {
            if step == page_count && first_bit == 0 {
                break;
            }
            let page_idx = (first_page + step) % page_count;
            let page = &inner.pages[page_idx];
            let (notified, voluntary, dropped, borrowed) = page.peek_lanes();
            let mut bits = if yielded {
                voluntary & !notified
            } else {
                notified
            } & !dropped
                & !borrowed;
            if step == 0 {
                bits &= u64::MAX << first_bit;
            } else if step == page_count {
                bits &= (1u64 << first_bit) - 1;
            }
            while bits != 0 {
                let subpage_idx = bits.trailing_zeros() as usize;
                let slot = page_idx * WAKER_PAGE_SIZE + subpage_idx;
                if inner.slab.get(slot).is_some() {
                    return Some(slot);
                }
                page.clear(subpage_idx);
                bits &= bits - 1;
            }
        }
        None
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

    /// Rows for the three functions this module tests.
    ///
    /// Nothing here allocates, locks or touches memory it did not already
    /// have in a register, so the figures are single nanoseconds and the only
    /// way to read them is against each other. These run in the test profile,
    /// so `pack_key`'s three `debug_assert!`s are *in* its figure and are not
    /// in the kernel's.
    #[cfg(test)]
    mod benches {
        use super::*;
        use test::{black_box, Bencher};

        // No control row. The one that stood here measured 1.24 ns and every
        // row below it is cheaper -- `strip_the_priority_field` at 0.29 --
        // because a control that must not be inlined pays a `call` and a `ret`
        // and these three shifts-and-masks do not even survive as a call. A
        // floor above the rows it bounds measures the floor, so it is gone;
        // the same happened in the crate's other three bench modules, and
        // `crate::waker_page::waker_page_tests::benches` carries the long form
        // of the argument.

        #[bench]
        fn pack_a_key(b: &mut Bencher) {
            b.iter(|| pack_key(black_box(DEFAULT_PRIORITY), black_box(1_000), black_box(17)));
        }

        #[bench]
        fn unpack_a_key(b: &mut Bencher) {
            let key = pack_key(DEFAULT_PRIORITY, 1_000, 17);
            b.iter(|| unpack_key(black_box(key)));
        }

        #[bench]
        fn strip_the_priority_field(b: &mut Bencher) {
            let key = pack_key(DEFAULT_PRIORITY, 1_000, 17);
            b.iter(|| unmask_priority(black_box(key)));
        }

        /// What the generator does per handout: pack the slot it chose, then
        /// mask the priority off again for the slab lookup.
        #[bench]
        fn pack_then_strip_as_the_generator_does(b: &mut Bencher) {
            b.iter(|| {
                unmask_priority(pack_key(
                    black_box(DEFAULT_PRIORITY),
                    black_box(1_000),
                    black_box(17),
                ))
            });
        }
    }
}

/// One CPU's run queue: what it will hand its executor, and what it refuses to.
///
/// This is the cross-CPU half of the scheduler. A collection belongs to one
/// logical CPU, its cursor is used both by that CPU's own executor
/// (`take_task`) and by thieves from other CPUs (`try_take_task`), and the
/// three load figures it publishes are what placement and work stealing steer
/// by. 799 lines of it, and the only tests were of the key packing.
///
/// Host CPU identity defaults to 0; migration tests override it per host thread.
#[cfg(test)]
mod collection_tests {
    use super::*;
    use alloc::sync::Arc;
    use alloc::vec::Vec;

    fn pending() -> impl Future<Output = ()> + Send + 'static {
        core::future::pending::<()>()
    }

    #[test]
    fn task_ids_are_handed_out_once_each() {
        let first = alloc_id();
        assert!(alloc_id() > first, "los ids ni se repiten ni bajan");
        let a = Task::new(pending(), 0, None);
        let b = Task::new(pending(), 0, None);
        assert_ne!(a.id(), b.id());
        assert_ne!(a.id(), 0, "el cero es «ninguna tarea» en el ejecutor");
    }

    /// `affinity_mask` es la lectura que usan el reparto y el rescate; lo que
    /// la separa de `allowed_on` es que distingue «sin mascara» de «mascara
    /// vacia», y la segunda es una tarea que no puede correr en ningun sitio.
    #[test]
    fn a_task_reports_its_mask_apart_from_having_none() {
        let free = Task::new(pending(), 0, None);
        assert_eq!(free.affinity_mask(), None);
        assert!(free.allowed_on(0) && free.allowed_on(63) && free.allowed_on(64));

        let mask = Arc::new(AtomicU64::new(0b0101));
        let pinned = Task::new(pending(), 0, Some(mask.clone()));
        assert_eq!(pinned.affinity_mask(), Some(0b0101));
        assert!(pinned.allowed_on(0) && pinned.allowed_on(2));
        assert!(!pinned.allowed_on(1) && !pinned.allowed_on(3));
        assert!(
            pinned.allowed_on(64),
            "la mascara solo cubre las 64 primeras CPUs logicas"
        );

        // `sched_setaffinity` cambia la mascara DENTRO del `Arc`, no el
        // `Option`: la lectura tiene que ver el cambio.
        mask.store(0b1000, Ordering::Relaxed);
        assert_eq!(pinned.affinity_mask(), Some(0b1000));
        assert!(!pinned.allowed_on(0) && pinned.allowed_on(3));

        let nowhere = Task::new(pending(), 0, Some(Arc::new(AtomicU64::new(0))));
        assert_eq!(
            nowhere.affinity_mask(),
            Some(0),
            "«en ninguna parte» no es lo mismo que «donde sea»"
        );
        assert!(!nowhere.allowed_on(0));
    }

    /// **Una tarea cuyo futuro ya acabo no se vuelve a sondear.**
    ///
    /// `finish` lo pone `drop_by_ref` en el instante en que un sondeo devuelve
    /// `Ready`, ANTES de que el cursor quite el hueco del slab y libere el
    /// `Box` del futuro. Un waker rancio -- un temporizador de red o de
    /// `IoMultiplexWait` que aun tiene un clon -- que notifique en esa ventana
    /// conseguiria que el hueco se repartiera y el futuro acabado se sondeara
    /// otra vez, sobre un monton ya liberado y envenenado: era el `#GP`
    /// intermitente de `sys_epoll_pwait` con `0xa5..` en el registro.
    ///
    /// El futuro de este test entra en panico si alguien lo sondea, asi que lo
    /// que se comprueba no es solo el `Ready` que sale, es que el futuro no se
    /// toco.
    #[test]
    fn a_task_whose_future_already_finished_is_not_polled_again() {
        struct Exploding;
        impl Future for Exploding {
            type Output = ();
            fn poll(self: Pin<&mut Self>, _: &mut Context) -> Poll<()> {
                panic!("se sondeo el futuro de una tarea acabada");
            }
        }
        let task = Task::new(Exploding, 0, None);
        let mut cx = Context::from_waker(core::task::Waker::noop());
        task.finish.store(true, Ordering::Relaxed);
        assert!(matches!(task.poll(&mut cx), Poll::Ready(())));
        // Y sin la marca el sondeo si llega al futuro, que es lo que dice que
        // el `Ready` de arriba sale de la marca y no de otra cosa.
        task.finish.store(false, Ordering::Relaxed);
        let entered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut cx = Context::from_waker(core::task::Waker::noop());
            task.poll(&mut cx)
        }));
        assert!(entered.is_err(), "la marca es lo unico que tapa el sondeo");
    }

    /// **El `false` de `abandon`: el cerrojo del futuro no se recupero.**
    ///
    /// `abandon` fuerza la apertura del cerrojo que el sondeo en panico se
    /// llevo y lo vuelve a tomar; entre esas dos cosas otra CPU puede tomarlo,
    /// y entonces lo unico seguro es no tocar nada y decirlo, porque el
    /// `mem::replace` de abajo escribiria un futuro que otro esta sondeando.
    /// La carrera se inyecta con `steal_next_abandon_lock` (ver su doc).
    ///
    /// El cerrojo se toma y se olvida antes de llamar, que es la precondicion
    /// de la funcion y no un detalle del test: `spin::Mutex` es aqui un
    /// TicketMutex (`kernel-sync` enciende `spin/use_ticket_mutex` para todo
    /// el arbol), y en un ticket un `force_unlock` sin dueño adelanta el turno
    /// servido y deja el cerrojo imposible de tomar para siempre.
    ///
    /// Lo que se comprueba es la mitad que importa: que la tarea se queda
    /// EXACTAMENTE como estaba, es decir que el futuro original sigue ahi y no
    /// lo ha sustituido el `Pending` inerte.
    #[test]
    fn abandoning_a_task_whose_lock_a_peer_took_changes_nothing() {
        static POLLED: AtomicBool = AtomicBool::new(false);
        struct Marking;
        impl Future for Marking {
            type Output = ();
            fn poll(self: Pin<&mut Self>, _: &mut Context) -> Poll<()> {
                POLLED.store(true, Ordering::SeqCst);
                Poll::Pending
            }
        }
        fn poll_once(task: &Task) -> Poll<()> {
            let mut cx = Context::from_waker(core::task::Waker::noop());
            task.poll(&mut cx)
        }
        let task = Task::new(Marking, 0, None);

        // El sondeo que entro en panico: tiene el cerrojo y no lo suelta.
        core::mem::forget(task.future.lock());
        POLLED.store(false, Ordering::SeqCst);
        steal_next_abandon_lock();
        assert!(
            !unsafe { task.abandon() },
            "se dio por bueno un cerrojo ajeno"
        );
        assert_eq!(poll_once(&task), Poll::Pending);
        assert!(
            POLLED.load(Ordering::SeqCst),
            "el futuro original se perdio"
        );

        // Y sin la carrera si lo cambia por el `Pending` inerte, que es el
        // otro lado del contrato.
        core::mem::forget(task.future.lock());
        POLLED.store(false, Ordering::SeqCst);
        assert!(unsafe { task.abandon() });
        assert_eq!(poll_once(&task), Poll::Pending);
        assert!(
            !POLLED.load(Ordering::SeqCst),
            "el futuro abandonado se volvio a entrar"
        );
    }

    /// El `Debug` de `Task` toma el cerrojo interno con `diag_lock`, que es lo
    /// que lo hace imprimible desde el camino de un informe: con el cerrojo ya
    /// tomado por el que informa habria sido un deadlock dentro del panico.
    #[test]
    fn a_task_prints_its_scheduler_state() {
        let task = Task::new(pending(), 3, None);
        let printed = alloc::format!("{:?}", task);
        assert!(printed.contains("priority: 3"), "{}", printed);
        assert!(printed.contains("RUNNABLE"), "{}", printed);
        assert!(printed.contains("intr_enable: false"), "{}", printed);
    }

    /// La preferencia del cursor: de los huecos listos, el primero a partir de
    /// `next`, y solo si no hay ninguno se vuelve al principio. Es lo que hace
    /// que el reparto sea una rueda y no un «siempre el hueco 0».
    #[test]
    fn the_cursor_prefers_the_first_slot_at_or_after_it_before_wrapping() {
        const NEXT: usize = 10;
        let mut best = None;
        TaskCollection::consider_slot(&mut best, 3, NEXT);
        assert_eq!(best, Some(3), "sin nada mejor, el que haya");
        TaskCollection::consider_slot(&mut best, 20, NEXT);
        assert_eq!(
            best,
            Some(20),
            "un hueco a partir del cursor gana al de antes"
        );
        TaskCollection::consider_slot(&mut best, 12, NEXT);
        assert_eq!(best, Some(12), "y entre esos, el mas cercano al cursor");
        TaskCollection::consider_slot(&mut best, 30, NEXT);
        assert_eq!(best, Some(12));
        TaskCollection::consider_slot(&mut best, 0, NEXT);
        assert_eq!(best, Some(12), "dar la vuelta no adelanta a nadie");
        TaskCollection::consider_slot(&mut best, NEXT, NEXT);
        assert_eq!(
            best,
            Some(NEXT),
            "el cursor mismo es el primero de la vuelta"
        );

        // Y antes del cursor, el mas pequeño: es la vuelta completa.
        let mut wrapped = None;
        for slot in [7, 2, 9, 5] {
            TaskCollection::consider_slot(&mut wrapped, slot, NEXT);
        }
        assert_eq!(wrapped, Some(2));
    }

    /// `bitmap_candidate` es el barrido que elige el hueco; lo que se prueba
    /// aqui es a quien NO mira, porque cada uno de esos carriles repartido es
    /// una tarea corriendo dos veces o un futuro ya muerto.
    #[test]
    fn the_bitmap_scan_skips_dropped_and_borrowed_slots() {
        let mut inner = FutureCollection::new(DEFAULT_PRIORITY, 0);
        assert_eq!(
            TaskCollection::bitmap_candidate(&mut inner, 0, false),
            None,
            "una coleccion sin paginas no nombra ningun hueco"
        );
        for _ in 0..3 {
            inner.insert(pending(), None);
        }
        let (page, _) = inner.page(pack_key(DEFAULT_PRIORITY, 0, 0));
        let page = page.clone();

        assert_eq!(
            TaskCollection::bitmap_candidate(&mut inner, 0, false),
            Some(0)
        );
        page.mark_borrowed(0, true);
        assert_eq!(
            TaskCollection::bitmap_candidate(&mut inner, 0, false),
            Some(1),
            "un hueco prestado es un poll en vuelo: repartirlo es correrlo dos veces"
        );
        page.mark_dropped(1);
        assert_eq!(
            TaskCollection::bitmap_candidate(&mut inner, 0, false),
            Some(2),
            "un hueco retirado es un futuro muerto"
        );
        page.mark_borrowed(2, true);
        assert_eq!(TaskCollection::bitmap_candidate(&mut inner, 0, false), None);
    }

    /// El carril voluntario se sirve aparte y detras: una cesion que tambien
    /// tiene una urgencia pendiente se reparte por la urgencia, no por la
    /// cesion.
    #[test]
    fn a_voluntary_yield_behind_an_urgent_wake_is_not_served_as_a_yield() {
        let mut inner = FutureCollection::new(DEFAULT_PRIORITY, 0);
        for _ in 0..2 {
            inner.insert(pending(), None);
        }
        let page = inner.pages[0].clone();
        // `insert` publica el hueco como urgente; una cesion es lo que queda
        // cuando esa urgencia ya se sirvio.
        page.reclaim_notified(0b11);
        page.mark_yielded(0);
        page.mark_yielded(1);
        assert_eq!(
            TaskCollection::bitmap_candidate(&mut inner, 0, true),
            Some(0)
        );

        page.notify(0);
        assert_eq!(
            TaskCollection::bitmap_candidate(&mut inner, 0, true),
            Some(1),
            "el hueco 0 tiene una urgencia pendiente: ya no es una cesion"
        );
        assert_eq!(
            TaskCollection::bitmap_candidate(&mut inner, 0, false),
            Some(0)
        );

        // Lo de arriba lo decide `notify`, que al publicar la urgencia APAGA
        // el bit de cesion: el hueco 0 sale de la via voluntaria y el barrido
        // ni lo mira. Pero los dos bits si pueden estar encendidos a la vez,
        // y por el camino de siempre: la urgencia llega mientras un ejecutor
        // tiene la tarea (`notify` sobre un hueco prestado, que se queda
        // publicado y diferido) y despues la tarea cede ella misma, que es
        // `mark_yielded` y no apaga nada. Ahi el que decide es el `& !notified`
        // del barrido, y servir ese hueco como cesion es servirlo detras de
        // todas las urgencias teniendo una pendiente.
        page.mark_yielded(0);
        assert_eq!(
            page.peek_lanes().0 & 1,
            1,
            "la urgencia del hueco 0 sigue publicada"
        );
        assert_eq!(page.peek_lanes().1 & 1, 1, "y ademas ha cedido");
        assert_eq!(
            TaskCollection::bitmap_candidate(&mut inner, 0, true),
            Some(1),
            "el hueco 0 esta en las dos vias: es una urgencia, no una cesion"
        );
        assert_eq!(
            TaskCollection::bitmap_candidate(&mut inner, 0, false),
            Some(0)
        );
    }

    /// Un bit puesto sobre un hueco que ya no esta en el slab se limpia por el
    /// camino y el barrido sigue. Sin eso el bit se queda encendido para
    /// siempre y cada vuelta del reparto vuelve a tropezar con el.
    #[test]
    fn a_bit_with_no_slab_slot_behind_it_is_cleared_and_the_scan_goes_on() {
        let mut inner = FutureCollection::new(DEFAULT_PRIORITY, 0);
        let first = inner.insert(pending(), None);
        inner.insert(pending(), None);
        let page = inner.pages[0].clone();

        // El hueco se va del slab sin que nadie apague su bit.
        let (_, _, subpage) = unpack_key(first);
        inner.slab.remove(subpage);
        assert_eq!(page.peek_lanes().0 & 1, 1, "el bit sigue encendido");

        assert_eq!(
            TaskCollection::bitmap_candidate(&mut inner, 0, false),
            Some(1)
        );
        assert_eq!(
            page.peek_lanes().0 & 1,
            0,
            "el bit huerfano se queda encendido y el barrido tropieza con el en \
             cada vuelta"
        );
    }

    /// Los tres informes de corrupcion. Son `#[cold]` y el valor que tienen es
    /// NO ser un panico dentro del ejecutor: una CPU con los cerrojos del
    /// planificador tomados que panica se lleva la maquina y entierra la
    /// corrupcion que la causo. Asi que lo que se fija es que se puedan llamar
    /// y que el limitador de una sola vez no atrape.
    #[test]
    fn the_corruption_reports_do_not_panic_inside_the_executor() {
        report_missing_generator(0);
        report_missing_generator(3);
        report_overwritten_collection(0, 0);
        report_overwritten_collection(7, MAX_PRIORITY + 1);
    }

    /// El unico de los tres que SI panica, porque su llamante no puede llevar
    /// un `None`. El mensaje tiene que decir cual de las dos cosas es: un
    /// `Vec` pisado o una clave que nombra una prioridad que no existe. Decir
    /// la equivocada manda a leer el planificador cuando lo que hay es memoria
    /// corrupta.
    #[test]
    fn an_overwritten_vec_and_an_impossible_priority_say_different_things() {
        let smashed = std::panic::catch_unwind(|| overwritten_collection(2, 0, 4)).unwrap_err();
        let smashed = smashed
            .downcast_ref::<alloc::string::String>()
            .expect("el panico lleva su mensaje");
        assert!(smashed.contains("overwritten"), "{}", smashed);
        assert!(smashed.contains("len 0"), "{}", smashed);
        assert!(
            smashed.contains("not to be trusted") || smashed.contains("same wrecked struct"),
            "el cpu_id sale de la misma estructura destrozada y hay que decirlo: {}",
            smashed
        );

        let out_of_range =
            std::panic::catch_unwind(|| overwritten_collection(2, MAX_PRIORITY, MAX_PRIORITY))
                .unwrap_err();
        let out_of_range = out_of_range
            .downcast_ref::<alloc::string::String>()
            .expect("el panico lleva su mensaje");
        assert!(
            !out_of_range.contains("overwritten"),
            "con la longitud correcta el culpable es la clave, no el `Vec`: {}",
            out_of_range
        );
        assert!(out_of_range.contains("outside"), "{}", out_of_range);
    }

    #[test]
    fn the_last_external_owner_releases_the_collection() {
        let tc = TaskCollection::new(0);
        let weak = Arc::downgrade(&tc);
        drop(tc);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn bitmap_cursor_rotates_across_sparse_pages_and_wraps_within_a_page() {
        let _g = crate::runtime::resched_test_lock();
        let tc = TaskCollection::new(0);
        let keys: Vec<_> = (0..130).map(|_| tc.add_task(pending(), None)).collect();
        for (slot, &key) in keys.iter().enumerate() {
            if ![2, 64, 129].contains(&slot) {
                tc.remove_task(key);
            }
        }
        for slot in [2, 64, 129, 2, 64, 129, 2] {
            let (key, _, waker) = tc.take_task().unwrap();
            assert_eq!(key, keys[slot]);
            cede(&waker);
        }
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

    /// A task pinned to a CPU that MAY run it is not stranded work.
    ///
    /// The other three tests on the stranded mark all pin a task away from
    /// the asking CPU, so each one checks that the mark goes UP. Nothing
    /// checked that it stays down: `stranded` and the kick are decided by
    /// one condition, and a condition that answers "yes" for every pinned
    /// task leaves a CPU with perfectly runnable work asking the rest of the
    /// machine to come and rescue it, with an IPI per pass.
    #[test]
    fn a_pinned_task_this_cpu_may_run_is_not_stranded_work() {
        let _g = crate::runtime::resched_test_lock();
        crate::runtime::clear_stranded_for_test();
        let previous = crate::arch::set_cpu_id_for_test(LONE_CPU);
        let tc = TaskCollection::new(LONE_CPU);
        let key = tc.add_task(pending(), Some(Arc::new(AtomicU64::new(LONE_BIT))));
        let (got, _, waker) = tc
            .take_task()
            .expect("a task pinned to this very CPU was refused");
        assert_eq!(got, key);
        assert_eq!(
            crate::runtime::stranded_mask_for_test() & LONE_BIT,
            0,
            "the queue asked to be rescued from work it can run itself"
        );
        waker.mark_borrowed(false);
        crate::arch::set_cpu_id_for_test(previous);
    }

    /// A sleeping task does not hide a runnable one from the pinned branch.
    ///
    /// The per-bit walk builds its own `runnable` set, and what it proposes
    /// is then claimed with `reclaim_*`, which refuses a slot that is not
    /// really published. So a walk that proposes too much does not hand out
    /// a sleeping task -- it hands out NOTHING: the claim fails, `schedule`
    /// returns `None`, and a queue with runnable work reports itself empty
    /// to the executor about to halt. One pinned task anywhere in the queue
    /// is all it takes to be in this branch.
    #[test]
    fn a_sleeping_task_does_not_hide_a_runnable_one_in_the_pinned_branch() {
        let _g = crate::runtime::resched_test_lock();
        let tc = TaskCollection::new(0);
        // Allowed everywhere: here only to put the queue in the pinned branch.
        tc.add_task(pending(), Some(Arc::new(AtomicU64::new(u64::MAX))));
        let _sleeper = tc.add_task(pending(), None);
        let awake = tc.add_task(pending(), None);
        drain(&tc);
        let page = tc.get_mut_inner(DEFAULT_PRIORITY).pages[0].clone();
        page.notify(unpack_key(awake).2);
        let (key, _, waker) = tc
            .take_task()
            .expect("the queue reported itself empty with a woken task in it");
        assert_eq!(key, awake, "a task nobody woke was handed out");
        waker.mark_borrowed(false);
    }

    /// A pinned task that cedes the CPU gets it back.
    ///
    /// The pinned walk decides which lane a slot is in by reading the two
    /// lane words, and then claims the slot from THAT lane. Reading the
    /// wrong one is not a mis-ordering: the claim comes back empty, every
    /// later pass picks the same slot and fails the same way, and the task
    /// is never polled again. The existing pinned-lane tests all pin the
    /// task away from the asking CPU, where it is refused on purpose.
    #[test]
    fn a_pinned_task_that_cedes_the_cpu_is_handed_out_again() {
        let _g = crate::runtime::resched_test_lock();
        let tc = TaskCollection::new(0);
        let key = tc.add_task(pending(), Some(Arc::new(AtomicU64::new(u64::MAX))));
        let (first, _, waker) = tc.take_task().expect("a task this CPU may run was refused");
        assert_eq!(first, key);
        cede(&waker);
        assert_eq!(
            tc.take_task()
                .expect("the volunteer never came back: its yield was claimed from the wrong lane")
                .0,
            key
        );
    }

    /// The voluntary quota, asked of the branch that runs once anything in
    /// the queue is pinned.
    ///
    /// `schedule` has two shapes -- a bitmap fast path and a per-bit walk
    /// for queues holding a pinned task -- and each carries its own copy of
    /// the quota comparison. The test below exercises the fast path only, so
    /// the second copy was free to be off by one; and `urgent_turns`
    /// saturates AT the quota, so a copy that asks for strictly more than it
    /// is never satisfied: the volunteer waits behind the urgent lane for
    /// ever, on any CPU whose queue happens to hold one pinned task.
    #[test]
    fn the_voluntary_quota_holds_in_the_pinned_branch_too() {
        let _g = crate::runtime::resched_test_lock();
        crate::runtime::clear_stranded_for_test();
        let tc = TaskCollection::new(0);
        // Allowed here, so it neither strands the queue nor kicks anyone; it
        // is here to make `inner.affine` non-zero, which is the whole of what
        // chooses the branch under test. It sleeps through the rest.
        tc.add_task(pending(), Some(Arc::new(AtomicU64::new(u64::MAX))));
        let hog = tc.add_task(pending(), None);
        let yielder = tc.add_task(pending(), None);
        drain(&tc);
        let page = tc.get_mut_inner(DEFAULT_PRIORITY).pages[0].clone();
        page.notify(unpack_key(hog).2);
        page.mark_yielded(unpack_key(yielder).2);
        let mut served_on = None;
        for turn in 1..=4 * (URGENT_QUOTA + 1) {
            let (key, _, waker) = tc
                .take_task()
                .expect("the queue stopped handing work out entirely");
            if key == yielder {
                served_on = Some(turn);
                cede(&waker);
                break;
            }
            assert_eq!(key, hog, "a sleeping task was handed out");
            waker.wake_by_ref();
            waker.mark_borrowed(false);
        }
        // Not an exact turn: the cursor's quota counter is shared across
        // calls, so the drain above leaves it part-way through. What has to
        // hold is that the volunteer IS served, and within the quota.
        let served_on = served_on
            .expect("the volunteer was never served: the urgent lane kept the CPU for ever");
        assert!(
            served_on <= URGENT_QUOTA + 1,
            "the volunteer waited {} turns behind a quota of {}",
            served_on,
            URGENT_QUOTA
        );
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

    /// **Una marca de muerte sobre un hueco que nadie ocupa no descuenta.**
    ///
    /// El drenaje de muertes de `schedule` descuenta `task_num` por cada bit
    /// que la pagina le da, y `FutureCollection::remove` es lo unico que sabe
    /// si detras de ese bit habia de verdad una tarea. Si no se le preguntara,
    /// un bit rancio -- el que deja un `WakerRef` viejo de un hueco ya
    /// reutilizado y retirado -- bajaria la cuenta de tareas vivas por una que
    /// no existe, y con eso el contador se va por debajo de la realidad y
    /// `has_ready` empieza a mentir.
    #[test]
    fn a_death_mark_on_a_slot_nobody_occupies_does_not_lower_the_count() {
        let _g = crate::runtime::resched_test_lock();
        let tc = TaskCollection::new(0);
        let key = tc.add_task(pending(), None);
        tc.get_mut_inner(DEFAULT_PRIORITY).pages[0].mark_dropped(7);
        assert_eq!(tc.task_num(), 1);
        // El reparto drena las muertes antes de elegir: la del hueco 7 no
        // tiene a quien quitar, la tarea de verdad sigue repartiendose.
        assert_eq!(tc.take_task().map(|(k, _, _)| k), Some(key));
        assert_eq!(tc.task_num(), 1, "se descontó una tarea que no existía");
    }

    /// **Un bit listo y fijado de un hueco vacio se limpia, no se reparte.**
    ///
    /// La rama de afinidad de `schedule` recorre bit a bit los huecos fijados
    /// y busca cada uno en el slab para leer su mascara. Un bit que sobrevive
    /// a su ocupante no tiene mascara que leer, y dejarlo publicado haria que
    /// cada pasada lo mirara otra vez para nada; se limpia ahi mismo.
    #[test]
    fn a_pinned_ready_bit_for_an_empty_slot_is_cleared_and_not_handed_out() {
        let _g = crate::runtime::resched_test_lock();
        let tc = TaskCollection::new(0);
        // Una tarea fijada AQUI: hace `inner.affine != 0`, que es lo que saca
        // a `schedule` del camino de mapas de bits, y se puede repartir.
        let key = tc.add_task(pending(), Some(Arc::new(AtomicU64::new(1))));
        {
            let inner = tc.get_mut_inner(DEFAULT_PRIORITY);
            inner.pages[0].initialize(9);
            inner.pages[0].mark_affine(9);
        }
        assert_eq!(tc.take_task().map(|(k, _, _)| k), Some(key));
        let (notified, _, _, _) = tc.get_mut_inner(DEFAULT_PRIORITY).pages[0].peek_lanes();
        assert_eq!(notified & (1 << 9), 0, "el bit rancio se quedo publicado");
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
                    crate::runtime::stranded_mask_for_test() & LONE_BIT != 0,
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
        let tc = TaskCollection::new(LONE_CPU);
        tc.add_task(pending(), Some(Arc::new(AtomicU64::new(1 << 2))));
        tc.add_task(pending(), None);
        let (_, _, waker) = tc.try_take_task().unwrap();
        assert_eq!(
            crate::runtime::stranded_mask_for_test() & LONE_BIT,
            LONE_BIT
        );
        assert!(PUBLISHED_BEFORE_KICK.load(Ordering::SeqCst));
        // Y el grabador solo contesta por la CPU 2: una patada a cualquier
        // otra no puede pisar lo que acaba de leer, que es lo que hace que la
        // lectura de arriba sea sobre el destino y no sobre la ultima patada
        // que haya pasado por aqui.
        record(3);
        assert!(PUBLISHED_BEFORE_KICK.load(Ordering::SeqCst));
        waker.mark_borrowed(false);
        crate::runtime::clear_need_resched(2);
        crate::runtime::set_executor_ready_mask_for_test(ready);
        crate::arch::set_cpu_id_for_test(previous);
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn disjoint_affinity_masks_forward_once_per_scan_and_chain_to_the_other_cpu() {
        static KICKS: AtomicUsize = AtomicUsize::new(0);
        static DESTINATIONS: AtomicU64 = AtomicU64::new(0);
        fn record(cpu: usize) {
            KICKS.fetch_add(1, Ordering::SeqCst);
            DESTINATIONS.fetch_or(1u64 << cpu, Ordering::SeqCst);
        }
        let _g = crate::runtime::resched_test_lock();
        let previous = crate::arch::set_cpu_id_for_test(0);
        let ready = crate::runtime::set_executor_ready_mask_for_test(0b111);
        for cpu in [1, 2] {
            crate::runtime::clear_need_resched(cpu);
            crate::runtime::set_cpu_sleeping(cpu, false);
        }
        crate::runtime::set_resched_ipi_sender(record);
        KICKS.store(0, Ordering::SeqCst);
        DESTINATIONS.store(0, Ordering::SeqCst);
        let tc = TaskCollection::new(0);
        for _ in 0..16 {
            tc.add_task(pending(), Some(Arc::new(AtomicU64::new(1 << 1))));
            tc.add_task(pending(), Some(Arc::new(AtomicU64::new(1 << 2))));
        }
        assert!(tc.take_task().is_none());
        assert_eq!(KICKS.load(Ordering::SeqCst), 1);
        assert_eq!(DESTINATIONS.load(Ordering::SeqCst), 1 << 1);
        crate::arch::set_cpu_id_for_test(1);
        let (_, task, waker) = tc.try_take_task().unwrap();
        assert!(task.allowed_on(1));
        assert_eq!(KICKS.load(Ordering::SeqCst), 2);
        assert_eq!(DESTINATIONS.load(Ordering::SeqCst), 0b110);
        waker.mark_borrowed(false);
        for cpu in [1, 2] {
            crate::runtime::clear_need_resched(cpu);
        }
        crate::runtime::set_resched_ipi_sender(|_| {});
        crate::runtime::set_executor_ready_mask_for_test(ready);
        crate::arch::set_cpu_id_for_test(previous);
    }

    /// A mask that allows CPU 1 and not CPU 0, i.e. not us.
    fn pinned_elsewhere() -> Option<Arc<AtomicU64>> {
        Some(Arc::new(AtomicU64::new(1 << 1)))
    }

    /// A CPU id no other test in this module builds a collection for, for the
    /// four tests that assert on the stranded mark.
    ///
    /// That mark is a per-CPU global keyed by the *collection's* owner CPU,
    /// and `schedule` clears it through `note_not_stranded` on every pass
    /// that refuses nothing. Two dozen tests here hand tasks out of a
    /// collection owned by CPU 0 without holding `resched_test_lock`, so a
    /// test asserting on CPU 0's mark loses it to whichever of those the
    /// harness happens to run beside it: `cargo test` failed about one run in
    /// twelve, naming a different victim each time, while CI never saw it
    /// because `--test-threads=1` serialises the suite. Taking the lock in
    /// the asserting test is not enough -- the other two dozen would all have
    /// to take it too. Owning a CPU nobody else names takes the shared word
    /// out of the question instead.
    const LONE_CPU: u8 = 5;
    /// [`LONE_CPU`]'s bit in the stranded mask.
    const LONE_BIT: u64 = 1 << LONE_CPU;

    /// Drain the queue, returning the keys in the order they were handed out.
    /// Releases each borrow, as a Pending poll would.
    fn drain(tc: &TaskCollection) -> Vec<Key> {
        let mut keys = Vec::new();
        while let Some((key, _task, waker)) = tc.take_task() {
            keys.push(key);
            waker.mark_borrowed(false);
            // Sin el numero de reparto en el mensaje a proposito: se
            // comprueba en cada vuelta, asi que al fallar vale siempre
            // `DRAIN_CAP + 1` y un argumento calculado en un `assert!` es
            // codigo que solo se ejecuta el dia que el test falla.
            assert!(
                keys.len() <= DRAIN_CAP,
                "the queue never empties: take_task kept handing work out"
            );
        }
        keys
    }

    /// How many hand-outs a drain is allowed before it is called a bug.
    ///
    /// Every drain in these tests empties a queue of at most a few dozen
    /// tasks, so this is nowhere near a limit on anything legitimate. It is
    /// here because a collection that hands a task out and leaves its wake
    /// published turns every "drain until empty" into a loop that never
    /// ends, and **a test that spins does not fail**: it stops the whole run
    /// with no name attached, which is the one verdict neither CI nor a
    /// mutation pass can read. Asserting instead turns that into the test
    /// that was going to fail anyway.
    const DRAIN_CAP: usize = 4096;

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

    /// Hand out up to `vueltas` tasks, calling `cada` for each one with the
    /// turn number, and **stop the moment the queue has nothing left**.
    ///
    /// The five fairness tests below each asked for a fixed number of turns
    /// out of a queue that re-publishes its tasks, so each one carried its own
    /// copy of this loop -- and its own "the queue ran dry" exit that nothing
    /// ever took, because none of those queues can run dry. One copy, taken
    /// by the test right below it, says what that exit does: an empty queue is
    /// the end of the round and not a failure.
    fn vueltas(
        tc: &Arc<TaskCollection>,
        vueltas: usize,
        mut cada: impl FnMut(usize, Key, Arc<WakerRef>),
    ) {
        for vuelta in 0..vueltas {
            let Some((key, _t, waker)) = tc.take_task() else {
                return;
            };
            cada(vuelta, key, waker);
        }
    }

    #[test]
    fn una_ronda_sobre_una_cola_vacia_termina_en_la_primera_vuelta() {
        let _g = crate::runtime::resched_test_lock();
        let tc = TaskCollection::new(0);
        let mut repartidas = 0;
        vueltas(&tc, 10, |_, _, _| repartidas += 1);
        assert_eq!(repartidas, 0, "una cola vacia no reparte nada");
        // Y con una tarea reparte exactamente las vueltas que se piden, que
        // es el otro lado: la salida de arriba no es «no repartir nunca».
        tc.add_task(pending(), None);
        vueltas(&tc, 3, |_, _, waker| {
            repartidas += 1;
            cede(&waker);
        });
        assert_eq!(repartidas, 3);
    }

    /// **La rama de afinidad reparte en el MISMO orden que la rapida.**
    ///
    /// Es la propiedad que se podia romper al dejar de recorrer bit a bit: el
    /// orden de reparto lo fija el cursor (`consider_slot` prefiere el hueco
    /// mas bajo que este en el cursor o despues, y si no hay, el mas bajo de
    /// todos), y `consider_bits` lo calcula con mascaras en vez de visitando
    /// cada bit. Si las dos no coinciden, una tarea fijada en cualquier parte
    /// de la cola cambiaria el reparto de todas las demas, que es justo lo que
    /// no puede pasar.
    ///
    /// Asi que se comparan: la misma cola sin nada fijado (camino rapido) y
    /// con UNA tarea fijada a otra CPU (rama de afinidad), y la secuencia de
    /// huecos repartidos tiene que ser la misma salvo la fijada, que no puede
    /// correr aqui.
    #[test]
    fn la_rama_de_afinidad_reparte_en_el_mismo_orden_que_la_rapida() {
        let _g = crate::runtime::resched_test_lock();

        fn secuencia(tc: &Arc<TaskCollection>, n: usize) -> Vec<usize> {
            let mut salida = Vec::new();
            vueltas(tc, n, |_, key, waker| {
                let (_, page_idx, subpage_idx) = unpack_key(key);
                salida.push(page_idx * WAKER_PAGE_SIZE + subpage_idx);
                cede(&waker);
            });
            salida
        }

        // Sin afinidad ninguna: el camino de `bitmap_candidate`, siete tareas
        // en los huecos 0..6.
        let libre = TaskCollection::new(0);
        for _ in 0..7 {
            libre.add_task(pending(), None);
        }
        let referencia = secuencia(&libre, 21);
        assert_eq!(referencia.len(), 21, "la cola libre reparto tres vueltas");

        // Las mismas siete, mas una octava fijada a la CPU 1 en el hueco 7.
        // Eso basta para que `inner.affine != 0` y el reparto de las SIETE
        // pase por la rama de afinidad.
        let fijada = TaskCollection::new(0);
        for _ in 0..7 {
            fijada.add_task(pending(), None);
        }
        fijada.add_task(pending(), Some(Arc::new(AtomicU64::new(0b10))));
        let obtenido = secuencia(&fijada, 21);

        assert_eq!(
            obtenido, referencia,
            "la rama de afinidad reparte en otro orden que la rapida"
        );
        assert!(
            !obtenido.contains(&7),
            "ha repartido la tarea fijada a otra CPU"
        );
    }

    /// Los bordes de `consider_bits`, que es donde una mascara mal construida
    /// no se nota: el cursor dentro de la pagina, justo en su primer hueco,
    /// una pagina entera por delante del cursor y otra entera por detras.
    ///
    /// El orden que tiene que salir es el de `consider_slot`: gana el hueco
    /// mas bajo que este en el cursor o despues; si ninguno lo esta, el mas
    /// bajo de todos (el cursor da la vuelta).
    #[test]
    fn consider_bits_respeta_el_orden_del_cursor_en_los_bordes() {
        let caso = |bits: u64, page_idx: usize, next: usize| -> Option<usize> {
            let mut best = None;
            TaskCollection::consider_bits(&mut best, bits, page_idx, next);
            best
        };
        let bits = (1 << 3) | (1 << 10) | (1 << 40);

        // Cursor por detras de la pagina entera: gana el mas bajo.
        assert_eq!(caso(bits, 1, 0), Some(64 + 3));
        // Cursor dentro de la pagina: gana el primero a partir de ahi.
        assert_eq!(caso(bits, 1, 64 + 4), Some(64 + 10));
        assert_eq!(
            caso(bits, 1, 64 + 10),
            Some(64 + 10),
            "el cursor es inclusivo"
        );
        assert_eq!(caso(bits, 1, 64 + 11), Some(64 + 40));
        // Cursor pasada la pagina entera: ninguno esta en el cursor o despues,
        // asi que da la vuelta al mas bajo.
        assert_eq!(caso(bits, 1, 128), Some(64 + 3));
        assert_eq!(caso(bits, 0, 64), Some(3));
        // Sin bits no hay candidato, y no se pisa el que ya hubiera.
        let mut best = Some(7);
        TaskCollection::consider_bits(&mut best, 0, 3, 0);
        assert_eq!(best, Some(7), "una pagina vacia no propone nada");
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
        vueltas(&tc, 40, |_, key, waker| {
            // `else` y no `else if key == b`: solo hay dos tareas, asi que una
            // tercera rama no la puede tomar nada y la igualdad es mas
            // estricta escrita como aserto.
            if key == a {
                n_a += 1;
            } else {
                assert_eq!(key, b);
                n_b += 1;
            }
            cede(&waker);
        });
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
        vueltas(&tc, 40, |round, key, waker| {
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
                return;
            }
            cede(&waker);
        });
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
        vueltas(&tc, 10, |_, key, waker| {
            if key == a {
                back += 1;
            }
            cede(&waker);
        });
        assert_eq!(back, 5, "soltar el prestamo la devuelve a la rotacion");
    }

    /// The `--yieldstall` shape of `eclipse-bench` #1690, as the queue sees it:
    /// two threads in a tight `sched_yield(2)` loop and a third waking every
    /// 200 us, all on one CPU. On Linux that sustains ~1M yields/s.
    ///
    /// The two yielders get service both between the sleeper's wakes and under
    /// continuously urgent work. The cursor's urgent quota bounds voluntary
    /// service without relying on the urgent lane ever becoming empty.
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
        let mut sleeping = None;
        // The sleeper is awake for one hand-out, then asleep: it publishes no
        // wake until the next 200 us tick. Everything else on the CPU is a
        // yielder re-ceding immediately.
        vueltas(&tc, 30, |round, key, waker| {
            handed.push(key);
            if key == sleeper {
                // Asleep now; nothing re-publishes it until its timer.
                waker.mark_borrowed(false);
                sleeping = Some(waker);
            } else {
                cede(&waker);
            }
            // The tick lands every tenth round whoever happens to be running,
            // which is what a timer does. Hanging it off the round the sleeper
            // was handed out instead meant it never landed at all -- the
            // sleeper is served on round 2 and never again, so `round % 10 ==
            // 9` was never true while holding it, and the "wakes every 200 us"
            // half of this test was not being exercised.
            if round % 10 == 9 {
                notified_back(sleeping.as_ref().expect("el durmiente ya se repartio"));
            }
        });

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
        // `has_ready` is the one of these that cannot answer "nothing": it is
        // the last thing the executor asks before `wait_for_interrupt`, and
        // there is no timer backstop in that idle path. A wake may be sitting
        // in a page we can no longer name, so the CPU stays awake and the
        // corruption reaches the log through the takers instead of wedging a
        // core on a queue nobody can read.
        assert!(
            inner.has_ready(),
            "a collection we cannot read is not grounds to halt"
        );
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
        let tc = TaskCollection::new(LONE_CPU);
        tc.add_task(pending(), pinned_elsewhere());

        // The pass refuses it (the owner is not in the mask) and hands out
        // nothing.
        assert!(tc.take_task().is_none());
        assert_eq!(
            crate::runtime::stranded_mask_for_test() & LONE_BIT,
            LONE_BIT,
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
        let tc = TaskCollection::new(LONE_CPU);
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
            crate::runtime::stranded_mask_for_test() & LONE_BIT,
            LONE_BIT,
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
        let tc = TaskCollection::new(LONE_CPU);
        let pinned = tc.add_task(pending(), pinned_elsewhere());
        assert!(tc.take_task().is_none());
        assert_eq!(
            crate::runtime::stranded_mask_for_test() & LONE_BIT,
            LONE_BIT
        );

        // The task goes away (it migrated, or it exited). The next clean pass
        // has nothing to refuse.
        tc.remove_task(pinned);
        let _ = tc.take_task();
        assert_eq!(
            crate::runtime::stranded_mask_for_test() & LONE_BIT,
            0,
            "the mark outlived the task that earned it"
        );
    }

    /// `has_ready` is read with interrupts off in the instant before
    /// `wait_for_interrupt`, and a wake that lands on a slot some executor is
    /// already polling is not work for this CPU: both takers *defer* a
    /// borrowed slot (they re-publish the bit and report nothing). Answering
    /// "yes" here makes the CPU skip the halt, drain nothing and come straight
    /// back round -- a spin for as long as the other CPU holds the borrow,
    /// which under work stealing is a whole poll. The shortcut this collection
    /// takes when no task carries a mask reads the page lanes itself, so it
    /// has to mask `borrowed` out exactly as the per-slot path below it does.
    #[test]
    fn a_wake_on_a_task_being_polled_is_not_work_to_skip_the_halt_for() {
        let _g = crate::runtime::resched_test_lock();
        let tc = TaskCollection::new(0);
        tc.add_task(pending(), None);
        let (_key, _task, waker) = tc.take_task().expect("a fresh task is runnable");
        assert_eq!(
            tc.get_mut_inner(DEFAULT_PRIORITY).affine,
            0,
            "no task here carries a mask, so this is the shortcut's own path"
        );
        assert!(!tc.has_ready(), "the only task is checked out");
        // An external wake arriving mid-poll: published on the lane, and
        // deferred by whoever drains it next.
        waker.wake_by_ref();
        assert!(
            !tc.has_ready(),
            "a deferred wake is not work this CPU can act on yet"
        );
        // Handing the slot back is what makes it work again.
        waker.mark_borrowed(false);
        assert!(tc.has_ready(), "the released slot carries the wake");
    }

    /// A runnable bit the collection cannot resolve to a task must not let the
    /// CPU halt either. The slot is gone from the slab with its lane bit still
    /// up -- the state `bitmap_candidate` clears as it walks past it -- so for
    /// one pass `has_ready` has a published wake and no task to ask
    /// `allowed_on`. Conservative is the only safe answer here for the same
    /// reason as the smashed header above: nothing else will look again.
    #[test]
    fn a_runnable_bit_with_no_task_behind_it_keeps_the_cpu_awake() {
        let tc = TaskCollection::new(0);
        // A mask takes `has_ready` off its no-mask shortcut and onto the
        // per-slot path, which is the one that fetches the task.
        tc.add_task(pending(), pinned_elsewhere());
        assert!(
            !tc.has_ready(),
            "while the task is there it is pinned elsewhere and not ours"
        );
        {
            let mut inner = tc.get_mut_inner(DEFAULT_PRIORITY);
            assert_ne!(
                inner.affine, 0,
                "the mask is what keeps us off the shortcut"
            );
            // Out of the slab without anyone putting its bit out.
            inner.slab.remove(0);
            let (notified, _, dropped, borrowed) = inner.pages[0].peek_lanes();
            assert_eq!(
                (
                    notified & 1,
                    dropped & 1,
                    borrowed & 1,
                    inner.pages[0].affine_bits() & 1
                ),
                (1, 0, 0, 1),
                "the orphan bit is still published and still marked affine"
            );
        }
        assert!(
            tc.has_ready(),
            "an unresolvable wake is not grounds to halt"
        );
    }

    /// A claim that comes back empty hands out nothing.
    ///
    /// The lanes are read and the slot chosen under this collection's lock,
    /// but `notify`, `mark_dropped` and a peer's own claim all write them
    /// without it, so between choosing a slot and claiming its bit the bit can
    /// be gone. `reclaim_notified`/`reclaim_yielded` say so by reporting 0,
    /// and "nothing ready" is the only safe answer: handing the task over
    /// anyway polls a future whose wake another CPU has already consumed, and
    /// marks a slot `borrowed` on behalf of an executor that was never given
    /// it -- a slot no release will ever come back for, which every later
    /// reader then masks out as work in flight.
    ///
    /// See `waker_page::steal_next_reclaim` for why the race is injected at
    /// the page and not here.
    #[test]
    fn a_slot_whose_bit_a_peer_took_first_is_not_handed_out() {
        let tc = TaskCollection::new(0);
        tc.add_task(pending(), None);
        let waker = tc
            .get_mut_inner(DEFAULT_PRIORITY)
            .slab
            .get(0)
            .expect("the task is in the slab")
            .waker()
            .clone();

        crate::waker_page::steal_next_reclaim();
        assert!(
            tc.take_task().is_none(),
            "the bit was gone, so there was nothing to hand out"
        );
        {
            let inner = tc.get_mut_inner(DEFAULT_PRIORITY);
            let (notified, yielded, _, borrowed) = inner.pages[0].peek_lanes();
            assert_eq!(
                borrowed & 1,
                0,
                "a slot nobody was handed must not be left checked out"
            );
            assert_eq!(
                (notified & 1, yielded & 1),
                (0, 0),
                "the winning peer's claim consumed the bit"
            );
        }
        // And the task itself is untouched: the next wake serves it normally.
        let _g = crate::runtime::resched_test_lock();
        waker.wake_by_ref();
        assert!(tc.take_task().is_some(), "the task survived the lost race");
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
        // `assert_eq!` ya imprime los dos lados, asi que el numero en el
        // mensaje era un argumento calculado que solo se evalua al fallar.
        assert_eq!(
            req1 - req0,
            1,
            "eight pinned tasks raised the wrong number of kicks"
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
        drain(&tc);
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
        let mut handouts = 0usize;
        while let Some((key, _t, w)) = tc.take_task() {
            if key == dying {
                w.drop_by_ref();
            } else {
                wakers.push((key, w));
            }
            handouts += 1;
            assert!(
                handouts <= DRAIN_CAP,
                "the queue never empties (see DRAIN_CAP)"
            );
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
        // The old generator emptied a lane into a suspended coroutine snapshot.
        // The cursor now leaves every unselected bit published. Every figure
        // a peer reads — which tasks can be
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
        // spins on its scheduling cursor; the victim holds that cursor
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

    // ── the per-slot affinity lane ─────────────────────────────────────────

    /// What `has_ready` and `ready_num_for` would answer if they still asked
    /// the slab about every runnable bit: the code that stood in both before
    /// `WakerPage::affine_bits` existed, spelled out here so the two can be
    /// compared on the same queue.
    fn per_bit_walk(tc: &TaskCollection, cpu: usize) -> (bool, usize) {
        let mut inner = tc.get_mut_inner(DEFAULT_PRIORITY);
        let mut ready = false;
        let mut n = 0usize;
        for page_idx in 0..inner.pages.len() {
            let (notified, dropped, borrowed) = inner.pages[page_idx].peek();
            let runnable = notified & !dropped & !borrowed;
            for subpage_idx in BitIter::from(runnable) {
                let key = pack_key(DEFAULT_PRIORITY, page_idx, subpage_idx);
                let allowed = inner
                    .slab
                    .get(unmask_priority(key))
                    .map(|task| task.allowed_on(cpu))
                    .unwrap_or(true);
                if allowed {
                    ready = true;
                    n += 1;
                }
            }
        }
        (ready, n)
    }

    /// The mask pattern for task `i` under `shape`. `None` is a task that may
    /// run anywhere, which is the case the lane exists to answer cheaply.
    fn mask_for(shape: usize, i: usize) -> Option<u64> {
        match shape {
            // Nothing pinned: the queue-wide `affine == 0` shortcut, which
            // the lane must not disturb.
            0 => None,
            // One pinned task among many, which is the case that used to
            // switch the whole queue to its per-bit form.
            1 => (i == 7).then_some(0b10),
            // Everything pinned elsewhere: the 3.3 us worst case.
            2 => Some(0b10),
            // Everything pinned here.
            3 => Some(0b01),
            // Alternating, and alternating which CPU.
            4 => i
                .is_multiple_of(2)
                .then_some(if i.is_multiple_of(4) { 0b01 } else { 0b10 }),
            // Pinned to a CPU past the end of the mask's reach, which
            // `allowed_on` answers `true` for when the asker is >= 64.
            5 => Some(1u64 << 63),
            // A mask naming nobody, which no CPU satisfies.
            6 => Some(0),
            _ => i.is_multiple_of(3).then_some(0b101),
        }
    }

    #[test]
    fn the_affine_lane_answers_exactly_what_a_per_bit_walk_would() {
        let _g = crate::runtime::resched_test_lock();
        let mut compared = 0usize;
        let mut saw_fast_bits = false;
        let mut saw_walked_bits = false;
        let mut saw_ready = false;
        let mut saw_not_ready = false;
        let mut saw_partial_count = false;

        // Three page counts (one page exactly, one bit over, three pages),
        // eight mask shapes, and four disturbances of the lanes underneath.
        for &n in &[1usize, 64, 65, 130] {
            for shape in 0..8 {
                for disturb in 0..4 {
                    let tc = TaskCollection::new(0);
                    let keys: Vec<_> = (0..n)
                        .map(|i| {
                            tc.add_task(
                                pending(),
                                mask_for(shape, i).map(|m| Arc::new(AtomicU64::new(m))),
                            )
                        })
                        .collect();

                    // Put the lanes in a state the scans have to cope with:
                    // tasks checked out to an executor, tasks retired, and
                    // slots whose wake was never published.
                    match disturb {
                        0 => {}
                        1 => {
                            // Borrow a few, which masks them out of runnable.
                            for _ in 0..3.min(n) {
                                if let Some((_k, _t, w)) = tc.take_task() {
                                    core::mem::forget(w);
                                }
                            }
                        }
                        2 => {
                            // Retire a few, which masks them out too.
                            for key in keys.iter().take(5.min(n)) {
                                tc.remove_task(*key);
                            }
                        }
                        _ => {
                            // Drain everything, so no wake is published at all
                            // and every page is runnable-empty.
                            drain(&tc);
                        }
                    }

                    for cpu in [0usize, 1, 2, 64] {
                        let (want_ready, want_n) = per_bit_walk(&tc, cpu);
                        // `has_ready` asks about the CPU it is running on,
                        // which on the host is 0, so it is only comparable
                        // against the walk for CPU 0.
                        if cpu == 0 {
                            assert_eq!(
                                tc.has_ready(),
                                want_ready,
                                "has_ready disagreed: n={n} shape={shape} disturb={disturb}",
                            );
                            saw_ready |= want_ready;
                            saw_not_ready |= !want_ready;
                        }
                        assert_eq!(
                            tc.ready_num_for(cpu),
                            Some(want_n),
                            "ready_num_for({cpu}) disagreed: n={n} shape={shape} disturb={disturb}",
                        );
                        compared += 1;
                    }

                    // Was this configuration actually exercising the split?
                    let inner = tc.get_mut_inner(DEFAULT_PRIORITY);
                    for page in &inner.pages {
                        let (notified, dropped, borrowed) = page.peek();
                        let runnable = notified & !dropped & !borrowed;
                        let affine = page.affine_bits();
                        saw_fast_bits |= runnable & !affine != 0;
                        saw_walked_bits |= runnable & affine != 0;
                        if runnable & affine != 0 && runnable & !affine != 0 {
                            saw_partial_count = true;
                        }
                    }
                }
            }
        }

        // Non-vacuity: the comparison above is only worth anything if the
        // configurations reached both sides of the new branch, both answers
        // of `has_ready`, and at least one page holding some of each.
        assert_eq!(compared, 4 * 8 * 4 * 4);
        assert!(
            saw_fast_bits,
            "no configuration had an unmasked runnable slot"
        );
        assert!(
            saw_walked_bits,
            "no configuration had a masked runnable slot"
        );
        assert!(saw_partial_count, "no page ever held both kinds at once");
        assert!(
            saw_ready && saw_not_ready,
            "has_ready never answered both ways"
        );
    }

    #[test]
    fn a_reused_slot_does_not_inherit_the_last_occupants_affinity() {
        let _g = crate::runtime::resched_test_lock();
        let tc = TaskCollection::new(0);
        let pinned = tc.add_task(pending(), Some(Arc::new(AtomicU64::new(0b10))));
        let (page_idx, subpage_idx) = {
            let (_, p, sp) = unpack_key(pinned);
            (p, sp)
        };
        let bit = 1u64 << subpage_idx;
        let page = tc.get_mut_inner(DEFAULT_PRIORITY).pages[page_idx].clone();
        assert_eq!(page.affine_bits() & bit, bit, "the mask went unrecorded");

        // Retire it and spawn an unpinned task, which the slab hands the same
        // slot back for. A stale bit here would only cost a `PinSlab` lookup,
        // but the invariant the scans are written against is exactness.
        tc.remove_task(pinned);
        assert_eq!(page.affine_bits() & bit, 0, "retirement left the bit up");
        let reused = tc.add_task(pending(), None);
        assert_eq!(unmask_priority(reused), unmask_priority(pinned));
        assert_eq!(page.affine_bits() & bit, 0, "the new task inherited a mask");
        assert_eq!(
            tc.ready_num_for(1),
            Some(1),
            "the reused slot went uncounted"
        );
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

    /// Rows for the run queue: what a spawn costs, what a handout costs, and
    /// what the three load figures cost to read.
    ///
    /// The load figures are the ones worth measuring. `has_ready` runs on
    /// every trip into `hlt`, `ready_num`/`ready_num_for` run on every victim
    /// on every idle steal pass, and `placement_load` runs per spawn and per
    /// load-average sample — so each of them is read here at two queue depths,
    /// because all four walk `pages` and `pages` only ever grows: a
    /// `FutureCollection` that once held a thousand tasks keeps its sixteen
    /// pages after they have all retired (`FutureCollection::remove` frees the
    /// slab entry, never a page). The depth these rows bound is a high-water
    /// mark, not a current task count.
    ///
    /// Same four caveats as `waker_page::waker_page_tests::benches`:
    /// uncontended host atomics, no IPI, a coalesced `NEED_RESCHED` after the
    /// first iteration, and `debug_assert!`s that the kernel does not pay.
    #[cfg(test)]
    mod benches {
        use super::*;
        use test::{black_box, Bencher};

        /// A queue holding `n` runnable tasks, which is `n / 64` rounded up
        /// waker pages for the scans to walk. Every task is fresh, so every
        /// slot is published in the urgent lane.
        fn queue_of(n: usize) -> Arc<TaskCollection> {
            let tc = TaskCollection::new(0);
            for _ in 0..n {
                tc.add_task(pending(), None);
            }
            tc
        }

        /// The same, with every task pinned to CPU 1 — so `affine` is not
        /// zero, the bitmap shortcut is off, and the scans ask the slab about
        /// each runnable bit in turn.
        fn queue_of_pinned(n: usize) -> Arc<TaskCollection> {
            let tc = TaskCollection::new(0);
            for _ in 0..n {
                tc.add_task(pending(), Some(Arc::new(AtomicU64::new(0b10))));
            }
            tc
        }

        /// `n` tasks of which the first `pinned` are pinned to CPU 1 and the
        /// rest may run anywhere. This is the shape that actually happens --
        /// an IRQ pool, or one `sched_setaffinity` on a queue of ordinary
        /// threads -- and the one `WakerPage::affine_bits` was added for: the
        /// queue-wide `FutureCollection::affine` counter cannot tell it from
        /// `queue_of_pinned`, so before the lane both scans walked all `n`
        /// bits here too.
        fn queue_part_pinned(n: usize, pinned: usize) -> Arc<TaskCollection> {
            let tc = TaskCollection::new(0);
            for i in 0..n {
                let mask = (i < pinned).then(|| Arc::new(AtomicU64::new(0b10)));
                tc.add_task(pending(), mask);
            }
            tc
        }

        // No control row: see the note in
        // `crate::waker_page::waker_page_tests::benches`. The one that stood
        // here measured 0.64 ns and `read_the_raw_task_count` -- a single
        // relaxed load -- came out at 0.29, so the floor was above the row it
        // bounded. The rows below are read against each other and against the
        // two ends named in that note.

        // ── birth and death ────────────────────────────────────────────────

        /// One CPU's whole run queue: 32 priority queues built, each with its
        /// own lock and slab, of which exactly one is ever used. Paid once per
        /// CPU at boot.
        #[bench]
        fn build_a_run_queue(b: &mut Bencher) {
            b.iter(|| black_box(TaskCollection::new(0)));
        }

        /// A spawn and the retirement that undoes it. They are measured as a
        /// pair because a row that only spawned would grow the slab without
        /// bound across the harness's millions of iterations; the pair is also
        /// what a short-lived kernel thread actually costs.
        #[bench]
        fn spawn_a_task_and_retire_it(b: &mut Bencher) {
            let tc = TaskCollection::new(0);
            b.iter(|| {
                let key = tc.add_task(pending(), None);
                tc.remove_task(black_box(key));
            });
        }

        /// The same with an affinity mask, which is one more `Arc` and the
        /// `affine` counter that turns the bitmap shortcuts off for the whole
        /// queue.
        #[bench]
        fn spawn_a_pinned_task_and_retire_it(b: &mut Bencher) {
            let tc = TaskCollection::new(0);
            b.iter(|| {
                let key = tc.add_task(pending(), Some(Arc::new(AtomicU64::new(0b11))));
                tc.remove_task(black_box(key));
            });
        }

        // ── handing a task out ─────────────────────────────────────────────

        /// The scheduler's inner loop: take the next task under both locks and
        /// hand it back as a voluntary yield, which is what a CPU does once
        /// per timeslice per task.
        #[bench]
        fn hand_out_a_task_and_take_it_back(b: &mut Bencher) {
            let _g = crate::runtime::resched_test_lock();
            let tc = queue_of(1);
            b.iter(|| {
                let (_key, _task, waker) = tc.take_task().unwrap();
                cede(&waker);
            });
        }

        /// The handout as it costs on a queue where ONE task is pinned, which
        /// is what a single `sched_setaffinity` or an IRQ pool leaves behind:
        /// `inner.affine != 0`, so `schedule` leaves the bitmap path and takes
        /// the affinity branch for every handout, for every task on the queue.
        ///
        /// Read this against `hand_out_a_task_and_take_it_back`: the gap is
        /// what one pinned task costs the other 1023, and the affine lane is
        /// what closes it. The pinned task is not the one handed out -- it is
        /// pinned to CPU 1 and this collection is CPU 0's -- so what is being
        /// measured is purely the scan.
        #[bench]
        fn hand_out_a_task_with_one_pinned_task_among_a_thousand(b: &mut Bencher) {
            let _g = crate::runtime::resched_test_lock();
            let tc = queue_part_pinned(1024, 1);
            b.iter(|| {
                let (_key, _task, waker) = tc.take_task().unwrap();
                cede(&waker);
            });
        }

        /// The floor of the row above: every task pinned, so there is nothing
        /// for the lane to skip and the branch really does fetch each runnable
        /// bit. The two rows together say whether a slow handout is "the queue
        /// is pinned" or "the scan is wasteful".
        #[bench]
        fn hand_out_a_task_with_a_thousand_pinned_tasks(b: &mut Bencher) {
            let _g = crate::runtime::resched_test_lock();
            let tc = TaskCollection::new(0);
            // The first task may run here, so there is something to hand out;
            // the other 1023 are pinned to CPU 1 and only make the scan long.
            tc.add_task(pending(), Some(Arc::new(AtomicU64::new(0b11))));
            for _ in 0..1023 {
                tc.add_task(pending(), Some(Arc::new(AtomicU64::new(0b10))));
            }
            b.iter(|| {
                let (_key, _task, waker) = tc.take_task().unwrap();
                cede(&waker);
            });
        }

        /// The same handout to a thief from another CPU: `try_lock` on both,
        /// and the borrow released without a self-wake.
        #[bench]
        fn hand_out_a_task_to_a_thief(b: &mut Bencher) {
            let _g = crate::runtime::resched_test_lock();
            let tc = queue_of(1);
            b.iter(|| {
                let (_key, _task, waker) = tc.try_take_task().unwrap();
                waker.wake_by_ref();
                waker.mark_borrowed(false);
            });
        }

        /// Asking an empty queue, which is what the idle path and every thief
        /// that picks a quiet victim get. One page to walk.
        #[bench]
        fn ask_an_empty_queue_for_work(b: &mut Bencher) {
            let tc = queue_of(1);
            drain(&tc);
            b.iter(|| black_box(tc.take_task()).is_none());
        }

        /// The same refusal on a queue that once held a thousand tasks: the
        /// pages are still there and the generator still walks them.
        #[bench]
        fn ask_an_empty_queue_that_once_held_a_thousand(b: &mut Bencher) {
            let tc = queue_of(1024);
            drain(&tc);
            b.iter(|| black_box(tc.take_task()).is_none());
        }

        // ── the pre-halt recheck ───────────────────────────────────────────

        #[bench]
        fn pre_halt_recheck_one_page(b: &mut Bencher) {
            let tc = queue_of(64);
            b.iter(|| tc.has_ready());
        }

        #[bench]
        fn pre_halt_recheck_sixteen_pages(b: &mut Bencher) {
            let tc = queue_of(1024);
            b.iter(|| tc.has_ready());
        }

        /// The worst case the `affine` shortcut exists for: every task on the
        /// queue is pinned to another CPU, so the walk visits all 1024
        /// runnable bits, asks the slab about each one, and answers "nothing
        /// for you" — on the way into `hlt`. `WakerPage::affine_bits` does not
        /// improve this and was not expected to: every bit it would skip is a
        /// bit that genuinely carries a mask.
        #[bench]
        fn pre_halt_recheck_with_a_thousand_tasks_pinned_elsewhere(b: &mut Bencher) {
            let tc = queue_of_pinned(1024);
            b.iter(|| tc.has_ready());
        }

        /// The row that says what the lane does *not* buy, which was worth
        /// finding out: the first 512 slots pinned to another CPU, the rest
        /// free. The lane answers a page at a time, so a page whose every
        /// runnable slot is masked is still walked bit by bit — the question
        /// "is any of these allowed here" cannot be answered without the
        /// masks. Measured 1,741 ns before the lane and 1,347 after, and all
        /// of that 23% is the eight wholly-unmasked pages at the far end.
        ///
        /// `has_ready` was never the one paying, because it has always
        /// returned at the first allowed slot: what it costs is however many
        /// disallowed slots sort ahead of the first allowed one, and the lane
        /// cannot shorten that. The scan that had no early exit is
        /// `ready_num_for`, two sections down.
        #[bench]
        fn pre_halt_recheck_with_half_the_queue_pinned_elsewhere(b: &mut Bencher) {
            let tc = queue_part_pinned(1024, 512);
            b.iter(|| tc.has_ready());
        }

        // ── the three load figures ─────────────────────────────────────────

        #[bench]
        fn count_runnable_one_page(b: &mut Bencher) {
            let tc = queue_of(64);
            b.iter(|| tc.ready_num());
        }

        #[bench]
        fn count_runnable_sixteen_pages(b: &mut Bencher) {
            let tc = queue_of(1024);
            b.iter(|| tc.ready_num());
        }

        #[bench]
        fn count_runnable_for_a_thief_one_page(b: &mut Bencher) {
            let tc = queue_of(64);
            b.iter(|| tc.ready_num_for(black_box(0)));
        }

        #[bench]
        fn count_runnable_for_a_thief_sixteen_pages(b: &mut Bencher) {
            let tc = queue_of(1024);
            b.iter(|| tc.ready_num_for(black_box(0)));
        }

        /// What the steal scan would pay per victim per pass without the
        /// `affine == 0` shortcut: a `PinSlab` lookup and an `allowed_on` for
        /// every runnable bit, and no early exit, because this figure is a
        /// count and not a question.
        #[bench]
        fn count_runnable_for_a_thief_with_a_thousand_pinned_tasks(b: &mut Bencher) {
            let tc = queue_of_pinned(1024);
            b.iter(|| tc.ready_num_for(black_box(0)));
        }

        /// The headline row for the lane: one pinned task among a thousand,
        /// which is what an IRQ pool or a single `sched_setaffinity` leaves
        /// on an otherwise ordinary queue. `ready_num_for` has no early exit,
        /// so before the lane this was 1024 `PinSlab` lookups and an
        /// `allowed_on` each — per victim, per idle steal pass — for a figure
        /// that is now a popcount and one lookup. **3,273 ns before, 55
        /// after.**
        ///
        /// Read it next to
        /// `count_runnable_for_a_thief_with_a_thousand_pinned_tasks`, which
        /// the lane leaves at 3.7 us: when every task really does carry a
        /// mask there is nothing to skip, and the saving here is exactly the
        /// 1023 tasks that never had one.
        #[bench]
        fn count_runnable_for_a_thief_with_one_pinned_task_among_a_thousand(b: &mut Bencher) {
            let tc = queue_part_pinned(1024, 1);
            b.iter(|| tc.ready_num_for(black_box(0)));
        }

        #[bench]
        fn read_the_placement_load_one_page(b: &mut Bencher) {
            let tc = queue_of(64);
            b.iter(|| tc.placement_load());
        }

        #[bench]
        fn read_the_placement_load_sixteen_pages(b: &mut Bencher) {
            let tc = queue_of(1024);
            b.iter(|| tc.placement_load());
        }

        /// The figure placement used to steer by, and the reason the one above
        /// exists: a relaxed load of one counter, whatever the queue holds.
        #[bench]
        fn read_the_raw_task_count(b: &mut Bencher) {
            let tc = queue_of(1024);
            b.iter(|| tc.task_num());
        }

        // ── diagnostics ────────────────────────────────────────────────────

        #[bench]
        fn summarise_the_queue_for_the_hang_detector_one_page(b: &mut Bencher) {
            let tc = queue_of(64);
            b.iter(|| tc.debug_pending());
        }

        /// Read this against the row above and against
        /// `count_runnable_sixteen_pages`: unlike the three load figures,
        /// `debug_pending` walks and locks all 32 priority queues, of which 31
        /// are empty by construction (`priority_add_task` asserts that every
        /// task lands on `DEFAULT_PRIORITY`). It is a hang-detector path, so
        /// that is a choice and not a bug — but the 31 locks are what the
        /// difference is.
        #[bench]
        fn summarise_the_queue_for_the_hang_detector_sixteen_pages(b: &mut Bencher) {
            let tc = queue_of(1024);
            b.iter(|| tc.debug_pending());
        }
    }
}
