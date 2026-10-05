//! The Linux-compatible vDSO: image plus the shape of the data page the kernel
//! publishes into it.
//!
//! This crate is deliberately inert — it holds no state and touches no kernel
//! subsystem. It exists so the ELF image and the layout of `_vdso_data` are
//! defined in exactly one place: `vdso/vdso.c` compiles the C view of that
//! struct, `build.rs` locates it inside the linked image, and [`VdsoData`]
//! below is the Rust view the kernel writes through. A field added on one side
//! and forgotten on the other would otherwise produce a clock that is wrong
//! rather than absent, which is the one outcome the whole design is arranged to
//! avoid.
//!
//! See `vdso/vdso.c` for why a vDSO at all, and `build.rs` for what makes the
//! image acceptable to a C library.

#![no_std]
#![deny(missing_docs)]

mod elf;

pub use elf::vdsosym;

// `AVAILABLE`, `DATA_OFFSET` and `IMAGE_LEN`, computed by `build.rs` from the
// image it actually linked. Their doc comments live in the generated file:
// rustdoc does not attach one written here to what a macro expands to.
include!(concat!(env!("OUT_DIR"), "/vdso_meta.rs"));

/// The linked ELF image, ready to be mapped into a process.
///
/// Empty when the build had no usable C compiler; callers must treat that as
/// "this kernel has no vDSO" rather than as an error. [`AVAILABLE`] says the
/// same thing without inspecting the slice.
pub const IMAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/vdso.img"));

/// The clock parameters the kernel publishes for userspace.
///
/// Mirrors `struct vdso_data` in `vdso/vdso.c`. It lives on its own page inside
/// the image, shared by every process, so a single write by the kernel is seen
/// by all of them at once.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct VdsoData {
    /// Non-zero when the remaining fields are valid.
    ///
    /// Cleared when the TSC is not a usable time source — not invariant, or
    /// never calibrated — in which case userspace declines to answer and the
    /// caller falls back to the `clock_gettime` syscall.
    pub enabled: u32,
    /// Non-zero when userspace may answer `getcpu` from `RDTSCP` instead of
    /// trapping: the CPU implements it and the kernel has written every CPU's
    /// own id into `IA32_TSC_AUX`.
    ///
    /// Independent of [`enabled`](Self::enabled), which vouches only for the
    /// clock: a machine with an unusable TSC as a *time source* can still
    /// report which CPU it is on, and the reverse. It also sits exactly where
    /// the padding used to, so the 64-bit fields below stay naturally aligned
    /// and the struct's size does not move.
    pub getcpu_enabled: u32,
    /// Fixed-point multiplier: monotonic ns =
    /// `((rdtsc() - tsc_base) * tsc_mult) >> 32`.
    ///
    /// The same constant the kernel's own `timer_now` uses, so the two clocks
    /// cannot drift apart by construction.
    pub tsc_mult: u64,
    /// Nanoseconds to add to the monotonic clock to obtain `CLOCK_REALTIME`.
    pub wall_off_ns: u64,
    /// The raw `rdtsc()` reading this boot calls time zero, subtracted before
    /// scaling: monotonic ns = `((rdtsc() - tsc_base) * tsc_mult) >> 32`.
    ///
    /// The TSC is not zero when the kernel takes over — firmware ran first, and
    /// a warm reboot never resets it — so scaling the absolute reading reported
    /// the time since the machine was last powered on as uptime. The kernel
    /// subtracts this same base in `timer_now`; publishing it is what keeps the
    /// two clocks the one clock they are documented to be.
    pub tsc_base: u64,
}

/// Size of the data page reserved inside the image.
pub const DATA_PAGE_SIZE: usize = 4096;

const _: () = {
    // `build.rs` guarantees `_vdso_data` starts on a page boundary; this
    // guarantees the Rust view still fits in that page.
    assert!(core::mem::size_of::<VdsoData>() <= DATA_PAGE_SIZE);
    // And this is the one the paragraph at the top of this file rests on.
    // `DATA_SIZE` is `_vdso_data`'s `st_size` in the linked image, which is the
    // size the C compiler gave `struct vdso_data` in `vdso.c`. Until this
    // existed, "a field added on one side and forgotten on the other" was only
    // asserted in prose: it compiled, linked, passed verification and booted,
    // and the kernel published the new field into bytes past the end of the
    // struct userspace reads -- a clock that is wrong rather than absent, the
    // one outcome this design says it is arranged to avoid. Now it stops the
    // build, on either side. Skipped when there is no image, where `DATA_SIZE`
    // is zero and nothing maps the struct at all.
    assert!(!AVAILABLE || core::mem::size_of::<VdsoData>() == DATA_SIZE);
};
