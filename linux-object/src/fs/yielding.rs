//! Locks for a filesystem whose holder may wait for its disk.
//!
//! A disk served from userspace (`loader/src/linux/block_serve.rs`) is
//! waited for by giving the CPU away ([`SyncWait`]): the caller's poll is
//! parked, and the CPU runs its other tasks, the driver among them. A
//! filesystem read under a spin lock is then a parked holder, and any task
//! of the same CPU that spins on that lock -- a second writer, the
//! filesystem's own committer -- keeps the CPU for ever: the driver never
//! runs, the holder is never taken up again, and the machine stops.
//!
//! These locks spin a little, as a spin lock does, and then give the CPU
//! away between tries, the way the disk's wait does, so the holder and the
//! driver it waits for run.

use kernel_hal::thread::SyncWait;
use lock::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Tries before the CPU is given away: a holder that does not wait for a
/// disk lets go well before.
const SPINS: usize = 128;

/// The lock depth every task runs at with no lock of its own (the measure
/// `loader/src/linux/block_serve.rs` uses too).
const TASK_LOCK_DEPTH: i32 = 1;

/// `try_` until it gives the lock, spinning [`SPINS`] times and then
/// giving the CPU away between tries.
fn take<G>(mut try_: impl FnMut() -> Option<G>) -> G {
    // A caller that holds a spin lock of its own -- a page of a program is
    // read from its file with the VMO's lock held -- must not park with
    // it: the next task of this CPU to want that lock would spin on it for
    // ever. It spins here instead, as it would on a plain spin lock; the
    // holder of this one is running, or parked holding nothing. The VMO's
    // lock is asked about by name: in the page fault a syscall takes on
    // user memory, the depth alone did not show it.
    if lock::lock_depth() > TASK_LOCK_DEPTH || zircon_object::vm::family_lock_held_here() {
        loop {
            if let Some(guard) = try_() {
                return guard;
            }
            core::hint::spin_loop();
        }
    }
    for _ in 0..SPINS {
        if let Some(guard) = try_() {
            return guard;
        }
        core::hint::spin_loop();
    }
    // Dropped after the guard is taken: the thread is taken up again, with
    // its address space (nothing in between touches user memory).
    let mut away = SyncWait::new();
    loop {
        away.yield_now();
        if let Some(guard) = try_() {
            return guard;
        }
    }
}

/// A [`Mutex`] that gives the CPU away while it waits.
pub(crate) struct YieldingMutex<T>(Mutex<T>);

impl<T> YieldingMutex<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(Mutex::new(value))
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, T> {
        take(|| self.0.try_lock())
    }
}

/// The exclusive side of `rw`, giving the CPU away while it waits.
pub(crate) fn write<T>(rw: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    take(|| rw.try_write())
}

/// The shared side of `rw`, giving the CPU away while it waits.
pub(crate) fn read<T>(rw: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    take(|| rw.try_read())
}
