use crate::utils::EventHandler;

pub trait EventScheme {
    type Event;

    /// Trigger the event manually and call its handler immediately.
    fn trigger(&self, event: Self::Event);

    /// Subscribe events, call the `handler` when an input event occurs.
    /// If `once` is true, unsubscribe automatically after handling.
    /// Returns a subscription id for [`Self::unsubscribe`].
    fn subscribe(&self, handler: EventHandler<Self::Event>, once: bool) -> Option<u64>;

    /// Drop a parked waker subscription. Required when an async poll future is
    /// Ready/`Drop`ed before the once-handler fires.
    fn unsubscribe(&self, id: u64);
}

macro_rules! impl_event_scheme {
    ($struct:ident $(, $event_ty:ty)?) => {
        impl_event_scheme!(@impl_base $struct $(, $event_ty)?);
    };
    ($struct:ident<'_> $(, $event_ty:ty)?) => {
        impl_event_scheme!(@impl_base $struct<'_> $(, $event_ty)?);
    };
    ($struct:ident < $($types:ident),* > $(where $($preds:tt)+)? $(, $event_ty:ty)?) => {
        impl_event_scheme!(@impl_base $struct < $($types),* > $(where $($preds)+)? $(, $event_ty)?);
    };

    (@impl_base $struct:ident $(, $event_ty:ty)?) => {
        impl $crate::scheme::EventScheme for $struct {
            impl_event_scheme!(@impl_body $(, $event_ty)?);
        }
    };
    (@impl_base $struct:ident<'_> $(, $event_ty:ty)?) => {
        impl $crate::scheme::EventScheme for $struct<'_> {
            impl_event_scheme!(@impl_body $(, $event_ty)?);
        }
    };
    (@impl_base $struct:ident < $($types:ident),* > $(where $($preds:tt)+)? $(, $event_ty:ty)?) => {
        impl < $($types),* > $crate::scheme::EventScheme for $struct < $($types),* >
            $(where $($preds)+)?
        {
            impl_event_scheme!(@impl_body $(, $event_ty)?);
        }
    };

    (@impl_assoc_type) => {
        type Event = ();
    };
    (@impl_assoc_type, $event_ty:ty) => {
        type Event = $event_ty;
    };
    (@impl_body $(, $event_ty:ty)?) => {
        impl_event_scheme!(@impl_assoc_type $(, $event_ty)?);

        #[inline]
        fn trigger(&self, event: Self::Event) {
            self.listener.trigger(event);
        }

        #[inline]
        fn subscribe(&self, handler: $crate::utils::EventHandler<Self::Event>, once: bool) -> Option<u64> {
            self.listener.subscribe(handler, once)
        }

        #[inline]
        fn unsubscribe(&self, id: u64) {
            self.listener.unsubscribe(id);
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::EventListener;
    use alloc::boxed::Box;
    use alloc::sync::Arc;
    use core::marker::PhantomData;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// The plain shape: `impl_event_scheme!(Name)`, which is what every UART
    /// uses. Its event type must come out as `()`.
    struct PlainDevice {
        listener: EventListener,
    }

    impl_event_scheme!(PlainDevice);

    /// The shape with an event type, which is what the input devices use.
    struct TypedDevice {
        listener: EventListener<u32>,
    }

    impl_event_scheme!(TypedDevice, u32);

    /// The borrowed shape, `Name<'_>`, which the virtio drivers use because
    /// they hold a transport borrow.
    struct BorrowedDevice<'a> {
        listener: EventListener<u32>,
        _borrow: PhantomData<&'a ()>,
    }

    impl_event_scheme!(BorrowedDevice<'_>, u32);

    /// The generic shape with a `where` clause, which the MMIO UARTs use.
    struct GenericDevice<T>
    where
        T: Send + Sync,
    {
        listener: EventListener,
        _marker: PhantomData<T>,
    }

    impl_event_scheme!(GenericDevice<T> where T: Send + Sync);

    /// A handler that counts how many times it fired, and with what.
    fn counting(hits: Arc<AtomicUsize>) -> EventHandler {
        Box::new(move |_| {
            hits.fetch_add(1, Ordering::Relaxed);
        })
    }

    /// The macro's whole job is to forward the three methods to the device's
    /// own `listener` field. A `trigger` that reached a different listener --
    /// or none -- is the shape of an input device whose events never wake
    /// anybody, so check the round trip rather than only that it compiles.
    #[test]
    fn the_plain_shape_forwards_to_its_own_listener() {
        let dev = PlainDevice {
            listener: EventListener::new(),
        };
        let hits = Arc::new(AtomicUsize::new(0));
        dev.subscribe(counting(hits.clone()), false).unwrap();
        dev.trigger(());
        dev.trigger(());
        assert_eq!(hits.load(Ordering::Relaxed), 2);
    }

    /// With an event type, the payload must arrive unchanged: an input device
    /// whose events arrived as the wrong value is worse than one with none.
    #[test]
    fn the_typed_shape_carries_the_payload_through() {
        let dev = TypedDevice {
            listener: EventListener::new(),
        };
        let seen = Arc::new(AtomicUsize::new(0));
        let sink = seen.clone();
        dev.subscribe(
            Box::new(move |e: &u32| sink.store(*e as usize, Ordering::Relaxed)),
            false,
        )
        .unwrap();
        dev.trigger(0xbeef);
        assert_eq!(seen.load(Ordering::Relaxed), 0xbeef);
    }

    /// `unsubscribe` is what an async poll future calls when it is Ready or
    /// dropped; if the macro did not forward it, a later `trigger` would wake
    /// freed task memory. So the handler must stop firing after it.
    #[test]
    fn unsubscribing_through_the_macro_actually_drops_the_handler() {
        let dev = PlainDevice {
            listener: EventListener::new(),
        };
        let hits = Arc::new(AtomicUsize::new(0));
        let id = dev.subscribe(counting(hits.clone()), false).unwrap();
        dev.trigger(());
        assert_eq!(hits.load(Ordering::Relaxed), 1);
        dev.unsubscribe(id);
        dev.trigger(());
        assert_eq!(
            hits.load(Ordering::Relaxed),
            1,
            "the handler fired after being unsubscribed"
        );
    }

    /// A one-shot handler is removed by the event that fires it, which is how
    /// a parked waker works: one wake, then gone.
    #[test]
    fn a_once_handler_fires_exactly_once() {
        let dev = PlainDevice {
            listener: EventListener::new(),
        };
        let hits = Arc::new(AtomicUsize::new(0));
        dev.subscribe(counting(hits.clone()), true).unwrap();
        dev.trigger(());
        dev.trigger(());
        assert_eq!(hits.load(Ordering::Relaxed), 1);
    }

    /// The borrowed shape expands to `impl ... for Name<'_>`; the test is
    /// that it forwards like the others, with a live borrow in the struct.
    #[test]
    fn the_borrowed_shape_forwards_too() {
        let anchor = ();
        let dev = BorrowedDevice {
            listener: EventListener::new(),
            _borrow: PhantomData::<&()>,
        };
        let _ = &anchor;
        let hits = Arc::new(AtomicUsize::new(0));
        dev.subscribe(
            Box::new({
                let hits = hits.clone();
                move |_: &u32| {
                    hits.fetch_add(1, Ordering::Relaxed);
                }
            }),
            false,
        )
        .unwrap();
        dev.trigger(1);
        assert_eq!(hits.load(Ordering::Relaxed), 1);
    }

    /// And the generic shape, whose `where` clause the macro has to carry
    /// over or the impl does not apply at all.
    #[test]
    fn the_generic_shape_forwards_too() {
        let dev: GenericDevice<u8> = GenericDevice {
            listener: EventListener::new(),
            _marker: PhantomData,
        };
        let hits = Arc::new(AtomicUsize::new(0));
        dev.subscribe(counting(hits.clone()), false).unwrap();
        dev.trigger(());
        assert_eq!(hits.load(Ordering::Relaxed), 1);
    }

    /// An event nobody subscribed to must be dropped quietly: every polled
    /// device triggers whether or not userspace has the fd open.
    #[test]
    fn an_event_with_no_subscribers_is_dropped_quietly() {
        let dev = PlainDevice {
            listener: EventListener::new(),
        };
        dev.trigger(());
        // And unsubscribing an id that was never handed out is harmless.
        dev.unsubscribe(12345);
        dev.trigger(());
    }
}
