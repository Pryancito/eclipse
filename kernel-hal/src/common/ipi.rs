use crate::{config::MAX_CORE_NUM, utils::mpsc_queue::MpscQueue};
use alloc::vec::Vec;

const REASON_SIZE: usize = 16;

pub type IpiEntry = usize;
type IRQueue = MpscQueue<'static, IpiEntry>;

/// Per-CPU backing storage for the IPI queues, indexed by dense logical CPU id.
static mut IPI_BUFFERS: [[IpiEntry; REASON_SIZE]; MAX_CORE_NUM] =
    [[0; REASON_SIZE]; MAX_CORE_NUM];

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

pub(crate) fn ipi_queue(cpuid: usize) -> &'static IRQueue {
    &IPI_QUEUE[cpuid]
}

pub(crate) fn ipi_reason() -> Vec<usize> {
    let cpu_id = crate::cpu::cpu_id() as usize;
    let queue = ipi_queue(cpu_id);
    queue.consume_entrys().iter().map(|entry| entry.1).collect()
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum IpiReason {
    Invalid,
    MockBlock { block_info: usize },
    TlbShutdown { vpn: usize }, // unused
    /// Kick a HLT'ed CPU so it rescans its run queue; no payload.
    Wake,
}

/// usize : 64bit
/// |  type reason : 4bit  |   ipi info : 60bit   |
///
/// MockBlock info : 60bit
/// |  reserved : 60 bit  |
///

const TYPE_SHIFT: usize = 60;
const TYPE_INVALID: usize = 0x0;
const TYPE_MOCK_BLOCK: usize = 0x1;
const TYPE_TLB_SHUTDOWN: usize = 0x2;
const TYPE_WAKE: usize = 0x3;

impl From<IpiEntry> for IpiReason {
    fn from(r: IpiEntry) -> Self {
        let ipi_type = r >> TYPE_SHIFT;
        let ipi_info = r & 0x000FFFFFFFFFFFFF;
        match ipi_type {
            TYPE_MOCK_BLOCK => Self::MockBlock {
                block_info: ipi_info,
            },
            TYPE_TLB_SHUTDOWN => Self::TlbShutdown { vpn: ipi_info },
            TYPE_WAKE => Self::Wake,
            _ => Self::Invalid,
        }
    }
}

impl From<IpiReason> for IpiEntry {
    fn from(reason: IpiReason) -> Self {
        match reason {
            IpiReason::MockBlock { block_info: info } => (TYPE_MOCK_BLOCK << TYPE_SHIFT) | info,
            IpiReason::TlbShutdown { vpn: info } => (TYPE_TLB_SHUTDOWN << TYPE_SHIFT) | info,
            IpiReason::Wake => TYPE_WAKE << TYPE_SHIFT,
            IpiReason::Invalid => 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Idle-CPU wake support
// ---------------------------------------------------------------------------
// The scheduler has per-CPU run queues, and idle CPUs HLT until the next
// interrupt. Without an explicit kick, a task woken (or spawned) onto an idle
// CPU's queue sits there until that CPU's periodic timer tick — adding up to a
// full tick of latency to *every* cross-CPU wakeup. The executor calls
// [`wake_cpu_if_idle`] after marking a task runnable; the idle loop brackets
// its HLT with [`idle_enter`]/[`idle_exit`].
//
// The `WAKE_PENDING` flag closes the race where the waker reads `IDLE == false`
// while the target is already committed to sleeping: both sides use SeqCst, so
// either the waker observes `IDLE == true` (and sends the IPI), or the sleeper
// observes `WAKE_PENDING == true` in `idle_should_skip_halt` (and skips HLT).

use core::sync::atomic::{AtomicBool, Ordering};

#[allow(clippy::declare_interior_mutable_const)]
const IDLE_FALSE: AtomicBool = AtomicBool::new(false);
static IDLE_CPUS: [AtomicBool; MAX_CORE_NUM] = [IDLE_FALSE; MAX_CORE_NUM];
static WAKE_PENDING: [AtomicBool; MAX_CORE_NUM] = [IDLE_FALSE; MAX_CORE_NUM];

/// Mark the current CPU as about to HLT.
pub fn idle_enter(cpu_id: usize) {
    if let Some(flag) = IDLE_CPUS.get(cpu_id) {
        flag.store(true, Ordering::SeqCst);
    }
}

/// Consume a pending wake; when `true` the caller must skip the HLT.
pub fn idle_should_skip_halt(cpu_id: usize) -> bool {
    WAKE_PENDING
        .get(cpu_id)
        .map(|f| f.swap(false, Ordering::SeqCst))
        .unwrap_or(false)
}

/// Mark the current CPU as running again (after HLT or a skipped HLT).
pub fn idle_exit(cpu_id: usize) {
    if let Some(flag) = IDLE_CPUS.get(cpu_id) {
        flag.store(false, Ordering::SeqCst);
    }
    // Any pending wake is satisfied by the run-queue rescan that follows.
    if let Some(flag) = WAKE_PENDING.get(cpu_id) {
        flag.store(false, Ordering::SeqCst);
    }
}

/// Kick `cpu_id` out of HLT if it is (or is about to go) idle.
///
/// Safe to call from IRQ context; does nothing for the calling CPU itself
/// (it is evidently running) or for out-of-range ids.
pub fn wake_cpu_if_idle(cpu_id: usize) {
    if cpu_id >= MAX_CORE_NUM || cpu_id == crate::cpu::cpu_id() as usize {
        return;
    }
    WAKE_PENDING[cpu_id].store(true, Ordering::SeqCst);
    if IDLE_CPUS[cpu_id].load(Ordering::SeqCst) {
        // ICR writes are two MMIO stores in xAPIC mode; mask interrupts so an
        // IRQ handler on this CPU cannot interleave another IPI send.
        let was_enabled = crate::interrupt::intr_get();
        if was_enabled {
            crate::interrupt::intr_off();
        }
        let _ = crate::interrupt::send_ipi(cpu_id, IpiReason::Wake.into());
        if was_enabled {
            crate::interrupt::intr_on();
        }
    }
}
