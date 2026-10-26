//! GEM frees that cannot run yet.
//!
//! Closing a GEM object whose pages a channel may still be reading cannot free
//! them on the spot: the ring has to pass the point where it last touched the
//! buffer first. Those frees wait here until their probes pass, or until the
//! fence timeout says the ring is wedged and nothing is coming.
//!
//! Own file for the size ceiling of `nvidia.rs` (`xtask tamano`); no logic
//! change beyond taking `&NvidiaGpu` where these were methods.

use super::super::nouveau_uapi::NouveauVmMapping;
use super::{NvidiaGpu, RingProbe, NOUVEAU_GEM_BYTES};
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

pub(super) struct DeferredGemFree {
    pub(super) handle: u32,
    pub(super) h_memory: u32,
    /// The context whose RM client holds `h_memory` -- carried, not looked up
    /// when the free finally runs: by then the owning process is long gone and
    /// its context with it. See `NouveauGemObject::owner_ctx`.
    pub(super) owner_ctx: u32,
    pub(super) size: u64,
    /// Where the dma-buf reservation of the buffer is keyed, if it was ever
    /// exportable ([`crate::scheme::resv`]).
    pub(super) phys: Option<u64>,
    pub(super) mappings: Vec<NouveauVmMapping>,
    pub(super) probes: Vec<RingProbe>,
    pub(super) born_us: u64,
}

/// Free every closed object (`DeferredGemFree`) whose rings have passed
/// their probes, or that has waited the fence timeout: a ring that has
/// not moved in that long is wedged, and its own fence timeout is
/// latching it. Called wherever a zombie is reaped -- a submit, a
/// process exit, a context allocation -- and at every close.
pub(super) fn reap_deferred_frees(gpu: &NvidiaGpu) {
    let now = unsafe { crate::bus::drivers_timer_now_as_micros() };
    let due: Vec<DeferredGemFree> = {
        let mut list = gpu.nouveau_deferred_frees.lock();
        if list.is_empty() {
            return;
        }
        let mut due = Vec::new();
        let mut i = 0;
        while i < list.len() {
            let d = &list[i];
            let expired = now.wrapping_sub(d.born_us) >= crate::scheme::syncobj::FENCE_TIMEOUT_US;
            if expired || d.probes.iter().all(|p| gpu.ring_probe_passed(p)) {
                due.push(list.swap_remove(i));
            } else {
                i += 1;
            }
        }
        due
    };
    for d in due {
        let DeferredGemFree {
            handle,
            h_memory,
            owner_ctx,
            size,
            phys,
            mappings,
            ..
        } = d;
        let context = alloc::format!("deferred GEM_CLOSE handle={}", handle);
        if let Some(phys) = phys {
            crate::scheme::resv::forget(phys);
        }
        gpu.rm_unmap_mappings(&context, mappings);
        NOUVEAU_GEM_BYTES.fetch_sub(size, Ordering::Relaxed);
        let status = (*gpu.rm_device_instance.lock()).map(|device_instance| {
            nvidia_rm_sys::rm_init::gem_free(device_instance, owner_ctx, h_memory)
        });
        log::info!(
            "[nouveau-uapi] {}: its ring(s) passed (or timed out) -> gem_free h_memory={:#010x} status={:?}",
            context,
            h_memory,
            status
        );
    }
}

/// Drop, without an RM unmap, the mappings owned by `pid` that closed
/// objects still waiting on a ring carry: `pid`'s VAS has just been
/// freed with its context.
pub(super) fn forget_deferred_mappings_of(gpu: &NvidiaGpu, pid: u64) -> usize {
    let mut n = 0;
    for d in gpu.nouveau_deferred_frees.lock().iter_mut() {
        let before = d.mappings.len();
        d.mappings.retain(|m| m.owner_pid != pid);
        n += before - d.mappings.len();
    }
    n
}
