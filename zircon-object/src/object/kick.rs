//! Telling a parked synchronous waiter that its object has something.

use super::{KernelObject, Signal};
use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicBool, Ordering};
use kernel_hal::thread::SyncKick;

/// While it lives, every time `object` raises one of the signals asked
/// for, the CPU of the thread that made it is told to take its parked
/// waiters up (a [`SyncKick`]).
///
/// For a wait that gives the CPU away with a `SyncWait` between looks at
/// an object -- the replies of a server's FIFO -- so that it is taken up
/// when the reply comes and not when its CPU next has nothing to do. Make
/// it on the thread that waits, before the first turn.
///
/// The waiter must still look for itself on every turn: a signal that
/// was already up raises nothing new, and what the kick promises is a
/// turn, not that the wait is over.
pub struct KickOn {
    over: Arc<AtomicBool>,
}

impl KickOn {
    /// Kick this thread's CPU on `signals` of `object` from now on.
    pub fn new(object: &dyn KernelObject, signals: Signal) -> Self {
        let over = Arc::new(AtomicBool::new(false));
        let kick = SyncKick::here();
        let done = over.clone();
        object.add_signal_callback(Box::new(move |now| {
            // A callback is dropped by answering `true`, at the next
            // change of the object's signals after the wait.
            if done.load(Ordering::Acquire) {
                return true;
            }
            if now.intersects(signals) {
                kick.kick();
            }
            false
        }));
        Self { over }
    }
}

impl Drop for KickOn {
    fn drop(&mut self) {
        self.over.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::DummyObject;

    #[test]
    fn the_callback_lives_as_long_as_the_wait() {
        let object = DummyObject::new();
        let kick = KickOn::new(&*object, Signal::READABLE);
        // Raised and cleared while the wait lasts: the callback stays.
        object.signal_set(Signal::READABLE);
        object.signal_clear(Signal::READABLE);
        assert_eq!(Arc::strong_count(&kick.over), 2);
        // The wait over, the next change of the signals drops it.
        let over = kick.over.clone();
        drop(kick);
        assert_eq!(Arc::strong_count(&over), 2);
        object.signal_set(Signal::READABLE);
        assert_eq!(Arc::strong_count(&over), 1);
    }
}
