//! Pure helpers for NIC wait-loop tuning (unit-testable).

pub(crate) const NET_POLL_INTERVAL_MIN_US: u64 = 4_000;
pub(crate) const NET_POLL_INTERVAL_MAX_US: u64 = 32_000;
pub(crate) const DEFERRED_NET_JOBS_PER_TICK_BASE: usize = 4;
pub(crate) const DEFERRED_NET_JOBS_PER_TICK_MAX: usize = 12;

/// Deferred IRQ jobs to run before a NIC poll, scaled by queue depth.
#[inline]
pub(crate) fn deferred_jobs_budget(pending: usize) -> usize {
    DEFERRED_NET_JOBS_PER_TICK_BASE
        + pending
            .min(DEFERRED_NET_JOBS_PER_TICK_MAX.saturating_sub(DEFERRED_NET_JOBS_PER_TICK_BASE))
}

/// Full [`super::poll_ifaces`] interval for multiplex wait loops (poll/epoll/
/// select — the path curl and most download tools wait on).
///
/// Any queued NIC bottom-half (`pending >= 1`) means a transfer is live, so poll
/// at the tight 4 ms interval rather than the old 16 ms base: at 16 ms the ACK
/// self-clock through a multiplexed waiter was stretched to ~16 ms, which — at a
/// 1-segment window after a slow-start drop — is the residual "1 MSS / RTT"
/// throughput cap. Only a genuinely idle stack (`pending == 0`) relaxes to the
/// 32 ms max, so idle CPU is unchanged.
#[inline]
pub(crate) fn net_poll_interval_us(pending: usize) -> u64 {
    if pending == 0 {
        NET_POLL_INTERVAL_MAX_US
    } else {
        NET_POLL_INTERVAL_MIN_US
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_jobs_scales_with_backlog() {
        assert_eq!(deferred_jobs_budget(0), 4);
        assert_eq!(deferred_jobs_budget(4), 8);
        assert_eq!(deferred_jobs_budget(100), 12);
    }

    #[test]
    fn net_poll_interval_tightens_when_busy() {
        assert_eq!(net_poll_interval_us(0), 32_000);
        assert_eq!(net_poll_interval_us(1), 4_000);
        assert_eq!(net_poll_interval_us(7), 4_000);
        assert_eq!(net_poll_interval_us(8), 4_000);
    }
}
