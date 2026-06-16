use crate::{config::MAX_CORE_NUM, utils::mpsc_queue::MpscQueue};
use alloc::vec::Vec;

const REASON_SIZE: usize = 16;

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

pub(crate) fn ipi_queue(cpuid: usize) -> &'static IRQueue {
    &IPI_QUEUE[cpuid]
}

pub(crate) fn ipi_reason() -> Vec<usize> {
    let cpu_id = crate::cpu::cpu_id() as usize;
    let queue = ipi_queue(cpu_id);
    queue.consume_entrys().iter().map(|entry| entry.1).collect()
}

use core::sync::atomic::{AtomicU64, Ordering};

/// Bitmask of logical CPU ids that are actually online and able to service
/// IPIs. The BSP (logical 0) is always online; APs OR in their bit once they
/// reach `secondary_init`. Shootdowns only target online CPUs — APs that failed
/// to start (partial SMP bring-up) must not be signalled.
static CPU_ONLINE: AtomicU64 = AtomicU64::new(1);

/// Per-CPU count of completed full TLB flushes (a "flush generation"). Each CPU
/// bumps its own counter every time it services a shootdown. An initiator
/// snapshots a target's counter *after* it has modified the page table, then
/// waits for the counter to advance: that proves the target performed a full
/// flush after the request, so it can no longer hold a stale entry for the page
/// we are about to free. A monotonic per-CPU generation (rather than a shared
/// down-counter) makes concurrent shootdowns from several CPUs compose.
static TLB_GEN: [AtomicU64; MAX_CORE_NUM] = {
    const ZERO: AtomicU64 = AtomicU64::new(0);
    [ZERO; MAX_CORE_NUM]
};

/// Upper bound on the busy-wait for remote acks. Generous: under real
/// parallelism (KVM) acks land in microseconds; under single-threaded TCG the
/// initiator must spin long enough for the emulator to round-robin to the
/// target vCPU so it can service the IPI. On timeout we fall back to the old
/// best-effort behaviour (the remote still flushes on its next context switch)
/// rather than hang the unmap forever on a wedged CPU.
const ACK_SPIN_LIMIT: u64 = 200_000_000;

/// Mark a logical CPU id as online (called from each CPU's bring-up path).
pub fn mark_cpu_online(logical_id: usize) {
    if logical_id < 64 {
        CPU_ONLINE.fetch_or(1u64 << logical_id, Ordering::Release);
    }
}

/// Service a pending shootdown on the *current* CPU: full-flush its TLB, publish
/// the advanced generation so initiators waiting on us can proceed, then drop
/// the coalesced requests. Allocation-free so it is safe both from the IPI
/// handler (IRQs off) and from the self-service step of the wait loop below.
pub fn tlb_shootdown_ack() {
    let me = crate::cpu::cpu_id() as usize;
    crate::vm::flush_tlb(None);
    // Publish *after* the flush (Release): an initiator that observes the new
    // generation (Acquire) is guaranteed our flush has already happened.
    if me < MAX_CORE_NUM {
        TLB_GEN[me].fetch_add(1, Ordering::Release);
    }
    ipi_queue(me).drain_discard();
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
/// Synchronous: signals every other online CPU and waits until each has done a
/// full flush (its generation advances) before returning, so the caller may
/// safely free the unmapped frame. `vaddr` is advisory — a full flush is used
/// for simplicity/safety.
///
/// Deadlock-free despite running under the IRQ-disabled page-table spinlock:
/// while waiting we *self-service* any shootdown aimed at us, so two CPUs
/// cross-shooting-down (each spinning with IRQs off) still make progress
/// instead of wedging on each other. The wait is also bounded — a CPU that
/// never acks (still in early bring-up, wedged) drops us to best-effort rather
/// than hanging the unmap.
pub fn remote_flush_tlb(_vaddr: Option<usize>) {
    let me = crate::cpu::cpu_id() as usize;
    let online = CPU_ONLINE.load(Ordering::Acquire) & !(1u64 << me);
    if online == 0 {
        return; // we are the only online CPU
    }
    let reason: IpiEntry = IpiReason::TlbShutdown { vpn: 0 }.into();

    // Snapshot each target's flush generation *before* signalling it, so any
    // advance we later observe corresponds to a flush triggered by this request.
    let mut snapshot = [0u64; MAX_CORE_NUM];
    for cpu in 0..MAX_CORE_NUM {
        if online & (1u64 << cpu) != 0 {
            snapshot[cpu] = TLB_GEN[cpu].load(Ordering::Acquire);
            let _ = crate::interrupt::send_ipi(cpu, reason);
        }
    }

    // Wait for every signalled CPU to advance its generation.
    let mut pending = online;
    let mut spins: u64 = 0;
    while pending != 0 {
        // Self-service: honour any shootdown targeted at us while our IRQs are
        // off, so we cannot wedge another initiator that is waiting on us.
        if !ipi_queue(me).is_empty() {
            tlb_shootdown_ack();
        }
        let mut cpu = 0;
        while cpu < MAX_CORE_NUM {
            let bit = 1u64 << cpu;
            if pending & bit != 0 && TLB_GEN[cpu].load(Ordering::Acquire) != snapshot[cpu] {
                pending &= !bit;
            }
            cpu += 1;
        }
        if pending == 0 {
            break;
        }
        spins += 1;
        if spins >= ACK_SPIN_LIMIT {
            // Best-effort fallback: do not hang the unmap on a CPU that never
            // acked. The remote still flushes on its next context switch.
            break;
        }
        core::hint::spin_loop();
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum IpiReason {
    Invalid,
    MockBlock { block_info: usize },
    TlbShutdown { vpn: usize }, // unused
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
