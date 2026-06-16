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

/// Monotonic TLB-shootdown generation, bumped once per `remote_flush_tlb`.
static TLB_GEN: AtomicU64 = AtomicU64::new(0);
/// Highest generation each CPU has flushed through (indexed by logical id).
static CPU_ACKED_GEN: [AtomicU64; crate::config::MAX_CORE_NUM] =
    [const { AtomicU64::new(0) }; crate::config::MAX_CORE_NUM];

/// Receiver side of the TLB shootdown: flush this CPU's TLB and publish the
/// generation it has now satisfied. Called both from the IPI handler and from
/// an initiator's spin-wait (so two CPUs shooting down each other can't
/// deadlock even with interrupts disabled). The queue payload is irrelevant —
/// a full flush covers every pending request — so it is just drained and
/// discarded; this also makes the path robust to IPI-queue overflow.
pub fn tlb_shootdown_ack() {
    let me = crate::cpu::cpu_id() as usize;
    let gen = TLB_GEN.load(Ordering::Acquire);
    crate::vm::flush_tlb(None);
    let _ = ipi_queue(me).consume_entrys();
    CPU_ACKED_GEN[me].store(gen, Ordering::Release);
}

/// Synchronous cross-CPU TLB shootdown.
///
/// x86 `flush_tlb` only invalidates the *local* CPU's TLB. Without this, after
/// one CPU unmaps/reprotects a page (COW copy-break, munmap, address-space
/// teardown) the other CPUs keep stale TLB entries pointing at the now-freed
/// physical frame; once it is reallocated to another VMO/process those entries
/// read/write the wrong owner's memory — the cross-process and kernel↔user
/// corruption that only shows up under SMP load.
///
/// It is *synchronous*: it returns only once every other online CPU has flushed
/// at or beyond this call's generation, so the caller may safely free/reuse the
/// frame afterwards. Callers usually hold an IRQ-disabling spinlock, so the
/// wait loop drains its own pending shootdowns to avoid a mutual-wait deadlock.
/// `vaddr` is currently advisory (a full flush is used for simplicity/safety).
pub fn remote_flush_tlb(_vaddr: Option<usize>) {
    let n = crate::cpu::cpu_count() as usize;
    if n <= 1 {
        return;
    }
    let me = crate::cpu::cpu_id() as usize;
    let gen = TLB_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    CPU_ACKED_GEN[me].store(gen, Ordering::Release);
    let reason: IpiEntry = IpiReason::TlbShutdown { vpn: 0 }.into();
    for cpu in 0..n {
        if cpu != me {
            let _ = crate::interrupt::send_ipi(cpu, reason);
        }
    }
    for cpu in 0..n {
        if cpu == me {
            continue;
        }
        while CPU_ACKED_GEN[cpu].load(Ordering::Acquire) < gen {
            // Service peers (and drain our own queue) so we can't deadlock with
            // another CPU that is simultaneously waiting on us.
            tlb_shootdown_ack();
            core::hint::spin_loop();
        }
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
