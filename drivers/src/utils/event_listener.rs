use alloc::{boxed::Box, vec::Vec};

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::sync::Mutex;

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
    events: Mutex<Vec<(u64, EventHandler<T>, bool)>>,
    next_id: Mutex<u64>,
    /// Ids that [`Self::unsubscribe`] could not find because a
    /// [`Self::trigger`] had the list drained. Empty outside that window.
    ///
    /// Locked *after* `events` wherever both are held, and alone in the
    /// invoke loop, which is what lets a handler unsubscribe from inside a
    /// handler without deadlocking.
    cancelled: Mutex<Vec<u64>>,
    /// How many [`Self::trigger`] calls are between their drain and their
    /// write-back. Read and written **only under the `events` lock**: that is
    /// the whole reason the hand-off is race-free, because the same lock is
    /// what `trigger` takes to drain and `unsubscribe` to retain, so one of
    /// the two orders always holds and there is no third.
    firing: AtomicUsize,
    /// Whether `cancelled` has anything in it. Set and cleared only while its
    /// lock is held; read on its own so the ordinary event --- nobody
    /// unsubscribing, which is every event on a healthy device --- does not
    /// take a second lock per handler from IRQ context.
    any_cancelled: AtomicBool,
}

impl<T> EventListener<T> {
    /// Construct a new, empty `EventListener`.
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            next_id: Mutex::new(0),
            cancelled: Mutex::new(Vec::new()),
            firing: AtomicUsize::new(0),
            any_cancelled: AtomicBool::new(false),
        }
    }

    /// Register a new `handler` into this `EventListener`.
    ///
    /// If `once` is `true`, the `handler` will be removed once it handles an event.
    /// One-shot handlers are capped at [`MAX_ONCE_HANDLERS`] to keep an idle
    /// device's waker list from growing without bound (see the constant docs);
    /// persistent (`once == false`) handlers are never dropped.
    ///
    /// Returns a subscription id for [`Self::unsubscribe`]. Callers that park
    /// a waker in an async future **must** unsubscribe on Ready/`Drop`, or a
    /// later `trigger` wakes freed task memory (UAF → delayed KERNEL PAGE FAULT
    /// after a long compositor session).
    pub fn subscribe(&self, handler: EventHandler<T>, once: bool) -> Option<u64> {
        let mut events = self.events.lock();
        if once {
            let once_count = events.iter().filter(|(_, _, o)| *o).count();
            if once_count >= MAX_ONCE_HANDLERS {
                if let Some(pos) = events.iter().position(|(_, _, o)| *o) {
                    drop(events.remove(pos));
                }
            }
        }
        let mut next = self.next_id.lock();
        let id = *next;
        *next = next.wrapping_add(1);
        events.push((id, handler, once));
        Some(id)
    }

    /// Remove a previously registered handler by id (no-op if already fired).
    ///
    /// The `retain` alone is not enough, because [`Self::trigger`] takes the
    /// whole list out before it calls anybody: an `unsubscribe` landing in
    /// that window --- a reader's `Drop` on one CPU while a device IRQ fires
    /// on another --- found an empty list, removed nothing, and the write-back
    /// then put the handler back. A persistent waker resurrected that way is
    /// called on **every later event**, which is the delayed page fault the
    /// docs on [`Self::subscribe`] describe: not a window of microseconds, but
    /// a handler that outlives its task for good. So the id is noted, and the
    /// write-back drops it.
    ///
    /// What this cannot promise is a call already under way: a handler whose
    /// turn came before this `retain` may still be running. Waiting for it
    /// would mean blocking a `Drop` on an IRQ, so the note covers what it can
    /// --- everything from the next event on.
    pub fn unsubscribe(&self, id: u64) {
        // `events` is held across the note on purpose: the write-back below
        // takes the same two locks in the same order, so it cannot tear the
        // note down in between and lose this id.
        let mut events = self.events.lock();
        events.retain(|(item_id, _, _)| *item_id != id);
        if self.firing.load(Ordering::Relaxed) > 0 {
            let mut cancelled = self.cancelled.lock();
            if !cancelled.contains(&id) {
                cancelled.push(id);
                self.any_cancelled.store(true, Ordering::Release);
            }
        }
    }

    /// Whether `id` was taken away while this pass was walking its own list.
    fn is_cancelled(&self, id: u64) -> bool {
        if !self.any_cancelled.load(Ordering::Acquire) {
            return false;
        }
        self.cancelled.lock().contains(&id)
    }

    /// Send an event to the `EventListener`.
    ///
    /// All the handlers handle the event, and those marked `once` will be removed immediately.
    /// Handlers whose fat pointer is clearly dead (null data or vtable, or a
    /// vtable that is not a kernel address — typical after a UAF of a once-waker
    /// or a coroutine stack overflow that smashes the heap) are skipped and
    /// *leaked*: calling them OR running `Drop` would both be null-range EXECUTE
    /// from IRQ/`timer_tick` → xHCI poll with no current thread.
    pub fn trigger(&self, event: T) {
        if super::fat_ptr::heap_smash_suspected() {
            return;
        }
        // Drain under the lock, invoke outside — avoids re-entrant deadlock and
        // keeps IRQ-off critical sections short (no alloc while holding after
        // the drain; the Vec is allocated once under lock then released).
        let drained: Vec<(u64, EventHandler<T>, bool)> = {
            let mut guard = self.events.lock();
            // Under the drain's own lock, so an `unsubscribe` that misses the
            // list is guaranteed to see this instead.
            self.firing.fetch_add(1, Ordering::Relaxed);
            guard.drain(..).collect()
        };
        let mut kept = Vec::with_capacity(drained.len());
        for (id, f, once) in drained {
            if !super::fat_ptr::dyn_fat_ptr_live(&f) {
                core::mem::forget(f);
                continue;
            }
            if self.is_cancelled(id) {
                // Its owner asked for it to go while we were holding it, so it
                // is no longer ours to call. Dropping it here is what
                // `unsubscribe`'s own `retain` would have done had it found it.
                continue;
            }
            f(&event);
            if !once {
                kept.push((id, f, once));
            }
        }
        let mut guard = self.events.lock();
        // The outermost pass is the one that closes the window, so a handler
        // that re-triggers from inside a handler does not throw away the note
        // the pass around it is still going to read.
        let outermost = self.firing.fetch_sub(1, Ordering::Relaxed) == 1;
        if self.any_cancelled.load(Ordering::Acquire) {
            let mut cancelled = self.cancelled.lock();
            kept.retain(|(id, _, _)| !cancelled.contains(id));
            if outermost {
                cancelled.clear();
                self.any_cancelled.store(false, Ordering::Release);
            }
        }
        if kept.is_empty() {
            return;
        }
        // Preserve any handlers subscribed while we were firing.
        kept.append(&mut *guard);
        *guard = kept;
    }
}

impl<T> Default for EventListener<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// Idle poll/epoll used to park a fresh once-waker every re-scan; without
    /// unsubscribe that filled the listener until trigger UAF'd freed tasks
    /// (~30–40 min desktop sessions). Cap + unsubscribe must keep growth flat.
    #[test]
    fn once_handlers_capped() {
        let listener = EventListener::<()>::new();
        for _ in 0..(MAX_ONCE_HANDLERS + 100) {
            let _ = listener.subscribe(Box::new(|_| {}), true);
        }
        let n = listener.events.lock().iter().filter(|(_, _, o)| *o).count();
        assert!(
            n <= MAX_ONCE_HANDLERS,
            "once handlers must stay ≤ {}, got {}",
            MAX_ONCE_HANDLERS,
            n
        );
    }

    #[test]
    fn unsubscribe_clears_parked_once_handler() {
        let listener = EventListener::<()>::new();
        let id = listener.subscribe(Box::new(|_| {}), true).unwrap();
        assert_eq!(listener.events.lock().len(), 1);
        listener.unsubscribe(id);
        assert_eq!(listener.events.lock().len(), 0);
    }

    #[test]
    fn subscribe_drop_cycle_does_not_accumulate() {
        let listener = EventListener::<()>::new();
        for _ in 0..10_000 {
            let id = listener.subscribe(Box::new(|_| {}), true).unwrap();
            listener.unsubscribe(id);
        }
        assert_eq!(
            listener.events.lock().len(),
            0,
            "park+unsubscribe cycles must leave the listener empty"
        );
    }

    #[test]
    fn trigger_removes_once_and_keeps_persistent() {
        let hits = Arc::new(AtomicUsize::new(0));
        let listener = EventListener::<()>::new();
        let h = hits.clone();
        let _ = listener.subscribe(
            Box::new(move |_| {
                h.fetch_add(1, Ordering::SeqCst);
            }),
            false,
        );
        let h2 = hits.clone();
        let _ = listener.subscribe(
            Box::new(move |_| {
                h2.fetch_add(1, Ordering::SeqCst);
            }),
            true,
        );
        listener.trigger(());
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(
            listener.events.lock().len(),
            1,
            "persistent handler remains"
        );
        listener.trigger(());
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    /// A reader's future parks a **persistent** waker in the listener and
    /// unsubscribes when it is dropped, which is what `input/event.rs` does for
    /// every `evdev` fd. `trigger` empties the list before it calls anybody, so
    /// an `unsubscribe` that lands in that window finds nothing to remove ---
    /// and the write-back then puts the handler straight back. From then on the
    /// listener calls a waker whose task is gone, on every event, for good.
    #[test]
    fn a_handler_unsubscribed_while_the_listener_is_firing_does_not_come_back() {
        let listener = Arc::new(EventListener::<()>::new());
        let hits = Arc::new(AtomicUsize::new(0));

        let h = hits.clone();
        let victim = listener
            .subscribe(
                Box::new(move |_| {
                    h.fetch_add(1, Ordering::SeqCst);
                }),
                false,
            )
            .unwrap();

        // Runs while the listener holds the drained list, exactly as another
        // CPU's `Drop` would.
        let weak = Arc::downgrade(&listener);
        let _ = listener.subscribe(
            Box::new(move |_| {
                if let Some(l) = weak.upgrade() {
                    l.unsubscribe(victim);
                }
            }),
            false,
        );

        listener.trigger(());
        listener.trigger(());
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "un handler dado de baja no puede volver a la lista"
        );
    }

    /// The same window, seen from the other side: the handler has not had its
    /// turn yet when its owner takes it away. Calling it at that point is the
    /// use-after-free the file's own docs describe, so it does not get called.
    #[test]
    fn a_handler_unsubscribed_before_its_turn_is_not_called_in_that_pass() {
        let listener = Arc::new(EventListener::<()>::new());
        let hits = Arc::new(AtomicUsize::new(0));

        let id_slot = Arc::new(Mutex::new(0u64));
        let weak = Arc::downgrade(&listener);
        let slot = id_slot.clone();
        let _ = listener.subscribe(
            Box::new(move |_| {
                if let Some(l) = weak.upgrade() {
                    l.unsubscribe(*slot.lock());
                }
            }),
            false,
        );

        let h = hits.clone();
        let victim = listener
            .subscribe(
                Box::new(move |_| {
                    h.fetch_add(1, Ordering::SeqCst);
                }),
                false,
            )
            .unwrap();
        *id_slot.lock() = victim;

        listener.trigger(());
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "el handler ya no era del listener cuando le tocaba"
        );
    }

    /// A `once` waker taken away mid-pass is not written back either way, so
    /// what matters is that it is not *called*.
    #[test]
    fn a_once_handler_unsubscribed_mid_pass_is_not_called() {
        let listener = Arc::new(EventListener::<()>::new());
        let hits = Arc::new(AtomicUsize::new(0));

        let id_slot = Arc::new(Mutex::new(0u64));
        let weak = Arc::downgrade(&listener);
        let slot = id_slot.clone();
        let _ = listener.subscribe(
            Box::new(move |_| {
                if let Some(l) = weak.upgrade() {
                    l.unsubscribe(*slot.lock());
                }
            }),
            false,
        );

        let h = hits.clone();
        let victim = listener
            .subscribe(
                Box::new(move |_| {
                    h.fetch_add(1, Ordering::SeqCst);
                }),
                true,
            )
            .unwrap();
        *id_slot.lock() = victim;

        listener.trigger(());
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    /// The bookkeeping the fix needs must not become the growth the cap and
    /// `unsubscribe` exist to prevent: outside a `trigger` there is no window,
    /// so an unsubscribe has nothing to remember.
    #[test]
    fn unsubscribing_outside_a_trigger_remembers_nothing() {
        let listener = EventListener::<()>::new();
        for _ in 0..10_000 {
            let id = listener.subscribe(Box::new(|_| {}), true).unwrap();
            listener.unsubscribe(id);
        }
        assert_eq!(listener.events.lock().len(), 0);
        assert_eq!(
            listener.cancelled.lock().len(),
            0,
            "sin trigger en curso no hay nada que anotar"
        );
    }

    /// And inside a `trigger` it is remembered only until that `trigger` ends:
    /// a device that fires once a second must not carry one id per reader that
    /// ever closed.
    #[test]
    fn the_cancel_note_is_torn_down_with_the_trigger_that_needed_it() {
        let listener = Arc::new(EventListener::<()>::new());
        let victim = listener.subscribe(Box::new(|_| {}), false).unwrap();
        let weak = Arc::downgrade(&listener);
        let _ = listener.subscribe(
            Box::new(move |_| {
                if let Some(l) = weak.upgrade() {
                    l.unsubscribe(victim);
                }
            }),
            false,
        );

        listener.trigger(());
        assert_eq!(
            listener.cancelled.lock().len(),
            0,
            "la nota muere con el trigger que la necesitaba"
        );
    }

    /// A handler that triggers the listener again must not tear that note down
    /// from under the pass that is still walking its own drained list.
    #[test]
    fn a_nested_trigger_does_not_throw_away_the_outer_passs_note() {
        let listener = Arc::new(EventListener::<()>::new());
        let hits = Arc::new(AtomicUsize::new(0));

        let h = hits.clone();
        let victim = listener
            .subscribe(
                Box::new(move |_| {
                    h.fetch_add(1, Ordering::SeqCst);
                }),
                false,
            )
            .unwrap();

        let weak = Arc::downgrade(&listener);
        let _ = listener.subscribe(
            Box::new(move |_| {
                if let Some(l) = weak.upgrade() {
                    l.unsubscribe(victim);
                    // Re-entrant: this inner pass finishes first, and its
                    // write-back must leave the outer one's note alone.
                    l.trigger(());
                }
            }),
            false,
        );

        listener.trigger(());
        let after_first = hits.load(Ordering::SeqCst);
        listener.trigger(());
        assert_eq!(
            hits.load(Ordering::SeqCst),
            after_first,
            "el victim no puede sobrevivir al trigger anidado"
        );
    }
}
