//! The three page-table entry formats, in one place the host compiles.
//!
//! Each of these used to live in `bare/arch/<arch>/vm.rs`, and all of `bare/`
//! is `#[cfg(not(feature = "libos"))]`: the bit layout of a page-table entry,
//! and the two conversions between it and [`MMUFlags`](crate::MMUFlags), were
//! compiled by exactly one build each and tested by none. That is the same
//! reason the walker in [`super::page_table`] went untested for so long, and
//! the same remedy: compile them always and let the `cfg` choose only which
//! one an architecture uses.
//!
//! Nothing here needs hardware. An entry is a `u64`, and what these modules
//! decide is which bits of it mean what -- which is exactly what a wrong
//! answer gets wrong, and what no amount of running the kernel on one
//! architecture can check for the other two.

pub mod aarch64;
pub mod riscv64;

// The x86 entry is spelled in terms of the `x86_64` crate's `PageTableFlags`,
// and that crate is a dependency of this one only for `target_arch =
// "x86_64"`. The host is x86_64, so the tests still run.
#[cfg(target_arch = "x86_64")]
pub mod x86_64;

// The x86 entry is the only one that needs a target-specific crate, so the
// shared contract below runs on an x86_64 host. That is where `cargo test`
// runs.
#[cfg(all(test, target_arch = "x86_64"))]
mod tests;
