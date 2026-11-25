//! What the lock algorithms are made of: the atomics and the spin hint.
//!
//! In every ordinary build these are `core`'s own, so nothing changes. Built
//! with `--cfg loom` (see `loom/models.rs`) they are loom's instead, and
//! loom then runs each test body once for **every** interleaving and every
//! memory-order outcome the C11 model allows, not the few a real machine
//! happens to show. A lock that releases with `Relaxed` where it needs
//! `Release`, or a reader count that can be seen half-updated, fails there
//! every time instead of once a month on hardware.

#[cfg(not(loom))]
pub(crate) use core::{
    hint::spin_loop,
    sync::atomic::{AtomicBool, AtomicUsize},
};

#[cfg(loom)]
pub(crate) use loom::{
    hint::spin_loop,
    sync::atomic::{AtomicBool, AtomicUsize},
};

/// `const fn` in the kernel, where every lock is built in a `static`; plain
/// `fn` under loom, whose atomics register with the running model when they
/// are made and so cannot be built at compile time.
macro_rules! loom_const_fn {
    ($(#[$m:meta])* $v:vis fn $($rest:tt)*) => {
        #[cfg(not(loom))]
        $(#[$m])* $v const fn $($rest)*
        #[cfg(loom)]
        $(#[$m])* $v fn $($rest)*
    };
}
pub(crate) use loom_const_fn;

/// `AtomicUsize::try_update`, which loom's atomics do not have yet: the same
/// loop under its older name. Only the loom build sees it; elsewhere the
/// inherent method wins.
#[cfg(loom)]
pub(crate) trait TryUpdate {
    fn try_update(
        &self,
        set: core::sync::atomic::Ordering,
        fetch: core::sync::atomic::Ordering,
        f: impl FnMut(usize) -> Option<usize>,
    ) -> Result<usize, usize>;
}

#[cfg(loom)]
impl TryUpdate for AtomicUsize {
    fn try_update(
        &self,
        set: core::sync::atomic::Ordering,
        fetch: core::sync::atomic::Ordering,
        f: impl FnMut(usize) -> Option<usize>,
    ) -> Result<usize, usize> {
        self.fetch_update(set, fetch, f)
    }
}
