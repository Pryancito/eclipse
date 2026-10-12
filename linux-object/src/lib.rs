//! Linux kernel objects

#![no_std]
// `#[bench]` and `test::Bencher` are libtest's, and still unstable. Most of
// this crate's rows live in the `benches/` targets, but the syscall-accounting
// ones cannot: `ProcPerf::record` is private to `perf`, which is the point of
// it -- so those rows sit in `#[cfg(test)]` modules beside the code, the way
// `linux-syscall` and `kernel-hal` do it. Gated on `test` so a kernel build
// never asks for the feature.
#![cfg_attr(test, feature(test))]
#![deny(warnings)]
// #![deny(missing_docs)] 形同虚设了
#![allow(clippy::upper_case_acronyms)]
#![allow(clippy::uninit_vec)]

#[macro_use]
extern crate alloc;
#[cfg(test)]
extern crate test;

#[macro_use]
extern crate log;

// layer 0
pub mod boot_trace;
pub mod error;

// layer 1
pub mod fs;

// layer 2
pub mod ipc;
pub mod loadavg;
pub mod loader;
pub mod net;
pub mod perf;
pub mod process;
pub mod signal;
pub mod sync;
pub mod syscall_stats;
pub mod thread;
pub mod time;
pub mod uname;
pub mod vdso;
