//! Per-CPU storage block.
//!
//! Modeled on Redox OS's `PercpuBlock` / `ProcessorControlRegion`: each CPU owns
//! a single [`PercpuBlock`] that consolidates its per-CPU state, instead of
//! scattering separate `[_; MAX_CORE_NUM]` arrays indexed by CPU id.
//!
//! The current CPU's block is reached through [`current`]. On x86_64 the block
//! pointer lives in the GS region set up by `trapframe` (read with a single
//! `mov reg, gs:[off]`, no array indexing) — the same trick Redox uses with its
//! PCR. On architectures whose per-CPU register fast-path is not wired up yet,
//! [`current`] falls back to indexing [`PERCPU`] by the dense logical CPU id,
//! which is bounded and therefore safe.

use alloc::sync::Arc;
use core::any::Any;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::common::cpu_topology::{percpu_slot, PERCPU_SLOTS};
use crate::config::MAX_CORE_NUM;
use crate::utils::PerCpuCell;

/// Consolidated per-CPU state.
///
/// One cache line per CPU. These are the fields a CPU writes on its own timer
/// tick — the quantum on every one of them — and the block was 32 bytes, so
/// two CPUs shared a line and each one's tick invalidated the other's copy.
///
/// It also fixes the stride [`current`] checks a published pointer against.
/// Thirty-two was a power of two by luck; one more `u64` field and it would
/// not have been, and the mask below would have started rejecting live blocks.
/// The `const` assert is what turns the luck into a rule.
#[repr(align(64))]
pub struct PercpuBlock {
    /// Dense logical CPU id that owns this block (`u32::MAX` until registered).
    cpu_id: AtomicU32,
    /// The thread currently running on this CPU.
    pub current_thread: PerCpuCell<Option<Arc<dyn Any + Send + Sync>>>,
    /// Remaining timer ticks before the current task must yield.
    /// See [`tick_should_preempt`].
    tick_quantum: PerCpuCell<u32>,
    /// Whether this CPU's LAPIC timer has been stretched for tickless idle
    /// (see `timer::timer_idle_enter`). Touched only by its owning CPU.
    timer_idle_armed: PerCpuCell<bool>,
    /// Nesting depth of `timer_tick` on this CPU (housekeeping + Box
    /// callbacks). A counter (not a bool) because work can re-enable IRQs via
    /// lock `pop_off` and re-enter `timer_tick`. Lets the #PF path distinguish
    /// a corrupt fn-ptr/vtable in the timer path from a userspace fault that
    /// merely interrupted the same process.
    timer_callback_depth: PerCpuCell<u32>,
}

impl PercpuBlock {
    const fn new() -> Self {
        Self {
            cpu_id: AtomicU32::new(u32::MAX),
            current_thread: PerCpuCell::new(None),
            tick_quantum: PerCpuCell::new(0),
            timer_idle_armed: PerCpuCell::new(false),
            timer_callback_depth: PerCpuCell::new(0),
        }
    }

    /// The dense logical id of the CPU this block belongs to.
    #[inline]
    pub fn cpu_id(&self) -> u32 {
        self.cpu_id.load(Ordering::Relaxed)
    }
}

/// Distance between two blocks in [`PERCPU`].
///
/// A power of two by construction (see the alignment on [`PercpuBlock`]),
/// which is what lets [`is_percpu_block`] check a pointer's alignment with a
/// mask rather than a division, on a path that runs on every timer tick.
const BLOCK_STRIDE: usize = core::mem::size_of::<PercpuBlock>();

const _: () = assert!(
    BLOCK_STRIDE.is_power_of_two(),
    "PercpuBlock must keep a power-of-two stride: is_percpu_block masks with it"
);

/// The dense logical id of the CPU running this code.
///
/// Behind a seam because it is the one input to this module that the host test
/// suite has to be able to drive: every answer here — which block is mine,
/// whether I have a block at all — is a function of it, and none of it is
/// architectural.
#[cfg(not(test))]
#[inline]
fn this_cpu_id() -> usize {
    crate::cpu::cpu_id() as usize
}

#[cfg(test)]
fn this_cpu_id() -> usize {
    tests::host::cpu_id()
}

/// How many timer ticks one task gets before the preemption point fires.
///
/// At 250 Hz (4 ms tick), 5 ticks ≈ 20 ms — close to Linux's default
/// non-interactive time slice. Yielding on every raw tick (the previous
/// behaviour) churned the async executor at 250 Hz even when the same task
/// was the only runnable one, which showed up as scheduler overhead on
/// CPU-bound workloads.
const TICKS_PER_QUANTUM: u32 = 5;

/// Decrement the current CPU's quantum and report whether the caller should
/// yield. Returns `true` exactly once every [`TICKS_PER_QUANTUM`] calls from a
/// given CPU.
///
/// Called from the timer-interrupt path of the user-trap handler; the cell is
/// only ever touched by its owning CPU, so the unsynchronised access is sound.
#[inline]
pub fn tick_should_preempt() -> bool {
    let cell = &current().tick_quantum;
    let n = *cell.get();
    if n == 0 {
        *cell.get_mut() = TICKS_PER_QUANTUM - 1;
        true
    } else {
        *cell.get_mut() = n - 1;
        false
    }
}

/// Whether the current CPU's LAPIC timer is currently stretched for tickless
/// idle. Only ever read/written by the owning CPU.
#[inline]
pub fn timer_idle_armed() -> bool {
    *current().timer_idle_armed.get()
}

/// Record whether the current CPU's LAPIC timer is stretched for tickless idle.
#[inline]
pub fn set_timer_idle_armed(armed: bool) {
    *current().timer_idle_armed.get_mut() = armed;
}

/// Whether this CPU is currently inside `timer_tick` (any stage).
#[inline]
pub fn in_timer_callback() -> bool {
    *current().timer_callback_depth.get() > 0
}

/// Enter `timer_tick` on this CPU (nesting-safe).
#[inline]
pub fn begin_timer_callback() {
    let cell = &current().timer_callback_depth;
    *cell.get_mut() = cell.get().saturating_add(1);
}

/// Leave `timer_tick` on this CPU (nesting-safe).
#[inline]
pub fn end_timer_callback() {
    let cell = &current().timer_callback_depth;
    *cell.get_mut() = cell.get().saturating_sub(1);
}

/// Backing storage for every CPU's block, indexed by dense logical CPU id.
///
/// Used both as cross-CPU storage and as the fallback for [`current`] before the
/// per-CPU register fast-path is established.
///
/// One slot longer than there are CPUs: the last one is the quarantine
/// ([`percpu_slot`]), where a CPU whose id resolved to nothing lands. It used
/// to land on slot 0, which is the boot CPU's — and this block holds
/// `current_thread`, so that made two CPUs believe they were running the same
/// thread, each writing the other's quantum and timer state.
static PERCPU: [PercpuBlock; PERCPU_SLOTS] = [const { PercpuBlock::new() }; PERCPU_SLOTS];

/// Architecture fast-path: pointer to the current CPU's block, or null if not
/// yet established on this CPU / arch.
#[inline]
fn arch_percpu_ptr() -> *const PercpuBlock {
    #[cfg(test)]
    {
        tests::host::published_ptr()
    }
    #[cfg(all(not(test), target_arch = "x86_64"))]
    {
        trapframe::read_cpu_local() as *const PercpuBlock
    }
    #[cfg(all(not(test), not(target_arch = "x86_64")))]
    {
        core::ptr::null()
    }
}

/// Record this CPU's block pointer in its per-CPU register, if supported.
#[inline]
fn set_arch_percpu_ptr(_block: &'static PercpuBlock) {
    #[cfg(test)]
    tests::host::publish_ptr(_block as *const PercpuBlock);
    #[cfg(all(not(test), target_arch = "x86_64"))]
    unsafe {
        // Safe: `trapframe::init()` has run on this CPU before `register`.
        trapframe::write_cpu_local(_block as *const PercpuBlock as usize);
    }
}

/// How many times [`current`] has refused a published block pointer, and the
/// last one it refused. Zero on a healthy machine; the panic reporter says so
/// when it is not, because a bogus per-CPU pointer explains a class of crash
/// that otherwise reads as heap corruption with no author.
static BOGUS_PTR_COUNT: AtomicU32 = AtomicU32::new(0);
static BOGUS_PTR_LAST: AtomicUsize = AtomicUsize::new(0);

/// Whether `ptr` is one of [`PERCPU`]'s blocks.
///
/// The pointer comes out of the per-CPU publisher — the x86_64 GS region — and
/// that region is not a trusted input. Two words away from it lives the
/// published logical CPU id, and every reader of *that* cross-checks it:
/// `lock::current_cpu_id` accepts it only if bring-up actually registered it
/// and records `note_bogus` otherwise, and the NMI shootdown rescue refuses to
/// read the region at all ("Never trust GS here"). Both say why: a `swapgs`
/// imbalance or a wild write makes the region name a CPU that does not exist.
///
/// The pointer was the one value in there nobody checked, and it is the one
/// whose corruption is not a wrong answer. [`current`] hands out a
/// `&'static PercpuBlock`, and `set_current_thread` writes an `Arc` through
/// it — so a bogus GS does not merely confuse two CPUs about whose quantum is
/// whose, it drops a reference count on whatever the bad address happens to
/// point at, and the wreck surfaces later somewhere else entirely.
///
/// Refusing costs the caller nothing: the fallback below is the bounded index
/// into the same table, which is what every architecture without a per-CPU
/// register uses all the time.
#[inline]
fn is_percpu_block(ptr: *const PercpuBlock) -> bool {
    // `wrapping_sub` and not a pair of comparisons against the two ends: an
    // address below the table wraps to a huge offset, so the one bound rules
    // out both ends. Null is such an address, so it is refused here too, which
    // is what the caller wants — it is just not an address worth reporting.
    let offset = (ptr as usize).wrapping_sub(PERCPU.as_ptr() as usize);
    offset < BLOCK_STRIDE * PERCPU_SLOTS && offset & (BLOCK_STRIDE - 1) == 0
}

/// How many published block pointers [`current`] has refused, and the last one.
///
/// For the panic reporter. Not logged from [`current`] itself: it runs on the
/// timer-interrupt path and every console writer takes a lock, which is the
/// same reason `lock` only records its own bogus ids.
pub fn bogus_percpu_ptrs() -> (u32, usize) {
    (
        BOGUS_PTR_COUNT.load(Ordering::Relaxed),
        BOGUS_PTR_LAST.load(Ordering::Relaxed),
    )
}

/// The current CPU's [`PercpuBlock`].
#[inline]
pub fn current() -> &'static PercpuBlock {
    let ptr = arch_percpu_ptr();
    if is_percpu_block(ptr) {
        // Safe: the pointer lands exactly on one of `PERCPU`'s blocks, which
        // are `'static`. That is the whole of what the check buys, and it is
        // the whole of what the dereference needs.
        unsafe { &*ptr }
    } else {
        if !ptr.is_null() {
            // Null is "no fast-path on this CPU yet", the ordinary state on
            // every arch but x86_64. Anything else was published and is wrong.
            BOGUS_PTR_COUNT.fetch_add(1, Ordering::Relaxed);
            BOGUS_PTR_LAST.store(ptr as usize, Ordering::Relaxed);
        }
        // Fallback before the register fast-path is set (or on arches without
        // one). `cpu_id()` is the dense logical id, and it can now say "this
        // CPU has none" (`lock`'s `NO_CPU`) instead of quietly saying 0 —
        // which is why the table has a slot that belongs to nobody.
        &PERCPU[percpu_slot(this_cpu_id())]
    }
}

/// Bind the current CPU to its [`PercpuBlock`].
///
/// Call once per CPU, after `trapframe::init()` (which sets up the GS region on
/// x86_64) and after the CPU's logical id is known.
pub fn register() {
    // Establish this CPU's hardware-id -> logical-id mapping where the arch needs
    // to self-assign (riscv). On x86_64 the mapping is set during SMP enumeration.
    #[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
    {
        crate::cpu::register_logical_id();
    }
    let id = this_cpu_id();
    if id >= MAX_CORE_NUM {
        // This CPU has no logical id, so it has no block: registering it would
        // mean binding it to the quarantine slot and letting it run as if it
        // had one. Say so instead. It cannot take a kernel lock either — the
        // `lock` crate refuses the same id by name — so this is a report, not
        // a recovery.
        crate::klog_warn!(
            "[smp] a CPU reached percpu::register with no logical id ({}) \
             — it must not run kernel code",
            id
        );
        return;
    }
    let block = &PERCPU[id];
    block.cpu_id.store(id as u32, Ordering::Relaxed);
    #[cfg(all(not(test), target_arch = "x86_64"))]
    unsafe {
        trapframe::write_logical_cpu_id(id as u8);
    }
    set_arch_percpu_ptr(block);
}

/// The per-CPU block is `bare`-only, so until now nothing compiled it outside a
/// kernel build: not the host suite, and not the emulator either in any way
/// that exercises these paths, since QEMU boots one or two cores. What is
/// pinned here is the part that is not architectural — which block a CPU gets,
/// what happens to a CPU that has no id, and what the quantum and the callback
/// depth count — plus the one thing the machine had been taking on faith: that
/// the pointer the per-CPU register hands back names a block at all.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::cpu_topology::QUARANTINE_SLOT;

    /// Stands in for the per-CPU register: one simulated CPU per test thread,
    /// the same shape `lock`'s host backend uses. `PUBLISHED` is this thread's
    /// GS word and `CPU_ID` is what the hardware id would resolve to.
    pub(super) mod host {
        use super::super::PercpuBlock;
        use core::cell::Cell;

        std::thread_local! {
            static PUBLISHED: Cell<*const PercpuBlock> = const { Cell::new(core::ptr::null()) };
            static CPU_ID: Cell<usize> = const { Cell::new(0) };
        }

        pub(crate) fn published_ptr() -> *const PercpuBlock {
            PUBLISHED.with(|c| c.get())
        }

        pub(crate) fn publish_ptr(ptr: *const PercpuBlock) {
            PUBLISHED.with(|c| c.set(ptr));
        }

        pub(crate) fn cpu_id() -> usize {
            CPU_ID.with(|c| c.get())
        }

        pub(crate) fn set_cpu_id(id: usize) {
            CPU_ID.with(|c| c.set(id));
        }
    }

    /// A block that is not in [`PERCPU`]. Real storage, so the failure this
    /// guards against is not "the test would segfault" but the quiet one: a
    /// pointer that reads and writes perfectly well and belongs to nobody.
    static STRAY: PercpuBlock = PercpuBlock::new();

    /// `PERCPU` is one table shared by every test thread, so a test that writes
    /// through a block owns an id no other test uses.
    fn as_cpu(id: usize) {
        host::set_cpu_id(id);
    }

    fn block_ptr(slot: usize) -> *const PercpuBlock {
        &PERCPU[slot] as *const PercpuBlock
    }

    #[test]
    fn a_published_block_pointer_is_the_block_it_names() {
        host::publish_ptr(block_ptr(1));
        assert!(
            core::ptr::eq(current(), &PERCPU[1]),
            "the whole point of the per-CPU register is to skip the indexing; \
             a published block that is not used is one GS read wasted on every tick"
        );
    }

    #[test]
    fn a_wild_published_pointer_is_refused_and_counted() {
        as_cpu(0);
        let before = bogus_percpu_ptrs().0;
        host::publish_ptr(&STRAY as *const PercpuBlock);
        assert!(
            core::ptr::eq(current(), &PERCPU[0]),
            "a published pointer that is not one of the blocks was dereferenced: \
             `set_current_thread` writes an Arc through this reference, so that \
             is a refcount dropped on whatever the bad address points at"
        );
        assert!(
            bogus_percpu_ptrs().0 > before,
            "the refusal left no trace, so the crash it prevents still reads as \
             heap corruption with no author"
        );
    }

    #[test]
    fn a_published_pointer_that_lands_between_two_blocks_is_refused() {
        as_cpu(0);
        // Inside the table's address range, so a bounds check alone accepts it,
        // and one byte off the stride, so the fields it names are every field
        // straddled: another CPU's `current_thread` read as this one's quantum.
        let between = (block_ptr(2) as usize + 1) as *const PercpuBlock;
        host::publish_ptr(between);
        assert!(
            core::ptr::eq(current(), &PERCPU[0]),
            "a pointer inside the table but off the stride was accepted: being \
             in range is not the same as naming a block"
        );
    }

    #[test]
    fn a_published_pointer_one_block_past_the_table_is_refused() {
        as_cpu(0);
        let past = (PERCPU.as_ptr() as usize + BLOCK_STRIDE * PERCPU_SLOTS) as *const PercpuBlock;
        host::publish_ptr(past);
        assert!(
            core::ptr::eq(current(), &PERCPU[0]),
            "the first address past the table was accepted as a block"
        );
    }

    #[test]
    fn a_published_pointer_just_below_the_table_is_refused() {
        as_cpu(0);
        let below = (PERCPU.as_ptr() as usize - BLOCK_STRIDE) as *const PercpuBlock;
        host::publish_ptr(below);
        assert!(
            core::ptr::eq(current(), &PERCPU[0]),
            "an address below the table was accepted as a block"
        );
    }

    #[test]
    fn with_nothing_published_a_cpu_gets_the_block_of_its_own_id() {
        as_cpu(6);
        assert!(
            core::ptr::eq(current(), &PERCPU[6]),
            "the fallback is what riscv and aarch64 use all the time"
        );
    }

    #[test]
    fn a_cpu_with_no_logical_id_gets_the_quarantine_and_never_the_boot_cpus_block() {
        as_cpu(MAX_CORE_NUM);
        let block = current();
        assert!(
            core::ptr::eq(block, &PERCPU[QUARANTINE_SLOT]),
            "a CPU whose id resolved to nothing must land in the slot that \
             belongs to nobody"
        );
        assert!(
            !core::ptr::eq(block, &PERCPU[0]),
            "it landed on the boot CPU's block, which holds the thread the boot \
             CPU is running: two CPUs then believe they are running it"
        );
    }

    #[test]
    fn registering_stamps_the_block_and_publishes_it() {
        as_cpu(8);
        register();
        assert!(
            core::ptr::eq(current(), &PERCPU[8]),
            "register did not publish this CPU's block"
        );
        assert_eq!(
            PERCPU[8].cpu_id(),
            8,
            "a registered block that does not know whose it is cannot be told \
             apart from one nobody registered"
        );
    }

    #[test]
    fn registering_a_core_with_no_logical_id_publishes_nothing() {
        as_cpu(MAX_CORE_NUM + 3);
        register();
        assert!(
            host::published_ptr().is_null(),
            "a core with no logical id was bound to a block, so it runs kernel \
             code as if it had an id — and the lock crate refuses it by name"
        );
        assert_eq!(
            PERCPU[QUARANTINE_SLOT].cpu_id(),
            u32::MAX,
            "the quarantine slot was stamped as if it belonged to a CPU"
        );
    }

    #[test]
    fn the_quantum_fires_once_every_five_ticks() {
        as_cpu(10);
        let fired: std::vec::Vec<bool> = (0..15).map(|_| tick_should_preempt()).collect();
        assert_eq!(
            fired,
            std::vec![
                true, false, false, false, false, true, false, false, false, false, true, false,
                false, false, false
            ],
            "the preemption point is the quantum: firing on every tick churns \
             the executor at 250 Hz, firing late stretches every time slice"
        );
        assert_eq!(
            fired.iter().filter(|f| **f).count(),
            15 / TICKS_PER_QUANTUM as usize
        );
    }

    #[test]
    fn two_cpus_count_their_quantum_separately() {
        as_cpu(11);
        assert!(tick_should_preempt(), "CPU 11's first tick");
        assert!(!tick_should_preempt());
        as_cpu(12);
        assert!(
            tick_should_preempt(),
            "CPU 12 spent CPU 11's quantum: the per-CPU block is per CPU or it \
             is nothing"
        );
        as_cpu(11);
        assert!(!tick_should_preempt(), "CPU 11 lost its place to CPU 12");
    }

    #[test]
    fn timer_callbacks_nest_and_unwind() {
        as_cpu(13);
        assert!(!in_timer_callback());
        begin_timer_callback();
        assert!(in_timer_callback());
        // A callback can re-enable IRQs through a lock's `pop_off` and be
        // re-entered, which is why the depth is a counter and not a flag.
        begin_timer_callback();
        end_timer_callback();
        assert!(
            in_timer_callback(),
            "the inner callback's exit cleared the outer one: the #PF reporter \
             then calls a fault inside the timer path a plain userspace fault"
        );
        end_timer_callback();
        assert!(!in_timer_callback());
    }

    #[test]
    fn leaving_a_timer_callback_nobody_entered_does_not_wrap() {
        as_cpu(14);
        end_timer_callback();
        assert!(
            !in_timer_callback(),
            "the depth wrapped to u32::MAX, so this CPU reports itself inside a \
             timer callback for the rest of the boot"
        );
    }

    #[test]
    fn the_tickless_idle_flag_belongs_to_its_cpu() {
        as_cpu(15);
        set_timer_idle_armed(true);
        as_cpu(16);
        assert!(
            !timer_idle_armed(),
            "one CPU stretching its LAPIC timer told another CPU its timer was \
             stretched, and that other CPU will not rearm it"
        );
        as_cpu(15);
        assert!(timer_idle_armed());
        set_timer_idle_armed(false);
        assert!(!timer_idle_armed());
    }

    #[test]
    fn no_two_cpus_share_a_cache_line() {
        assert_eq!(
            BLOCK_STRIDE % 64,
            0,
            "two CPUs' blocks share a cache line, so each one's timer tick \
             invalidates the other's"
        );
        assert!(
            BLOCK_STRIDE.is_power_of_two(),
            "the stride is what `is_percpu_block` masks with"
        );
        for slot in 1..PERCPU_SLOTS {
            assert_eq!(
                block_ptr(slot) as usize - block_ptr(slot - 1) as usize,
                BLOCK_STRIDE,
                "the table is not laid out at the stride the pointer check assumes"
            );
        }
    }
}

