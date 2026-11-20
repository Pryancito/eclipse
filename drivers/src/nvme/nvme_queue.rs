use alloc::slice;
use core::marker::PhantomData;
use volatile::Volatile;

use super::NvmeCommonCommand;
use super::NvmeCompletion;
use crate::{DeviceError, DeviceResult};

// The DMA memory, the clock and the test clock: they moved to
// `utils::dma_provider` and are still reached by this name.
pub use crate::utils::dma_provider::*;

#[derive(Debug)]
pub struct NvmeQueue<P: Provider> {
    provider: PhantomData<P>,

    pub sq: &'static mut [Volatile<NvmeCommonCommand>],
    pub cq: &'static mut [Volatile<NvmeCompletion>],

    pub qid: usize,

    pub cq_head: usize,

    pub cq_phase: usize,

    pub sq_tail: usize,

    /// Per-queue command identifier counter (NVMe requires unique CIDs among
    /// outstanding commands; we run one command at a time per queue).
    pub cid_counter: u16,

    pub sq_pa: usize,

    pub cq_pa: usize,

    /// Page-rounded byte lengths of the two rings. Kept because giving a DMA
    /// region back needs the size it was taken with, and `q_size * 64` is not
    /// that size once it has been rounded up to a page.
    sq_len: usize,
    cq_len: usize,

    /// DMA bounce buffer backing this queue's data transfers.
    pub data_pa: usize,
    pub data_va: usize,
    pub data_len: usize,

    /// One DMA page used as the command's PRP list when a transfer spans more
    /// than two pages (PRP1 = page 0, PRP2 -> this list with the remaining
    /// page addresses). 512 u64 entries fit; the 32-page bounce needs 31.
    pub prp_list_pa: usize,
    pub prp_list_va: usize,
}

impl<P: Provider> NvmeQueue<P> {
    /// Build a queue and the four DMA regions it runs on.
    ///
    /// Fallible, because every one of those four can fail and the old code
    /// could not say so: it took whatever `alloc_dma` answered, and when the
    /// kernel has no contiguous run of pages that is the physical address `0`.
    /// `phys_to_virt` made a pointer out of it and the zeroing below wrote
    /// through it; the queue then ran with its rings at physical zero.
    pub fn new(qid: usize, q_size: usize) -> DeviceResult<Self> {
        if q_size == 0 {
            // `from_raw_parts_mut` would build two empty slices and every
            // index into them is then out of bounds. `controller_window`
            // refuses a queue this small already; this is the belt to that.
            return Err(DeviceError::InvalidParam);
        }
        // SQ: 64 bytes per entry. CQ: 16 bytes per entry.
        let sq_bytes = q_size * 64;
        let cq_bytes = q_size * 16;

        // Round up to page size
        let sq_pages = sq_bytes.div_ceil(P::PAGE_SIZE);
        let cq_pages = cq_bytes.div_ceil(P::PAGE_SIZE);

        // 32 pages (128 KiB), not 2: with a 2-page bounce every 1 MiB
        // read-ahead window became 128 serialized 8 KiB commands — enough
        // per-command latency to leave NVMe SLOWER than SATA. 128 KiB per
        // command needs a PRP list (below); `io_rw` still clamps each command
        // to the controller's advertised MDTS.
        let data_len = P::PAGE_SIZE * 32;
        let sq_len = sq_pages * P::PAGE_SIZE;
        let cq_len = cq_pages * P::PAGE_SIZE;

        let regions = [
            (P::alloc_dma(data_len), data_len),
            (P::alloc_dma(P::PAGE_SIZE), P::PAGE_SIZE),
            (P::alloc_dma(sq_len), sq_len),
            (P::alloc_dma(cq_len), cq_len),
        ];

        // All four or none. This is a probe, and one that gives up holding
        // three of them makes the retry likelier to fail than the attempt that
        // just did: 128 KiB of CONTIGUOUS pages is the hardest ask in here.
        // Until now nothing in the tree called `dealloc_dma` at all.
        if regions.iter().any(|((_, pa), _)| *pa == 0) {
            for ((va, pa), len) in regions {
                if pa != 0 {
                    P::dealloc_dma(va, len);
                }
            }
            return Err(DeviceError::DmaError);
        }

        let [((data_va, data_pa), _), ((prp_list_va, prp_list_pa), _), ((sq_va, sq_pa), _), ((cq_va, cq_pa), _)] =
            regions;

        trace!(
            "data_va: {:x}, sq_pa: {:x}, cq_pa: {:x}",
            data_va,
            sq_pa,
            cq_pa
        );

        // The zeroing the completion queue needs -- so phase-bit polling does
        // not read stale memory as a valid completion -- happens inside
        // `alloc_dma` now, together with the writeback that makes it visible
        // to the controller. Doing it here with a plain `write_bytes` left
        // those lines DIRTY in cache, and the first `clflush_range` in
        // `wait_cq` then writes them back over a completion the controller has
        // already DMA'd in. `DmaRegion` carries the same warning.

        // Safety: the two slices alias DMA regions this queue owns and gives
        // back in `Drop`, so they do not outlive it despite the `'static`.
        let submit_queue =
            unsafe { slice::from_raw_parts_mut(sq_va as *mut Volatile<NvmeCommonCommand>, q_size) };

        let complete_queue =
            unsafe { slice::from_raw_parts_mut(cq_va as *mut Volatile<NvmeCompletion>, q_size) };

        Ok(NvmeQueue {
            provider: PhantomData,
            sq: submit_queue,
            cq: complete_queue,
            qid,
            cq_head: 0,
            cq_phase: 1, // Phase starts at 1
            sq_tail: 0,
            cid_counter: 0,
            sq_pa,
            cq_pa,
            sq_len,
            cq_len,
            data_pa,
            data_va,
            data_len,
            prp_list_pa,
            prp_list_va,
        })
    }

    pub fn next_cid(&mut self) -> u16 {
        self.cid_counter = self.cid_counter.wrapping_add(1);
        self.cid_counter
    }
}

/// Give the four regions back.
///
/// There was no `Drop` at all, so `dealloc_dma` -- the other half of the
/// `Provider` trait -- had no caller anywhere in the tree, and a queue that
/// went away took 128 KiB of contiguous DMA plus its rings and its PRP list
/// with it. `utils::dma::DmaRegion` next door has had this since it was
/// written; this file allocates by hand and so has none of what it does.
impl<P: Provider> Drop for NvmeQueue<P> {
    fn drop(&mut self) {
        P::dealloc_dma(self.data_va, self.data_len);
        P::dealloc_dma(self.prp_list_va, P::PAGE_SIZE);
        P::dealloc_dma(self.sq.as_ptr() as usize, self.sq_len);
        P::dealloc_dma(self.cq.as_ptr() as usize, self.cq_len);
    }
}
