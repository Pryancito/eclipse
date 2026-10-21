//! The integration tests, built into the unit-test binary under `cargo
//! llvm-cov` (see `lib.rs`).
//!
//! `cargo llvm-cov` counts a function's lines from its best compiled copy,
//! and the unit tests and the integration tests build two copies of the
//! crate. Run here too, the integration tests make one copy see everything
//! the tests do; `extern crate self as eclipsefs` lets their paths resolve.

#![allow(dead_code, unused_imports)]

// Not here: tests that install a global allocator of their own.

#[path = "../tests/atomic_write.rs"]
mod atomic_write;
#[path = "../tests/attrs.rs"]
mod attrs;
#[path = "../tests/bitmap_sums.rs"]
mod bitmap_sums;
#[path = "../tests/commit_interval.rs"]
mod commit_interval;
#[path = "../tests/compress.rs"]
mod compress;
#[path = "../tests/cow.rs"]
mod cow;
#[path = "../tests/crash.rs"]
mod crash;
#[path = "../tests/csum.rs"]
mod csum;
#[path = "../tests/defrag.rs"]
mod defrag;
#[path = "../tests/edge_cases.rs"]
mod edge_cases;
#[path = "../tests/encrypt.rs"]
mod encrypt;
#[path = "../tests/faults.rs"]
mod faults;
#[path = "../tests/fsync_log.rs"]
mod fsync_log;
#[path = "../tests/full.rs"]
mod full;
#[path = "../tests/integration.rs"]
mod integration;
#[path = "../tests/mapping.rs"]
mod mapping;
#[path = "../tests/performance.rs"]
mod performance;
#[path = "../tests/quota.rs"]
mod quota;
#[path = "../tests/raid56.rs"]
mod raid56;
#[path = "../tests/repair.rs"]
mod repair;
#[path = "../tests/resize.rs"]
mod resize;
#[path = "../tests/send.rs"]
mod send;
#[path = "../tests/shared_read.rs"]
mod shared_read;
#[path = "../tests/subvol.rs"]
mod subvol;
#[path = "../tests/tiering.rs"]
mod tiering;
#[path = "../tests/volume.rs"]
mod volume;
