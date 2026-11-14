//! A card of the kernel's, lent to a network stack that is not the kernel's.
//!
//! The kernel's drivers each carry the kernel's own stack above their
//! registers: an interrupt ends in a poll of smoltcp, which reads the frames
//! off the card. A machine whose network is a stack that runs as a process
//! (`tools/eclipse-netstack`) has no use for that half, and until that stack
//! has drivers of its own for these cards it borrows them: the driver keeps
//! the registers, the rings and the interrupt, and hands every frame it
//! receives, whole, to a [`FrameTaker`] instead of to smoltcp. What the
//! borrower wants sent goes out through [`NetScheme::send`], which was
//! always a frame straight to the card.
//!
//! A driver that can be lent keeps a [`Lent`] and looks at it where it
//! would poll the kernel's stack ([`NetScheme::lend`] sets it). The other
//! half, the borrower and how a process reaches it, is in the loader
//! (`eclipse_net_borrow`); this half knows nothing about processes.
//!
//! [`NetScheme::send`]: crate::scheme::NetScheme::send
//! [`NetScheme::lend`]: crate::scheme::NetScheme::lend

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::sync::Mutex;
use crate::{DeviceError, DeviceResult};

/// The most frames a driver hands over for one interrupt or poll: what it
/// leaves on the card brings it back.
pub const BURST: usize = 256;

/// Who takes the frames of a lent card.
pub trait FrameTaker: Send + Sync {
    /// A frame the card received: an Ethernet frame as the wire had it,
    /// without the checksum.
    fn take(&self, frame: Vec<u8>);
    /// The driver is through with what brought it, an interrupt or a poll:
    /// the frames given since the last call are all there are for now, and
    /// a card that had no room for a frame to send may have it again.
    fn done(&self);
}

/// Where a driver keeps the taker of its card: empty while the card is the
/// kernel's own.
#[derive(Clone)]
pub struct Lent {
    taker: Arc<Mutex<Option<Arc<dyn FrameTaker>>>>,
    /// Whether there is a taker, without the lock: this is looked at on
    /// every interrupt of every card, lent or not.
    on: Arc<AtomicBool>,
}

impl Default for Lent {
    fn default() -> Self {
        Self {
            taker: Arc::new(Mutex::new(None)),
            on: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Lent {
    /// Lend the card to `taker`, or take it back with `None`. A card has
    /// one borrower at a time: lending one that is lent is refused.
    pub fn set(&self, taker: Option<Arc<dyn FrameTaker>>) -> DeviceResult {
        let mut held = self.taker.lock();
        if taker.is_some() && held.is_some() {
            return Err(DeviceError::AlreadyExists);
        }
        self.on.store(taker.is_some(), Ordering::Release);
        *held = taker;
        Ok(())
    }

    /// Who the card's frames go to, if it is lent.
    pub fn taker(&self) -> Option<Arc<dyn FrameTaker>> {
        if !self.on.load(Ordering::Acquire) {
            return None;
        }
        self.taker.lock().clone()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A taker that keeps what it is given.
    #[derive(Default)]
    pub(crate) struct Kept {
        pub frames: Mutex<Vec<Vec<u8>>>,
        pub dones: Mutex<usize>,
    }

    impl FrameTaker for Kept {
        fn take(&self, frame: Vec<u8>) {
            self.frames.lock().push(frame);
        }
        fn done(&self) {
            *self.dones.lock() += 1;
        }
    }

    #[test]
    fn a_card_has_one_borrower_at_a_time() {
        let lent = Lent::default();
        assert!(lent.taker().is_none());
        let first: Arc<dyn FrameTaker> = Arc::new(Kept::default());
        assert_eq!(lent.set(Some(first.clone())), Ok(()));
        assert!(Arc::ptr_eq(&lent.taker().unwrap(), &first));
        // Another while it is lent: refused, and the first keeps it.
        let second: Arc<dyn FrameTaker> = Arc::new(Kept::default());
        assert_eq!(
            lent.set(Some(second.clone())),
            Err(DeviceError::AlreadyExists)
        );
        assert!(Arc::ptr_eq(&lent.taker().unwrap(), &first));
        // Taken back, twice over, and lent again.
        assert_eq!(lent.set(None), Ok(()));
        assert_eq!(lent.set(None), Ok(()));
        assert!(lent.taker().is_none());
        assert_eq!(lent.set(Some(second.clone())), Ok(()));
        assert!(Arc::ptr_eq(&lent.taker().unwrap(), &second));
        // A copy of the place is the same place: a driver's interrupt
        // half and its interface each have one.
        let copy = lent.clone();
        assert_eq!(copy.set(None), Ok(()));
        assert!(lent.taker().is_none());
    }
}
