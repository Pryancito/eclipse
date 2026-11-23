//! Per-process kernel memory quota: what a process may leave in the kernel's
//! heap, and the part of that heap no process may touch at all.
//!
//! A kernel heap allocation that fails calls `alloc_error`, which panics, and
//! the panic is contained only when nothing in the kernel is left holding a
//! lock -- which on an allocation path is rarely the case. So the only kernel
//! OOM that does not end the machine is the one that never happens. Most of
//! the heap a process can take is bytes it parks in a queue the kernel keeps
//! for it: a unix socket holds up to [`UNIX_STREAM_BUF_MAX`] per end, and a
//! process can open thousands of them. Before this module every one of those
//! bytes was an infallible `VecDeque::extend`, and a process that filled a
//! few hundred socket pairs without reading them took the heap to its
//! ceiling and the machine with it.
//!
//! Two limits, both checked when such a byte is queued, never when the
//! kernel allocates for itself:
//!
//! * **The quota.** A process may hold at most [`KmemAccount::limit`] bytes,
//!   by default a [`DEFAULT_SHARE_DIV`]th of the most the heap can reach, so
//!   that one runaway process leaves the heap to everybody else.
//!   `/proc/<pid>/kmem_limit` sets it; a fork inherits it.
//! * **The reserve.** No charged allocation may take the heap's last
//!   [`RESERVE_DIV`]th, whoever asks: that part is for the kernel's own,
//!   infallible allocations, the ones that would panic.
//!
//! What goes over either is `ENOMEM` to the process that asked, which is what
//! Linux answers when `__GFP_ACCOUNT` memory is over its memory cgroup's
//! limit. The bytes are charged to the process that **wrote** them, as
//! Linux charges a socket's queued bytes to its sender (`sk_wmem_alloc`) and
//! a pipe's pages to the writer's cgroup, and given back when they are read
//! or the queue is freed -- by whoever reads them, even after the writer has
//! exited, which is why a charge holds its account by `Arc`.
//!
//! [`UNIX_STREAM_BUF_MAX`]: crate::net::UNIX_STREAM_BUF_MAX

use alloc::{collections::VecDeque, sync::Arc};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::process::ProcessExt as _;

/// A process may hold this share of the most the heap can reach unless its
/// limit says otherwise: a quarter, so that it takes four runaway processes
/// at once to reach the reserve.
pub const DEFAULT_SHARE_DIV: usize = 4;

/// The share of the most the heap can reach that no charged allocation may
/// take: an eighth, left to the kernel's own allocations.
pub const RESERVE_DIV: usize = 8;

/// Why a charge was refused. Both are `ENOMEM` to the process; they are told
/// apart in `/proc/<pid>/status` and in the log, because they point at
/// different culprits: the process itself, or the whole machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The process would go over its own limit.
    Quota,
    /// The heap would go into the kernel's reserve.
    Reserve,
}

impl From<Refusal> for crate::error::LxError {
    fn from(_: Refusal) -> Self {
        crate::error::LxError::ENOMEM
    }
}

impl From<Refusal> for rcore_fs::vfs::FsError {
    fn from(_: Refusal) -> Self {
        rcore_fs::vfs::FsError::NoMemory
    }
}

/// The heap's figures one decision is made on, in bytes. `reach` is asked
/// only on the slow path: on x86_64 it sums a per-CPU counter.
struct Heap<R: FnOnce() -> usize> {
    used: usize,
    total: usize,
    reach: R,
}

/// Whether `n` more bytes, which would bring a process to `after`, may be
/// charged under its limit `own` (`0`: the default share).
///
/// The fast path decides from the heap's size now, which the most it can
/// reach is never below: under a quarter of it, and with the heap out of its
/// reserve even then, nothing further can refuse. Only near either edge is
/// the reach asked for. A `total` of `0` is a heap of unknown size (the host
/// build): only an explicit limit applies.
fn admit<R: FnOnce() -> usize>(
    after: usize,
    n: usize,
    own: usize,
    heap: Heap<R>,
) -> Result<(), Refusal> {
    let Heap { used, total, reach } = heap;
    if total == 0 {
        return if own != 0 && after > own {
            Err(Refusal::Quota)
        } else {
            Ok(())
        };
    }
    let quota_fast = if own != 0 {
        own
    } else {
        total / DEFAULT_SHARE_DIV
    };
    let floor_fast = total - total / RESERVE_DIV;
    if after <= quota_fast && used.saturating_add(n) <= floor_fast {
        return Ok(());
    }
    let reach = reach().max(total);
    let quota = if own != 0 {
        own
    } else {
        reach / DEFAULT_SHARE_DIV
    };
    if after > quota {
        return Err(Refusal::Quota);
    }
    if used.saturating_add(n) > reach - reach / RESERVE_DIV {
        return Err(Refusal::Reserve);
    }
    Ok(())
}

/// One process's kernel memory: what it holds now, the most it has held, its
/// limit, and how often it was refused.
///
/// One per process, shared by its threads, outside every lock: a charge is
/// two atomic adds and, near the edges, a read of the heap's figures.
#[derive(Debug, Default)]
pub struct KmemAccount {
    charged: AtomicUsize,
    peak: AtomicUsize,
    /// `0`: the default share of the heap, computed when it is asked.
    limit: AtomicUsize,
    refused_quota: AtomicU64,
    refused_reserve: AtomicU64,
}

impl KmemAccount {
    /// A new account with its limit (`0`: the default share).
    pub fn with_limit(limit: usize) -> Arc<Self> {
        Arc::new(KmemAccount {
            limit: AtomicUsize::new(limit),
            ..Default::default()
        })
    }

    /// The account a fork's child starts with: nothing charged, the parent's
    /// limit. What the parent holds stays the parent's.
    pub fn forked(&self) -> Arc<Self> {
        Self::with_limit(self.limit_setting())
    }

    /// Charge `n` bytes, or refuse them and charge nothing.
    pub fn try_charge(&self, n: usize) -> Result<(), Refusal> {
        // The cheap reading: a check on every queued byte must not sum a
        // counter over every CPU, and a few batches of slack on a reserve
        // of an eighth of the heap change nothing.
        let (used, total) = kernel_hal::mem::kernel_heap_usage_approx();
        self.try_charge_on(
            n,
            Heap {
                used,
                total,
                reach: kernel_hal::mem::kernel_heap_limit,
            },
        )
    }

    fn try_charge_on<R: FnOnce() -> usize>(&self, n: usize, heap: Heap<R>) -> Result<(), Refusal> {
        if n == 0 {
            return Ok(());
        }
        let after = self
            .charged
            .fetch_add(n, Ordering::Relaxed)
            .saturating_add(n);
        match admit(after, n, self.limit.load(Ordering::Relaxed), heap) {
            Ok(()) => {
                // A load first: the peak moves rarely, and a `fetch_max` is
                // a locked read-modify-write on every queued byte otherwise.
                if after > self.peak.load(Ordering::Relaxed) {
                    self.peak.fetch_max(after, Ordering::Relaxed);
                }
                Ok(())
            }
            Err(why) => {
                self.charged.fetch_sub(n, Ordering::Relaxed);
                let counter = match why {
                    Refusal::Quota => &self.refused_quota,
                    Refusal::Reserve => &self.refused_reserve,
                };
                counter.fetch_add(1, Ordering::Relaxed);
                REFUSED.fetch_add(1, Ordering::Relaxed);
                Err(why)
            }
        }
    }

    /// Give back `n` bytes charged earlier.
    pub fn uncharge(&self, n: usize) {
        let prev = self.charged.fetch_sub(n, Ordering::Relaxed);
        debug_assert!(prev >= n, "kmem: uncharged {} of {} charged", n, prev);
    }

    /// Bytes charged now.
    pub fn charged(&self) -> usize {
        self.charged.load(Ordering::Relaxed)
    }

    /// The most ever charged at once.
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }

    /// The limit as set: `0` is the default share.
    pub fn limit_setting(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    /// The limit in force now, in bytes; `0` when the heap's size is unknown
    /// and none was set.
    pub fn limit(&self) -> usize {
        match self.limit_setting() {
            0 => default_limit(),
            own => own,
        }
    }

    /// Set the limit (`0`: back to the default share). A limit below what
    /// is charged takes nothing back; it refuses the next charge.
    pub fn set_limit(&self, limit: usize) {
        self.limit.store(limit, Ordering::Relaxed);
    }

    /// Charges refused by the limit, and by the reserve.
    pub fn refused(&self) -> (u64, u64) {
        (
            self.refused_quota.load(Ordering::Relaxed),
            self.refused_reserve.load(Ordering::Relaxed),
        )
    }
}

/// The default share now: a [`DEFAULT_SHARE_DIV`]th of the most the heap can
/// reach, `0` when that is unknown.
pub fn default_limit() -> usize {
    let (_, total) = kernel_hal::mem::kernel_heap_usage();
    kernel_hal::mem::kernel_heap_limit().max(total) / DEFAULT_SHARE_DIV
}

/// Charges refused since boot, all processes and both reasons.
static REFUSED: AtomicU64 = AtomicU64::new(0);

/// Charges refused since boot, for `/proc/meminfo`.
pub fn refused_total() -> u64 {
    REFUSED.load(Ordering::Relaxed)
}

/// The account of the process making the call; `None` for the kernel's own
/// work, which is never charged.
pub fn current() -> Option<Arc<KmemAccount>> {
    let arc = kernel_hal::thread::get_current_thread()?;
    let thread = arc.downcast::<zircon_object::task::Thread>().ok()?;
    let proc = thread.proc();
    Some(proc.try_linux()?.kmem().clone())
}

/// A run of queued bytes and who they are charged to.
#[derive(Debug)]
struct Run {
    bytes: usize,
    to: Option<Arc<KmemAccount>>,
}

/// What one run costs besides its bytes, charged with its first byte: a
/// writer that alternated with another one byte at a time would otherwise
/// fill a queue's bookkeeping with runs nobody is charged for.
const RUN_COST: usize = core::mem::size_of::<Run>();

/// The charges of a byte queue (a socket's or a pipe's), in the queue's
/// order: bytes go in at the back charged to their writer and come out of
/// the front uncharged from whoever they were charged to.
///
/// Consecutive bytes of one writer are one run, so the common queue -- one
/// writer -- has one. Dropping it gives everything back, which is what a
/// queue freed with bytes still in it needs.
#[derive(Debug, Default)]
pub struct ChargedBytes {
    runs: VecDeque<Run>,
}

impl ChargedBytes {
    /// No bytes, no charges.
    pub const fn new() -> Self {
        ChargedBytes {
            runs: VecDeque::new(),
        }
    }

    /// Charge `n` bytes about to go in at the back to the calling process.
    pub fn charge(&mut self, n: usize) -> Result<(), Refusal> {
        self.charge_to(current(), n)
    }

    /// Charge `n` bytes about to go in at the back to `to` (`None`: the
    /// kernel's own, uncharged). All or nothing.
    pub fn charge_to(&mut self, to: Option<Arc<KmemAccount>>, n: usize) -> Result<(), Refusal> {
        if n == 0 {
            return Ok(());
        }
        let same = match (self.runs.back(), &to) {
            (Some(back), Some(to)) => back.to.as_ref().is_some_and(|b| Arc::ptr_eq(b, to)),
            (Some(back), None) => back.to.is_none(),
            (None, _) => false,
        };
        if same {
            if let Some(to) = &to {
                to.try_charge(n)?;
            }
            self.runs.back_mut().unwrap().bytes += n;
            return Ok(());
        }
        let cost = if to.is_some() { n + RUN_COST } else { n };
        if let Some(to) = &to {
            to.try_charge(cost)?;
        }
        if self.runs.try_reserve(1).is_err() {
            if let Some(to) = &to {
                to.uncharge(cost);
            }
            return Err(Refusal::Reserve);
        }
        self.runs.push_back(Run {
            bytes: if to.is_some() { n + RUN_COST } else { n },
            to,
        });
        // The run's own cost rides on its byte count and goes back with
        // its last byte: see `release`.
        Ok(())
    }

    /// `n` bytes have come out of the front: give back their charges.
    pub fn release(&mut self, mut n: usize) {
        while n > 0 {
            let Some(front) = self.runs.front_mut() else {
                return;
            };
            let overhead = if front.to.is_some() { RUN_COST } else { 0 };
            let held = front.bytes - overhead;
            if n < held {
                front.bytes -= n;
                if let Some(to) = &front.to {
                    to.uncharge(n);
                }
                return;
            }
            n -= held;
            // The last run stays, emptied, with its bookkeeping still
            // charged: a queue its one writer fills and its reader drains
            // on every call (every pipe and socket in a request/response
            // loop) would otherwise charge, push, pop and uncharge a run
            // per message. Its writer's next bytes join it; anybody else's
            // start a new run behind it, and the empty one goes at the next
            // release. Dropping the queue gives its bookkeeping back.
            if self.runs.len() == 1 {
                let front = self.runs.front_mut().unwrap();
                if let Some(to) = &front.to {
                    to.uncharge(held);
                }
                front.bytes = overhead;
                return;
            }
            let run = self.runs.pop_front().unwrap();
            if let Some(to) = &run.to {
                to.uncharge(run.bytes);
            }
        }
    }

    /// Bytes charged to anybody, bookkeeping included.
    pub fn charged(&self) -> usize {
        self.runs
            .iter()
            .filter(|r| r.to.is_some())
            .map(|r| r.bytes)
            .sum()
    }
}

impl Drop for ChargedBytes {
    fn drop(&mut self) {
        for run in self.runs.drain(..) {
            if let Some(to) = &run.to {
                to.uncharge(run.bytes);
            }
        }
    }
}

/// A charge for one kernel object a process made the kernel keep -- a
/// queued message, a watch, a timer -- given back when it is dropped, by
/// whoever drops it.
///
/// [`ChargedBytes`] is for a byte queue, where bytes of many writers come
/// and go in order; this is for an object that is made and freed whole. It
/// is the size of a pointer and a length, and the kernel's own work (no
/// calling process) costs nothing: no account, nothing charged.
#[derive(Debug, Default)]
pub struct Charge {
    to: Option<Arc<KmemAccount>>,
    bytes: usize,
}

impl Charge {
    /// Nothing charged to anybody.
    pub const fn none() -> Self {
        Charge { to: None, bytes: 0 }
    }

    /// Charge `n` bytes to the calling process, or refuse them.
    pub fn try_new(n: usize) -> Result<Self, Refusal> {
        Self::try_to(current(), n)
    }

    /// Charge `n` bytes to `to` (`None`: the kernel's own, uncharged), or
    /// refuse them.
    pub fn try_to(to: Option<Arc<KmemAccount>>, n: usize) -> Result<Self, Refusal> {
        match to {
            Some(acct) if n != 0 => {
                acct.try_charge(n)?;
                Ok(Charge {
                    to: Some(acct),
                    bytes: n,
                })
            }
            _ => Ok(Self::none()),
        }
    }

    /// Bytes this charge holds.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// The account the bytes are charged to.
    pub fn account(&self) -> Option<&Arc<KmemAccount>> {
        self.to.as_ref()
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        if let Some(to) = &self.to {
            to.uncharge(self.bytes);
        }
    }
}

#[cfg(test)]
#[path = "kmem_tests.rs"]
mod kmem_tests;
