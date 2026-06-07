//! Eclipse Pulse — IRQ-first I/O reactor (HID + NIC + idle).
//!
//! Drivers signal work with [`pulse_signal`]; multiplex wait loops call
//! [`pulse_io_tick`] instead of unconditionally polling xHCI and smoltcp.

use core::task::Waker;
use cfg_if::cfg_if;


/// xHCI HID backup poll interval when no MSI (tier C).
pub const PULSE_HID_FALLBACK_US: u64 = 16_000;
/// smoltcp/NIC backup when no RX IRQ (tier C).
pub const PULSE_NET_FALLBACK_US: u64 = 32_000;
/// Deferred NIC jobs when not watching sockets.
pub const PULSE_DEFERRED_IDLE_US: u64 = 20_000;

pub const PULSE_HID: u32 = 1 << 0;
pub const PULSE_NET_RX: u32 = 1 << 1;
pub const PULSE_LINK: u32 = 1 << 2;

/// What [`pulse_io_tick`] decided to run this cycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PulseWork {
    pub run_hid_backup: bool,
    pub run_net_deferred: bool,
    /// Full NIC/smoltcp poll (throttled unless `run_net_poll_now`).
    pub run_net_poll: bool,
    /// IRQ pending: bypass net throttle once.
    pub run_net_poll_now: bool,
    pub did_hlt: bool,
}

impl PulseWork {
    #[inline]
    pub fn any_work(self) -> bool {
        self.run_hid_backup
            || self.run_net_deferred
            || self.run_net_poll
            || self.run_net_poll_now
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PulseStats {
    pub signals: u64,
    pub hid_backup: u64,
    pub net_poll: u64,
    pub net_poll_irq: u64,
    pub hlt: u64,
}

cfg_if! {
    if #[cfg(feature = "libos")] {
        /// Host/libos build: Pulse is a no-op (real hardware path is bare-metal only).
        pub fn pulse_signal(_bits: u32) {}

        pub fn register_pulse_waker(_waker: Waker) {}

        pub fn pulse_stats() -> PulseStats {
            PulseStats::default()
        }

        pub fn pulse_io_tick(_watch_net: bool, _watch_hid: bool) -> PulseWork {
            PulseWork::default()
        }

        pub fn retain_pulse_waker(_waker: &Waker) {}

        pub fn pending_bits() -> u32 {
            0
        }

        pub fn consume_pending(_mask: u32) -> u32 {
            0
        }

        pub fn idle_deferred_due() -> bool {
            true
        }
    } else {
        use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
        use alloc::vec::Vec;
        use lazy_static::lazy_static;
        use lock::Mutex;

        lazy_static! {
            static ref PENDING: AtomicU32 = AtomicU32::new(0);
            static ref PULSE_WAKERS: Mutex<Vec<Waker>> = Mutex::new(Vec::new());
            static ref LAST_HID_FALLBACK_US: AtomicU64 = AtomicU64::new(0);
            static ref LAST_NET_FALLBACK_US: AtomicU64 = AtomicU64::new(0);
            static ref LAST_DEFERRED_IDLE_US: AtomicU64 = AtomicU64::new(0);
            static ref STATS: Mutex<PulseStats> = Mutex::new(PulseStats::default());
            static ref SIGNAL_COUNT: AtomicU64 = AtomicU64::new(0);
        }

        const MAX_PULSE_WAKERS: usize = 256;

        #[inline]
        fn mono_us() -> u64 {
            crate::timer::timer_now().as_micros() as u64
        }

        fn push_pulse_waker(waker: &Waker) {
            let mut wakers = PULSE_WAKERS.lock();
            if wakers.iter().any(|w| w.will_wake(waker)) {
                return;
            }
            if wakers.len() >= MAX_PULSE_WAKERS {
                wakers.remove(0);
            }
            wakers.push(waker.clone());
        }

        fn wake_pulse_wakers() {
            let wakers: Vec<Waker> = core::mem::take(&mut *PULSE_WAKERS.lock());
            for w in wakers {
                w.wake();
            }
        }

        fn bump_stat<F: FnOnce(&mut PulseStats)>(f: F) {
            f(&mut *STATS.lock());
        }

        /// Called from IRQ or thread context; must not take mutexes before waking waiters.
        pub fn pulse_signal(bits: u32) {
            if bits == 0 {
                return;
            }
            PENDING.fetch_or(bits, Ordering::Release);
            SIGNAL_COUNT.fetch_add(1, Ordering::Relaxed);
            if bits & PULSE_NET_RX != 0 {
                crate::net::wake_net_rx_waiters_inner();
            }
            wake_pulse_wakers();
        }

        pub fn register_pulse_waker(waker: Waker) {
            push_pulse_waker(&waker);
        }

        /// Keep the waker registered after an IRQ-driven wake (multiplex wait loops).
        pub fn retain_pulse_waker(waker: &Waker) {
            PULSE_WAKERS.lock().retain(|w| w.will_wake(waker));
        }

        /// Pending IRQ bits without consuming (socket syscall fast path).
        pub fn pending_bits() -> u32 {
            PENDING.load(Ordering::Acquire)
        }

        /// Clear and return `mask` bits that were pending.
        pub fn consume_pending(mask: u32) -> u32 {
            let mut cur = PENDING.load(Ordering::Acquire);
            loop {
                let hit = cur & mask;
                if hit == 0 {
                    return 0;
                }
                match PENDING.compare_exchange_weak(cur, cur & !mask, Ordering::AcqRel, Ordering::Acquire)
                {
                    Ok(_) => return hit,
                    Err(v) => cur = v,
                }
            }
        }

        pub fn pulse_stats() -> PulseStats {
            let mut stats = *STATS.lock();
            stats.signals = SIGNAL_COUNT.load(Ordering::Relaxed);
            stats
        }

        pub fn pulse_io_tick(watch_net: bool, watch_hid: bool) -> PulseWork {
            let now = mono_us();
            let mut mask = 0;
            if watch_hid {
                mask |= PULSE_HID;
            }
            if watch_net {
                mask |= PULSE_NET_RX | PULSE_LINK;
            }
            let pending = consume_pending(mask);
            let mut work = PulseWork::default();

            if watch_hid {
                let last = LAST_HID_FALLBACK_US.load(Ordering::Relaxed);
                let tier_c = now.wrapping_sub(last) >= PULSE_HID_FALLBACK_US;
                if (pending & PULSE_HID) != 0 || tier_c {
                    work.run_hid_backup = true;
                    LAST_HID_FALLBACK_US.store(now, Ordering::Relaxed);
                    bump_stat(|s| s.hid_backup += 1);
                }
            }

            if watch_net {
                let last = LAST_NET_FALLBACK_US.load(Ordering::Relaxed);
                let tier_c = now.wrapping_sub(last) >= PULSE_NET_FALLBACK_US;
                if (pending & (PULSE_NET_RX | PULSE_LINK)) != 0 {
                    work.run_net_deferred = true;
                    if (pending & PULSE_NET_RX) != 0 {
                        work.run_net_poll_now = true;
                        bump_stat(|s| s.net_poll_irq += 1);
                    } else if tier_c {
                        work.run_net_poll = true;
                    }
                    LAST_NET_FALLBACK_US.store(now, Ordering::Relaxed);
                } else if tier_c {
                    work.run_net_deferred = true;
                    work.run_net_poll = true;
                    LAST_NET_FALLBACK_US.store(now, Ordering::Relaxed);
                }
                if work.run_net_poll || work.run_net_poll_now {
                    bump_stat(|s| s.net_poll += 1);
                }
            } else {
                let last = LAST_DEFERRED_IDLE_US.load(Ordering::Relaxed);
                if now.wrapping_sub(last) >= PULSE_DEFERRED_IDLE_US {
                    work.run_net_deferred = true;
                    LAST_DEFERRED_IDLE_US.store(now, Ordering::Relaxed);
                }
            }

            if !work.any_work() {
                crate::interrupt::wait_for_interrupt();
                work.did_hlt = true;
                bump_stat(|s| s.hlt += 1);
            }

            work
        }

        /// Throttle deferred NIC work in the kernel idle loop (same cadence as I/O wait).
        pub fn idle_deferred_due() -> bool {
            let now = mono_us();
            let last = LAST_DEFERRED_IDLE_US.load(Ordering::Relaxed);
            if now.wrapping_sub(last) >= PULSE_DEFERRED_IDLE_US {
                LAST_DEFERRED_IDLE_US.store(now, Ordering::Relaxed);
                true
            } else {
                false
            }
        }
    }
}
