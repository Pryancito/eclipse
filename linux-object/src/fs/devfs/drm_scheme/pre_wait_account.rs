//! `PreWaitAccount`: the per-wait account every pre-ioctl wait opens and
//! closes (`zcore_drivers::scheme::prewait`). Out of `drm_scheme.rs` for
//! its size ceiling.

use super::*;

/// Accounts one pre-wait to [`zcore_drivers::scheme::prewait`], however the
/// wait ends.
///
/// The five pre-waits park a thread BEFORE the driver is ever called, so none
/// of that time appears in the driver's per-ioctl profile -- which starts
/// timing at the dispatch, when the parking is already over. A frame that
/// spends most of itself parked therefore reads, in `/proc/gpudbg`, as a
/// table of small numbers and no account of where the frame went. This is
/// what closes that gap on the machine that has the GPU.
///
/// It accounts on DROP because these functions return from several places --
/// the request does not read, the condition is already met, the deadline
/// passed -- and a `record` at each of them is a record missed the next time
/// a return is added.
pub(super) struct PreWaitAccount {
    pub(super) kind: zcore_drivers::scheme::prewait::Kind,
    pub(super) start: Duration,
    /// Times the loop woke, looked, and went back to sleep. Also what tells a
    /// wait that parked from one that was satisfied on its first look, since
    /// a coarse clock can read zero microseconds for a real park.
    pub(super) probes: u32,
    pub(super) timed_out: bool,
}

impl PreWaitAccount {
    pub(super) fn new(kind: zcore_drivers::scheme::prewait::Kind) -> Self {
        Self {
            kind,
            start: kernel_hal::timer::timer_now(),
            probes: 0,
            timed_out: false,
        }
    }

    /// One more turn of the poll loop.
    pub(super) fn probe(&mut self) {
        self.probes = self.probes.saturating_add(1);
    }

    /// This wait is ending on its deadline, not on the thing it waited for.
    pub(super) fn timed_out(&mut self) {
        self.timed_out = true;
    }
}

impl Drop for PreWaitAccount {
    fn drop(&mut self) {
        // `as_micros` is a u128 and a plain `as u64` wraps. It takes a
        // clock 584 000 years old to overflow honestly, but a timer that
        // jumps does it in one read, and the counter is a sum: one wrapped
        // value poisons the column for the rest of the boot.
        let elapsed_us = core::convert::TryFrom::try_from(
            kernel_hal::timer::timer_now()
                .saturating_sub(self.start)
                .as_micros(),
        )
        .unwrap_or(u64::MAX);
        zcore_drivers::scheme::prewait::record(
            self.kind,
            pre_wait_parked_us(elapsed_us, self.probes),
            self.probes,
            self.timed_out,
        );
    }
}
