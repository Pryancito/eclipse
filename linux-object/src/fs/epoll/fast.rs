//! The quick look `epoll_wait` takes first: past the entries nothing has
//! happened to.
//!
//! Every wait polled every watched file, ready or not. With 64 pipes watched
//! and the last one ready, eclipse-bench on hardware gave 6.06 us an
//! `epoll_wait`, 0.73 of a `poll` of the same set, where Linux gives 184 ns,
//! 0.13 of its `poll`: Linux's wait reads a ready list that the files' wake
//! callbacks fill, and touches no other entry.
//!
//! Here a level-triggered entry carries an [`Armed`] subscription on its file
//! for as long as it is quiet: taken right after the poll that found the
//! entry not ready, and fired at once when the file's bus already has the
//! flag up, so any change after that poll fires it. The first look of a wait
//! polls only the entries that fired, were ready last time, or have no
//! subscription. A file whose producers do not publish everything would be
//! missed by that look, so it is never the last word:
//!
//! - a wait that finds nothing and is about to sleep looks again at every
//!   entry before it sleeps, as every look did before;
//! - every look is a full one when the last full look is older than
//!   [`FULL_LOOK_NS`].

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use super::super::{FileLike, PollEvents};
use crate::sync::ReadinessSub;

/// How old the last look at every entry may be before the next look is one
/// too.
pub(super) const FULL_LOOK_NS: u64 = 1_000_000;

/// Set when the file publishes an event the entry asked about.
struct Fired(AtomicBool);

impl alloc::task::Wake for Fired {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }
}

/// A level-triggered entry's subscription on its file, and what the last
/// poll of it said.
pub(super) struct Armed {
    fired: Arc<Fired>,
    /// Held for the unsubscribe on drop.
    _sub: ReadinessSub,
    /// The last poll found the entry ready.
    pub(super) ready: bool,
}

impl Armed {
    /// Subscribe on `file` for `interest`. `None` when the file has nothing
    /// to subscribe to, or the subscription stored nothing and did not fire:
    /// such an entry is polled on every look.
    pub(super) fn arm(file: &dyn FileLike, interest: PollEvents) -> Option<Self> {
        let fired = Arc::new(Fired(AtomicBool::new(false)));
        let waker = core::task::Waker::from(fired.clone());
        let sub = file.subscribe_readiness(interest, &waker)?;
        if sub.is_noop() && !fired.0.load(Ordering::Acquire) {
            return None;
        }
        Some(Armed {
            fired,
            _sub: sub,
            ready: false,
        })
    }

    /// Whether the file has published something since this was taken. A
    /// fired subscription is spent: the entry is armed again when polled.
    pub(super) fn fired(&self) -> bool {
        self.fired.0.load(Ordering::Acquire)
    }

    /// Whether the first look of a wait may pass the entry: it was not ready
    /// and nothing has happened to it since.
    pub(super) fn quiet(&self) -> bool {
        !self.ready && !self.fired()
    }
}
