use alloc::{boxed::Box, vec::Vec};

use lock::Mutex;

/// A type alias for the closure to handle device event.
pub type EventHandler<T = ()> = Box<dyn Fn(&T) + Send + Sync>;

/// Upper bound on the number of pending one-shot (`once`) handlers kept at a
/// time. Each `poll(2)`/`select(2)` iteration on an input device fd registers a
/// fresh one-shot waker; while the device is idle nothing fires them (there is
/// no event to drain the list), so without a cap they accumulate without bound
/// and every later `trigger` pays an O(n) walk over the whole list — under the
/// lock, and from IRQ context for HID. Dropping the oldest stale waker is safe:
/// the io-wait loop re-polls on a timer, so at worst a missed wake costs one
/// tick of latency.
const MAX_ONCE_HANDLERS: usize = 64;

/// Device event listener.
///
/// It keeps a series of [`EventHandler`]s that handle events of one single type.
pub struct EventListener<T = ()> {
    events: Mutex<Vec<(EventHandler<T>, bool)>>,
}

impl<T> EventListener<T> {
    /// Construct a new, empty `EventListener`.
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    /// Register a new `handler` into this `EventListener`.
    ///
    /// If `once` is `true`, the `handler` will be removed once it handles an event.
    /// One-shot handlers are capped at [`MAX_ONCE_HANDLERS`] to keep an idle
    /// device's waker list from growing without bound (see the constant docs);
    /// persistent (`once == false`) handlers are never dropped.
    pub fn subscribe(&self, handler: EventHandler<T>, once: bool) {
        let mut events = self.events.lock();
        if once {
            let once_count = events.iter().filter(|(_, o)| *o).count();
            if once_count >= MAX_ONCE_HANDLERS {
                if let Some(pos) = events.iter().position(|(_, o)| *o) {
                    drop(events.remove(pos));
                }
            }
        }
        events.push((handler, once));
    }

    /// Send an event to the `EventListener`.
    ///
    /// All the handlers handle the event, and those marked `once` will be removed immediately.
    pub fn trigger(&self, event: T) {
        self.events.lock().retain(|(f, once)| {
            f(&event);
            !once
        });
    }
}

impl<T> Default for EventListener<T> {
    fn default() -> Self {
        Self::new()
    }
}
