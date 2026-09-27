//! The buffer's reservation (`dma_resv`): the fences a client imports onto
//! a dma-buf and the ring probes the driver appends, as `CPU_PREP` and
//! `EXPORT_SYNC_FILE` see them. A child of `nouveau_bookkeeping_tests` so
//! it has the same harness (`LOCK`, `LiveBytes`, `gpu_rm_fast`); a file of
//! its own because the parent is at its size ceiling (`xtask tamano`).

use super::*;

/// `CPU_PREP` waits for the fences userspace imported onto the buffer's
/// dma-buf (`DMA_BUF_IOCTL_IMPORT_SYNC_FILE`), with the usage rule of
/// `dma_resv_usage_rw`: a read waits for the writes only, a write for
/// every fence; `NOWAIT` is EBUSY while one is in flight, a blocking
/// prep on one that never lands is EBUSY after the deadline, and the
/// reservation goes with the buffer's memory. Nothing on the ring is
/// touched for any of it. Before this the import was refused
/// (`ENOTTY`), so a prep on a buffer another client had fenced went
/// straight through to memory the GPU was still writing.
#[test]
fn cpu_prep_waits_for_the_fences_imported_onto_the_buffers_dma_buf() {
    use crate::scheme::resv;
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let _ch = client_with_pushbuf(&gpu, A);
    let h = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    let (phys, _) = crate::scheme::gem_mmap::lookup(h).expect("registered: exportable");
    let read_fence = syncobj::create(false);
    let write_fence = syncobj::create(false);
    let prep = |flags: u32| cpu_prep_flags(&gpu, h, flags, A);
    const NOWAIT: u32 = nv::NOUVEAU_GEM_CPU_PREP_NOWAIT;
    const WRITE: u32 = nv::NOUVEAU_GEM_CPU_PREP_WRITE;
    let before = FAKE_RM.lock().calls.len();
    assert!(resv::add_fence(phys, read_fence, 1, false));
    assert_eq!(prep(NOWAIT), Ok(0), "a reader does not wait for a read");
    assert_eq!(prep(NOWAIT | WRITE), Err(nv::EBUSY), "a writer does");
    assert!(resv::add_fence(phys, write_fence, 1, true));
    assert_eq!(prep(NOWAIT), Err(nv::EBUSY), "a reader waits for a write");
    // Blocking, on fences that never land: EBUSY once the deadline has
    // passed, in virtual time (1 ms per clock read from here on).
    test_clock::set_auto_advance(1_000);
    let t0 = test_clock::now();
    assert_eq!(prep(0), Err(nv::EBUSY));
    let spent = test_clock::now() - t0;
    assert!(
        (10_000_000..12_000_000).contains(&spent),
        "waited the CPU_PREP deadline: {} us",
        spent
    );
    test_clock::set_auto_advance(0);
    assert!(syncobj::signal(write_fence));
    assert_eq!(prep(NOWAIT), Ok(0), "the write landed: a reader is free");
    assert_eq!(prep(0), Ok(0));
    assert_eq!(
        prep(NOWAIT | WRITE),
        Err(nv::EBUSY),
        "the read is still in flight: a writer waits"
    );
    assert!(syncobj::signal(read_fence));
    assert_eq!(prep(NOWAIT | WRITE), Ok(0));
    assert_eq!(resv::count(phys), 0, "signaled fences leave the buffer");
    assert_eq!(
        rm_calls_since(before),
        [] as [&str; 0],
        "nothing on the ring"
    );
    assert!(!has_chan(1), "no channel was prepared for it");
    // The reservation goes with the memory.
    let late = syncobj::create(false);
    assert!(resv::add_fence(phys, late, 1, true));
    assert_eq!(resv::count(phys), 1);
    assert!(gpu.nouveau_gem_close(h, A));
    assert_eq!(resv::count(phys), 0, "GEM_CLOSE dropped it");
    assert_eq!(cpu_prep_nowait(&gpu, h, A), Err(nv::ENOENT));
    for s in [read_fence, write_fence, late] {
        syncobj::destroy(s);
    }
}

/// `queued_fences` (what `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` hands out):
/// the idle probe of the owner's ring and of every mapper's, real
/// fences that land when those rings drain; nothing for an idle buffer
/// or an unknown handle. A bundle of them is reached only once every
/// ring has passed its probe.
#[test]
fn queued_fences_names_the_rings_of_the_owner_and_of_every_mapper() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch_a = client_with_pushbuf(&gpu, A);
    let ch_b = client_with_pushbuf(&gpu, B);
    let h = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    assert_eq!(gpu.queued_fences(h), [], "nothing queued");
    assert_eq!(
        gpu.queued_fences(h + 1000),
        [],
        "not a buffer of this driver"
    );
    assert_eq!(exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    let own = gpu.queued_fences(h);
    assert_eq!(own.len(), 1, "the owner's ring");
    assert_eq!(own[0].ctx_idx, 1);
    assert_eq!(userd(&chan(1)), (0, 2), "a probe behind the push");
    // B imports and maps the buffer, and has work of its own queued.
    assert!(crate::scheme::gem_mmap::add_ref(h, B).is_some());
    const VA: u64 = 0x1_a000_0000;
    assert_eq!(vm_bind_ops(&gpu, B, &mut [map(h, VA, 4096)]), Ok(0));
    assert_eq!(exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    let both = gpu.queued_fences(h);
    let mut ctxs: Vec<u32> = both.iter().map(|f| f.ctx_idx).collect();
    ctxs.sort_unstable();
    assert_eq!(ctxs, [1, 2], "the owner's ring and the mapper's");
    let bundle = syncobj::bundle_fence(&both, &[]);
    assert_eq!(syncobj::query(bundle), Some(0), "both rings busy");
    for f in &both {
        assert!(!syncobj::hw_fence_landed(f.fence_va, f.payload));
    }
    run_gpu(2);
    let _ = syncobj::poll_pending();
    assert_eq!(
        syncobj::query(bundle),
        Some(0),
        "the owner's ring is still busy"
    );
    run_gpu(1);
    let _ = syncobj::poll_pending();
    assert_eq!(
        syncobj::query(bundle),
        Some(1),
        "both rings passed their probes"
    );
    assert_eq!(gpu.queued_fences(h), [], "idle again: no probe is appended");
    syncobj::destroy(bundle);
}
