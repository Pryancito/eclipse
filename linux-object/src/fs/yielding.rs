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

/// `try_` until it gives the lock, spinning [`SPINS`] times and then
/// giving the CPU away between tries.
fn take<G>(mut try_: impl FnMut() -> Option<G>) -> G {
    // A caller that holds a VMO's family lock must not park with it: the
    // next task of this CPU to want that lock spins on it with interrupts
    // off, for ever. No page is read from its file under that lock any more
    // (`zircon-object/src/vm/vmo/paged.rs`, `FillNeed`), so this is a
    // tripwire. A caller that holds some other lock still parks: the holder
    // of this one may itself be parked, waiting for a disk served from
    // userspace, and a caller that spun for it could be keeping the CPU
    // from that disk's driver -- every boot of a served root hung so in
    // `metadata`, which asks its inode with the file's lock held.
    if zircon_object::vm::family_lock_held_here() {
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
