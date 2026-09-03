//! Interrupt-safe synchronization primitives used by the kernel and drivers.
//!
//! On every target this re-exports the vendored `lock` crate (kernel-sync).
//! The zCore copy that the fork-sync merge dropped in here indexed a 16-slot
//! array by the sparse Local APIC ID (`cpu_id() == initial_local_apic_id()`),
//! which panics on real SMP hardware (`index out of bounds: the len is 16
//! but the index is 27`). `lock` already maps APIC IDs to dense logical ids
//! (`0..MAX_CORE_NUM`, 64) and owns the single `push_off`/`pop_off` nesting
//! counter; mixing a second copy would desync IRQ state.

pub use lock::{
    Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockUpgradableGuard, RwLockWriteGuard,
};
