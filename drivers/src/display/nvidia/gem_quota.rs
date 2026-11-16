//! How much GEM memory a card may hold, and what `GEM_NEW` says when it
//! answers `ENOMEM`.
//!
//! # Why
//!
//! Abyss on two RTX 2060 SUPER ran into `ENOMEM` on every `GEM_NEW`, VRAM
//! and GART alike (zink: "couldn't allocate memory heap=0" and "heap=4",
//! smithay's swapchain: os error 12), while labwc on the same machine did
//! not. Both kinds failing at once is not the RM running out of VRAM: it is
//! the quota in front of it. That quota was one counter for every card
//! (`NOUVEAU_GEM_BYTES`) held under 4 GiB, with a second 2 GiB ceiling per
//! process. Abyss opens both GPUs, so the two cards' buffers shared the one
//! 4 GiB, and nouveau has no such per-process limit at all: Linux fails a
//! `GEM_NEW` when the memory is really gone, not before.
//!
//! So the limit is now this card's own: what its table holds plus the closed
//! objects still waiting on a ring, against twice its VRAM (VRAM plus as
//! much again in host memory), 4 GiB when the VRAM size is unknown. The
//! per-process ceiling is gone. Before refusing, the closed objects whose
//! rings have passed are freed (`reap_deferred_frees`), as TTM waits on
//! idle buffers before it gives up, because a compositor that stopped
//! rendering no longer submits or closes anything, and those are the only
//! places that queue was emptied.
//!
//! # When it still says no
//!
//! The refusal is logged at error level, in `dmesg`, with where this card's
//! memory is: per owner, orphaned objects, closed ones waiting on a ring and
//! objects only a framebuffer or a dma-buf keep alive. A leak shows up there
//! by name. The same breakdown goes out each time the card's live total
//! crosses another 512 MiB, so a run that never fails still shows a leak
//! growing.

use super::{NvidiaGpu, NOUVEAU_GEM_BYTES, ZOMBIE_PID_BASE};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Hard cap on a single `GEM_NEW` allocation (`GEM_NEW_MAX_SINGLE`).
pub(super) const MAX_SINGLE: u64 = 256 << 20;
/// The card's limit when its VRAM size is unknown.
const FALLBACK_CARD_CAP: u64 = 4 << 30;
/// How far the live total climbs between two breakdowns.
const REPORT_STEP: u64 = 512 << 20;

/// The card's limit: twice its VRAM, at least [`FALLBACK_CARD_CAP`].
pub(super) fn card_cap(vram_mb: u32) -> u64 {
    let vram = u64::from(vram_mb) << 20;
    vram.saturating_mul(2).max(FALLBACK_CARD_CAP)
}

/// Where a card's GEM memory is, for the lines below.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub(super) struct Usage {
    /// Objects in the card's table, and their bytes.
    pub objects: usize,
    pub bytes: u64,
    /// Closed objects whose memory waits on a ring (`gem_free_queue`).
    pub deferred: usize,
    pub deferred_bytes: u64,
    /// Objects whose creator is gone (owner 0 or a zombie's tombstone): kept
    /// alive only by other holders.
    pub orphans: usize,
    pub orphan_bytes: u64,
    /// Objects only a KMS framebuffer and/or a dma-buf still holds: no
    /// process has a handle to them any more.
    pub pinned_only: usize,
    pub pinned_only_bytes: u64,
    /// The three owners with the most bytes: (pid, objects, bytes).
    pub top: Vec<(u64, usize, u64)>,
}

impl Usage {
    /// What the quota counts: live objects plus those waiting to be freed.
    pub fn charged(&self) -> u64 {
        self.bytes.saturating_add(self.deferred_bytes)
    }
}

/// A holder that is not a process: a dma-buf or a KMS framebuffer
/// (`DMABUF_HOLDER`, `KMS_FB_HOLDER` in `linux-object`).
fn non_process_holder(h: u64) -> bool {
    h >= crate::scheme::gem_mmap::DMABUF_HOLDER
}

/// Fold the table into a [`Usage`]. `holders` answers a handle's holders
/// (`gem_mmap::holders`), `None` for an untracked one.
pub(super) fn usage(
    objects: &[(u32, u64, u64)],
    deferred: &[u64],
    holders: impl Fn(u32) -> Option<Vec<u64>>,
) -> Usage {
    let mut u = Usage::default();
    let mut owners: Vec<(u64, usize, u64)> = Vec::new();
    for &(handle, owner, size) in objects {
        u.objects += 1;
        u.bytes = u.bytes.saturating_add(size);
        if owner == 0 || owner & ZOMBIE_PID_BASE != 0 {
            u.orphans += 1;
            u.orphan_bytes = u.orphan_bytes.saturating_add(size);
        }
        if let Some(h) = holders(handle) {
            if !h.is_empty() && h.iter().all(|&p| non_process_holder(p)) {
                u.pinned_only += 1;
                u.pinned_only_bytes = u.pinned_only_bytes.saturating_add(size);
            }
        }
        match owners.iter_mut().find(|o| o.0 == owner) {
            Some(o) => {
                o.1 += 1;
                o.2 = o.2.saturating_add(size);
            }
            None => owners.push((owner, 1, size)),
        }
    }
    for &size in deferred {
        u.deferred += 1;
        u.deferred_bytes = u.deferred_bytes.saturating_add(size);
    }
    owners.sort_unstable_by_key(|o| core::cmp::Reverse(o.2));
    owners.truncate(3);
    u.top = owners;
    u
}

/// The card's RM device instance, the number the other lines use for it.
fn card_of(gpu: &NvidiaGpu) -> u32 {
    (*gpu.rm_device_instance.lock()).unwrap_or(u32::MAX)
}

fn usage_of(gpu: &NvidiaGpu) -> Usage {
    let objects: Vec<(u32, u64, u64)> = gpu
        .nouveau_gem
        .lock()
        .iter()
        .map(|o| (o.handle, o.owner_pid, o.size))
        .collect();
    let deferred: Vec<u64> = gpu
        .nouveau_deferred_frees
        .lock()
        .iter()
        .map(|d| d.size)
        .collect();
    usage(&objects, &deferred, crate::scheme::gem_mmap::holders)
}

/// Write `u` to `dmesg` at error level, in lines that fit its 256 bytes.
fn log_usage(card: u32, u: &Usage, cap: u64) {
    crate::klog_err!(
        "[gem-quota] card {}: {} obj {} MiB + {} closed waiting on a ring {} MiB = {} of {} MiB (all cards {} MiB)",
        card,
        u.objects,
        u.bytes >> 20,
        u.deferred,
        u.deferred_bytes >> 20,
        u.charged() >> 20,
        cap >> 20,
        NOUVEAU_GEM_BYTES.load(Ordering::Relaxed) >> 20
    );
    crate::klog_err!(
        "[gem-quota] card {}: orphans {} obj {} MiB; held only by fb/dma-buf {} obj {} MiB",
        card,
        u.orphans,
        u.orphan_bytes >> 20,
        u.pinned_only,
        u.pinned_only_bytes >> 20
    );
    for (pid, n, bytes) in &u.top {
        crate::klog_err!(
            "[gem-quota] card {}: pid {:#x} holds {} obj {} MiB",
            card,
            pid,
            n,
            bytes >> 20
        );
    }
}

/// Refusals logged so far; the first 8 and then one in 256 get the full
/// breakdown, so a compositor retrying every frame does not flood `dmesg`.
static REFUSALS: AtomicU32 = AtomicU32::new(0);

/// Log a `GEM_NEW` that answers `ENOMEM`, with where the card's memory is.
pub(super) fn report_enomem(gpu: &NvidiaGpu, why: &str, pid: u64, size: u64, domain: u32) {
    let n = REFUSALS.fetch_add(1, Ordering::Relaxed);
    if n >= 8 && !n.is_multiple_of(256) {
        return;
    }
    let card = card_of(gpu);
    crate::klog_err!(
        "[gem-quota] GEM_NEW card {} pid {:#x} size {} domain {:#x} -> ENOMEM: {} (refusal #{})",
        card,
        pid,
        size,
        domain,
        why,
        n + 1
    );
    log_usage(card, &usage_of(gpu), card_cap(gpu.effective_vram_mb()));
}

/// The highest 512 MiB step of the all-cards total reported so far.
static REPORTED_STEP: AtomicU64 = AtomicU64::new(0);

/// After a `GEM_NEW`: report once per new 512 MiB step of the total.
pub(super) fn note_growth(gpu: &NvidiaGpu) {
    let step = NOUVEAU_GEM_BYTES.load(Ordering::Relaxed) / REPORT_STEP;
    if step == 0 || REPORTED_STEP.fetch_max(step, Ordering::Relaxed) >= step {
        return;
    }
    crate::klog_err!(
        "[gem-quota] GEM memory of all cards passed {} MiB (not an error by itself; a leak keeps climbing)",
        (step * REPORT_STEP) >> 20
    );
    log_usage(
        card_of(gpu),
        &usage_of(gpu),
        card_cap(gpu.effective_vram_mb()),
    );
}

/// The quota check `GEM_NEW` makes before the RM is asked. `Err` carries
/// the reason, already logged.
pub(super) fn check(gpu: &NvidiaGpu, pid: u64, size: u64, domain: u32) -> Result<(), i32> {
    if size > MAX_SINGLE {
        report_enomem(
            gpu,
            "larger than the single-allocation cap",
            pid,
            size,
            domain,
        );
        return Err(super::super::nouveau_uapi::ENOMEM);
    }
    let cap = card_cap(gpu.effective_vram_mb());
    let charged = |gpu: &NvidiaGpu| {
        let live: u64 = gpu.nouveau_gem.lock().iter().map(|o| o.size).sum();
        let waiting: u64 = gpu
            .nouveau_deferred_frees
            .lock()
            .iter()
            .map(|d| d.size)
            .sum();
        live.saturating_add(waiting)
    };
    if charged(gpu).saturating_add(size) <= cap {
        return Ok(());
    }
    // Free what the rings are done with, then look again.
    super::gem_free_queue::reap_deferred_frees(gpu);
    if charged(gpu).saturating_add(size) <= cap {
        return Ok(());
    }
    report_enomem(gpu, "this card's GEM limit is reached", pid, size, domain);
    Err(super::super::nouveau_uapi::ENOMEM)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn the_cap_is_twice_the_vram_and_never_under_4_gib() {
        assert_eq!(card_cap(8192), 16 << 30);
        assert_eq!(card_cap(0), 4 << 30);
        assert_eq!(card_cap(1024), 4 << 30);
    }

    #[test]
    fn usage_sorts_owners_and_finds_orphans_and_pinned_objects() {
        const FB: u64 = u64::MAX;
        const DMABUF: u64 = crate::scheme::gem_mmap::DMABUF_HOLDER;
        let objects = [
            (1, 100, 4 << 20),
            (2, 100, 4 << 20),
            (3, 200, 16 << 20),
            (4, 0, 8 << 20),
            (5, ZOMBIE_PID_BASE | 3, 1 << 20),
        ];
        let holders = |h: u32| match h {
            1 => Some(vec![100]),
            2 => Some(vec![FB, DMABUF]),
            4 => Some(vec![FB]),
            _ => None,
        };
        let u = usage(&objects, &[2 << 20], holders);
        assert_eq!(u.objects, 5);
        assert_eq!(u.bytes, 33 << 20);
        assert_eq!((u.deferred, u.deferred_bytes), (1, 2 << 20));
        assert_eq!(u.charged(), 35 << 20);
        assert_eq!((u.orphans, u.orphan_bytes), (2, 9 << 20));
        assert_eq!((u.pinned_only, u.pinned_only_bytes), (2, 12 << 20));
        assert_eq!(u.top[0], (200, 1, 16 << 20));
        assert_eq!(u.top[1], (100, 2, 8 << 20));
        assert_eq!(u.top.len(), 3);
    }
}
