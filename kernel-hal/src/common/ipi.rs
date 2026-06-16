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

/// Broadcast a TLB-shootdown IPI to every *other* online CPU.
///
/// x86 `flush_tlb` only invalidates the *local* CPU's TLB. Without this, after
/// one CPU unmaps a page (COW copy-break, munmap, process/address-space
/// teardown) the other CPUs keep stale TLB entries that still point at the
/// now-freed physical frame; once that frame is reallocated to another
/// VMO/process the stale entries read/write the wrong owner's memory — the
/// cross-process and kernel↔user corruption that only appears under SMP load.
///
/// Asynchronous (fire-and-forget): remote CPUs flush when they next take the
/// IPI. `vaddr = None` (encoded as `vpn = 0`) requests a full remote flush.
pub fn remote_flush_tlb(vaddr: Option<usize>) {
    let n = crate::cpu::cpu_count() as usize;
    if n <= 1 {
        return;
    }
    let me = crate::cpu::cpu_id() as usize;
    let vpn = vaddr.map(|v| v >> 12).unwrap_or(0);
    let reason: IpiEntry = IpiReason::TlbShutdown { vpn }.into();
    for cpu in 0..n {
        if cpu == me {
            continue;
        }
        let _ = crate::interrupt::send_ipi(cpu, reason);
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
