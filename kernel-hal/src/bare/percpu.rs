//! Per-CPU storage block.
//!
//! Modeled on Redox OS's `PercpuBlock` / `ProcessorControlRegion`: each CPU owns
//! a single [`PercpuBlock`] that consolidates its per-CPU state, instead of
//! scattering separate `[_; MAX_CORE_NUM]` arrays indexed by CPU id.
//!
//! The current CPU's block is reached through [`current`]. On x86_64 the block
//! pointer lives in the GS region set up by `trapframe` (read with a single
//! `mov reg, gs:[off]`, no array indexing) — the same trick Redox uses with its
//! PCR. On architectures whose per-CPU register fast-path is not wired up yet,
//! [`current`] falls back to indexing [`PERCPU`] by the dense logical CPU id,
//! which is bounded and therefore safe.

use alloc::sync::Arc;
use core::any::Any;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::config::MAX_CORE_NUM;
use crate::utils::PerCpuCell;

/// Consolidated per-CPU state.
pub struct PercpuBlock {
    /// Dense logical CPU id that owns this block (`u32::MAX` until registered).
    cpu_id: AtomicU32,
    /// The thread currently running on this CPU.
    pub current_thread: PerCpuCell<Option<Arc<dyn Any + Send + Sync>>>,
}

impl PercpuBlock {
    const fn new() -> Self {
        Self {
            cpu_id: AtomicU32::new(u32::MAX),
            current_thread: PerCpuCell::new(None),
        }
    }

    /// The dense logical id of the CPU this block belongs to.
    #[inline]
    pub fn cpu_id(&self) -> u32 {
        self.cpu_id.load(Ordering::Relaxed)
    }
}

/// Backing storage for every CPU's block, indexed by dense logical CPU id.
///
/// Used both as cross-CPU storage and as the fallback for [`current`] before the
/// per-CPU register fast-path is established.
static PERCPU: [PercpuBlock; MAX_CORE_NUM] = [const { PercpuBlock::new() }; MAX_CORE_NUM];

/// Architecture fast-path: pointer to the current CPU's block, or null if not
/// yet established on this CPU / arch.
#[inline]
fn arch_percpu_ptr() -> *const PercpuBlock {
    #[cfg(target_arch = "x86_64")]
    {
        trapframe::read_cpu_local() as *const PercpuBlock
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        core::ptr::null()
    }
}

/// Record this CPU's block pointer in its per-CPU register, if supported.
#[inline]
fn set_arch_percpu_ptr(_block: &'static PercpuBlock) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        // Safe: `trapframe::init()` has run on this CPU before `register`.
        trapframe::write_cpu_local(_block as *const PercpuBlock as usize);
    }
}

/// The current CPU's [`PercpuBlock`].
#[inline]
pub fn current() -> &'static PercpuBlock {
    let ptr = arch_percpu_ptr();
    if ptr.is_null() {
        // Fallback before the register fast-path is set (or on arches without
        // one). `cpu_id()` is the dense logical id; bound-check defensively.
        let id = crate::cpu::cpu_id() as usize;
        PERCPU.get(id).unwrap_or(&PERCPU[0])
    } else {
        unsafe { &*ptr }
    }
}

/// Bind the current CPU to its [`PercpuBlock`].
///
/// Call once per CPU, after `trapframe::init()` (which sets up the GS region on
/// x86_64) and after the CPU's logical id is known.
pub fn register() {
    let id = crate::cpu::cpu_id() as usize;
    if let Some(block) = PERCPU.get(id) {
        block.cpu_id.store(id as u32, Ordering::Relaxed);
        set_arch_percpu_ptr(block);
    }
}
