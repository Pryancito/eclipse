//! The fences userspace attaches to a buffer through
//! `DMA_BUF_IOCTL_IMPORT_SYNC_FILE`: this kernel's `dma_resv`.
//!
//! In Linux every dma-buf carries a reservation object, and a client using
//! an explicitly synchronised API (Vulkan, and so NVK) that has to hand a
//! buffer to a consumer which only knows implicit sync (a compositor that
//! flips it, a `CPU_PREP`, an X server copying it) puts its render fence on
//! that object with `DMA_BUF_IOCTL_IMPORT_SYNC_FILE`, and reads the fences of
//! the buffer's other users back out with `DMA_BUF_IOCTL_EXPORT_SYNC_FILE`
//! before it writes into it again (`dma_buf_import_sync_file` /
//! `dma_buf_export_sync_file`, `drivers/dma-buf/dma-buf.c`). Xwayland does
//! exactly that on the copy path of a window it cannot flip, and in
//! `xwl_glamor_get_fence`; Mesa's WSI does it on every present without
//! `linux-drm-syncobj-v1`.
//!
//! There was no such object here: both ioctls were `ENOTTY`, so a fence a
//! client imported was refused and the consumer read the buffer whenever it
//! liked. The nouveau `EXEC` of this tree carries no buffer list (as the real
//! one does not), so a buffer's own GPU work is not on this table either: it
//! is what the ring probes of `DrmScheme::queued_fences` stand for, and an
//! export merges both.
//!
//! Keyed by the buffer's physical address, which is what every dma-buf here
//! carries (a nouveau GEM object or a dumb buffer alike), and what `CPU_PREP`
//! and the scanout path can recover from a handle or a framebuffer. Each
//! entry owns a syncobj of its own whose point 1 is the imported fence: a
//! `sync_file` names a fence taken at export time, and a link that read the
//! source live would follow its next reset ([`syncobj::export_fence`]).
//!
//! Usage follows `dma_resv_usage_rw`: a **reader** (a read-only export, a
//! `CPU_PREP` for reading, a scanout) waits for the WRITE fences only; a
//! **writer** waits for READ and WRITE alike.

use super::syncobj;
use alloc::vec::Vec;
use lock::Mutex;

/// One imported fence: "`carrier` reaches 1".
struct Entry {
    phys: u64,
    carrier: u32,
    /// `DMA_BUF_SYNC_WRITE` was in the import's flags: the fence is a
    /// write into the buffer, which readers and writers both wait for.
    write: bool,
}

lazy_static::lazy_static! {
    static ref RESV: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
}

/// `dma_resv_add_fence`: the buffer at `phys` now carries the fence "`src`
/// reaches `point`", as a write into it when `write`. Returns `false` when
/// `src` is not a live syncobj.
pub fn add_fence(phys: u64, src: u32, point: u64, write: bool) -> bool {
    let carrier = syncobj::create(false);
    if !syncobj::transfer(carrier, 1, src, point) {
        syncobj::destroy(carrier);
        return false;
    }
    let gone = {
        let mut list = RESV.lock();
        let gone = take_reached(&mut list);
        list.push(Entry {
            phys,
            carrier,
            write,
        });
        gone
    };
    release(gone);
    true
}

/// `dma_resv_get_singleton` for an access of the given kind: the fences of
/// the buffer at `phys` still in flight that a **writer** (`write`) or a
/// **reader** has to wait for, as `(syncobj, point)` pairs. A reader waits
/// for the writes only (`DMA_RESV_USAGE_WRITE`); a writer for every fence
/// (`DMA_RESV_USAGE_READ`). Fences that have signaled are dropped on the way.
pub fn fences(phys: u64, write: bool) -> Vec<(u32, u64)> {
    let (out, gone) = {
        let mut list = RESV.lock();
        let gone = take_reached(&mut list);
        let out = list
            .iter()
            .filter(|e| e.phys == phys && (write || e.write))
            .map(|e| (e.carrier, 1))
            .collect();
        (out, gone)
    };
    release(gone);
    out
}

/// How many fences, of either usage, the buffer at `phys` still carries.
pub fn count(phys: u64) -> usize {
    let (n, gone) = {
        let mut list = RESV.lock();
        let gone = take_reached(&mut list);
        (list.iter().filter(|e| e.phys == phys).count(), gone)
    };
    release(gone);
    n
}

/// The buffer at `phys` is gone (its memory freed): drop what it carried.
/// An export still waiting on one of these fences keeps the fence, not the
/// buffer: its carrier stays in the syncobj table as an orphan until the
/// source signals ([`syncobj::destroy`]'s orphan rule), as a `dma_fence`
/// outlives the reservation that held it.
pub fn forget(phys: u64) {
    let gone: Vec<u32> = {
        let mut list = RESV.lock();
        let mut gone = Vec::new();
        list.retain(|e| {
            if e.phys == phys {
                gone.push(e.carrier);
                false
            } else {
                true
            }
        });
        gone
    };
    release(gone);
}

/// Pull out the entries whose fence has signaled (or whose syncobj is
/// gone). Callers hold the list lock; the carriers go to [`release`] once it
/// is dropped.
fn take_reached(list: &mut Vec<Entry>) -> Vec<u32> {
    let mut gone = Vec::new();
    list.retain(|e| {
        let reached = syncobj::query(e.carrier).is_none_or(|p| p >= 1);
        if reached {
            gone.push(e.carrier);
        }
        !reached
    });
    gone
}

fn release(carriers: Vec<u32>) {
    for c in carriers {
        syncobj::destroy(c);
    }
}

#[cfg(test)]
mod tests {
    //! The reservation object of a dma-buf: what an import puts on it, what
    //! a reader and a writer take from it, and when a fence leaves it.

    extern crate std;

    use super::*;

    lazy_static::lazy_static! {
        static ref LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    }

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A reader waits for the writes only, a writer for everything, and a
    /// fence that has signaled is no longer on the buffer.
    #[test]
    fn a_reader_waits_for_the_writes_and_a_writer_for_every_fence() {
        let _g = lock();
        let phys = 0x7_0000_0000;
        let read_fence = syncobj::create(false);
        let write_fence = syncobj::create(false);
        assert!(add_fence(phys, read_fence, 1, false));
        assert!(add_fence(phys, write_fence, 1, true));
        assert_eq!(count(phys), 2);
        let reader = fences(phys, false);
        assert_eq!(reader.len(), 1, "a reader sees the write only");
        let writer = fences(phys, true);
        assert_eq!(writer.len(), 2, "a writer sees both");
        for &(c, p) in reader.iter().chain(writer.iter()) {
            assert_eq!(p, 1);
            assert_eq!(syncobj::query(c), Some(0), "still in flight");
        }
        // The carrier stands for the source's point at import time, not
        // for the source itself: a reset of the source leaves it waiting.
        assert!(syncobj::signal(write_fence));
        assert_eq!(
            fences(phys, false),
            [] as [(u32, u64); 0],
            "the write landed"
        );
        assert_eq!(count(phys), 1, "the read is still in flight");
        assert!(syncobj::signal(read_fence));
        assert_eq!(count(phys), 0);
        assert_eq!(fences(phys, true), [] as [(u32, u64); 0]);
        syncobj::destroy(read_fence);
        syncobj::destroy(write_fence);
    }

    /// A fence taken at import time is the point the source had then: a
    /// timeline point already reached is a fence already signaled, an
    /// unknown source is refused, and the buffers do not see each other.
    #[test]
    fn an_import_names_the_point_of_its_moment_and_buffers_are_apart() {
        let _g = lock();
        let (a, b) = (0x7_1000_0000, 0x7_2000_0000);
        let tl = syncobj::create(false);
        assert!(syncobj::timeline_signal(tl, 3));
        assert!(
            add_fence(a, tl, 3, true),
            "reached already: signaled at once"
        );
        assert_eq!(count(a), 0);
        assert!(add_fence(a, tl, 5, true));
        assert_eq!(count(a), 1);
        assert_eq!(count(b), 0, "the other buffer carries nothing");
        assert!(!add_fence(b, 0xdead_beef, 1, true), "not a syncobj");
        assert_eq!(count(b), 0);
        assert!(syncobj::timeline_signal(tl, 5));
        assert_eq!(count(a), 0);
        syncobj::destroy(tl);
    }

    /// Freeing the buffer drops its fences from the buffer, and an export
    /// that was waiting on one keeps waiting for the fence itself, which
    /// goes once it has signaled.
    #[test]
    fn forgetting_a_buffer_drops_its_fences_and_an_export_keeps_the_fence() {
        let _g = lock();
        let phys = 0x7_3000_0000;
        let src = syncobj::create(false);
        assert!(add_fence(phys, src, 1, true));
        let deps = fences(phys, false);
        assert_eq!(deps.len(), 1);
        let export = syncobj::merge_fences(&deps);
        assert_eq!(syncobj::query(export), Some(0), "waiting on the import");
        forget(phys);
        assert_eq!(count(phys), 0);
        assert_eq!(fences(phys, true), [] as [(u32, u64); 0]);
        assert_eq!(
            syncobj::query(deps[0].0),
            Some(0),
            "the carrier lives on as an orphan while the export names it"
        );
        assert_eq!(
            syncobj::query(export),
            Some(0),
            "still waiting for the source"
        );
        assert!(syncobj::signal(src));
        assert_eq!(syncobj::query(export), Some(1));
        assert_eq!(
            syncobj::query(deps[0].0),
            None,
            "and is collected once done"
        );
        syncobj::destroy(export);
        syncobj::destroy(src);
    }
}
