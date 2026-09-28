use crate::object::*;
use alloc::sync::{Arc, Weak};

/// Mutually signalable pair of events for concurrent programming
///
/// ## SYNOPSIS
///
/// Event Pairs are linked pairs of user-signalable objects. The 8 signal
/// bits reserved for userspace (`ZX_USER_SIGNAL_0` through
/// `ZX_USER_SIGNAL_7`) may be set or cleared on the local or opposing
/// endpoint of an Event Pair.
pub struct EventPair {
    base: KObjectBase,
    _counter: CountHelper,
    peer: Weak<EventPair>,
}

impl_kobject!(EventPair
    fn allowed_signals(&self) -> Signal {
        Signal::USER_ALL | Signal::SIGNALED
    }
    fn peer(&self) -> ZxResult<Arc<dyn KernelObject>> {
        let peer = self.peer.upgrade().ok_or(ZxError::PEER_CLOSED)?;
        Ok(peer)
    }
    fn related_koid(&self) -> KoID {
        self.peer.upgrade().map(|p| p.id()).unwrap_or(0)
    }
);
define_count_helper!(EventPair);

impl EventPair {
    /// Create a pair of event.
    #[allow(unsafe_code)]
    pub fn create() -> (Arc<Self>, Arc<Self>) {
        let event0 = Arc::new(EventPair {
            base: KObjectBase::default(),
            _counter: CountHelper::new(),
            peer: Weak::default(),
        });
        let event1 = Arc::new(EventPair {
            base: KObjectBase::default(),
            _counter: CountHelper::new(),
            peer: Arc::downgrade(&event0),
        });
        // no other reference of `channel0`
        unsafe { &mut *(Arc::as_ptr(&event0) as *mut EventPair) }.peer = Arc::downgrade(&event1);
        (event0, event1)
    }

    /// Get the peer event.
    pub fn peer(&self) -> ZxResult<Arc<Self>> {
        self.peer.upgrade().ok_or(ZxError::PEER_CLOSED)
    }
}

impl Drop for EventPair {
    fn drop(&mut self) {
        if let Some(peer) = self.peer.upgrade() {
            peer.base.signal_set(Signal::PEER_CLOSED);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allowed_signals() {
        let (event0, event1) = EventPair::create();
        assert!(Signal::verify_user_signal(
            event0.allowed_signals(),
            (Signal::USER_SIGNAL_5 | Signal::SIGNALED).bits().into()
        )
        .is_ok());
        assert_eq!(event0.allowed_signals(), event1.allowed_signals());

        event0.peer().unwrap();
    }

    /// There are **two** `peer()`s: the inherent one, which hands back an
    /// `EventPair`, and the [`KernelObject`] one behind the trait object,
    /// which is what `zx_object_get_related` reaches through. Only the
    /// inherent one was tested, so the trait one could answer any error it
    /// liked -- and `PEER_CLOSED` is the one a process tells apart from a
    /// handle that was never good.
    #[test]
    fn the_peer_reached_through_the_trait_is_the_same_one_and_fails_the_same_way() {
        let (event0, event1) = EventPair::create();
        let as_object: Arc<dyn KernelObject> = event0.clone();

        let through_trait = KernelObject::peer(&*as_object).unwrap();
        assert_eq!(
            through_trait.id(),
            event1.id(),
            "the trait handed back something other than the peer"
        );

        // What comes back is a *strong* reference -- the trait upgrades the
        // `Weak` -- so the peer is not gone until this one goes too. Holding
        // it across the drop below is what makes this read as "still open".
        drop(through_trait);
        drop(event1);
        assert_eq!(
            KernelObject::peer(&*as_object).err(),
            Some(ZxError::PEER_CLOSED),
            "a closed peer has to read as PEER_CLOSED and not as some other error"
        );
    }

    #[test]
    fn peer_closed() {
        let (event0, event1) = EventPair::create();
        assert!(Arc::ptr_eq(&event0.peer().unwrap(), &event1));
        assert_eq!(event0.related_koid(), event1.id());

        drop(event1);
        assert_eq!(event0.signal(), Signal::PEER_CLOSED);
        assert_eq!(event0.peer().err(), Some(ZxError::PEER_CLOSED));
        assert_eq!(event0.related_koid(), 0);
    }
}
