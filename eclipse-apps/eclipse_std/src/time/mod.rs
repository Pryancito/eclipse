//! Time Module - Time utilities using eclipse-libc
//!
//! Provides std-like Duration and Instant interfaces.

use eclipse_libc::*;

/// A Duration type to represent a span of time.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Duration {
    pub secs: u64,
    pub nanos: u32,
}

impl Duration {
    pub const fn from_millis(millis: u64) -> Duration {
        Duration {
            secs: millis / 1000,
            nanos: ((millis % 1000) * 1_000_000) as u32,
        }
    }
    
    pub const fn from_secs(secs: u64) -> Duration {
        Duration { secs, nanos: 0 }
    }
    
    pub fn as_nanos(&self) -> u128 {
        (self.secs as u128 * 1_000_000_000) + self.nanos as u128
    }
}

/// A measurement of a monotonically increasing clock.
pub struct Instant {
    // TODO: Use eclipse-libc clock() or time()
}

impl Instant {
    pub fn now() -> Instant {
        Instant {}
    }
    
    pub fn elapsed(&self) -> Duration {
        Duration::from_secs(0)
    }
}
