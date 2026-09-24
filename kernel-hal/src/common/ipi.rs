use crate::common::cpu_topology::{percpu_slot, PERCPU_SLOTS};
use crate::{config::MAX_CORE_NUM, utils::mpsc_queue::MpscQueue};
use alloc::vec::Vec;

/// Two shapes bound a cpu id in this module. `CPU_ONLINE`, `IPI_READY` and
/// every wait mask are `AtomicU64`, so an id needs a
/// bit; the per-CPU tables below — the queues, `ACTIVE_VMTOKEN`,
/// `SHOOTDOWN_SEQ`, `SHOOTDOWN_GOAL` and the overflow counters — are
/// `[_; MAX_CORE_NUM]`, so it needs a slot. `MAX_CORE_NUM` is the tighter, and
/// the sibling module that hands the ids out says as much: "every guard in
/// this module is spelled `< MAX_CORE_NUM` rather than `< 64` so the day that
/// limit moves there is exactly one thing to widen". Three guards here were
/// spelled 64 — and one of them, `note_ipi_queue_overflow`, indexes a
/// `MAX_CORE_NUM`-sized array on its very next line, from interrupt context
/// with locks held.
const _: () = assert!(
    MAX_CORE_NUM <= 64,
    "the online/ready/wait masks are u64: widen them before raising MAX_CORE_NUM"
);

const REASON_SIZE: usize = 64;

pub type IpiEntry = usize;
type IRQueue = MpscQueue<'static, IpiEntry>;

/// Per-CPU backing storage for the IPI queues, indexed by dense logical CPU id.
static mut IPI_BUFFERS: [[IpiEntry; REASON_SIZE]; MAX_CORE_NUM] = [[0; REASON_SIZE]; MAX_CORE_NUM];

lazy_static::lazy_static! {
    /// One IPI queue per CPU, each backed by its slot in `IPI_BUFFERS`.
    static ref IPI_QUEUE: Vec<IRQueue> = (0..MAX_CORE_NUM)
        .map(|i| {
            IRQueue::new(unsafe {
                core::slice::from_raw_parts_mut(
                    core::ptr::addr_of_mut!(IPI_BUFFERS[i]).cast::<IpiEntry>(),
                    REASON_SIZE,
                )
            })
        })
        .collect();
}

/// The IPI queue of dense logical CPU `cpuid`, or `None` when no such CPU
/// exists.
///
/// Bounds-checked because the id is not always ours to trust. `cpu_id()` reads
/// the LAPIC, and reading it through the wrong interface is a documented
/// failure on this kernel: see [`SMP_ENABLED`] for the x2APIC window that made
/// every CPU report the same bogus id on real hardware. Every accessor in this
/// module already re-checks the id for that reason; this makes the check part
/// of the lookup instead, so a new caller cannot forget it. Indexing straight
/// into the table would panic — and the callers run in an interrupt handler,
/// or inside a ticket lock's spin pump with that lock held, where a panic
/// takes the machine with it and buries the cause.
pub(crate) fn ipi_queue(cpuid: usize) -> Option<&'static IRQueue> {
    IPI_QUEUE.get(cpuid)
}

/// Publish `reason` into `cpuid`'s IPI queue, or record that it could not be
/// published. Returns whether `cpuid` names a CPU at all.
///
/// This is the receiving half of the shootdown contract and it belongs in one
/// place: **a payload that does not reach the queue must set the target's
/// overflow bit**, because that bit is what makes the target's next drain a
/// full flush instead of the precise per-page one. Drop the payload without
/// the bit and the target never invalidates the page, while the initiator —
/// which only learns of a failed *send* — goes on to free the frame.
///
/// Each architecture used to write this out itself, and they had drifted:
/// x86_64 and aarch64 noted the overflow, riscv returned an error and noted
/// nothing. See `send_ipi` in `bare/arch/*/interrupt.rs`.
pub fn publish_ipi_entry(cpuid: usize, reason: IpiEntry) -> bool {
    let Some(queue) = ipi_queue(cpuid) else {
        return false;
    };
    // Reentrancy, not contention: see [`PUBLISHING`]. A publish already in
    // flight on this CPU owns a reserved slot that only it can commit, so a
    // nested one must not reserve a second — it would wait for a predecessor
    // that cannot run until the nested call returns.
    let Some(_guard) = PublishGuard::enter() else {
        note_ipi_queue_overflow(cpuid);
        return true;
    };
    let delivered = match queue.alloc_entry() {
        Some(idx) => {
            *queue.entry_at(idx) = reason;
            queue.commit_entry(idx)
        }
        None => false,
    };
    if !delivered {
        // Queue full, or the commit lost the publish race: the receiver cannot
        // learn this entry's payload, so force its next ack to full-flush.
        note_ipi_queue_overflow(cpuid);
    }
    true
}

/// Per-CPU "a publish is in flight here right now".
///
/// [`MpscQueue`] publishes strictly in index order: `commit_entry` spins until
/// `ptail` reaches the slot it reserved, and it deliberately never gives up —
/// abandoning a reserved slot freezes `ptail` for good, which is the bug its
/// own comment records. That makes the reserve-write-commit sequence
/// **non-reentrant**, and this one is reached from interrupt context: nothing
/// on the shootdown path turns interrupts off (`remote_flush_tlb_on` runs with
/// whatever state its caller had), so an interrupt can land between the CAS in
/// `alloc_entry` and the `fetch_add` in `commit_entry`. If its handler
/// shoots a page down too — a timer tick into the scheduler, into a task drop,
/// into a VMAR teardown — it reserves the next slot on that same queue and
/// spins for a predecessor that cannot commit until the handler returns. Both
/// halves of one CPU, waiting on each other, with no timeout by design.
///
/// A nested publish therefore does not queue at all: it sets the target's
/// overflow bit and returns. That is the same degradation this function
/// already applies to a full queue, and it is exactly what the bit means —
/// the payload did not reach the receiver, so its next drain full-flushes
/// instead of invalidating one page. One flush is the price of not wedging a
/// CPU, and it never fires outside the nested case.
///
/// Indexed by [`percpu_slot`], not by the raw id: a `cpu_id()` past the tables
/// is a documented failure on this kernel (see [`SMP_ENABLED`]), and it is
/// precisely the CPU whose publishes must still be guarded — the quarantine
/// slot costs two such CPUs a full flush each and wedges neither.
///
/// [`MpscQueue`]: crate::utils::mpsc_queue::MpscQueue
/// [`percpu_slot`]: crate::common::cpu_topology::percpu_slot
static PUBLISHING: [AtomicBool; PERCPU_SLOTS] = [const { AtomicBool::new(false) }; PERCPU_SLOTS];

/// Claims this CPU's publish slot for the duration of one
/// [`publish_ipi_entry`], and gives it back however that call leaves.
struct PublishGuard(usize);

impl PublishGuard {
    /// `None` when a publish is already in flight on this CPU.
    fn enter() -> Option<Self> {
        let slot = percpu_slot(crate::cpu::cpu_id() as usize);
        if PUBLISHING[slot].swap(true, Ordering::Acquire) {
            return None;
        }
        Some(PublishGuard(slot))
    }
}

impl Drop for PublishGuard {
    fn drop(&mut self) {
        PUBLISHING[self.0].store(false, Ordering::Release);
    }
}

/// Arm the hardware write-watch on an IPI ring's `size` word, so the next
/// store into it traps with the writer's `rip`.
///
/// This word is the best probe in the kernel for the corruption this hunt is
/// chasing, for one reason: **its correct value is a compile-time constant**.
/// `size` is set once in `MpscQueue::new` to `REASON_SIZE` and never written
/// again, so unlike a name buffer or a stack slot there is no legitimate store
/// to filter out. Any hit is the bug.
///
/// It is also the probe that fires most: `entry_at` has caught this word
/// wrong on most recent boots, and what it holds is telling —
///
///     len=0xffffff00218688e0  size=0xffffff00006567c1   (two kernel pointers)
///     len=18446742974756925680  size=18446742974756926704  (a pair 1024 apart)
///     len=6  size=0
///
/// — foreign data, not a single stray byte. A `Vec` that is allocated once by
/// a `lazy_static` and never freed cannot be written by its owner, so either
/// something writes wild, or the allocator handed this block out twice. The
/// two look identical in a post-mortem and completely different in a trap:
/// the `rip` says whether the writer thought it owned the memory.
///
/// Costs nothing until it fires (the CPU checks DR0 in hardware), reports
/// through the existing `[watchpoint]` path, and self-disarms after a few hits
/// so it cannot storm the console.
pub fn arm_queue_watch() -> bool {
    // `&...size`, not the struct address: `MpscQueue` is `repr(Rust)` and the
    // compiler is free to put `size` anywhere in it.
    let Some(q) = ipi_queue(0) else {
        return false;
    };
    let addr = &q.size as *const usize as usize;
    crate::watchpoint::watch_write(addr, 8)
}

/// Drain this CPU's IPI queue and report what was in it.
///
/// This is an architecture's IPI *handler*, reached straight out of the
/// interrupt vector, so it is a shootdown receive path and not merely a
/// reporting one. It used to be `consume_entrys()` and nothing else: the
/// entries were collected, `chead` was advanced to `ptail`, and the caller
/// logged them. That is exactly the state [`drain_and_ack_on`] calls unsound
/// and goes out of its way to make impossible -- **entries consumed while no
/// watermark is published**. A `TlbShutdown` eaten here never reaches a
/// `flush_tlb`, so the CPU keeps the stale mapping; and because the queue then
/// reads empty, no later ack, pump or NMI kick has anything to service, so the
/// initiator's wait (which has no timeout, by design) never ends. riscv's
/// `super_soft` handler was wired to this, so on riscv every cross-CPU
/// shootdown was a guaranteed hang on top of a TLB that was never flushed.
///
/// It drains through the same path as every other ack now, and only reports
/// on the way past.
pub(crate) fn ipi_reason() -> Vec<usize> {
    let me = crate::cpu::cpu_id() as usize;
    // An id past the table has no queue to drain, so there is nothing to
    // report. Every other accessor here already returns rather than index; this
    // one used to index, and it runs straight out of the IPI vector.
    let Some(me) = shootdown_self(me) else {
        return Vec::new();
    };
    ipi_reason_on(me)
}

/// [`ipi_reason`] for a known dense logical CPU id.
fn ipi_reason_on(me: usize) -> Vec<IpiEntry> {
    let mut out = Vec::new();
    drain_and_ack_on(me, Some(&mut out));
    out
}

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Master switch for application-processor (SMP) bring-up. Default **ON**;
/// `smp=off` on the kernel cmdline forces a single-core boot.
///
/// This defaulted off while multi-core wedged real hardware at the hand-off to
/// the scheduler (boot reached 100% and stopped). The cause was the APIC id
/// plumbing, not the scheduler: `LocalApicBuilder` switches the LAPIC into
/// x2APIC mode on any CPU that advertises support for it — every real x86 since
/// roughly 2008, and *not* QEMU's default TCG CPU, which is why only physical
/// machines were affected. In that mode the LAPIC stops decoding its MMIO page,
/// but `kernel-sync` still read the APIC id from that window, so every CPU
/// resolved to the same bogus id and the APs shared one per-CPU slot. Those
/// reads now go through the MSR interface (see `kernel-sync::interrupt` and
/// `drivers::irq::x86_apic::lapic`), so the ids are distinct again.
///
/// `smp=off` stays as the escape hatch for bringing a suspect machine up
/// single-core without a rebuild.
static SMP_ENABLED: AtomicBool = AtomicBool::new(true);

/// Override AP bring-up (called from `zCore` when `smp=off` is on the cmdline).
pub fn set_smp_enabled(v: bool) {
    SMP_ENABLED.store(v, Ordering::Relaxed);
}

/// Whether AP bring-up is enabled. Read by `start_application_processors`.
pub fn smp_enabled() -> bool {
    SMP_ENABLED.load(Ordering::Relaxed)
}

/// Bitmask of logical CPU ids that are actually online and able to service
/// IPIs. The BSP (logical 0) is always online; APs OR in their bit once they
/// reach `secondary_init`. Shootdowns only target online CPUs — APs that failed
/// to start (partial SMP bring-up) must not be signalled.
static CPU_ONLINE: AtomicU64 = AtomicU64::new(1);

/// Mark a logical CPU id as online (called from each CPU's bring-up path).
pub fn mark_cpu_online(logical_id: usize) {
    if logical_id < MAX_CORE_NUM {
        CPU_ONLINE.fetch_or(1u64 << logical_id, Ordering::Release);
    }
}

/// Bitmask of CPUs that reached `secondary_init` (BSP bit 0 always set).
/// Prefer this over `(1 << cpu_count()) - 1`, which includes APs that never
/// came online.
pub fn cpu_online_mask() -> u64 {
    CPU_ONLINE.load(Ordering::Acquire)
}

/// Number of logical CPUs that actually came online (BSP + every AP that
/// reached `secondary_init`). May be less than the detected/configured CPU
/// count when SMP bring-up is partial — useful for accounting that must not
/// divide by cores that never ran (e.g. the `/proc/perf` busy% denominator,
/// which would otherwise count a never-started AP as 100% busy).
pub fn online_cpu_count() -> usize {
    cpu_online_mask().count_ones() as usize
}

/// Whether a bare reschedule kick for dense logical CPU `cpuid` is worth an
/// interrupt.
///
/// A wake kick carries no payload: it exists only so the target looks at its
/// run queue now instead of at its next 250 Hz tick, and the receiving side is
/// the ordinary IPI handler finding nothing to drain. So, unlike a shootdown,
/// nothing is lost by not sending one — and three ids are never worth the
/// interrupt:
///
///  * one past the per-CPU tables, which names no CPU at all. `send_ipi`
///    refuses such an id on all three architectures; the wake path had no
///    notion of it, and every architecture's delivery ends in a `1 << id`
///    (a hart mask, a GICv2 target list) that is not a no-op past the word.
///  * our own. `send_wake_ipi(me)` is only ever reached from an interrupt
///    handler on this CPU — we are executing, so we reach the scheduler
///    without an interrupt, and the one the LAPIC would deliver to itself is
///    pure cost. The scheduler already skips the self-kick on one of its two
///    paths (`request_resched`) and not on the other
///    (`maybe_send_resched_ipi`), which is exactly the kind of difference this
///    predicate exists to end.
///  * one that never came online. It is running no task, so there is nothing
///    on it to wake; `remote_flush_tlb_on` already refuses to wait on such a
///    CPU for the same reason.
///
/// Deliberately [`cpu_online_mask`] and not [`IPI_READY`]: the narrower set is
/// published from the executor's own entry path, and a kick exists precisely
/// to reach a CPU sitting in that loop — gating on it would make the kick
/// depend on the thing it is trying to poke.
pub fn wake_kick_wanted(cpuid: usize) -> bool {
    cpuid < MAX_CORE_NUM
        && cpuid != crate::cpu::cpu_id() as usize
        && cpu_online_mask() & (1u64 << cpuid) != 0
}

/// Bitmask of CPUs that are actually *servicing* IPIs: running the executor
/// loop with interrupts enabled, so a TLB-shootdown IPI to them will be taken
/// and acknowledged promptly.
///
/// This is deliberately narrower than [`CPU_ONLINE`]. An AP is marked online in
/// `secondary_init` but then spins on the boot `STARTED` flag with interrupts
/// DISABLED until the BSP has spawned init — during which it cannot ack. Waiting
/// on such a CPU would stall *every* shootdown the BSP issues while it spawns
/// init (the heavy fork/exec/unmap burst) until the spin budget runs out, which
/// looks like a hang. A not-yet-ready CPU runs no user process, so it holds no
/// user TLB entry worth flushing — skipping it is safe.
static IPI_READY: AtomicU64 = AtomicU64::new(0);

/// Page-table root (CR3 frame) each CPU currently has loaded, or 0 = unknown.
///
/// Written by `activate_paging` BEFORE the hardware switch: a flusher that
/// reads the OLD token and skips the switching CPU is still correct, because
/// the CR3 write it is racing flushes every non-global entry anyway. Written
/// as the frame base (low 12 bits masked) so flag bits at the call sites
/// cannot break the comparison.
static ACTIVE_VMTOKEN: [core::sync::atomic::AtomicUsize; MAX_CORE_NUM] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; MAX_CORE_NUM];

/// Record that this CPU is about to load `token` (a page-table root).
pub fn note_active_vmtoken(token: usize) {
    let me = crate::cpu::cpu_id() as usize;
    if me < MAX_CORE_NUM {
        ACTIVE_VMTOKEN[me].store(token & !0xfff, Ordering::SeqCst);
    }
}

/// Mark this CPU as ready to service TLB-shootdown IPIs. Called once, when the
/// CPU enters its executor loop with interrupts enabled.
pub fn mark_cpu_ipi_ready(logical_id: usize) {
    if logical_id < MAX_CORE_NUM {
        IPI_READY.fetch_or(1u64 << logical_id, Ordering::Release);
    }
}

/// Per-CPU TLB-shootdown acknowledgement watermark: the queue index (the
/// drain's consumed `ptail`) this CPU has flushed up to. An initiator records
/// its target-queue `ptail` right after enqueueing its request and waits for
/// this watermark to REACH it — proof the target flushed after consuming that
/// very request, not merely that "some" drain completed (the plain counter
/// this used to be had exactly that TOCTOU; see the publish site in
/// `tlb_shootdown_ack`).
#[allow(clippy::declare_interior_mutable_const)]
const ZERO_SEQ: AtomicU64 = AtomicU64::new(0);
static SHOOTDOWN_SEQ: [AtomicU64; MAX_CORE_NUM] = [ZERO_SEQ; MAX_CORE_NUM];

/// Per-CPU "an ack is in flight on this CPU right now" flag, set for the
/// duration of [`tlb_shootdown_ack`]. Read ONLY by the NMI-driven ack
/// ([`tlb_shootdown_ack_nmi`]): if a normal pump/IRQ ack is already draining
/// this CPU's queue, the NMI must NOT re-enter it (that would double-drain),
/// so it skips and lets the in-flight ack finish. When the flag is clear the
/// CPU is wedged somewhere *other* than an ack (a fault storm, an IRQs-off
/// busy-wait) and the NMI is the only thing that can service the queue for it.
#[allow(clippy::declare_interior_mutable_const)]
const FALSE_FLAG: AtomicBool = AtomicBool::new(false);
static SHOOTDOWN_ACK_ACTIVE: [AtomicBool; MAX_CORE_NUM] = [FALSE_FLAG; MAX_CORE_NUM];

/// [diag] Per-CPU "I am spin-waiting for these CPUs to ack my shootdown" mask,
/// published live by [`remote_flush_tlb_aspace`]'s wait loop (0 = not waiting).
/// The deadlock banner reads it: a lock HOLDER that is *also* here is the
/// convoy's head — it is stuck because the CPUs in its mask never acked (a
/// non-pumping IRQs-off spinner, e.g. deep in vendor RM MMIO polling), NOT
/// because of a lock-ordering cycle. That single bitmask is what tells an
/// on-screen-only (no serial) hardware capture "shootdown starvation" apart
/// from "AB-BA", and names the CPU to go look at.
static SHOOTDOWN_WAIT_MASK: [AtomicU64; MAX_CORE_NUM] = [ZERO_SEQ; MAX_CORE_NUM];

/// The set of CPUs `cpu` is currently blocked waiting on for a TLB-shootdown
/// ack, or 0 if it is not in a shootdown wait. Racy by nature — diagnostics.
pub fn shootdown_wait_mask(cpu: usize) -> u64 {
    if cpu < MAX_CORE_NUM {
        SHOOTDOWN_WAIT_MASK[cpu].load(Ordering::Relaxed)
    } else {
        0
    }
}

/// [diag] Per-(waiter, target) ack goal published by the wait loop while it
/// spins (0 = not waiting on that target). Together with [`shootdown_seq_of`]
/// and [`shootdown_queue_state`], the deadlock banner can print the live
/// protocol state of a starved shootdown — which invariant is broken, not
/// just where the CPUs are parked.
#[allow(clippy::declare_interior_mutable_const)]
const GOAL_ROW: [AtomicU64; MAX_CORE_NUM] = [ZERO_SEQ; MAX_CORE_NUM];
static SHOOTDOWN_GOAL: [[AtomicU64; MAX_CORE_NUM]; MAX_CORE_NUM] = [GOAL_ROW; MAX_CORE_NUM];

/// [diag] The ack goal `waiter` is holding `target` to, or 0 when idle.
pub fn shootdown_goal(waiter: usize, target: usize) -> u64 {
    if waiter < MAX_CORE_NUM && target < MAX_CORE_NUM {
        SHOOTDOWN_GOAL[waiter][target].load(Ordering::Relaxed)
    } else {
        0
    }
}

/// [diag] `cpu`'s published shootdown-flush watermark.
pub fn shootdown_seq_of(cpu: usize) -> u64 {
    if cpu < MAX_CORE_NUM {
        SHOOTDOWN_SEQ[cpu].load(Ordering::Relaxed)
    } else {
        0
    }
}

/// [diag] `cpu`'s IPI-queue counters and in-flight-ack flag:
/// `(chead, ptail, phead, ack_active, overflow_bit)`.
pub fn shootdown_queue_state(cpu: usize) -> (usize, usize, usize, bool, bool) {
    if cpu >= MAX_CORE_NUM {
        return (0, 0, 0, false, false);
    }
    let Some(q) = ipi_queue(cpu) else {
        return (0, 0, 0, false, false);
    };
    (
        q.chead(),
        q.ptail(),
        q.phead(),
        SHOOTDOWN_ACK_ACTIVE[cpu].load(Ordering::Relaxed),
        overflow_pending(cpu),
    )
}

/// How many payloads for this CPU could not be published, and how many of
/// those a drain has covered with a full flush. `GEN > ACK` **is** the demand
/// for a full flush; there is no separate flag.
///
/// A sender that cannot publish (queue full, lost commit race, or a publish
/// already in flight on its own CPU) has no way to say *which* page to
/// invalidate, so the target's next drain must full-flush instead. That demand
/// cannot ride the queue: an overflow advances no queue index, so
/// `SHOOTDOWN_SEQ >= ptail` can be satisfied by a drain that predates the
/// dropped request — the initiator would free frames the target still maps.
/// Initiators that saw the generation move wait on [`IPI_OVERFLOW_ACK`]
/// instead.
///
/// **One counter pair, and deliberately no second flag.** This used to be a
/// generation *and* an `IPI_QUEUE_OVERFLOW` bitmask, written by one note in
/// two separate atomic operations — the bit first, the generation second — and
/// read by a drain in the same two steps. A drain landing between them saw the
/// bit, read the *old* generation, full-flushed and acknowledged that old
/// value; the sender then read the new generation and waited for an
/// acknowledgement of it. Nothing was left to produce one: the bit had been
/// consumed, so the next drain saw no overflow at all and published nothing,
/// and a re-kick that fits in the queue notes no overflow either. On x86_64
/// the NMI rung of [`remote_flush_tlb_on`]'s ladder eventually rescued it,
/// after ~1M spins; [`nmi_kick_pending_targets`] is empty on riscv and
/// aarch64, so there the initiator waits for an acknowledgement that can never
/// arrive — and that wait has no timeout, by design. Deriving the demand from
/// the counters makes the note a single atomic operation, so the window does
/// not exist and no future reader can get the two halves out of step.
static IPI_OVERFLOW_GEN: [AtomicU64; MAX_CORE_NUM] = [ZERO_SEQ; MAX_CORE_NUM];
static IPI_OVERFLOW_ACK: [AtomicU64; MAX_CORE_NUM] = [ZERO_SEQ; MAX_CORE_NUM];

/// Whether `cpu` still owes a full flush for a payload that never reached its
/// queue. The one place this question is answered, for the peek, the pump, the
/// NMI rescue, the initiator's self-pump and the diagnostics alike.
fn overflow_pending(cpu: usize) -> bool {
    cpu < MAX_CORE_NUM
        && IPI_OVERFLOW_GEN[cpu].load(Ordering::Acquire)
            > IPI_OVERFLOW_ACK[cpu].load(Ordering::Acquire)
}

/// Note that `cpuid`'s IPI queue dropped an entry (called by the arch sender).
pub fn note_ipi_queue_overflow(cpuid: usize) {
    if cpuid < MAX_CORE_NUM {
        IPI_OVERFLOW_GEN[cpuid].fetch_add(1, Ordering::Release);
    }
}

/// Call `f` once per set bit in `mask` (logical CPU ids 0..63).
fn for_each_cpu(mask: u64, mut f: impl FnMut(usize)) {
    let mut bits = mask;
    while bits != 0 {
        let cpu = bits.trailing_zeros() as usize;
        bits &= bits - 1;
        f(cpu);
    }
}

/// Max shootdown entries serviced precisely (per-page `invlpg`) in one ack;
/// beyond this a full flush is cheaper than the invlpg sequence.
const MAX_PRECISE_SHOOTDOWN: usize = 8;

/// Receiver side of the TLB shootdown.
///
/// Drains the queue and services the requests:
///  * empty drain and no overflow → **pure wake** (a reschedule kick): return
///    without touching the TLB and *without* bumping the ack sequence, so a
///    shootdown initiator can never mistake a wake for a completed flush.
///  * a few well-formed per-page requests → `invlpg` each (user PTEs are
///    never GLOBAL here, so a targeted invalidation fully covers the request
///    on x86; see `vm.rs`).
///  * anything else (overflow flag, full-flush sentinel `vpn == 0`, or too
///    many entries) → one full flush, the previous behaviour.
///
/// Spin-loop pump: drain this CPU's pending shootdown queue if — and only if —
/// there is something in it. One queue-pointer compare when idle, so it is
/// cheap enough to call every few hundred spins from inside a held-IRQs-off
/// spin loop (see kernel-sync's `set_spin_pump`).
pub fn tlb_shootdown_pump() {
    let me = crate::cpu::cpu_id() as usize;
    if me >= MAX_CORE_NUM || IPI_READY.load(Ordering::Relaxed) & (1u64 << me) == 0 {
        return;
    }
    let Some(q) = ipi_queue(me) else {
        return;
    };
    if q.chead() == q.ptail() && !overflow_pending(me) {
        return;
    }
    tlb_shootdown_ack();
}

pub fn tlb_shootdown_ack() {
    let me = crate::cpu::cpu_id() as usize;
    tlb_shootdown_ack_on(me);
}

/// Drain+ack for a known dense logical CPU id. The NMI path passes an
/// APIC-resolved id so a transient user GSBASE cannot publish the watermark
/// into the wrong slot (see [`tlb_shootdown_ack_nmi`]).
fn tlb_shootdown_ack_on(me: usize) {
    drain_and_ack_on(me, None)
}

/// The drain itself, optionally reporting the entries it consumed.
///
/// `out` is `None` on every interrupt-time path, which keeps the drain
/// allocation-free: it runs from a ticket lock's spin pump with that lock held
/// and interrupts off, where a heap allocation is a lock-ordering hazard. It
/// is `Some` only for [`ipi_reason`], whose caller wants to log what arrived
/// -- and which must not have a second, ack-less way to empty this queue.
fn drain_and_ack_on(me: usize, mut out: Option<&mut Vec<IpiEntry>>) {
    if me >= MAX_CORE_NUM {
        return;
    }
    // Pure-wake peek BEFORE arming the drain flag: an empty queue with no
    // overflow bit needs no drain at all, so returning here keeps the
    // ACK_ACTIVE window closed for the overwhelmingly common reschedule-kick
    // case — every avoided arm is one fewer instant in which an NMI-driven
    // rescue would have found the flag set. Peek only (the overflow bit is
    // NOT consumed here); the flagged path below re-checks after consuming.
    {
        let Some(q) = ipi_queue(me) else {
            return;
        };
        if q.chead() == q.ptail() && !overflow_pending(me) {
            return;
        }
    }
    // Publish that a drain is in flight on this CPU, and clear it on every exit
    // (the guard's Drop), so an NMI-driven ack landing mid-drain skips instead
    // of double-consuming the queue. Set AFTER the range check so `me` is valid.
    SHOOTDOWN_ACK_ACTIVE[me].store(true, Ordering::SeqCst);
    let _ack_active = AckActiveGuard(me);
    // Read the overflow generation BEFORE draining, and acknowledge exactly
    // the value read, at the end. Any sender that bumped it did so before
    // ringing the IPI, so either we see it here or its interrupt is still
    // pending and the NEXT ack handles it — and a note that lands while this
    // drain runs leaves the counter above this snapshot, so it stays pending
    // for the next drain instead of being acknowledged by a flush that
    // predates it.
    let ovf_seen = IPI_OVERFLOW_GEN[me].load(Ordering::Acquire);
    let overflow = ovf_seen > IPI_OVERFLOW_ACK[me].load(Ordering::Acquire);
    let ovf_gen = if overflow { ovf_seen } else { 0 };
    // Non-allocating bounded drain of this CPU's queue (single consumer).
    let Some(q) = ipi_queue(me) else {
        return;
    };
    let mut vpns = [0usize; MAX_PRECISE_SHOOTDOWN];
    let mut n_vpns = 0usize;
    let mut precise = true;
    // Only TlbShutdown / overflow may bump SHOOTDOWN_SEQ. MockBlock (and other
    // non-TLB reasons) used to demote to a full flush *and* bump seq, which
    // let a concurrent shootdown initiator observe a false ack and free frames
    // while our TLB still held the stale mapping.
    let mut saw_tlb = overflow;
    let chead = q.chead();
    let ptail = q.ptail();
    for idx in chead..ptail {
        let entry = *q.entry_at(idx);
        if let Some(v) = out.as_mut() {
            v.push(entry);
        }
        match IpiReason::from(entry) {
            IpiReason::TlbShutdown { vpn } if vpn != 0 => {
                saw_tlb = true;
                if n_vpns < MAX_PRECISE_SHOOTDOWN {
                    vpns[n_vpns] = vpn;
                    n_vpns += 1;
                } else {
                    precise = false;
                }
            }
            IpiReason::TlbShutdown { vpn: 0 } => {
                // Full-flush sentinel.
                saw_tlb = true;
                precise = false;
            }
            // Non-TLB payload (e.g. MockBlock): drain it so it cannot block
            // the queue, but do NOT treat it as a shootdown ack.
            _ => {}
        }
    }
    // No second pure-wake check here: the peek above already returned for an
    // empty queue with no overflow bit, and neither condition can have become
    // true since. `chead` moves only by this CPU's own drain (single
    // consumer), `ptail` only grows, and the overflow demand was captured
    // into `ovf_gen` rather than cleared — it is cleared by the
    // acknowledgement at the bottom, after the flush that honours it. A guard
    // nothing can reach is a guard nothing can test.
    // Consume exactly the snapshot we serviced — never past it. Entries that
    // commit after `ptail` was read stay queued for the ack their own IPI
    // triggers (the old `discard_entrys()` jumped to the *current* tail, which
    // could drop a just-committed request whose PTE change our flush above
    // predates). CAS so a nested ack (IRQ landing inside the initiator's
    // self-pump) that already advanced the head is never rewound.
    let _ = q
        .chead
        .compare_exchange(chead, ptail, Ordering::AcqRel, Ordering::Relaxed);
    if !saw_tlb {
        // Drained only non-TLB reasons. The old behaviour (advance chead,
        // publish nothing) left a liveness hole: if a TlbShutdown entry is
        // ever consumed here without being recognized (any decode/race bug,
        // now or future), the queue reads empty forever after, every later
        // ack and NMI kick no-ops, and the initiator spins until the 8s
        // deadlock detector kills the machine — the real-hardware
        // "shootdown starvation" panics. Consuming entries while publishing
        // no watermark is exactly the unsound state; make it impossible:
        // full-flush and publish the consumed index. Cost: one extra full
        // flush on the rare non-TLB-only drain; safety: a full flush always
        // over-satisfies whatever the consumed entries asked.
        //
        // It falls through to the one publish below rather than carrying its
        // own copy. The copy it used to carry acknowledged the overflow
        // generation as well — which this branch can never have, since
        // `saw_tlb` starts out as `overflow` — and it published the two
        // watermarks in the opposite order to the copy below. A mutation that
        // deleted that acknowledgement altogether was caught by no test,
        // which is how the branch was found: a guard nothing can reach is a
        // guard nothing can test.
        crate::vm::flush_tlb(None);
    } else if precise && !overflow {
        for &vpn in &vpns[..n_vpns] {
            crate::vm::flush_tlb(Some(vpn << 12));
        }
    } else {
        crate::vm::flush_tlb(None);
    }
    // Publish the completed flush LAST (Release) so an initiator that observes
    // it is guaranteed our TLB is already clean.
    //
    // The published value is the queue index this drain CONSUMED UP TO — not a
    // plain +1 counter. A "+1 means acked" protocol had a real TOCTOU, caught
    // live by the fork-hammer's torn-page failures: a drain already in flight
    // when the initiator's entry was enqueued (its `ptail` snapshot taken just
    // before the commit) finishes, flushes only the OLDER pages, and bumps the
    // counter — the initiator mistakes that for its own ack and returns while
    // its entry is still queued and the target's TLB still holds the stale
    // writable entry for MICROSECONDS more. A hot writer thread on that CPU
    // keeps storing into a frame a fork child now shares — the 3-generation
    // torn pages. Publishing the consumed index makes the ack unambiguous:
    // the initiator waits for `SHOOTDOWN_SEQ >= the ptail it observed right
    // after its own enqueue`, which no earlier drain can satisfy. fetch_max
    // because the pump/IRQ/NMI paths race benignly (drains are serialized by
    // SHOOTDOWN_ACK_ACTIVE, but keep the publish monotone regardless).
    SHOOTDOWN_SEQ[me].fetch_max(ptail as u64, Ordering::Release);
    if ovf_gen != 0 {
        IPI_OVERFLOW_ACK[me].fetch_max(ovf_gen, Ordering::Release);
    }
}

/// Clears [`SHOOTDOWN_ACK_ACTIVE`] on scope exit, covering every early return in
/// [`tlb_shootdown_ack`].
struct AckActiveGuard(usize);
impl Drop for AckActiveGuard {
    fn drop(&mut self) {
        SHOOTDOWN_ACK_ACTIVE[self.0].store(false, Ordering::SeqCst);
    }
}

/// Service this CPU's pending shootdown from the NMI handler.
///
/// A normal 0xf3 IPI reaches a CPU only when it takes interrupts. A CPU wedged
/// with IRQs off — deep in a fault storm, a non-pumping busy-wait, or corrupt
/// code — never does, so it starves any peer waiting on its ack, and that wait
/// has no timeout (correctness > latency). An NMI is delivered regardless; this
/// is what it runs, the same non-allocating, lock-free drain as the pump path.
///
/// NMI-safe: it takes no locks, allocates nothing, and prints nothing (a print
/// would deadlock against a console lock the interrupted code may hold). It
/// skips when a normal ack is already draining this CPU's queue (the guard
/// flag) so it never double-consumes the single-consumer queue, and no-ops when
/// nothing is pending. NMIs do not nest, so the flag check is race-free here.
pub fn tlb_shootdown_ack_nmi() {
    // Never trust GS here: see `lock::current_cpu_id_via_apic`. Publishing the
    // watermark into the wrong per-CPU slot is indistinguishable from "NMI
    // ran but SHOOTDOWN_SEQ never moved" — the surviving field hypothesis.
    let me = {
        #[cfg(all(target_os = "none", any(target_arch = "x86", target_arch = "x86_64")))]
        {
            lock::current_cpu_id_via_apic() as usize
        }
        #[cfg(not(all(target_os = "none", any(target_arch = "x86", target_arch = "x86_64"))))]
        {
            crate::cpu::cpu_id() as usize
        }
    };
    let Some(me) = shootdown_self(me) else {
        return;
    };
    tlb_shootdown_ack_nmi_on(me)
}

/// [`tlb_shootdown_ack_nmi`] for a known dense logical CPU id.
fn tlb_shootdown_ack_nmi_on(me: usize) {
    let Some(q) = ipi_queue(me) else {
        return;
    };
    // The single-consumer drain may only run when no other drain is in flight
    // on this CPU (the NMI may have interrupted a pump/IRQ ack mid-queue).
    if !SHOOTDOWN_ACK_ACTIVE[me].load(Ordering::SeqCst)
        && (q.chead() != q.ptail() || overflow_pending(me))
    {
        tlb_shootdown_ack_on(me);
        // Reinforcement publish: the drain's watermark is its consumed
        // snapshot, which can trail entries committed while it ran. We were
        // NMI-kicked because a peer is starving on this CPU — leave nothing
        // to interpretation: one full flush covers everything enqueued up to
        // now, and then the current tail is soundly publishable.
        // Snapshot BOTH watermarks before the flush, for the one reason the
        // overflow line below already gives: what is published has to be what
        // the flush covered. Reading `ptail` *after* the flush published an
        // index that could include entries committed in between -- requests
        // this flush predates, reported as serviced. The initiator then frees
        // the frame while this CPU still holds the mapping, which is the
        // corruption the whole consumed-index protocol exists to rule out.
        let ovf_snap = IPI_OVERFLOW_GEN[me].load(Ordering::Acquire);
        let tail_snap = q.ptail() as u64;
        crate::vm::flush_tlb(None);
        IPI_OVERFLOW_ACK[me].fetch_max(ovf_snap, Ordering::Release);
        SHOOTDOWN_SEQ[me].fetch_max(tail_snap, Ordering::Release);
        return;
    }
    // Every other state — queue looks empty, or a drain is in flight so the
    // queue must not be touched — gets the unconditional rescue. An NMI kick
    // only ever fires after a peer starved on THIS CPU's watermark for ~1M
    // spins, and each previously-allowed silent return here was one more way
    // for that starvation to persist until the 8s deadlock detector killed
    // the machine (four real-hardware panics with this signature: non-ackers
    // parked in unrelated code, queues clean or a drain nominally in flight,
    // the HOLDER waiting forever). A full local flush over-satisfies every
    // request enqueued up to this instant, so publishing the current tail
    // afterwards is sound no matter what ate the entry or what a concurrent
    // drain is doing (its invlpgs become redundant; fetch_max keeps the
    // watermark monotone). Every waiter's goal is <= the tail it observed at
    // send time <= the tail published here, so the starvation ends on this
    // very NMI. NMI-safe: no locks, no allocation, queue untouched.
    // Snapshot overflow gen AND the tail BEFORE the flush so a request that
    // races in after the INVLPG is not acknowledged as already done.
    let ovf_snap = IPI_OVERFLOW_GEN[me].load(Ordering::Acquire);
    let tail_snap = q.ptail() as u64;
    crate::vm::flush_tlb(None);
    IPI_OVERFLOW_ACK[me].fetch_max(ovf_snap, Ordering::Release);
    SHOOTDOWN_SEQ[me].fetch_max(tail_snap, Ordering::Release);
}

/// Whether this architecture has a kick that reaches a CPU with interrupts
/// disabled — the top rung of [`remote_flush_tlb_on`]'s escalation ladder.
///
/// **x86_64 only.** The rungs below it (re-send the IPI, pump our own queue)
/// all need the target to be *able* to take an interrupt; this one exists for
/// the target that is not. On riscv a supervisor software interrupt is masked
/// by `sstatus.SIE` like any other, and on aarch64 the shootdown SGI is a
/// Group 1 interrupt masked by `DAIF.I` — and `intr_off` sets exactly that
/// bit. Neither has anything unmaskable wired up today.
///
/// That is worth naming rather than leaving as an empty function, because the
/// `slow ack wait` line is read as "not even the NMI got through" when on two
/// of the three architectures the honest reading is "the last rung was never
/// there". On aarch64 the one unmaskable path that *does* exist is FIQ:
/// `intr_off` never touches `DAIF.F`, and the vector table already has its
/// four FIQ entries, so routing the shootdown SGI to GIC Group 0 with
/// `GICC_CTLR.FIQEn` would give that architecture the rung it is missing. Not
/// done here: it is a GIC and vector-table change that nothing available can
/// boot-test.
pub const HAS_UNMASKABLE_KICK: bool = cfg!(all(target_arch = "x86_64", target_os = "none"));

/// Broadcast an NMI to every other CPU so a wedged target services its pending
/// shootdown ([`tlb_shootdown_ack_nmi`]). x86_64/bare only; a no-op elsewhere
/// (see [`HAS_UNMASKABLE_KICK`]). Healthy CPUs no-op the ack, so the broadcast
/// only actually helps the stuck one; a targeted NMI would need a new
/// low-level APIC entry point and buys nothing here, where this runs only once
/// a shootdown is already starving.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn nmi_kick_pending_targets() {
    zcore_drivers::irq::x86::Apic::send_nmi_all_others();
}
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
fn nmi_kick_pending_targets() {}

/// One spin in `2^REKICK_SHIFT` re-sends the shootdown to the targets still
/// pending, for the target that took its IPI as a pure wake before our queue
/// entry was visible and will never be told again.
const REKICK_SHIFT: u32 = 16;

/// One spin in `2^UNMASKABLE_SHIFT` escalates to the kick that does not need
/// the target to be able to take an interrupt. Deliberately 16x rarer than the
/// re-kick, so a plain lost wakeup is handled by the cheap rung first.
const UNMASKABLE_SHIFT: u32 = 20;

const _: () = assert!(
    UNMASKABLE_SHIFT > REKICK_SHIFT,
    "a lost wakeup is likelier than a wedged CPU: the cheap rung must get \
     several tries before the expensive one fires at all"
);

/// Whether the ack wait should re-send the shootdown at this spin count.
fn should_rekick(spins: u64) -> bool {
    spins != 0 && spins & ((1u64 << REKICK_SHIFT) - 1) == 0
}

/// Whether the ack wait should escalate past a maskable interrupt at this spin
/// count. Always `false` where there is nothing to escalate *to*: firing an
/// empty function on a schedule is not an escalation, and reading the code as
/// if it were is how "the NMI did not help either" gets written about a
/// machine that never sent one.
fn should_escalate(spins: u64, unmaskable: bool) -> bool {
    unmaskable && spins != 0 && spins & ((1u64 << UNMASKABLE_SHIFT) - 1) == 0
}

/// Cross-CPU TLB shootdown.
///
/// x86 `flush_tlb` only invalidates the *local* CPU's TLB. Without this, after
/// one CPU unmaps/reprotects a page (COW copy-break, munmap, address-space
/// teardown) the other CPUs keep stale TLB entries pointing at the now-freed
/// physical frame; once it is reallocated to another VMO/process those entries
/// read/write the wrong owner's memory — the cross-process and kernel↔user
/// corruption that only shows up under SMP load.
///
/// Synchronous. The initiator waits for every signalled CPU to acknowledge the
/// flush (so the freed frame cannot be reused while a stale entry still points
/// at it), with:
///
///  * **Self-pump.** While waiting we service our OWN pending shootdowns, so two
///    CPUs that signal each other at the same instant cannot deadlock waiting on
///    each other's ack.
///  * **Long bounded wait** with repeated self-pump. We do **not** treat idle
///    targets as acked (TOCTOU with wake→user) and we do **not** silently
///    fire-and-forget after a short budget — that was the COW/TLB corruption
///    class under SMP. Ticket-lock spin-pump covers the common IRQs-off case.
///
/// `vaddr`, when given, is delivered to each target so its ack can `invlpg`
/// just that page instead of flushing its whole TLB; `None` (or a dropped
/// queue entry) demotes the ack to a full flush.
pub fn remote_flush_tlb(vaddr: Option<usize>) {
    remote_flush_tlb_aspace(vaddr, None)
}

/// [`remote_flush_tlb`] with an optional address-space filter.
///
/// `aspace = Some(root)`: only CPUs whose ACTIVE page-table root is `root` (or
/// unknown) are targeted. Sound without PCID because a CR3 write flushes every
/// non-global entry: a CPU that switched away from this address space has no
/// stale user entries left to invalidate, and a CPU switching TO it publishes
/// its token before the CR3 write (see `note_active_vmtoken`). Kernel-table
/// flushes pass `None` and keep targeting everyone — global-bit entries
/// survive CR3 writes, so no CPU can be filtered out for those.
///
/// Under a fork/exec-heavy parallel load this is most of the win: unrelated
/// processes on other CPUs stop taking (and stop having to ack) IPIs for
/// address spaces they have never loaded.
pub fn remote_flush_tlb_aspace(vaddr: Option<usize>, aspace: Option<usize>) {
    let me = crate::cpu::cpu_id() as usize;
    // An id that names no CPU is not ours to shift by or index with. Every
    // other entry point in this module already refuses one -- `ipi_queue` is
    // bounds-checked for exactly this reason, and `tlb_shootdown_ack_on`,
    // `mark_cpu_online`, `mark_cpu_ipi_ready` and every diagnostic getter
    // re-check it. This one, the busiest of them, did not: `1u64 << me` is
    // undefined past 63 and on x86 wraps to bit `me % 64`, so the target mask
    // excluded some *other* CPU and kept US in it as a phantom target -- the
    // `spins=… targets=0x1 me=2` wedge the single-core case below describes,
    // but with no single-core short-circuit to catch it. Then
    // `SHOOTDOWN_WAIT_MASK[me]` and `SHOOTDOWN_GOAL[me][cpu]` index straight
    // into 64-entry tables and panic, in a spin loop with the caller's lock
    // held, where a panic cannot be contained and buries its own cause.
    //
    // A corrupt per-CPU identity is a documented failure here, not a
    // hypothetical: see [`SMP_ENABLED`] for the x2APIC window in which every
    // CPU reported the same bogus id on real hardware. A full local flush
    // over-satisfies whatever this call was asking for; the peers keep their
    // mappings, the same exposure as a send that could not be delivered.
    let Some(me) = shootdown_self(me) else {
        crate::vm::flush_tlb(None);
        return;
    };
    remote_flush_tlb_on(me, vaddr, aspace)
}

/// The dense logical id to run the shootdown protocol as, or `None` when
/// `cpu_id()` handed back something that names no CPU.
///
/// The bound is what every `1u64 << cpu` and every per-CPU table in this
/// module assumes, so it is one predicate rather than a repeated comparison.
fn shootdown_self(raw: usize) -> Option<usize> {
    (raw < MAX_CORE_NUM).then_some(raw)
}

/// [`remote_flush_tlb_aspace`] for a known dense logical CPU id, split out for
/// the same reason [`tlb_shootdown_ack_on`] is: the id must be checked once,
/// by the caller that obtained it, and the protocol below must be reachable
/// with an id chosen by the test rather than by `cpu_id()`.
pub(crate) fn remote_flush_tlb_on(me: usize, vaddr: Option<usize>, aspace: Option<usize>) {
    debug_assert!(me < MAX_CORE_NUM);
    // Single-core short-circuit — and a corrupt-`cpu_id` safety net.
    //
    // On a uniprocessor boot there is no other CPU that can hold a stale TLB
    // entry, so a cross-CPU shootdown is *by definition* a local flush and must
    // never wait on anyone. Skipping the machinery entirely here also closes a
    // wedge seen in the field: when memory corruption clobbers this CPU's
    // per-CPU identity, `cpu_id()` returns a bogus non-zero id (e.g. 2 on a
    // single-core box). `targets = IPI_READY & !(1<<me)` then keeps bit 0 set —
    // a *phantom* target that is really us — and the loop below spins forever
    // (`spins=16777216 targets=0x1 me=2`) self-pumping the wrong queue, so the
    // machine hangs right after a fault was otherwise cleanly contained. When
    // only one CPU is online, no stale-TLB correctness is at stake, so a local
    // flush is both sufficient and the only safe thing to do.
    if online_cpu_count() <= 1 {
        crate::vm::flush_tlb(vaddr);
        return;
    }
    // Always invalidate locally. Gathered range ops skip per-page INVLPG and
    // pay here; callers that already flushed a single page just do it twice.
    crate::vm::flush_tlb(vaddr);
    // Only target CPUs that are actually servicing IPIs — NOT merely online.
    // Waiting on a CPU still spinning for `STARTED` with IRQs off (so it can't
    // ack) would stall the whole init spawn until the budget runs out.
    let mut targets = IPI_READY.load(Ordering::Acquire) & !(1u64 << me);
    if let Some(root) = aspace {
        let root = root & !0xfff;
        for_each_cpu(targets, |cpu| {
            let tok = ACTIVE_VMTOKEN[cpu].load(Ordering::Acquire);
            if tok != 0 && tok != root {
                targets &= !(1u64 << cpu);
            }
        });
    }
    if targets == 0 {
        return; // nobody else is servicing IPIs yet, or nobody has this aspace
    }
    // vpn 0 doubles as the full-flush sentinel (page 0 is never mapped).
    let reason: IpiEntry = IpiReason::TlbShutdown {
        vpn: vaddr.map_or(0, |va| va >> 12),
    }
    .into();
    // Signal each target, then record the GOAL its ack must reach: the
    // target-queue `ptail` observed right after our enqueue. `send_ipi`
    // commits the entry (or sets the overflow bit) BEFORE ringing the APIC
    // and before returning, so this ptail is `>= our entry's index + 1` — and
    // a drain that publishes a consumed-index `>= goal` has provably flushed
    // AFTER consuming our request (or full-flushed on the overflow bit, whose
    // consuming drain also reaches this goal). Waiting on a bare "counter
    // advanced" was a TOCTOU: an in-flight drain that predated our enqueue
    // bumped it without servicing us. See the publish site in
    // `tlb_shootdown_ack`.
    //
    // A CPU whose IPI could not be delivered is dropped from the wait set: it
    // will never acknowledge, and the loop below has no timeout, so keeping it
    // as a target is an unconditional hang. That is strictly worse than the
    // stale mapping it reports — and the drop is loud, because a shootdown we
    // could not deliver does leave that CPU's TLB unflushed.
    let mut goal = [0u64; MAX_CORE_NUM];
    let mut ovf_goal = [0u64; MAX_CORE_NUM];
    for_each_cpu(targets, |cpu| {
        let ovf_before = IPI_OVERFLOW_GEN[cpu].load(Ordering::Acquire);
        if crate::interrupt::send_ipi(cpu, reason).is_err() {
            targets &= !(1u64 << cpu);
            // try_lock, NOT the spinning writer: see the wait loop below.
            crate::console::serial_write_fmt(format_args!(
                "\n[tlb-shootdown] cpu {} unreachable — skipped (its TLB may be stale)\n",
                cpu,
            ));
        } else {
            goal[cpu] = ipi_queue(cpu).map_or(0, |q| q.ptail() as u64);
            // Overflow does not advance `ptail`. If this send bumped the
            // overflow generation, wait for a drain that consumed it — not
            // merely for `SEQ >= ptail`, which a previous drain may already
            // satisfy.
            let ovf_after = IPI_OVERFLOW_GEN[cpu].load(Ordering::Acquire);
            if ovf_after > ovf_before {
                ovf_goal[cpu] = ovf_after;
            }
        }
    });
    if targets == 0 {
        return;
    }
    // Wait until every target's flush watermark reaches its goal. Idle skip was removed:
    // a CPU can leave idle and run user code with a stale TLB before taking the
    // pending IPI (TOCTOU). Spin-pump on ticket locks covers IRQs-off holders.
    // Soft warn after a long wait; keep waiting (correctness > latency).
    const SPIN_WARN: u64 = 1 << 24;
    let mut spins: u64 = 0;
    let mut warned = false;
    loop {
        let mut all_acked = true;
        let mut pending = 0u64;
        for_each_cpu(targets, |cpu| {
            let seq_ok = SHOOTDOWN_SEQ[cpu].load(Ordering::Acquire) >= goal[cpu];
            let ovf_ok = ovf_goal[cpu] == 0
                || IPI_OVERFLOW_ACK[cpu].load(Ordering::Acquire) >= ovf_goal[cpu];
            if !seq_ok || !ovf_ok {
                all_acked = false;
                pending |= 1u64 << cpu;
            }
        });
        // [diag] Publish who we are still blocked on — and the exact goal each
        // pending target is being held to — so the deadlock banner can show,
        // with no serial, the live protocol state of a starved shootdown
        // (which invariant broke), not just where the CPUs are parked.
        SHOOTDOWN_WAIT_MASK[me].store(pending, Ordering::Relaxed);
        for_each_cpu(targets, |cpu| {
            let g = if pending & (1u64 << cpu) != 0 {
                goal[cpu]
            } else {
                0
            };
            SHOOTDOWN_GOAL[me][cpu].store(g, Ordering::Relaxed);
        });
        if all_acked {
            // The store above already published this iteration's `pending`,
            // which is empty exactly when `all_acked`, so the mask is clear.
            break;
        }
        // Self-pump: if a peer asked US to flush, do it now (non-allocating) so
        // it isn't blocked on our ack while we block on its.
        if ipi_queue(me).is_some_and(|q| q.chead() < q.ptail()) || overflow_pending(me) {
            // `..._on(me)`, not `tlb_shootdown_ack()`: the queue that was just
            // tested is `me`'s, so the queue that gets drained has to be
            // `me`'s too. Re-reading `cpu_id()` here asked the question of one
            // CPU and acted on the answer of another -- and saved nothing, the
            // id is already in hand.
            tlb_shootdown_ack_on(me);
        }
        spins += 1;
        // Re-deliver the shootdown to still-pending targets periodically. A
        // target that took its IPI as a pure wake BEFORE our queue entry became
        // visible (the enqueue/signal TOCTOU handled by tlb_shootdown_ack) never
        // bumped its ack — and if it is alive with IRQs on but not spinning on
        // any ticket lock, it never pumps and never gets another IPI, so it
        // would starve this shootdown forever. Re-sending makes that lost wakeup
        // self-heal. It is harmless when the original entry is still queued (the
        // target just flushes the page twice) and correctness-safe on a full
        // queue (the overflow bit demotes the target's next ack to a full
        // flush). Gated far past the healthy fast path — which acks within a
        // handful of spins — so the common case never re-kicks, and only the
        // CPUs still in `pending` this iteration are poked.
        if should_rekick(spins) {
            for_each_cpu(pending, |cpu| {
                let _ = crate::interrupt::send_ipi(cpu, reason);
            });
        }
        // Escalate to NMI when the IPI re-kick has not helped for a while: the
        // target is not merely missing a wakeup but wedged where no maskable
        // interrupt lands (IRQs off, a fault storm, corrupt code). An NMI is
        // delivered regardless and its handler drains+acks the target's queue
        // (tlb_shootdown_ack_nmi). 16x rarer than the re-kick and far below the
        // deadlock detector's window, so a genuine lost wakeup is still handled
        // by the cheaper targeted IPI first; this is the last-resort unwedge.
        if should_escalate(spins, HAS_UNMASKABLE_KICK) {
            nmi_kick_pending_targets();
        }
        if spins >= SPIN_WARN && !warned {
            warned = true;
            // try_lock, NOT `serial_write_fmt_spin`. This runs from a hot wait
            // loop with interrupts off, and the spinning writer takes the
            // console lock unconditionally -- its contract ("caller must
            // disable interrupts") only rules out re-entry on the SAME CPU,
            // not a cross-CPU cycle. Here that cycle is reachable and fatal:
            // any other CPU holding the console lock while it waits on this
            // shootdown (or on a lock whose holder does) closes it, and this
            // CPU then spins inside the console lock forever -- with the lock
            // HELD, so the 8s deadlock detector, which prints through the same
            // writer, can never report it either. That is a silent full-machine
            // freeze: no serial, no screen, nothing, caused purely by the
            // diagnostic. A diagnostic must never be able to kill the machine
            // it is diagnosing, so losing this one line to a busy lock is the
            // right trade -- the detector's report is the one that matters.
            crate::console::serial_write_fmt(format_args!(
                "\n[tlb-shootdown] slow ack wait spins={} targets={:#x} me={} \
                 unmaskable-kick={}\n",
                spins,
                targets,
                me,
                if HAS_UNMASKABLE_KICK {
                    "sent"
                } else {
                    "none on this arch"
                },
            ));
        }
        core::hint::spin_loop();
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum IpiReason {
    Invalid,
    MockBlock {
        block_info: usize,
    },
    /// TLB shootdown request. `vpn` = the page to invalidate (vaddr >> 12);
    /// 0 requests a full flush.
    TlbShutdown {
        vpn: usize,
    },
}

/// usize : 64bit
/// |  type reason : 4bit  |   ipi info : 60bit   |
///
/// MockBlock info : 60bit
/// |  reserved : 60 bit  |
const TYPE_SHIFT: usize = 60;
const TYPE_INVALID: usize = 0x0;
const TYPE_MOCK_BLOCK: usize = 0x1;
const TYPE_TLB_SHUTDOWN: usize = 0x2;

impl From<IpiEntry> for IpiReason {
    fn from(r: IpiEntry) -> Self {
        let ipi_type = r >> TYPE_SHIFT;
        let ipi_info = r & 0x000FFFFFFFFFFFFF;
        match ipi_type {
            TYPE_MOCK_BLOCK => Self::MockBlock {
                block_info: ipi_info,
            },
            TYPE_TLB_SHUTDOWN => Self::TlbShutdown { vpn: ipi_info },
            _ => Self::Invalid,
        }
    }
}

impl From<IpiReason> for IpiEntry {
    fn from(reason: IpiReason) -> Self {
        match reason {
            IpiReason::MockBlock { block_info: info } => (TYPE_MOCK_BLOCK << TYPE_SHIFT) | info,
            IpiReason::TlbShutdown { vpn: info } => (TYPE_TLB_SHUTDOWN << TYPE_SHIFT) | info,
            IpiReason::Invalid => 0,
        }
    }
}

/// The masks in this module are `u64`, so a CPU above 63 has no overflow bit,
/// no online bit and no ready bit — and every one of those guards is spelled
/// `< 64` rather than `< MAX_CORE_NUM`. Raising the CPU limit past 64 without
/// widening them would leave those CPUs with a queue that exists, an id that
/// passes the bounds check, and a dropped shootdown every time the queue
/// fills. Caught here rather than in a test, because it is a property of the
/// constants and should fail the build.
const _: () = assert!(
    MAX_CORE_NUM <= 64,
    "IPI masks are u64: widen them before raising MAX_CORE_NUM past 64"
);

/// The TLB-shootdown protocol had no tests, on 810 lines whose own comments
/// record what its bugs cost: torn pages under fork, and full-machine freezes
/// with nothing on the serial line. None of it runs in CI — the emulator boots
/// one or two cores and never fills an IPI queue — so the only place these
/// paths execute is Moebius's machine.
///
/// Two things they pin down, both found by writing them:
///
/// 1. **A payload that does not fit must still set the overflow bit.** That bit
///    is the entire reason a full queue is survivable: it turns the target's
///    next drain into a full flush. riscv's `send_ipi` dropped the payload and
///    returned an error instead, and the initiator's response to a failed send
///    is to stop waiting for that CPU and free the frame — so the page was
///    never invalidated there. `publish_ipi_entry` is now the only place that
///    decision is written.
/// 2. **A CPU id is not to be trusted with an index.** `ipi_reason` indexed the
///    queue table with `cpu_id()` raw, from inside the IPI vector, while every
///    sibling accessor bounds-checked it first — against a failure this kernel
///    has actually seen (see `SMP_ENABLED`).
///
/// These tests share the module's global queues and masks, so they take
/// [`test_lock`] and each queue test owns a distinct CPU id.
/// Serialises every test in this file that writes the module's shared state.
///
/// The queue table, `CPU_ONLINE`, `IPI_READY`, the watermarks and the overflow
/// counters are one set of statics for the whole binary, and CI runs the suite
/// with `--test-threads=1` so it would never notice a test that raced another.
/// The host harness does not: `cpu_id_bounds_tests` used to take no lock at
/// all, and one of its tests sets **every** bit of `IPI_READY` — so a
/// `remote_flush_tlb_on` spinning in a neighbouring test picked up 62 extra
/// targets that no thread in the process would ever acknowledge, and sat there
/// until the deadlock deadline. That is a 60-second hang in roughly one run in
/// four, and the CI job is a single `set -e` block, so it takes everything
/// behind it with it. One lock for the file, not one per module.
#[cfg(test)]
fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod ipi_tests {
    use super::*;

    /// Distinct CPU ids per queue test, so one test's entries are never
    /// another's. Counts down from the top of the table.
    fn scratch_cpu(n: usize) -> usize {
        MAX_CORE_NUM - 1 - n
    }

    fn drain(cpu: usize) {
        let q = ipi_queue(cpu).unwrap();
        q.discard_entrys();
        clear_overflow(cpu);
    }

    /// Say that `cpu`'s outstanding full flush has been done. The demand is
    /// `GEN > ACK`, so settling it is one store and there is nothing else to
    /// clear — the point of [`the_demand_for_a_full_flush_lives_in_the_counters`].
    fn clear_overflow(cpu: usize) {
        IPI_OVERFLOW_ACK[cpu].store(
            IPI_OVERFLOW_GEN[cpu].load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
    }

    // ── the wire format ────────────────────────────────────────────────────

    #[test]
    fn every_reason_survives_the_round_trip() {
        for reason in [
            IpiReason::Invalid,
            IpiReason::MockBlock { block_info: 0 },
            IpiReason::MockBlock { block_info: 0x1234 },
            IpiReason::TlbShutdown { vpn: 0 },
            IpiReason::TlbShutdown { vpn: 1 },
            IpiReason::TlbShutdown { vpn: 0xdead_beef },
        ] {
            let wire: IpiEntry = reason.into();
            assert_eq!(
                IpiReason::from(wire),
                reason,
                "{:?} did not round-trip",
                reason
            );
        }
    }

    #[test]
    fn the_full_flush_sentinel_is_not_the_empty_slot() {
        // `IPI_BUFFERS` starts zeroed and a drain reads every index in
        // `chead..ptail`, so a slot that was never written must decode to
        // something that asks for nothing. `TlbShutdown { vpn: 0 }` means
        // "flush everything", so the two must not share an encoding.
        let empty: IpiEntry = 0;
        assert_eq!(IpiReason::from(empty), IpiReason::Invalid);
        let full_flush: IpiEntry = IpiReason::TlbShutdown { vpn: 0 }.into();
        assert_ne!(full_flush, empty);
        assert_eq!(
            IpiReason::from(full_flush),
            IpiReason::TlbShutdown { vpn: 0 }
        );
    }

    #[test]
    fn an_unknown_type_decodes_to_invalid_not_to_a_flush() {
        // Garbage in a slot must not be read as a shootdown request: a decode
        // that guessed `TlbShutdown` would have the drain invalidate an
        // arbitrary page, and one that guessed vpn 0 would full-flush on every
        // corrupt entry.
        for ty in [0x3usize, 0x7, 0xf] {
            let wire = (ty << TYPE_SHIFT) | 0x41;
            assert_eq!(IpiReason::from(wire), IpiReason::Invalid, "type {:#x}", ty);
        }
    }

    #[test]
    fn the_highest_kernel_page_number_still_fits_beside_the_type_field() {
        // `vpn` is `vaddr >> 12`, so it needs 52 bits; the decode masks the
        // payload to exactly that and the type sits above it. Narrowing either
        // would truncate the address of the very mappings this is used for —
        // the kernel half, whose vpns have every one of those 52 bits set.
        let top_vaddr = usize::MAX;
        let vpn = top_vaddr >> 12;
        let wire: IpiEntry = IpiReason::TlbShutdown { vpn }.into();
        assert_eq!(IpiReason::from(wire), IpiReason::TlbShutdown { vpn });
        assert_eq!(wire >> TYPE_SHIFT, TYPE_TLB_SHUTDOWN);
    }

    // ── the queue table ────────────────────────────────────────────────────

    #[test]
    fn a_cpu_past_the_table_has_no_queue_instead_of_a_panic() {
        let _g = test_lock();
        for cpu in 0..MAX_CORE_NUM {
            assert!(ipi_queue(cpu).is_some(), "cpu {} should have a queue", cpu);
        }
        assert!(ipi_queue(MAX_CORE_NUM).is_none());
        assert!(ipi_queue(usize::MAX).is_none());
        // And the diagnostics agree rather than indexing.
        assert_eq!(shootdown_queue_state(MAX_CORE_NUM), (0, 0, 0, false, false));
        assert_eq!(shootdown_seq_of(MAX_CORE_NUM), 0);
        assert_eq!(shootdown_wait_mask(MAX_CORE_NUM), 0);
        assert_eq!(shootdown_goal(MAX_CORE_NUM, 0), 0);
        assert_eq!(shootdown_goal(0, MAX_CORE_NUM), 0);
    }

    #[test]
    fn a_published_entry_arrives_with_its_payload_intact() {
        let _g = test_lock();
        let cpu = scratch_cpu(0);
        drain(cpu);
        let reason: IpiEntry = IpiReason::TlbShutdown { vpn: 0x5678 }.into();
        assert!(publish_ipi_entry(cpu, reason));
        let q = ipi_queue(cpu).unwrap();
        let (chead, ptail) = (q.chead(), q.ptail());
        assert_eq!(ptail - chead, 1, "exactly one entry should be queued");
        assert_eq!(
            IpiReason::from(*q.entry_at(chead)),
            IpiReason::TlbShutdown { vpn: 0x5678 }
        );
        // Publishing does not raise the overflow demand on the way.
        assert!(!overflow_pending(cpu));
        drain(cpu);
    }

    #[test]
    fn a_full_queue_notes_an_overflow_rather_than_losing_the_flush() {
        // The bug this pins: what does not fit must still be announced, or the
        // target never flushes and the initiator frees the frame anyway.
        let _g = test_lock();
        let cpu = scratch_cpu(1);
        drain(cpu);
        let gen_before = IPI_OVERFLOW_GEN[cpu].load(Ordering::SeqCst);
        let reason: IpiEntry = IpiReason::TlbShutdown { vpn: 1 }.into();
        for i in 0..REASON_SIZE {
            assert!(publish_ipi_entry(cpu, reason), "publish {} failed", i);
        }
        assert!(
            !overflow_pending(cpu),
            "a queue filled exactly to capacity has not overflowed"
        );
        // One past capacity: the CPU still exists, so the send is not an
        // error — but the target must be told to full-flush.
        assert!(publish_ipi_entry(cpu, reason));
        assert!(
            overflow_pending(cpu),
            "a dropped payload must leave a demand for a full flush"
        );
        assert!(
            IPI_OVERFLOW_GEN[cpu].load(Ordering::SeqCst) > gen_before,
            "the overflow generation must advance, or a waiter accepts an \
             older drain's watermark as its ack"
        );
        drain(cpu);
    }

    #[test]
    fn publishing_to_a_cpu_that_does_not_exist_reports_it_and_marks_nothing() {
        let _g = test_lock();
        let before = gens();
        assert!(!publish_ipi_entry(MAX_CORE_NUM, 1));
        assert!(!publish_ipi_entry(usize::MAX, 1));
        assert_eq!(
            gens(),
            before,
            "an id with no queue must not note an overflow on some other CPU"
        );
    }

    #[test]
    fn note_ipi_queue_overflow_ignores_an_id_with_no_row() {
        let _g = test_lock();
        let before = gens();
        note_ipi_queue_overflow(64);
        note_ipi_queue_overflow(usize::MAX);
        assert_eq!(gens(), before);
    }

    /// Every CPU's overflow generation, so a test can say "and no other CPU's
    /// moved" in one comparison.
    fn gens() -> alloc::vec::Vec<u64> {
        (0..MAX_CORE_NUM)
            .map(|c| IPI_OVERFLOW_GEN[c].load(Ordering::SeqCst))
            .collect()
    }

    // ── the CPU masks ──────────────────────────────────────────────────────

    #[test]
    fn for_each_cpu_visits_exactly_the_set_bits() {
        let mut seen = alloc::vec::Vec::new();
        for_each_cpu(0, |c| seen.push(c));
        assert!(seen.is_empty(), "an empty mask targets nobody");

        seen.clear();
        for_each_cpu(0b1010_0001, |c| seen.push(c));
        assert_eq!(seen, alloc::vec![0, 5, 7]);

        // The top bit is a real CPU (MAX_CORE_NUM is 64) and must not be
        // dropped or shifted out.
        seen.clear();
        for_each_cpu(1u64 << 63, |c| seen.push(c));
        assert_eq!(seen, alloc::vec![63]);

        seen.clear();
        for_each_cpu(u64::MAX, |c| seen.push(c));
        assert_eq!(seen.len(), 64);
        assert_eq!(seen[0], 0);
        assert_eq!(seen[63], 63);
    }

    #[test]
    fn the_online_mask_counts_only_cpus_that_reported_in() {
        let _g = test_lock();
        let before = cpu_online_mask();
        // The BSP is online from the start: accounting that divides by the
        // online count must never see zero.
        assert_ne!(before & 1, 0, "the BSP is always online");
        assert!(online_cpu_count() >= 1);

        mark_cpu_online(63);
        assert_ne!(cpu_online_mask() & (1u64 << 63), 0);
        assert_eq!(
            online_cpu_count(),
            (before | (1u64 << 63)).count_ones() as usize
        );

        // An id with no bit is ignored, not shifted: `1u64 << 64` is not a
        // no-op, it is undefined.
        let now = cpu_online_mask();
        mark_cpu_online(64);
        mark_cpu_online(usize::MAX);
        assert_eq!(cpu_online_mask(), now);

        CPU_ONLINE.store(before, Ordering::Release);
    }

    #[test]
    fn a_cpu_is_ready_to_service_ipis_only_after_it_says_so() {
        let _g = test_lock();
        let before = IPI_READY.load(Ordering::Acquire);
        let cpu = scratch_cpu(2);
        IPI_READY.fetch_and(!(1u64 << cpu), Ordering::Release);
        assert_eq!(IPI_READY.load(Ordering::Acquire) & (1u64 << cpu), 0);
        mark_cpu_ipi_ready(cpu);
        assert_ne!(IPI_READY.load(Ordering::Acquire) & (1u64 << cpu), 0);

        let now = IPI_READY.load(Ordering::Acquire);
        mark_cpu_ipi_ready(64);
        mark_cpu_ipi_ready(usize::MAX);
        assert_eq!(IPI_READY.load(Ordering::Acquire), now);

        IPI_READY.store(before, Ordering::Release);
    }

    // ── reentrancy: two publishes on one CPU ───────────────────────────────

    #[test]
    fn a_nested_publish_does_not_reserve_a_second_slot() {
        let _g = test_lock();
        let cpu = scratch_cpu(11);
        drain(cpu);
        let q = ipi_queue(cpu).unwrap();
        let before = q.phead();

        // Stand in for the interrupt: the outer publish is between its
        // `alloc_entry` CAS and its `commit_entry`, so this CPU's slot is
        // taken. Reserving another here is the wedge — `commit_entry` would
        // spin for a predecessor that cannot run until we return, and it
        // never gives up.
        let outer = PublishGuard::enter().expect("nothing was in flight");
        assert!(publish_ipi_entry(
            cpu,
            IpiReason::TlbShutdown { vpn: 0x1234 }.into()
        ));
        assert_eq!(q.phead(), before, "the nested publish took a slot");
        assert!(
            overflow_pending(cpu),
            "the payload was dropped without forcing a full flush"
        );

        drop(outer);
        drain(cpu);
    }

    #[test]
    fn the_slot_is_given_back_however_the_publish_ends() {
        let _g = test_lock();
        let cpu = scratch_cpu(12);
        drain(cpu);
        // A publish that fills the queue, one that names no CPU and an
        // ordinary one all have to leave the slot free, or this CPU can never
        // queue another payload for the rest of the boot.
        let q = ipi_queue(cpu).unwrap();
        for _ in 0..REASON_SIZE {
            assert!(publish_ipi_entry(
                cpu,
                IpiReason::TlbShutdown { vpn: 1 }.into()
            ));
        }
        assert!(q.alloc_entry().is_none(), "the queue was meant to be full");
        assert!(publish_ipi_entry(
            cpu,
            IpiReason::TlbShutdown { vpn: 2 }.into()
        ));
        assert!(!publish_ipi_entry(MAX_CORE_NUM, 0));
        drain(cpu);

        assert!(
            PublishGuard::enter().is_some(),
            "a publish left this CPU's slot claimed"
        );
    }

    // ── the reschedule kick ────────────────────────────────────────────────
    //
    // The other interrupt this module sends: no payload, no ack, no wait. It
    // exists only so a CPU looks at its run queue now instead of at its next
    // 250 Hz tick, and until this tanda it had a body on x86_64 alone.

    #[test]
    fn an_id_past_the_tables_is_never_kicked() {
        let _g = test_lock();
        // Every delivery ends in a `1 << id` — an SBI hart mask, a GICv2
        // target list, a table index. Past the word that is not a no-op.
        assert!(!wake_kick_wanted(MAX_CORE_NUM));
        assert!(!wake_kick_wanted(64));
        assert!(!wake_kick_wanted(usize::MAX));
    }

    #[test]
    fn we_do_not_interrupt_ourselves_to_look_at_our_own_run_queue() {
        let _g = test_lock();
        let me = crate::cpu::cpu_id() as usize;
        let before = CPU_ONLINE.load(Ordering::Acquire);
        // Online, so the only thing left to refuse it on is that it is us.
        // `send_wake_ipi(me)` is reachable: a wake raised from an interrupt
        // handler on this CPU, for a task this CPU owns, while this CPU's
        // sleeping bit is still published (it is cleared only after
        // `wait_for_interrupt` returns).
        mark_cpu_online(me);
        assert!(!wake_kick_wanted(me));
        CPU_ONLINE.store(before, Ordering::Release);
    }

    #[test]
    fn a_cpu_that_never_came_online_is_not_worth_an_interrupt() {
        let _g = test_lock();
        let cpu = scratch_cpu(9);
        let before = CPU_ONLINE.load(Ordering::Acquire);

        CPU_ONLINE.fetch_and(!(1u64 << cpu), Ordering::Release);
        assert!(
            !wake_kick_wanted(cpu),
            "an AP that failed to start runs no task to wake"
        );
        mark_cpu_online(cpu);
        assert!(wake_kick_wanted(cpu));

        CPU_ONLINE.store(before, Ordering::Release);
    }

    #[test]
    fn the_kick_does_not_wait_for_a_cpu_to_announce_itself_ready_for_ipis() {
        let _g = test_lock();
        let cpu = scratch_cpu(10);
        let before_online = CPU_ONLINE.load(Ordering::Acquire);
        let before_ready = IPI_READY.load(Ordering::Acquire);

        // `IPI_READY` is published from the executor's own entry path, and a
        // reschedule kick exists precisely to poke a CPU sitting in that loop.
        // Gating on it would make the kick depend on the thing it pokes, so a
        // CPU that is online but has not yet said "ready" is still kicked —
        // unlike a shootdown, which does wait for that narrower set.
        mark_cpu_online(cpu);
        IPI_READY.fetch_and(!(1u64 << cpu), Ordering::Release);
        assert!(wake_kick_wanted(cpu));

        IPI_READY.store(before_ready, Ordering::Release);
        CPU_ONLINE.store(before_online, Ordering::Release);
    }

    // ── the shootdown protocol ─────────────────────────────────────────────
    //
    // Everything above this line is the wire format and the queue. What
    // follows is the protocol they carry: who gets signalled, what a drain
    // owes the initiator, and what "acknowledged" means. None of it had a
    // test, and two of the three architectures' IPI handlers were wired to the
    // wrong end of it.

    use crate::imp::vm::flush_probe;
    use core::sync::atomic::AtomicBool;

    /// A fake SMP box: `n` CPUs, ids `0..n`, all online and all servicing
    /// IPIs, with empty queues, matching watermarks and an empty flush log.
    /// The module's shared tables are saved here and put back on drop.
    struct Smp {
        ready: u64,
        online: u64,
        cpus: usize,
    }

    impl Smp {
        fn with(cpus: usize) -> Self {
            let me = Smp {
                ready: IPI_READY.load(Ordering::Acquire),
                online: CPU_ONLINE.load(Ordering::Acquire),
                cpus,
            };
            let mask = if cpus >= 64 {
                u64::MAX
            } else {
                (1u64 << cpus) - 1
            };
            for cpu in 0..cpus {
                reset_cpu(cpu);
            }
            IPI_READY.store(mask, Ordering::Release);
            CPU_ONLINE.store(mask, Ordering::Release);
            flush_probe::reset();
            me
        }
    }

    impl Drop for Smp {
        fn drop(&mut self) {
            for cpu in 0..self.cpus {
                reset_cpu(cpu);
            }
            IPI_READY.store(self.ready, Ordering::Release);
            CPU_ONLINE.store(self.online, Ordering::Release);
            flush_probe::reset();
        }
    }

    /// Put one CPU's protocol state back to "idle and up to date".
    ///
    /// The watermark is the queue index a drain consumed up to, and the queue
    /// indices are monotone for the life of the process — they are never reset
    /// — so "up to date" is the current tail, not zero.
    fn reset_cpu(cpu: usize) {
        let Some(q) = ipi_queue(cpu) else {
            return;
        };
        q.discard_entrys();
        SHOOTDOWN_SEQ[cpu].store(q.ptail() as u64, Ordering::SeqCst);
        clear_overflow(cpu);
        SHOOTDOWN_WAIT_MASK[cpu].store(0, Ordering::SeqCst);
        SHOOTDOWN_ACK_ACTIVE[cpu].store(false, Ordering::SeqCst);
        ACTIVE_VMTOKEN[cpu].store(0, Ordering::SeqCst);
        for target in 0..MAX_CORE_NUM {
            SHOOTDOWN_GOAL[cpu][target].store(0, Ordering::SeqCst);
        }
    }

    fn tail(cpu: usize) -> u64 {
        ipi_queue(cpu).map_or(0, |q| q.ptail() as u64)
    }

    fn seq(cpu: usize) -> u64 {
        SHOOTDOWN_SEQ[cpu].load(Ordering::Acquire)
    }

    fn send(to: usize, vpn: usize) {
        let reason: IpiEntry = IpiReason::TlbShutdown { vpn }.into();
        assert!(publish_ipi_entry(to, reason), "publish to {} failed", to);
    }

    /// Block until the shootdown running as `me` is parked waiting on someone,
    /// failing fast — and by name — if it instead returned unacknowledged.
    fn wait_until_waiting(me: usize, done: &AtomicBool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            assert!(
                !done.load(Ordering::SeqCst),
                "the shootdown returned before any target acknowledged it"
            );
            if shootdown_wait_mask(me) != 0 {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the shootdown never published a wait mask"
            );
            std::thread::yield_now();
        }
    }

    fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !cond() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {}",
                what
            );
            std::thread::yield_now();
        }
    }

    // ── what a drain owes the initiator ────────────────────────────────────

    #[test]
    fn a_drain_that_consumed_a_request_flushes_the_page_and_publishes_the_index() {
        let _g = test_lock();
        let _smp = Smp::with(2);
        send(1, 0x1234);
        let goal = tail(1);
        tlb_shootdown_ack_on(1);
        assert_eq!(
            flush_probe::flushes(),
            vec![Some(0x1234 << 12)],
            "the page the request named is the page that must be invalidated"
        );
        assert_eq!(
            seq(1),
            goal,
            "the watermark is the index the drain consumed up to"
        );
    }

    #[test]
    fn reporting_the_reasons_does_not_consume_a_request_without_servicing_it() {
        // riscv's `super_soft` handler was `ipi_reason()` and a `debug!`: the
        // entries were collected, `chead` was advanced to `ptail`, and nothing
        // was flushed or published. The request was eaten — the CPU kept the
        // stale mapping, and with the queue now reading empty no later ack,
        // pump or NMI kick had anything left to service, so the initiator's
        // (deliberately untimed) wait could never end.
        let _g = test_lock();
        let _smp = Smp::with(2);
        send(1, 0x4444);
        let goal = tail(1);
        let reported = ipi_reason_on(1);

        assert_eq!(
            reported,
            vec![IpiReason::TlbShutdown { vpn: 0x4444 }.into()],
            "the drain still has to report what it consumed"
        );
        assert!(
            !flush_probe::flushes().is_empty(),
            "a consumed shootdown that was never flushed leaves a stale mapping"
        );
        assert_eq!(
            seq(1),
            goal,
            "a consumed shootdown that publishes no watermark starves its initiator forever"
        );
        assert_eq!(
            ipi_queue(1).unwrap().chead(),
            goal as usize,
            "and the entry must still be consumed"
        );
    }

    #[test]
    fn a_pure_wake_flushes_nothing_and_acknowledges_nothing() {
        // A reschedule kick carries no queue entry. Treating it as an ack
        // would let an initiator mistake somebody else's wake for its flush.
        let _g = test_lock();
        let _smp = Smp::with(2);
        let before = seq(1);
        tlb_shootdown_ack_on(1);
        assert!(flush_probe::flushes().is_empty(), "a wake is not a flush");
        assert_eq!(seq(1), before, "a wake is not an acknowledgement");
    }

    #[test]
    fn more_requests_than_the_precise_budget_become_one_full_flush() {
        let _g = test_lock();
        let _smp = Smp::with(2);
        for i in 1..=(MAX_PRECISE_SHOOTDOWN + 1) {
            send(1, i);
        }
        tlb_shootdown_ack_on(1);
        assert_eq!(
            flush_probe::flushes(),
            vec![None],
            "past the budget one full flush is cheaper than the invlpg run"
        );
    }

    #[test]
    fn the_full_flush_sentinel_demotes_the_whole_drain() {
        let _g = test_lock();
        let _smp = Smp::with(2);
        send(1, 7);
        send(1, 0); // the sentinel: "flush everything"
        tlb_shootdown_ack_on(1);
        assert!(
            flush_probe::saw_full_flush(),
            "a request for a full flush must not be serviced as one page"
        );
    }

    #[test]
    fn a_non_tlb_entry_is_drained_with_a_full_flush_rather_than_left_unpublished() {
        // Consuming entries while publishing no watermark is the unsound
        // state, whatever the entries were: the queue reads empty afterwards,
        // so nothing can ever service what was in it.
        let _g = test_lock();
        let _smp = Smp::with(2);
        let reason: IpiEntry = IpiReason::MockBlock { block_info: 9 }.into();
        assert!(publish_ipi_entry(1, reason));
        let goal = tail(1);
        tlb_shootdown_ack_on(1);
        assert!(flush_probe::saw_full_flush());
        assert_eq!(seq(1), goal);
    }

    #[test]
    fn an_overflowed_send_is_a_full_flush_and_its_own_acknowledgement() {
        // An overflow advances no queue index, so `SEQ >= ptail` can be
        // satisfied by a drain that predates the dropped request. The
        // generation counter is what the initiator waits on instead.
        let _g = test_lock();
        let _smp = Smp::with(2);
        note_ipi_queue_overflow(1);
        let gen = IPI_OVERFLOW_GEN[1].load(Ordering::Acquire);
        tlb_shootdown_ack_on(1);
        assert!(
            flush_probe::saw_full_flush(),
            "a dropped payload can only be covered by a full flush"
        );
        assert!(
            IPI_OVERFLOW_ACK[1].load(Ordering::Acquire) >= gen,
            "and the drain has to say which overflow it covered"
        );
        assert!(
            !overflow_pending(1),
            "the demand is settled by the drain that honoured it"
        );
    }

    #[test]
    fn a_cpu_owes_a_flush_exactly_when_its_generation_is_ahead_of_its_acknowledgement() {
        // The demand and the generation it is owed for are one counter pair,
        // and a note is one increment. They used to be two things written by
        // one note in two steps — a bitmask raised first, the generation
        // bumped second — and a drain landing between the steps saw the
        // demand, read the OLD generation, full-flushed and acknowledged
        // that; the sender then read the new generation and waited for an
        // acknowledgement of it, which nothing was left to publish (the bit
        // was consumed, and a re-kick that fits the queue notes no overflow).
        // Nothing here can reach that state, because there is no second step
        // to land between.
        let _g = test_lock();
        let cpu = scratch_cpu(11);
        clear_overflow(cpu);
        assert!(!overflow_pending(cpu), "a settled cpu owes nothing");

        let before = IPI_OVERFLOW_GEN[cpu].load(Ordering::SeqCst);
        note_ipi_queue_overflow(cpu);
        assert_eq!(
            IPI_OVERFLOW_GEN[cpu].load(Ordering::SeqCst),
            before + 1,
            "a note is one increment and nothing else"
        );
        assert!(overflow_pending(cpu));

        // Acknowledging the generation the note published is the whole of it.
        IPI_OVERFLOW_ACK[cpu].store(before + 1, Ordering::SeqCst);
        assert!(
            !overflow_pending(cpu),
            "a second flag would still be raised here, and every path that \
             asks would still be demanding a flush nobody owes"
        );
    }

    #[test]
    fn every_path_that_asks_whether_a_flush_is_owed_reads_the_same_counters() {
        // The drain's peek, the NMI rescue's probe and the diagnostics have to
        // answer this question from the same state. Seeded straight into the
        // counters — no note, no queue entry — so a path that consulted
        // anything else would find nothing to do and say so.
        let _g = test_lock();
        let _smp = Smp::with(2);
        let cpu = 1;
        let gen = IPI_OVERFLOW_GEN[cpu].load(Ordering::SeqCst);
        IPI_OVERFLOW_ACK[cpu].store(gen, Ordering::SeqCst);
        assert!(!shootdown_queue_state(cpu).4, "nothing is owed yet");

        IPI_OVERFLOW_GEN[cpu].store(gen + 1, Ordering::SeqCst);
        assert!(
            shootdown_queue_state(cpu).4,
            "the deadlock banner reports the demand"
        );
        flush_probe::reset();
        tlb_shootdown_ack_on(cpu);
        assert!(
            flush_probe::saw_full_flush(),
            "the drain's peek has to see it too: an empty queue with a demand \
             outstanding is not a pure wake, and treating it as one leaves the \
             page mapped"
        );
        assert!(!overflow_pending(cpu), "and the drain settles it");

        // And the NMI rescue, which is the only thing that reaches a CPU with
        // interrupts off — the case the demand most often survives into.
        IPI_OVERFLOW_GEN[cpu].store(gen + 2, Ordering::SeqCst);
        flush_probe::reset();
        tlb_shootdown_ack_nmi_on(cpu);
        assert!(flush_probe::saw_full_flush());
        assert!(
            !overflow_pending(cpu),
            "the rescue exists to end a wait; leaving the demand up would not"
        );
    }

    #[test]
    fn a_drain_covers_the_generation_it_read_and_leaves_a_later_note_pending() {
        // The acknowledgement names a generation rather than meaning
        // "everything": a payload dropped after this drain read the counter is
        // not covered by the flush this drain performed, so its sender is
        // released by the next drain and not by this one.
        let _g = test_lock();
        let _smp = Smp::with(2);
        note_ipi_queue_overflow(1);
        let first = IPI_OVERFLOW_GEN[1].load(Ordering::SeqCst);
        tlb_shootdown_ack_on(1);
        assert_eq!(IPI_OVERFLOW_ACK[1].load(Ordering::SeqCst), first);
        assert!(!overflow_pending(1));

        note_ipi_queue_overflow(1);
        assert!(overflow_pending(1), "the later note is a demand of its own");
        assert_eq!(
            IPI_OVERFLOW_ACK[1].load(Ordering::SeqCst),
            first,
            "and the earlier drain cannot have covered it"
        );
        flush_probe::reset();
        tlb_shootdown_ack_on(1);
        assert!(flush_probe::saw_full_flush());
        assert_eq!(IPI_OVERFLOW_ACK[1].load(Ordering::SeqCst), first + 1);
    }

    #[test]
    fn several_dropped_payloads_are_covered_by_one_flush_and_one_acknowledgement() {
        // A full flush over-satisfies every request that preceded it, so one
        // drain releases every sender waiting behind it — the acknowledgement
        // has to jump to the newest generation, not step one at a time, or the
        // oldest sender is released and the newest waits for a drain that has
        // nothing left to do.
        let _g = test_lock();
        let _smp = Smp::with(2);
        let before = IPI_OVERFLOW_GEN[1].load(Ordering::SeqCst);
        for _ in 0..3 {
            note_ipi_queue_overflow(1);
        }
        flush_probe::reset();
        tlb_shootdown_ack_on(1);
        assert!(flush_probe::saw_full_flush());
        assert_eq!(
            IPI_OVERFLOW_ACK[1].load(Ordering::SeqCst),
            before + 3,
            "one flush, every sender released"
        );
        assert!(!overflow_pending(1));
    }

    #[test]
    fn a_drain_that_carried_only_non_tlb_work_and_a_demand_settles_both() {
        // Two things arrive together that neither asks for a page: a payload
        // that never made it into the queue, and an entry that is not a
        // shootdown at all. The drain owes both watermarks — an exit that
        // consumes the queue while leaving either one behind is the unsound
        // state the whole consumed-index protocol exists to rule out, with the
        // overflow counter standing in for the queue index the dropped
        // payload never got.
        let _g = test_lock();
        let _smp = Smp::with(2);
        note_ipi_queue_overflow(1);
        let gen = IPI_OVERFLOW_GEN[1].load(Ordering::SeqCst);
        assert!(publish_ipi_entry(
            1,
            IpiReason::MockBlock { block_info: 7 }.into()
        ));
        let goal = tail(1);
        flush_probe::reset();
        tlb_shootdown_ack_on(1);
        assert!(flush_probe::saw_full_flush());
        assert_eq!(seq(1), goal, "the queue index it consumed");
        assert_eq!(
            IPI_OVERFLOW_ACK[1].load(Ordering::SeqCst),
            gen,
            "and the generation it covered"
        );
    }

    // ── what the initiator waits for ───────────────────────────────────────

    #[test]
    fn the_initiator_waits_until_the_target_acknowledges_its_own_request() {
        let _g = test_lock();
        let _smp = Smp::with(2);
        let done = alloc::sync::Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let t = std::thread::spawn(move || {
            remote_flush_tlb_on(0, Some(0x2000), None);
            flag.store(true, Ordering::SeqCst);
        });
        wait_until_waiting(0, &done);
        assert_ne!(
            shootdown_wait_mask(0) & (1u64 << 1),
            0,
            "cpu 1 was signalled, so cpu 1 is who the wait is on"
        );
        tlb_shootdown_ack_on(1);
        wait_until("the shootdown to finish", || done.load(Ordering::SeqCst));
        t.join().unwrap();
        assert_eq!(
            shootdown_wait_mask(0),
            0,
            "the wait mask is cleared on the way out"
        );
    }

    #[test]
    fn a_drain_that_predates_the_request_does_not_acknowledge_it() {
        // The TOCTOU a plain "the counter moved" protocol had: a drain already
        // in flight when the request was enqueued finishes, flushes only the
        // older pages, and bumps the counter. The initiator would take that
        // for its own ack and free the frame while the target still held the
        // stale writable entry.
        let _g = test_lock();
        let _smp = Smp::with(2);
        send(1, 0xaa);
        tlb_shootdown_ack_on(1);
        let earlier = seq(1);

        let done = alloc::sync::Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let t = std::thread::spawn(move || {
            remote_flush_tlb_on(0, Some(0xbb << 12), None);
            flag.store(true, Ordering::SeqCst);
        });
        wait_until_waiting(0, &done);
        assert_eq!(
            seq(1),
            earlier,
            "the earlier drain's watermark is still all the target has published"
        );
        tlb_shootdown_ack_on(1);
        wait_until("the shootdown to finish", || done.load(Ordering::SeqCst));
        t.join().unwrap();
        assert!(seq(1) > earlier);
    }

    #[test]
    fn a_pure_wake_on_the_target_is_not_the_acknowledgement_the_initiator_wants() {
        let _g = test_lock();
        let _smp = Smp::with(2);
        let done = alloc::sync::Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let t = std::thread::spawn(move || {
            remote_flush_tlb_on(0, Some(0x9000), None);
            flag.store(true, Ordering::SeqCst);
        });
        wait_until_waiting(0, &done);
        // The target takes an interrupt but its drain finds… whatever the
        // enqueue/signal race left it. A wake that consumed nothing publishes
        // nothing, so the initiator is still waiting afterwards.
        SHOOTDOWN_ACK_ACTIVE[1].store(false, Ordering::SeqCst);
        assert!(!done.load(Ordering::SeqCst));
        tlb_shootdown_ack_on(1);
        wait_until("the shootdown to finish", || done.load(Ordering::SeqCst));
        t.join().unwrap();
    }

    #[test]
    fn the_page_the_initiator_asked_for_is_the_page_the_target_invalidates() {
        let _g = test_lock();
        let _smp = Smp::with(2);
        const VA: usize = 0xdead_0000;
        let done = alloc::sync::Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let t = std::thread::spawn(move || {
            remote_flush_tlb_on(0, Some(VA), None);
            flag.store(true, Ordering::SeqCst);
        });
        wait_until_waiting(0, &done);
        flush_probe::reset();
        tlb_shootdown_ack_on(1);
        wait_until("the shootdown to finish", || done.load(Ordering::SeqCst));
        t.join().unwrap();
        assert_eq!(
            flush_probe::flushes(),
            vec![Some(VA & !0xfff)],
            "the vpn travels in the queue entry, so the target can invlpg it"
        );
    }

    #[test]
    fn a_single_online_cpu_flushes_locally_and_waits_on_no_one() {
        // Not an optimisation. On a uniprocessor box a bogus `cpu_id()` leaves
        // bit 0 in the target mask -- a phantom target that is really us -- so
        // the CPU ends up waiting for its own acknowledgement, self-pumping a
        // queue that is not the one it is watching, forever: the
        // `spins=16777216 targets=0x1 me=2` wedge. Nothing else stops it,
        // because the wait deliberately has no timeout. With only one CPU
        // online there is no stale TLB anywhere else, so a local flush is both
        // sufficient and the only safe answer.
        let _g = test_lock();
        let _smp = Smp::with(1);
        let done = alloc::sync::Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let t = std::thread::spawn(move || {
            remote_flush_tlb_on(2, Some(0x5000), None);
            flag.store(true, Ordering::SeqCst);
        });
        wait_until("the single-core flush to return", || {
            done.load(Ordering::SeqCst)
        });
        t.join().unwrap();
        assert_eq!(flush_probe::flushes(), vec![Some(0x5000)]);
    }

    #[test]
    fn the_initiator_never_signals_itself() {
        let _g = test_lock();
        let _smp = Smp::with(3);
        let before = tail(0);
        let done = alloc::sync::Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let t = std::thread::spawn(move || {
            remote_flush_tlb_on(0, Some(0x1000), None);
            flag.store(true, Ordering::SeqCst);
        });
        wait_until_waiting(0, &done);
        assert_eq!(
            shootdown_wait_mask(0) & 1,
            0,
            "waiting on our own acknowledgement is a deadlock with no timeout"
        );
        assert_eq!(tail(0), before, "and nothing was enqueued for us");
        tlb_shootdown_ack_on(1);
        tlb_shootdown_ack_on(2);
        wait_until("the shootdown to finish", || done.load(Ordering::SeqCst));
        t.join().unwrap();
    }

    #[test]
    fn a_cpu_that_has_not_said_it_services_ipis_is_not_waited_on() {
        // A CPU still spinning for STARTED with interrupts off cannot ack, so
        // targeting it would stall the spawn until the budget ran out.
        let _g = test_lock();
        let _smp = Smp::with(3);
        IPI_READY.fetch_and(!(1u64 << 2), Ordering::SeqCst);
        let before = tail(2);
        let done = alloc::sync::Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let t = std::thread::spawn(move || {
            remote_flush_tlb_on(0, Some(0x1000), None);
            flag.store(true, Ordering::SeqCst);
        });
        wait_until_waiting(0, &done);
        assert_eq!(shootdown_wait_mask(0) & (1u64 << 2), 0);
        assert_eq!(tail(2), before);
        tlb_shootdown_ack_on(1);
        wait_until("the shootdown to finish", || done.load(Ordering::SeqCst));
        t.join().unwrap();
    }

    #[test]
    fn the_address_space_filter_drops_a_cpu_that_has_another_page_table_loaded() {
        let _g = test_lock();
        let _smp = Smp::with(3);
        ACTIVE_VMTOKEN[1].store(0x11_000, Ordering::SeqCst);
        ACTIVE_VMTOKEN[2].store(0x22_000, Ordering::SeqCst);
        let untouched = tail(2);
        let done = alloc::sync::Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let t = std::thread::spawn(move || {
            remote_flush_tlb_on(0, Some(0x1000), Some(0x11_000));
            flag.store(true, Ordering::SeqCst);
        });
        wait_until_waiting(0, &done);
        assert_eq!(
            shootdown_wait_mask(0),
            1u64 << 1,
            "only the CPU with that page table loaded can hold a stale entry"
        );
        assert_eq!(tail(2), untouched);
        tlb_shootdown_ack_on(1);
        wait_until("the shootdown to finish", || done.load(Ordering::SeqCst));
        t.join().unwrap();
    }

    #[test]
    fn the_address_space_filter_keeps_a_cpu_whose_page_table_is_unknown() {
        // Token 0 means "we have not seen this CPU switch". Filtering it out
        // would skip a CPU that may well have the mapping.
        let _g = test_lock();
        let _smp = Smp::with(3);
        ACTIVE_VMTOKEN[1].store(0, Ordering::SeqCst);
        ACTIVE_VMTOKEN[2].store(0x22_000, Ordering::SeqCst);
        let done = alloc::sync::Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let t = std::thread::spawn(move || {
            remote_flush_tlb_on(0, Some(0x1000), Some(0x11_000));
            flag.store(true, Ordering::SeqCst);
        });
        wait_until_waiting(0, &done);
        assert_eq!(shootdown_wait_mask(0), 1u64 << 1);
        tlb_shootdown_ack_on(1);
        wait_until("the shootdown to finish", || done.load(Ordering::SeqCst));
        t.join().unwrap();
    }

    #[test]
    fn a_kernel_flush_targets_every_cpu_whatever_page_table_it_has() {
        // Global-bit entries survive a CR3 write, so no CPU may be filtered
        // out of a kernel-table flush.
        let _g = test_lock();
        let _smp = Smp::with(3);
        ACTIVE_VMTOKEN[1].store(0x11_000, Ordering::SeqCst);
        ACTIVE_VMTOKEN[2].store(0x22_000, Ordering::SeqCst);
        let done = alloc::sync::Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let t = std::thread::spawn(move || {
            remote_flush_tlb_on(0, None, None);
            flag.store(true, Ordering::SeqCst);
        });
        wait_until_waiting(0, &done);
        assert_eq!(shootdown_wait_mask(0), 0b110);
        tlb_shootdown_ack_on(1);
        tlb_shootdown_ack_on(2);
        wait_until("the shootdown to finish", || done.load(Ordering::SeqCst));
        t.join().unwrap();
    }

    #[test]
    fn an_overflowed_send_is_not_acknowledged_by_a_watermark_it_never_advanced() {
        // A send that could not fit advances no queue index, so the goal it
        // would otherwise wait on -- `SEQ >= ptail` -- may already be met by a
        // previous drain. Here that is made exact: the target's watermark is
        // put level with its tail, as a drain that predates the request would
        // leave it. Only the overflow generation still separates them, and
        // dropping that condition frees the frame with the request never
        // serviced.
        let _g = test_lock();
        let _smp = Smp::with(2);
        while publish_ipi_entry(1, IpiReason::TlbShutdown { vpn: 1 }.into()) && !overflow_pending(1)
        {
        }
        clear_overflow(1);
        SHOOTDOWN_SEQ[1].store(tail(1), Ordering::SeqCst);
        let gen_before = IPI_OVERFLOW_GEN[1].load(Ordering::Acquire);

        let done = alloc::sync::Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let t = std::thread::spawn(move || {
            remote_flush_tlb_on(0, Some(0x8000), None);
            flag.store(true, Ordering::SeqCst);
        });
        wait_until_waiting(0, &done);
        assert!(
            IPI_OVERFLOW_GEN[1].load(Ordering::Acquire) > gen_before,
            "the send had nowhere to put its payload, so it must note one"
        );
        assert_eq!(
            seq(1),
            tail(1),
            "the tail alone says the request was serviced, and it was not"
        );
        tlb_shootdown_ack_on(1);
        wait_until("the shootdown to finish", || done.load(Ordering::SeqCst));
        t.join().unwrap();
    }

    #[test]
    fn two_cpus_that_signal_each_other_at_once_do_not_deadlock() {
        // Each is waiting on the other's acknowledgement, so each has to
        // service the other's request while it waits. Without that self-pump
        // this is a deadlock with no timeout on either side.
        let _g = test_lock();
        let _smp = Smp::with(2);
        // A CPU does not stop taking IPIs when its own shootdown returns, and
        // here it has to be said out loud: a "CPU" in this test lives only as
        // long as its thread. The wait loop breaks the instant its targets
        // have acked, *before* its last self-pump, so whichever thread
        // finishes first leaves the other's request sitting in a queue no
        // thread will ever drain again — and the other side then spins out the
        // deadline for an acknowledgement that has no author. That is this
        // test's own model failing, not the protocol: on hardware cpu 0 keeps
        // running and takes the interrupt. So each thread keeps servicing its
        // queue until both shootdowns are done. It cost a 60-second hang in
        // about one host run in five.
        let done = alloc::sync::Arc::new([AtomicBool::new(false), AtomicBool::new(false)]);
        let spawn = |me: usize, page: usize| {
            let done = done.clone();
            std::thread::spawn(move || {
                remote_flush_tlb_on(me, Some(page), None);
                done[me].store(true, Ordering::SeqCst);
                while !done[1 - me].load(Ordering::SeqCst) {
                    tlb_shootdown_ack_on(me);
                    std::thread::yield_now();
                }
            })
        };
        let a = spawn(0, 0x1000);
        let b = spawn(1, 0x2000);
        // A real deadlock never finishes, so the deadline only decides how
        // long we wait before calling it one: generous costs nothing, and a
        // tight one turns a slow runner into a failure. And this thread
        // sleeps rather than spinning -- `yield_now` in a loop keeps a third
        // thread runnable while the two that matter are pumping each other,
        // which on a two-vCPU CI runner already busy with the rest of the
        // matrix is exactly how a live pair of shootdowns misses its budget.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !(a.is_finished() && b.is_finished()) {
            assert!(
                std::time::Instant::now() < deadline,
                "two simultaneous shootdowns deadlocked on each other"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        a.join().unwrap();
        b.join().unwrap();
    }

    #[test]
    fn a_request_the_target_never_saw_is_sent_again() {
        // The enqueue/signal race: the target takes the interrupt as a pure
        // wake just before the entry becomes visible, then never pumps again
        // because it is alive with interrupts on and contending for nothing.
        // The entry sits in a queue nobody will look at, and the wait has no
        // timeout, so the re-send is the only thing that heals it.
        let _g = test_lock();
        let _smp = Smp::with(2);
        let done = alloc::sync::Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let t = std::thread::spawn(move || {
            remote_flush_tlb_on(0, Some(0x1000), None);
            flag.store(true, Ordering::SeqCst);
        });
        wait_until_waiting(0, &done);
        // Swallow the request without acknowledging it, as that lost wake did.
        let swallowed = tail(1);
        ipi_queue(1).unwrap().discard_entrys();
        wait_until("the initiator to send the request again", || {
            tail(1) > swallowed
        });
        tlb_shootdown_ack_on(1);
        wait_until("the shootdown to finish", || done.load(Ordering::SeqCst));
        t.join().unwrap();
    }

    // ── the NMI rescue ─────────────────────────────────────────────────────

    #[test]
    fn the_nmi_rescue_publishes_the_tail_its_flush_covered_not_the_one_after_it() {
        // The rescue's whole claim is "a full flush over-satisfies everything
        // enqueued up to this instant, so the current tail is publishable".
        // That holds only if the tail is read BEFORE the flush. Reading it
        // after acknowledges requests the flush predates — the initiator then
        // frees the frame while this CPU still has the mapping, which is
        // exactly what the consumed-index protocol exists to rule out. The
        // adjacent overflow snapshot already gets this right.
        let _g = test_lock();
        let _smp = Smp::with(2);
        SHOOTDOWN_ACK_ACTIVE[1].store(true, Ordering::SeqCst); // a drain is in flight
        let before = tail(1);
        flush_probe::during_next_flush(|| send(1, 0x77));
        tlb_shootdown_ack_nmi_on(1);

        assert_eq!(tail(1), before + 1, "the hook did commit during the flush");
        assert!(
            seq(1) <= before,
            "published {} for a flush that only covered up to {}",
            seq(1),
            before
        );
        SHOOTDOWN_ACK_ACTIVE[1].store(false, Ordering::SeqCst);
    }

    #[test]
    fn the_nmi_rescue_drains_and_acknowledges_when_no_drain_is_in_flight() {
        let _g = test_lock();
        let _smp = Smp::with(2);
        send(1, 0x55);
        let goal = tail(1);
        tlb_shootdown_ack_nmi_on(1);
        assert!(flush_probe::saw_full_flush());
        assert!(
            seq(1) >= goal,
            "the kick has to end the starvation it was sent for"
        );
        assert_eq!(ipi_queue(1).unwrap().chead(), goal as usize);
    }

    #[test]
    fn the_nmi_rescue_covers_what_arrived_while_its_own_drain_was_running() {
        // The drain publishes the index it consumed, which is a snapshot taken
        // before it started, so a request that commits while it runs is left
        // out of it. The kick was sent because a peer is already starving on
        // this CPU, so leaving that peer to a further round is the one thing
        // it must not do: the reinforcement flush covers everything enqueued
        // up to now and the tail read for it is publishable.
        let _g = test_lock();
        let _smp = Smp::with(2);
        send(1, 0xa1);
        flush_probe::during_next_flush(|| send(1, 0xa2));
        let arrives_during = tail(1) + 1;
        tlb_shootdown_ack_nmi_on(1);
        assert_eq!(
            tail(1),
            arrives_during,
            "the hook did commit during the drain"
        );
        assert!(
            seq(1) >= arrives_during,
            "published {} for a rescue whose flush covered up to {}",
            seq(1),
            arrives_during
        );
    }

    #[test]
    fn the_nmi_rescue_leaves_an_in_flight_drain_its_queue_and_still_acknowledges() {
        // The queue is single-consumer: re-entering it under a drain would
        // double-consume. Returning silently instead was how the starvation
        // survived the kick, so it flushes and publishes without touching it.
        let _g = test_lock();
        let _smp = Smp::with(2);
        send(1, 0x66);
        let goal = tail(1);
        let chead = ipi_queue(1).unwrap().chead();
        SHOOTDOWN_ACK_ACTIVE[1].store(true, Ordering::SeqCst);
        tlb_shootdown_ack_nmi_on(1);
        assert_eq!(
            ipi_queue(1).unwrap().chead(),
            chead,
            "the in-flight drain still owns the queue"
        );
        assert!(flush_probe::saw_full_flush());
        assert!(seq(1) >= goal);
        SHOOTDOWN_ACK_ACTIVE[1].store(false, Ordering::SeqCst);
    }

    #[test]
    fn the_nmi_rescue_ends_a_wait_that_nothing_else_could() {
        let _g = test_lock();
        let _smp = Smp::with(2);
        let done = alloc::sync::Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let t = std::thread::spawn(move || {
            remote_flush_tlb_on(0, Some(0x3000), None);
            flag.store(true, Ordering::SeqCst);
        });
        wait_until_waiting(0, &done);
        tlb_shootdown_ack_nmi_on(1);
        wait_until("the rescued shootdown to finish", || {
            done.load(Ordering::SeqCst)
        });
        t.join().unwrap();
    }

    // ── the id the protocol runs as ────────────────────────────────────────

    #[test]
    fn an_id_that_names_no_cpu_does_not_get_to_shift_or_index() {
        // `cpu_id()` returning an id that names no CPU is a documented failure
        // here, not a hypothetical. `1u64 << me` is undefined past 63 and on
        // x86 wraps to bit `me % 64`, so the mask drops some other CPU and
        // keeps us in it as a phantom target; then `SHOOTDOWN_WAIT_MASK[me]`
        // indexes a 64-entry table, in a spin loop with the caller's lock held.
        for raw in 0..MAX_CORE_NUM {
            assert_eq!(shootdown_self(raw), Some(raw), "cpu {} is a real cpu", raw);
        }
        assert_eq!(shootdown_self(MAX_CORE_NUM), None);
        assert_eq!(shootdown_self(MAX_CORE_NUM + 1), None);
        assert_eq!(shootdown_self(usize::MAX), None);
    }

    #[test]
    fn the_cpu_bitmasks_cover_every_cpu_the_tables_hold() {
        // `IPI_READY`, `CPU_ONLINE` and every wait mask
        // are one `u64`, while the per-CPU tables are `MAX_CORE_NUM` long. Let
        // those two drift apart and the CPUs above 63 get queues, watermarks
        // and goals that no mask can ever name: they are never targeted, never
        // marked online, and an overflow on one is never recorded.
        assert!(
            MAX_CORE_NUM <= u64::BITS as usize,
            "MAX_CORE_NUM is {} but the masks hold {} bits",
            MAX_CORE_NUM,
            u64::BITS
        );
    }

    #[test]
    fn smp_can_be_turned_off_and_back_on() {
        let _g = test_lock();
        let before = smp_enabled();
        assert!(before, "AP bring-up defaults on");
        set_smp_enabled(false);
        assert!(!smp_enabled());
        set_smp_enabled(before);
        assert_eq!(smp_enabled(), before);
    }
}

/// The escalation ladder of [`remote_flush_tlb_on`]'s ack wait. It is the only
/// part of that wait a host test can reach — the loop itself spins on other
/// CPUs — and it is the part that decides whether a starved shootdown ever
/// gets another chance.
#[cfg(test)]
mod escalation_tests {
    use super::*;

    const REKICK: u64 = 1 << REKICK_SHIFT;
    const ESCALATE: u64 = 1 << UNMASKABLE_SHIFT;

    #[test]
    fn the_healthy_fast_path_never_re_kicks() {
        // A shootdown acks within a handful of spins. Nothing on that path may
        // re-send an IPI: the cost of the ladder has to be zero when the
        // protocol is working.
        for spins in 0..1000u64 {
            assert!(!should_rekick(spins), "re-kicked at spin {}", spins);
            assert!(!should_escalate(spins, true), "escalated at spin {}", spins);
        }
    }

    #[test]
    fn the_re_kick_comes_first_and_then_every_period() {
        assert!(should_rekick(REKICK));
        assert!(should_rekick(REKICK * 2));
        assert!(!should_rekick(REKICK - 1));
        assert!(!should_rekick(REKICK + 1));
    }

    #[test]
    fn the_unmaskable_kick_is_much_rarer_than_the_re_kick() {
        // A lost wakeup is far more likely than a wedged CPU, so the cheap
        // rung has to get several tries before the expensive one fires at all
        // (the ordering itself is a `const _: () = assert!` at the shifts).
        let rekicks = (1..=ESCALATE).filter(|&s| should_rekick(s)).count();
        let escalations = (1..=ESCALATE).filter(|&s| should_escalate(s, true)).count();
        assert_eq!(escalations, 1);
        assert_eq!(rekicks, (ESCALATE / REKICK) as usize);
    }

    #[test]
    fn an_escalation_spin_re_kicks_as_well() {
        // The two rungs are not exclusive: the spin that escalates also sends
        // the ordinary IPI, so a target that was merely missing a wakeup is
        // still served by the cheap path on that iteration.
        assert!(should_escalate(ESCALATE, true));
        assert!(should_rekick(ESCALATE));
    }

    #[test]
    fn an_architecture_with_nothing_unmaskable_never_pretends_to_escalate() {
        // Where there is no kick that reaches a CPU with interrupts off,
        // calling an empty function on a schedule is not an escalation. Saying
        // so is the whole point: `slow ack wait` gets read as "not even the
        // NMI got through" on architectures that never sent one.
        for spins in [1u64, REKICK, ESCALATE, ESCALATE * 7, u64::MAX] {
            assert!(
                !should_escalate(spins, false),
                "escalated at spin {}",
                spins
            );
        }
        // ...and the re-kick, which needs nothing special, still runs there.
        assert!(should_rekick(ESCALATE));
    }
}

/// Which cpu ids this module will act on.
///
/// Every bit set in `CPU_ONLINE` or `IPI_READY` becomes
/// an index into a `[_; MAX_CORE_NUM]` table further down — a shootdown
/// target's `SHOOTDOWN_GOAL[me][cpu]`, its ack watermark, its overflow
/// counter — and none of those indexings is bounds-checked at the point of
/// use. So the two shapes have to agree, and the guards that admit an id are
/// the only place they can be made to.
#[cfg(test)]
mod cpu_id_bounds_tests {
    use super::*;

    /// `lock::current_cpu_id()`'s answer for a CPU that never got a dense
    /// logical id. It arrives here through `cpu_id()` like any other.
    const NO_CPU: usize = 255;

    /// Restore whichever masks a test disturbs.
    struct Masks {
        online: u64,
        ready: u64,
    }

    impl Masks {
        fn empty() -> Self {
            let me = Masks {
                online: CPU_ONLINE.load(Ordering::SeqCst),
                ready: IPI_READY.load(Ordering::SeqCst),
            };
            CPU_ONLINE.store(0, Ordering::SeqCst);
            IPI_READY.store(0, Ordering::SeqCst);
            me
        }
    }

    impl Drop for Masks {
        fn drop(&mut self) {
            CPU_ONLINE.store(self.online, Ordering::SeqCst);
            IPI_READY.store(self.ready, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_cpu_with_no_slot_is_never_marked_online() {
        let _g = test_lock();
        // `cpu_online_mask` is what `can_receive_ipi` gates on and what the
        // bring-up counts; a bit for a cpu with no per-CPU row is a target
        // nothing can look up.
        let _m = Masks::empty();
        mark_cpu_online(MAX_CORE_NUM);
        mark_cpu_online(NO_CPU);
        mark_cpu_online(usize::MAX);
        assert_eq!(cpu_online_mask(), 0);
    }

    #[test]
    fn a_cpu_with_no_slot_is_never_marked_ready_for_shootdowns() {
        let _g = test_lock();
        // This is the mask `remote_flush_tlb_on` turns straight into
        // `targets`, and every target is indexed into `SHOOTDOWN_GOAL[me][..]`
        // with no bound of its own.
        let _m = Masks::empty();
        mark_cpu_ipi_ready(MAX_CORE_NUM);
        mark_cpu_ipi_ready(NO_CPU);
        mark_cpu_ipi_ready(usize::MAX);
        assert_eq!(IPI_READY.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn an_overflow_noted_for_a_cpu_with_no_slot_indexes_nothing() {
        let _g = test_lock();
        // The sharpest of the three: the guard admitted the id and the very
        // next line indexed `IPI_OVERFLOW_GEN[cpuid]`, from the IPI publish
        // path — interrupt context, locks held.
        let _m = Masks::empty();
        let before: alloc::vec::Vec<u64> = (0..MAX_CORE_NUM)
            .map(|c| IPI_OVERFLOW_GEN[c].load(Ordering::SeqCst))
            .collect();
        note_ipi_queue_overflow(MAX_CORE_NUM);
        note_ipi_queue_overflow(NO_CPU);
        note_ipi_queue_overflow(usize::MAX);
        for cpu in 0..MAX_CORE_NUM {
            assert_eq!(
                IPI_OVERFLOW_GEN[cpu].load(Ordering::SeqCst),
                before[cpu],
                "cpu {} has no business being touched by those ids",
                cpu
            );
        }
        // And the demand is still read through the same guard.
        assert!(!overflow_pending(MAX_CORE_NUM));
        assert!(!overflow_pending(NO_CPU));
        assert!(!overflow_pending(usize::MAX));
    }

    #[test]
    fn every_cpu_the_guards_do_admit_has_a_row_in_every_table() {
        let _g = test_lock();
        // The invariant the three guards exist to keep. Stated over the whole
        // admitted range rather than over one id, so it still means something
        // on a build that lowers MAX_CORE_NUM.
        let _m = Masks::empty();
        for cpu in 0..MAX_CORE_NUM {
            mark_cpu_online(cpu);
            mark_cpu_ipi_ready(cpu);
        }
        let admitted = cpu_online_mask() | IPI_READY.load(Ordering::SeqCst);
        for_each_cpu(admitted, |cpu| {
            assert!(
                cpu < MAX_CORE_NUM,
                "cpu {} was admitted but indexes no per-CPU row",
                cpu
            );
            // Touch the rows a shootdown would, to say which ones they are.
            let _ = SHOOTDOWN_SEQ[cpu].load(Ordering::Relaxed);
            let _ = SHOOTDOWN_GOAL[0][cpu].load(Ordering::Relaxed);
            let _ = IPI_OVERFLOW_GEN[cpu].load(Ordering::Relaxed);
            let _ = ACTIVE_VMTOKEN[cpu].load(Ordering::Relaxed);
        });
    }

    #[test]
    fn a_cpu_with_no_slot_is_never_worth_kicking_even_if_a_mask_names_it() {
        let _g = test_lock();
        // Belt and braces: `wake_kick_wanted` bounds the id itself, so an
        // online mask corrupted into naming a cpu past the tables stops here.
        let _m = Masks::empty();
        CPU_ONLINE.store(u64::MAX, Ordering::SeqCst);
        assert!(!wake_kick_wanted(MAX_CORE_NUM));
        assert!(!wake_kick_wanted(NO_CPU));
        assert!(!wake_kick_wanted(usize::MAX));
    }
}
