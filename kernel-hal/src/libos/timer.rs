//! Time and clock functions.

use async_std::task;
use std::time::{Duration, SystemTime};

hal_fn_impl! {
    impl mod crate::hal_fn::timer {
        fn timer_now() -> Duration {
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
        }

        fn timer_set(deadline: Duration, callback: Box<dyn FnOnce(Duration) + Send + Sync>) {
            task::spawn(async move {
                let dur = deadline - timer_now();
                task::sleep(dur).await;
                callback(timer_now());
            });
        }
    }
}

/// Wall-clock (libos: host time from `SystemTime`).
pub fn wall_clock_now() -> Duration {
    timer_now()
}

/// Adjust wall clock (libos: no effect on host; satisfies syscall for tests).
pub fn wall_clock_set(_target: Duration) {}
