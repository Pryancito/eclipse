#![no_std]
// The kernel lock implementations below are `cfg(target_os = "none")`, so a
// host build sees only the shims. A `cargo test` build compiles them anyway
// (see `KERNEL_LOCKS_ON_HOST`), which leaves whatever the tests do not reach
// looking unused; that is the point of building them, not a defect.
#![cfg_attr(test, allow(dead_code))]

#[cfg(test)]
extern crate std;

/// Single source of truth for the size of every per-CPU array indexed by the
/// dense logical cpu id (this crate's `CPUS`, the scheduler's `GLOBAL_RUNTIME`,
/// kernel-hal's percpu storage).
pub const MAX_CORE_NUM: usize = 64;

pub mod cpuid;

// Not behind any `cfg`: the question it answers -- is this word still a
// function? -- is asked by this crate, by the scheduler and by kernel-hal, on
// every build including the host one, so it lives where all three can reach it.
pub mod fn_slot;

// ── the kernel locks, on the host, under `cargo test` ────────────────────────
//
// `KERNEL_LOCKS_ON_HOST`: everything below this line used to be behind
// `cfg(target_os = "none")` alone — 2289 lines of the machine's most
// load-bearing code that no `cargo test` could even compile, let alone run.
// A `cargo test` build gets them too, against the host interrupt backend at
// the end of `interrupt.rs` (a thread-local IRQ flag and a thread-local cpu
// id, one simulated CPU per test thread). That is enough to exercise what the
// bugs live in: whether a guard's `push_off` and `pop_off` come in pairs.
#[cfg(all(test, not(target_os = "none")))]
mod deadlock;
#[cfg(all(test, not(target_os = "none")))]
mod interrupt;
#[cfg(all(test, not(target_os = "none")))]
mod mcslock;
// The external `spin` crate re-exports a module of the same name from the
// host branch below; ours takes precedence inside this crate, which is what
// the test build wants.
#[allow(hidden_glob_reexports)]
#[cfg(all(test, not(target_os = "none")))]
mod rwlock;
#[cfg(all(test, not(target_os = "none")))]
mod spin;
#[cfg(all(test, not(target_os = "none")))]
mod tests;
#[cfg(all(test, not(target_os = "none")))]
mod ticket;

cfg_if::cfg_if! {
    if #[cfg(all(target_os = "none", feature = "ticket"))] {
        extern crate alloc;
        mod interrupt;
        // One set of names on every architecture: registering a CPU, resolving
        // it, and the AP-boot window used to be x86-only exports because the
        // other two kept their own (drifted) maps.
        pub use interrupt::{
            bogus_cpu_id_events, clear_logical_cpu_id, current_cpu_id, current_cpu_id_via_apic,
            hardware_id_of, lock_depth, set_logical_cpu_id, with_ap_boot_logical,
        };
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        pub use interrupt::{hardware_apic_id, set_phys_virt_offset};
        pub mod mcslock;
        pub mod rwlock;
        pub use {rwlock::*, mcslock::*};
        mod deadlock;
        pub use deadlock::{
            pump, report_stuck, set_deadlock_holder_hook, set_deadlock_hook, set_deadlock_spins,
            set_spin_pump,
        };
        pub mod ticket;
        pub use ticket::{TicketMutex as Mutex, TicketMutexGuard as MutexGuard};
    } else if #[cfg(target_os = "none")] {
        extern crate alloc;
        mod interrupt;
        // One set of names on every architecture: registering a CPU, resolving
        // it, and the AP-boot window used to be x86-only exports because the
        // other two kept their own (drifted) maps.
        pub use interrupt::{
            bogus_cpu_id_events, clear_logical_cpu_id, current_cpu_id, current_cpu_id_via_apic,
            hardware_id_of, lock_depth, set_logical_cpu_id, with_ap_boot_logical,
        };
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        pub use interrupt::{hardware_apic_id, set_phys_virt_offset};
        pub mod mcslock;
        pub mod rwlock;
        pub use {rwlock::*, mcslock::*};
        mod deadlock;
        pub use deadlock::{
            pump, report_stuck, set_deadlock_holder_hook, set_deadlock_hook, set_deadlock_spins,
            set_spin_pump,
        };
        pub mod spin;
        pub use spin::{SpinMutex as Mutex, SpinMutexGuard as MutexGuard};
    } else {
        // `::spin` — the external crate, not this crate's `spin` module, which
        // a `cargo test` build declares at crate root (see KERNEL_LOCKS_ON_HOST).
        pub use ::spin::*;

        /// Hosted (libos) no-op twin of the bare-metal stuck-lock reporter.
        /// The preemptive executor's diagnostics call it unconditionally, and
        /// without it the whole libos build fails to compile; on a hosted
        /// target there is no interrupts-off spin to diagnose, so it does
        /// nothing.
        pub fn report_stuck(_file: &'static str, _line: u32) {}

        /// Hosted no-op: deadlock hooks only exist on bare-metal builds.
        pub fn set_deadlock_hook(_f: fn(&'static str, u32)) {}

        /// Hosted no-op twin of the holder-report hook installer.
        pub fn set_deadlock_holder_hook(_f: fn(usize, usize, u32, u32)) {}

        /// Hosted no-op: there is no interrupts-off spin to threshold.
        pub fn set_deadlock_spins(_spins: u64) {}

        /// Hosted no-op: no IRQs-off spins, so nothing to pump.
        pub fn set_spin_pump(_f: fn()) {}

        /// Hosted no-op twin of the spin-loop pump. There is no interrupts-off
        /// spin and no TLB-shootdown queue on a hosted target.
        pub fn pump() {}

        /// Hosted twin of the bare-metal lock-nesting depth. There is no
        /// `push_off` bookkeeping on a hosted target, so report 0 ("no kernel
        /// lock held") — the callers that gate on it are bare-metal only.
        pub fn lock_depth() -> i32 {
            0
        }

        /// Hosted twin of the dense logical cpu id. Host test builds (e.g.
        /// `cargo test -p linux-object`, which links zcore-drivers) run on one
        /// thread of a hosted OS; per-CPU diagnostics all collapse to slot 0.
        pub fn current_cpu_id() -> u8 {
            0
        }

        /// Hosted twin of the APIC-only id resolver (identical on a single
        /// hosted thread).
        pub fn current_cpu_id_via_apic() -> u8 {
            0
        }

        /// Hosted twin: GS plays no part, so there is nothing to disagree with.
        pub fn bogus_cpu_id_events() -> (u32, u32) {
            (u32::MAX, 0)
        }
    }
}

/// Whether a lock is held **right now, by this very CPU**.
///
/// A trait so callers read the same on every target. On bare metal it forwards
/// to the mutex's inherent check (see `ticket::TicketMutex::held_by_current_cpu`),
/// which is what lets the heap allocator and the shadow framebuffer refuse a
/// re-entrant acquire instead of wedging the machine. On a hosted build there
/// is no "this CPU" — `lock::Mutex` is plain `spin::Mutex` there — so the
/// answer is a constant `false` and each guard compiles away to nothing.
pub trait HeldByCurrentCpu {
    /// `true` only when this CPU is already inside this lock's critical
    /// section, i.e. when acquiring it again would spin forever.
    fn held_by_current_cpu(&self) -> bool;
}

cfg_if::cfg_if! {
    if #[cfg(all(target_os = "none", feature = "ticket"))] {
        impl<T: ?Sized> HeldByCurrentCpu for Mutex<T> {
            #[inline]
            fn held_by_current_cpu(&self) -> bool {
                ticket::TicketMutex::holder_is_current_cpu(self)
            }
        }
    } else if #[cfg(target_os = "none")] {
        impl<T: ?Sized> HeldByCurrentCpu for Mutex<T> {
            #[inline]
            fn held_by_current_cpu(&self) -> bool {
                spin::SpinMutex::holder_is_current_cpu(self)
            }
        }
    } else {
        impl<T: ?Sized> HeldByCurrentCpu for Mutex<T> {
            #[inline]
            fn held_by_current_cpu(&self) -> bool {
                false
            }
        }
    }
}
