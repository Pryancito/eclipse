//! Time Module - Time utilities using eclipse-libc
//!
//! Provides std-like Duration and Instant interfaces.

use libc::*;

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

    pub fn as_millis(&self) -> u64 {
        (self.secs * 1000) + (self.nanos / 1_000_000) as u64
    }
}

impl core::ops::Sub for Duration {
    type Output = Duration;

    fn sub(self, rhs: Duration) -> Duration {
        let self_nanos = self.as_nanos();
        let rhs_nanos = rhs.as_nanos();
        let res_nanos = self_nanos.saturating_sub(rhs_nanos);
        Duration {
            secs: (res_nanos / 1_000_000_000) as u64,
            nanos: (res_nanos % 1_000_000_000) as u32,
        }
    }
}

/// A measurement of a monotonically increasing clock.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Instant {
    ticks: u64,
}

impl Instant {
    pub fn now() -> Instant {
        let mut stats = SystemStats {
            uptime_ticks: 0,
            idle_ticks: 0,
            total_mem_frames: 0,
            used_mem_frames: 0,
            cpu_count: 0,
            cpu_temp: [0; 16],
            gpu_load: [0; 4],
            gpu_temp: [0; 4],
            gpu_vram_total_bytes: 0,
            gpu_vram_used_bytes: 0,
            anomaly_count: 0,
            heap_fragmentation: 0,
            wall_time_offset: 0,
        };
        unsafe {
            let _ = get_system_stats(&mut stats);
        }
        Instant { ticks: stats.uptime_ticks }
    }
    
    pub fn elapsed(&self) -> Duration {
        Duration::from_millis(Instant::now().ticks.saturating_sub(self.ticks))
    }
}

impl core::ops::Sub<Duration> for Instant {
    type Output = Instant;

    fn sub(self, rhs: Duration) -> Instant {
        Instant {
            ticks: self.ticks.saturating_sub(rhs.as_millis())
        }
    }
}
