use crate::context::{Context as ExecuterContext, ContextData};
use alloc::alloc::{Allocator, Global, Layout};
use core::pin::Pin;
use {
    alloc::boxed::Box,
    alloc::sync::Arc,
    core::ptr::NonNull,
    core::task::{Context, Poll},
};

use crate::arch::executor_entry;
use crate::task_collection::{Task, TaskCollection};

#[derive(Debug, PartialEq, Eq)]
enum ExecutorState {
    STRONG,
    WEAK, // 执行完一次future后就需要被drop
    KILLED,
    UNUSED,
}

pub struct Executor {
    id: usize,
    task_collection: Arc<TaskCollection>,
    stack_base: usize,
    pub context: ExecuterContext,
    #[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
    context_data: ContextData,
    task_id: usize,
    state: ExecutorState,
}

/// Idle-loop iterations since any task was last polled (hang detector; see the
/// idle branch in `run`). Global because all executors on a CPU share progress.
static IDLE_STREAK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

const STACK_SIZE: usize = 4096 * 32;
const STACK_LAYOUT: Layout = Layout::new::<[u8; STACK_SIZE]>();

/// DEBUG: magic written to the lowest words of every coroutine stack. The stack
/// grows down from `stack_base + STACK_SIZE`; if a deep kernel call chain reaches
/// `stack_base`, the canary is clobbered and detected after the future yields.
const STACK_CANARY: u64 = 0x5354_4143_4b5f_4f56; // "STACK_OV"

fn executor_alloc_id() -> usize {
    use core::sync::atomic::{AtomicUsize, Ordering};
    static EXECUTOR_ID: AtomicUsize = AtomicUsize::new(1);
    EXECUTOR_ID.fetch_add(1, Ordering::SeqCst)
}

impl Executor {
    pub fn new(task_collection: Arc<TaskCollection>) -> Pin<Box<Self>> {
        let stack: NonNull<u8> = Global
            .allocate(STACK_LAYOUT)
            .expect("Alloction Stack Failed.")
            .cast();
        let stack_base = stack.as_ptr() as usize;
        // DEBUG: lay down the stack-overflow canary at the lowest 4 words.
        unsafe {
            let p = stack_base as *mut u64;
            for i in 0..4 {
                core::ptr::write_volatile(p.add(i), STACK_CANARY ^ i as u64);
            }
        }
        let mut pin_executor = Pin::new(Box::new(Executor {
            id: executor_alloc_id(),
            task_collection,
            stack_base,
            context: ExecuterContext::default(),
            #[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
            context_data: ContextData::default(),
            task_id: 0,
            state: ExecutorState::UNUSED,
        }));

        pin_executor.init_stack_and_context();

        trace!(
            "stack top 0x{:x} executor addr 0x{:x}, pgbr = 0x{:x}",
            pin_executor.context.get_sp(),
            pin_executor.context.get_pc(),
            pin_executor.context.get_pgbr(),
        );
        pin_executor
    }

    // stack layout: [executor_addr | context ]
    fn init_stack_and_context(&mut self) {
        let mut stack_top = self.stack_base + STACK_SIZE;
        let self_addr = self as *const Self as usize;
        stack_top = unsafe { push_stack(stack_top, self_addr) };
        #[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
        {
            self.context_data = ContextData::new(
                executor_entry as *const () as usize,
                stack_top,
                crate::arch::pg_base_register(),
            );
            self.context
                .set_context(&self.context_data as *const _ as usize);
        }
        #[cfg(target_arch = "x86_64")]
        {
            let context_data = ContextData::new(
                executor_entry as *const () as usize,
                stack_top,
                crate::arch::pg_base_register(),
            );
            stack_top = unsafe { push_stack(stack_top, context_data) };
            self.context.set_context(stack_top);
        }
    }

    pub fn run(&mut self) {
        // Lazy-TLB safety pin.
        //
        // `ThreadSwitchFuture::poll` leaves this CPU on the polled thread's
        // *process* page table after the poll (lazy-TLB: it skips reloading the
        // kernel CR3 to avoid a TLB flush per poll). The scheduler code below —
        // `take_task` and `steal_task_from_other_cpu` — therefore runs under
        // that user CR3. Those routines only touch kernel-half memory, which is
        // mapped in every process page table, so that is fine *as long as the
        // page table still exists*.
        //
        // The danger is the page table being freed out from under us: if the
        // process whose CR3 we are holding exits and is reaped on another CPU,
        // its `PageTableImpl` drops and the root (PML4) / intermediate frames go
        // back to the frame allocator and get reused. The MMU then walks freed,
        // overwritten page-table memory on the next TLB miss, so our own kernel
        // stack / the iret frame we are about to build reads garbage -> `iretq`
        // to ring-0 junk -> #UD. This is exactly the intermittent SMP crash seen
        // under `apk` (rapid fork/exit) — and ctxcheck never fires because the
        // saved `UserContext` is valid; it is the physical memory behind it that
        // changes under the stale CR3.
        //
        // Hold an `Arc` to the most-recently-polled task across the next
        // `take_task`/`steal` step. That keeps its `Thread` -> `Process` ->
        // `vmar` -> page table alive, so a concurrent exit cannot free the page
        // table while its CR3 is still loaded here. The pin is replaced only
        // after the *next* poll has switched CR3 to another address space (or to
        // the kernel CR3, which `CurrentThread::drop` restores when a thread
        // finishes), so the previous page table is released only once its CR3 is
        // no longer loaded on this CPU.
        let mut _cr3_pin: Option<Arc<Task>> = None;
        loop {
            let mut task_info = self.task_collection.take_task();
            if task_info.is_none() {
                task_info = crate::runtime::steal_task_from_other_cpu();
            }
            if let Some((_key, task, waker_ref, droper)) = task_info {
                let waker_ref = Arc::new(waker_ref);
                let waker = woke::waker_ref(&waker_ref);
                let mut cx = Context::from_waker(&waker);
                waker_ref.mark_borrowed(true);
                self.task_id = task.id();
                debug!("running future {}:{}", self.id(), task.id());
                // Hang detector: a task is being polled, so clear the idle-loop
                // streak. If the machine then spins the idle loop many times with
                // tasks still present but nothing polled, a wake was lost (see the
                // else-branch dump below).
                IDLE_STREAK.store(0, core::sync::atomic::Ordering::Relaxed);
                let ret = task.poll(&mut cx);
                // DEBUG: did this future overflow the 128 KiB coroutine stack?
                unsafe {
                    let p = self.stack_base as *const u64;
                    for i in 0..4 {
                        if core::ptr::read_volatile(p.add(i)) != (STACK_CANARY ^ i as u64) {
                            error!(
                                "[stackcheck] COROUTINE STACK OVERFLOW: executor id={} task_id={} stack_base={:#x} size={:#x}",
                                self.id(), task.id(), self.stack_base, STACK_SIZE
                            );
                            break;
                        }
                    }
                }
                debug!("back from future {}:{}", self.id(), task.id());
                self.task_id = 0;
                waker_ref.mark_borrowed(false);
                // Pin this task's address space for the upcoming take_task/steal
                // (which run under the CR3 this poll just (re)loaded). Replacing
                // the previous pin here is safe: CR3 now points at *this* task's
                // page table (or at the kernel CR3 if the thread just finished —
                // `CurrentThread::drop` restored it), so the page table we drop
                // is no longer the active one. See the comment at the top of
                // `run`.
                _cr3_pin = Some(task.clone());
                match ret {
                    Poll::Ready(()) => {
                        debug!("task over id = {}", task.id());
                        droper.drop_by_ref();
                    }
                    Poll::Pending => {
                        // Do Nothing
                    }
                };
                if let ExecutorState::WEAK = self.state {
                    self.state = ExecutorState::KILLED;
                    return;
                }
            } else {
                let runtime = crate::runtime::get_current_runtime();
                let task_num = runtime.task_num();
                let weak_executor = runtime.weak_executor_num();
                drop(runtime);
                // TODO: some cores may exit by mistake when we have multi-cores
                if cfg!(feature = "baremetal-test") && task_num == 0 {
                    debug!("all done! exit and reboot");
                    crate::runtime::sched_yield();
                } else if weak_executor != 0 {
                    debug!("return to runtime and run weak executor");
                    crate::runtime::sched_yield();
                } else if crate::runtime::run_idle_callback() {
                    // The idle callback made progress (e.g. drained deferred
                    // driver jobs that may have woken tasks): re-check the run
                    // queue instead of halting until the next interrupt.
                    continue;
                } else {
                    // Hang detector (diagnostics only): the run queue is empty so
                    // we are about to halt until the next interrupt. If tasks still
                    // exist, count idle-loop iterations; the 250 Hz timer wakes us
                    // ~every 4 ms, so ~750 iterations with no task polled ≈ 3 s.
                    // `IDLE_STREAK` is global and reset on every real poll, so it
                    // only climbs while *no* CPU makes progress.
                    //
                    // Classify before shouting — not every idle state is a hang:
                    //   borrowed > 0 : a task is owned by an executor (being polled,
                    //                  or preempted mid-poll and waiting to resume —
                    //                  possibly on another CPU that stole it). That
                    //                  is in-flight work, so an idle peer seeing it
                    //                  is expected under SMP steal+preempt; only a
                    //                  borrow that stays outstanding for a very long
                    //                  time (≈20 s) is suspicious (a leaked borrow).
                    //   notified > 0 : a task is ready but unpicked here — usually
                    //                  CPU-affinity bound to another core, which will
                    //                  run it. Benign; report only if it persists.
                    //   n == 0 && b == 0 : tasks exist, none ready, none in-flight —
                    //                  the only state that can never recover on its
                    //                  own = a genuine lost wake. Flag promptly.
                    if task_num > 0 {
                        let s = IDLE_STREAK.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                        if s >= 750 {
                            let (tn, n, d, b) = self.task_collection.debug_pending();
                            let genuine_stall = n == 0 && b == 0;
                            let report = if genuine_stall {
                                s == 750 || s % 2500 == 0
                            } else {
                                // In-flight / affinity: far higher bar so a
                                // transient (a long timer wait, a task briefly
                                // stolen+preempted) doesn't spam the console.
                                s == 5000 || s % 5000 == 0
                            };
                            if report {
                                let cause = if genuine_stall {
                                    "LOST WAKE (no task ready or in-flight)"
                                } else if n != 0 {
                                    "NOTIFIED-NOT-PICKED (affinity-bound to another CPU?)"
                                } else {
                                    "BORROW OUTSTANDING (in-flight elsewhere, or leaked)"
                                };
                                if genuine_stall {
                                    error!(
                                        "[sched-hang] executor {} idle {} loops: task_num={} notified={} dropped={} borrowed={} -> {}",
                                        self.id(), s, tn, n, d, b, cause
                                    );
                                } else {
                                    warn!(
                                        "[sched-hang] executor {} idle {} loops: task_num={} notified={} dropped={} borrowed={} -> {}",
                                        self.id(), s, tn, n, d, b, cause
                                    );
                                }
                            }
                        }
                    }
                    debug!("no other tasks, wait for interrupt");
                    crate::arch::wait_for_interrupt();
                }
            }
        }
    }

    // 当前是否在运行future
    // 发生supervisor时钟中断时, 若executor在运行future, 则
    // 说明该future超时, 需要切换到另一个executor来执行其他future.
    pub fn is_running_future(&self) -> bool {
        self.task_id != 0
    }

    pub fn killed(&self) -> bool {
        self.state == ExecutorState::KILLED
    }

    pub fn mark_weak(&mut self) {
        self.state = ExecutorState::WEAK;
    }

    pub fn id(&self) -> usize {
        self.id
    }

    pub fn task_id(&self) -> usize {
        self.task_id
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        unsafe {
            let stack = NonNull::<u8>::new_unchecked(self.stack_base as *mut u8);
            Global.deallocate(stack, STACK_LAYOUT);
        }
    }
}

unsafe impl Send for Executor {}
unsafe impl Sync for Executor {}

pub unsafe fn push_stack<T>(stack_top: usize, val: T) -> usize {
    let stack_top = (stack_top as *mut T).sub(1);
    *stack_top = val;
    stack_top as _
}
