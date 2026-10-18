//! The double's own invariants.
//!
//! `rm_host_shims` is not production code: it is the RM that the 9.000-odd
//! lines of `nouveau_bookkeeping_tests` and `surfaceflip_tests` talk to. That
//! is exactly why it needs tests of its own. A double whose switch is dead,
//! whose bookkeeping forgets a free, or whose refusal answers `NV_OK` does
//! not fail anything: it makes every test above it go green while asserting
//! nothing. The same argument as `utils/host_hooks`.
//!
//! What is pinned here is the double's contract with the tests that use it:
//! that a handle it never handed out is refused, that a stage it never
//! reached is distinguishable from one that succeeded, that the hardware
//! ladder says "not supported" rather than "fine", and that `reset_fake_rm`
//! really empties it -- including the one field whose empty value is not
//! zero.
//!
//! Every test takes the same `LOCK` as `nouveau_bookkeeping_tests`: `FAKE_RM`
//! is one global, and these tests write it.

extern crate std;

use super::super::nouveau_bookkeeping_tests::LOCK;
use super::*;

/// These are plain `repr(C)` out-parameters the C side fills; none of them
/// derives `Default`, and all-bits-zero is what the caller hands in.
fn zeroed<T>() -> T {
    unsafe { core::mem::zeroed() }
}

/// `gem_alloc` through the shim, returning the handle it minted.
fn gem(size: u64, sysmem: bool) -> u32 {
    let mut out = zeroed::<GemAlloc>();
    assert_eq!(
        eclipse_rm_gem_alloc(0, size, u32::from(sysmem), &mut out),
        NV_OK
    );
    assert_eq!(out.alloc_status, 0, "the fake refused an allocation");
    out.h_memory
}

/// `vm_bind_map` through the shim.
fn bind(ctx: u32, h: u32, va: u64, size: u64) -> VmBind {
    let mut out = zeroed::<VmBind>();
    assert_eq!(
        eclipse_rm_vm_bind_map(0, ctx, h, size, va, 0, 0, 0, &mut out),
        NV_OK
    );
    out
}

/// `exec_submit` through the shim, as its four stage statuses.
fn submit(ctx: u32, va: u64, len: u32) -> [u32; 4] {
    let mut out = zeroed::<ExecSubmit>();
    assert_eq!(eclipse_rm_exec_submit(0, ctx, va, len, &mut out), NV_OK);
    [
        out.lookup_status,
        out.map_status,
        out.token_status,
        out.submit_status,
    ]
}

/// A context the fake knows about.
fn ctx(idx: u32) {
    let mut out = zeroed::<CtxAlloc>();
    assert_eq!(eclipse_rm_ctx_alloc(0, idx, &mut out), NV_OK);
    assert_eq!(out.sched_status, NV_OK);
}

/// `reset_fake_rm` is called at the top of every fixture in the suites
/// above. If it left anything behind, a test would be reading the state
/// its predecessor built -- and would still pass, because the state it
/// expects is usually a subset of what is there.
///
/// `next` is the field that matters most here: its empty value is `0x100`
/// and not zero, so a reset written as "zero everything" would leave the
/// fake minting handles from 1, and a test that happens to use a literal
/// handle would start colliding with real ones.
#[test]
fn resetting_the_fake_empties_every_ledger_including_the_one_that_is_not_zero() {
    let _g = LOCK.lock();
    reset_fake_rm();

    ctx(1);
    let h = gem(4096, true);
    bind(1, h, 0x1_0000, 4096);
    eclipse_rm_ctx_prime(0, 1);
    FAKE_RM.lock().ring_full = true;

    {
        let f = FAKE_RM.lock();
        assert!(!f.ctxs.is_empty() && !f.gems.is_empty() && !f.maps.is_empty());
    }

    reset_fake_rm();
    let f = FAKE_RM.lock();
    assert!(f.ctxs.is_empty(), "a context survived the reset");
    assert!(f.gems.is_empty(), "an object survived the reset");
    assert!(f.maps.is_empty(), "a binding survived the reset");
    assert!(f.primed.is_empty());
    assert!(f.calls.is_empty(), "the call trail survived the reset");
    assert!(!f.ring_full, "a switch survived the reset");
    assert_eq!((f.gem_frees, f.ctx_frees, f.unmaps, f.bad), (0, 0, 0, 0));
    assert_eq!(
        f.next, 0x100,
        "the handle counter's empty value is not zero"
    );
}

/// Handles are minted from one counter shared by objects, bindings and
/// class objects, so no two of them can collide -- and none is zero, which
/// is what the RM uses for "no handle". A test that compared a handle
/// against 0 to mean "none" would be wrong if the fake ever minted one.
#[test]
fn no_handle_the_fake_mints_is_zero_or_repeats() {
    let _g = LOCK.lock();
    reset_fake_rm();
    ctx(1);

    let mut seen = alloc::vec::Vec::new();
    for i in 0..8 {
        let h = gem(4096, i % 2 == 0);
        seen.push(h);
        seen.push(bind(1, h, 0x10_0000 + (i as u64) * 0x1000, 4096).h_virt);
        let (mut obj, mut st) = (0u32, 0u32);
        assert_eq!(
            eclipse_rm_class_alloc(0, 1, 0x902d, &mut obj, &mut st),
            NV_OK
        );
        assert_eq!(st, 0);
        seen.push(obj);
    }
    assert!(seen.iter().all(|h| *h != 0), "the fake minted handle 0");
    let mut sorted = seen.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), seen.len(), "two handles collided: {:?}", seen);
}

/// An object the fake never handed out must be refused, not quietly
/// accepted. This is the property the suites above lean on when they check
/// that the driver does not free a stranger's handle: if `gem_free`
/// answered `NV_OK` for anything, those tests would pass with the check
/// removed from the driver.
#[test]
fn freeing_or_unmapping_something_the_fake_never_handed_out_is_refused_and_counted() {
    let _g = LOCK.lock();
    reset_fake_rm();

    assert_eq!(eclipse_rm_gem_free(0, 0xdead), NV_ERR_OBJECT_NOT_FOUND);
    assert_eq!(
        eclipse_rm_vm_bind_unmap(0, 0xdead, 4096, 0),
        NV_ERR_OBJECT_NOT_FOUND
    );
    assert_eq!(eclipse_rm_class_free(0, 0xdead), NV_ERR_OBJECT_NOT_FOUND);
    let f = FAKE_RM.lock();
    assert_eq!(f.bad, 3, "a refusal was not counted");
    assert_eq!(f.gem_frees, 0, "a stranger counted as a free");
    assert_eq!(f.class_frees, 0);
    // The unmap counter counts attempts, not successes: it is what the
    // suites use to see that the driver tried.
    assert_eq!(f.unmaps, 1);
}

/// A sysmem object gets real, zeroed memory of the size asked for, and
/// `gem_map_cpu` reports that memory's own address -- `phys_to_virt` is
/// the identity in this binary, so the driver reads the pushbuffer it
/// wrote straight through it. If the fake handed out an invented address,
/// every test that writes a push and expects the fake to read it would be
/// scribbling on whatever lives there.
#[test]
fn a_sysmem_object_is_backed_by_real_zeroed_memory_at_the_address_it_reports() {
    let _g = LOCK.lock();
    reset_fake_rm();

    let h = gem(256, true);
    let mut m = zeroed::<GemMapCpu>();
    assert_eq!(eclipse_rm_gem_map_cpu(0, h, &mut m), NV_OK);
    assert_eq!(m.lookup_status, 0);
    assert_eq!(m.address_space, ADDR_SYSMEM);
    assert_eq!(m.size, 256);
    assert_ne!(m.phys_addr, 0);

    let p = m.phys_addr as *mut u8;
    unsafe {
        assert!(
            (0..256).all(|i| *p.add(i) == 0),
            "the fake handed out memory it had not zeroed"
        );
        *p.add(255) = 0xA5;
        assert_eq!(*p.add(255), 0xA5, "the memory is not writable");
    }
    assert_eq!(FAKE_RM.lock().pa_of(h), Some(m.phys_addr));
}

/// Vidmem has no CPU mapping and no host memory: it answers aperture 0
/// (FBMEM) with no address, and its offset comes from
/// `gem_fbmem_offset` instead. A fake that gave vidmem a host PA would
/// let a test "read" video memory that does not exist.
#[test]
fn a_vidmem_object_has_no_cpu_mapping_and_its_offset_comes_from_fbmem() {
    let _g = LOCK.lock();
    reset_fake_rm();

    let h = gem(MIB, false);
    let mut m = zeroed::<GemMapCpu>();
    assert_eq!(eclipse_rm_gem_map_cpu(0, h, &mut m), NV_OK);
    assert_eq!(m.lookup_status, 0, "the object exists");
    assert_eq!(m.address_space, 0, "vidmem is FBMEM");
    assert_eq!((m.phys_addr, m.size), (0, 0));
    assert_eq!(FAKE_RM.lock().pa_of(h), None);

    let mut off = 0u64;
    assert_eq!(eclipse_rm_gem_fbmem_offset(0, h, &mut off), NV_OK);
    assert_eq!(off, fake_fbmem_offset(h));

    // And a sysmem object has no FBMEM offset at all.
    let sys = gem(4096, true);
    assert_eq!(
        eclipse_rm_gem_fbmem_offset(0, sys, &mut off),
        NV_ERR_NOT_SUPPORTED
    );
}

/// An object the RM does not hold has no mapping either, and the status
/// says so rather than handing back a zeroed descriptor that reads as a
/// valid mapping at address 0.
#[test]
fn mapping_an_object_the_fake_does_not_hold_says_not_found() {
    let _g = LOCK.lock();
    reset_fake_rm();
    let mut m = zeroed::<GemMapCpu>();
    assert_eq!(eclipse_rm_gem_map_cpu(0, 0xbeef, &mut m), NV_OK);
    assert_eq!(m.lookup_status, NV_ERR_OBJECT_NOT_FOUND);
}

/// The VA conflict the fake models is the RM eheap's: a fixed-address
/// allocation over a LIVE range. Half-open, so two bindings that touch do
/// not conflict -- a fake that used closed ranges would refuse a perfectly
/// legal pair of adjacent buffers and send a test hunting a driver bug
/// that is not there.
#[test]
fn a_binding_conflicts_on_overlap_and_not_on_touching() {
    let _g = LOCK.lock();
    reset_fake_rm();
    ctx(1);
    let a = gem(0x2000, true);
    let b = gem(0x2000, true);

    assert_eq!(bind(1, a, 0x10_0000, 0x1000).map_status, 0);
    // Starts exactly where the first ends: no overlap.
    assert_eq!(
        bind(1, b, 0x10_1000, 0x1000).map_status,
        0,
        "two bindings that touch were treated as overlapping"
    );
    // One byte into the first: overlap.
    assert_eq!(bind(1, b, 0x10_0fff, 0x1000).map_status, RM_VA_TAKEN);
    // Wholly inside the first.
    assert_eq!(bind(1, b, 0x10_0100, 0x100).map_status, RM_VA_TAKEN);
    // And a refused binding leaves nothing behind.
    assert_eq!(FAKE_RM.lock().maps.len(), 2);
}

/// Each context has its own address space, so the same VA in another
/// context is not a conflict. The whole point of VM_BIND's per-context
/// VAS: a fake that kept one global VA map would make every second
/// client's binding fail.
#[test]
fn the_same_va_in_another_context_is_not_a_conflict() {
    let _g = LOCK.lock();
    reset_fake_rm();
    ctx(1);
    ctx(2);
    let h = gem(0x1000, true);
    assert_eq!(bind(1, h, 0x20_0000, 0x1000).map_status, 0);
    assert_eq!(bind(2, h, 0x20_0000, 0x1000).map_status, 0);
    assert_eq!(bind(1, h, 0x20_0000, 0x1000).map_status, RM_VA_TAKEN);
}

/// Unmapping frees the VA for the next binding, and takes exactly one
/// range with it. A fake that forgot to remove the range would make a
/// rebind of the same VA fail, which is the shape of the replace
/// semantics the driver's VM_BIND is built on.
#[test]
fn unmapping_frees_the_va_it_held_and_nothing_else() {
    let _g = LOCK.lock();
    reset_fake_rm();
    ctx(1);
    let h = gem(0x2000, true);
    let first = bind(1, h, 0x30_0000, 0x1000);
    bind(1, h, 0x30_1000, 0x1000);

    assert_eq!(bind(1, h, 0x30_0000, 0x1000).map_status, RM_VA_TAKEN);
    assert_eq!(
        eclipse_rm_vm_bind_unmap(0, first.h_virt, 0x1000, 0x30_0000),
        NV_OK
    );
    assert_eq!(
        bind(1, h, 0x30_0000, 0x1000).map_status,
        0,
        "the VA was not freed"
    );
    // The neighbour is untouched.
    assert_eq!(bind(1, h, 0x30_1000, 0x1000).map_status, RM_VA_TAKEN);
}

/// Freeing a context destroys its VAS, which takes every binding in it --
/// and only those. The suites above check that the driver does not unmap
/// one by one on teardown; that check is only meaningful if the fake
/// really drops them with the VAS.
#[test]
fn freeing_a_context_drops_its_bindings_and_leaves_the_other_contexts_alone() {
    let _g = LOCK.lock();
    reset_fake_rm();
    ctx(1);
    ctx(2);
    let h = gem(0x1000, true);
    bind(1, h, 0x40_0000, 0x1000);
    bind(1, h, 0x41_0000, 0x1000);
    bind(2, h, 0x40_0000, 0x1000);

    assert_eq!(eclipse_rm_ctx_free(0, 1), NV_OK);
    let f = FAKE_RM.lock();
    assert_eq!(f.maps.len(), 1, "a binding of the freed context survived");
    assert_eq!(f.maps[0].1, 2, "the wrong context's binding was dropped");
    assert_eq!(f.ctxs, [2]);
    assert_eq!(f.ctx_frees, 1);
    // And no unmap was counted: the VAS took them, nobody called unmap.
    assert_eq!(f.unmaps, 0);
}

/// `incomplete_ctx` is the C side's shape for a ladder that failed
/// part-way: `NV_OK` from the call, the failure in `sched_status`, and
/// nothing of the context alive. A fake that registered the context
/// anyway would let a test believe a half-built context is usable.
#[test]
fn a_context_that_failed_part_way_answers_ok_and_leaves_nothing_alive() {
    let _g = LOCK.lock();
    reset_fake_rm();
    FAKE_RM.lock().incomplete_ctx = true;

    let mut out = zeroed::<CtxAlloc>();
    assert_eq!(
        eclipse_rm_ctx_alloc(0, 3, &mut out),
        NV_OK,
        "the call itself succeeds"
    );
    assert_ne!(out.sched_status, NV_OK, "the failure is in the status word");
    assert!(
        FAKE_RM.lock().ctxs.is_empty(),
        "a context that never finished is alive"
    );

    // And `fail_ctx_alloc` is the other shape: the call itself refuses.
    FAKE_RM.lock().incomplete_ctx = false;
    FAKE_RM.lock().fail_ctx_alloc = true;
    assert_eq!(eclipse_rm_ctx_alloc(0, 3, &mut out), NV_ERR_NOT_SUPPORTED);
    assert!(FAKE_RM.lock().ctxs.is_empty());
}

/// A stage the submit never reached stays at `0xFFFF_FFFF`, which is what
/// tells a reader "not run" apart from `0` ("ran, succeeded"). A fake that
/// zeroed the unreached stages would make a failed submit look like three
/// successes followed by a failure, and the driver's own report
/// (`/proc/gpudbg`) is read that way.
#[test]
fn a_stage_the_submit_never_reached_is_not_a_success() {
    let _g = LOCK.lock();
    reset_fake_rm();
    ctx(1);

    const UNREACHED: u32 = 0xFFFF_FFFF;
    assert_eq!(
        submit(1, 0x50_0000, 64),
        [NV_ERR_OBJECT_NOT_FOUND, UNREACHED, UNREACHED, UNREACHED],
        "an unmapped push VA did not stop at the lookup"
    );
}

/// The RM looks the push up in the context's VAS and wants it WHOLLY
/// inside one binding: a push that starts inside and runs off the end is
/// refused. Overlap would be the wrong test -- the GPU would fetch past
/// the mapping.
#[test]
fn a_push_must_lie_wholly_inside_one_binding() {
    let _g = LOCK.lock();
    reset_fake_rm();
    ctx(1);
    let h = gem(0x1000, true);
    bind(1, h, 0x60_0000, 0x1000);

    assert_eq!(
        submit(1, 0x60_0000, 0x1000),
        [0, 0, 0, 0],
        "the exact range"
    );
    assert_eq!(submit(1, 0x60_0f00, 0x100), [0, 0, 0, 0], "the last bytes");
    assert_eq!(
        submit(1, 0x60_0f00, 0x101)[0],
        NV_ERR_OBJECT_NOT_FOUND,
        "a push running one byte off the end was accepted"
    );
    assert_eq!(
        submit(1, 0x5f_ffff, 0x10)[0],
        NV_ERR_OBJECT_NOT_FOUND,
        "a push starting one byte before the binding was accepted"
    );
    // And the push the OTHER context cannot see is refused too.
    ctx(2);
    assert_eq!(submit(2, 0x60_0000, 0x10)[0], NV_ERR_OBJECT_NOT_FOUND);
}

/// Every submit is recorded whether it went through or not: the suites
/// above count attempts, and a fake that only logged the successful ones
/// would make a driver that retries forever look quiet.
#[test]
fn a_refused_submit_is_still_recorded() {
    let _g = LOCK.lock();
    reset_fake_rm();
    ctx(1);
    submit(1, 0x70_0000, 16);
    submit(1, 0x70_0000, 16);
    let f = FAKE_RM.lock();
    assert_eq!(f.submits.len(), 2, "a refused submit was not recorded");
    assert_eq!(f.submits[0], (1, 0x70_0000, 16, None));
}

/// A full ring fails at the LAST stage, with the first three at zero: the
/// lookup and the token did happen. `NV_ERR_BUSY_RETRY` in the wrong
/// stage would send the driver's retry logic down a different arm.
#[test]
fn a_full_ring_fails_only_at_the_submit_stage() {
    let _g = LOCK.lock();
    reset_fake_rm();
    ctx(1);
    let h = gem(0x1000, true);
    bind(1, h, 0x80_0000, 0x1000);
    FAKE_RM.lock().ring_full = true;
    assert_eq!(submit(1, 0x80_0000, 16), [0, 0, 0, NV_ERR_BUSY_RETRY]);
}

/// The signalled submit lands its fence before returning -- the GPU
/// "runs" the push -- and `fence_stalls` is the switch that makes it not.
/// If the switch were dead, every test of the driver's wait path would be
/// waiting on a fence that was already there and would pass without ever
/// exercising the wait.
#[test]
fn the_fence_lands_unless_the_switch_says_it_stalls() {
    let _g = LOCK.lock();
    reset_fake_rm();
    ctx(1);
    let h = gem(0x1000, true);
    bind(1, h, 0x90_0000, 0x1000);

    let mut out = zeroed::<ExecSignal>();
    assert_eq!(
        eclipse_rm_exec_submit_signaled(0, 1, 0x90_0000, 16, 0x1234, 0, &mut out),
        NV_OK
    );
    assert_eq!(out.fence_submit_status, 0);
    assert_ne!(out.fence_sem_phys, 0);
    let sem = out.fence_sem_phys as *const AtomicU32;
    assert_eq!(unsafe { (*sem).load(Ordering::Acquire) }, 0x1234);

    FAKE_RM.lock().fence_stalls = true;
    assert_eq!(
        eclipse_rm_exec_submit_signaled(0, 1, 0x90_0000, 16, 0x5678, 0, &mut out),
        NV_OK
    );
    assert_eq!(
        unsafe { (*sem).load(Ordering::Acquire) },
        0x1234,
        "the stall switch is dead: the fence landed anyway"
    );
}

/// A submit that never happened reports no fence address at all, rather
/// than one the driver would then poll forever on a stale value.
#[test]
fn a_submit_that_never_happened_reports_no_fence_address() {
    let _g = LOCK.lock();
    reset_fake_rm();
    ctx(1);
    let mut out = zeroed::<ExecSignal>();
    assert_eq!(
        eclipse_rm_exec_submit_signaled(0, 1, 0xa0_0000, 16, 9, 0, &mut out),
        NV_OK
    );
    assert_eq!(out.lookup_status, NV_ERR_OBJECT_NOT_FOUND);
    assert_eq!(out.fence_submit_status, 0xFFFF_FFFF);
    assert_eq!(
        out.fence_sem_phys, 0,
        "a fence address for a submit that did not happen"
    );
}

/// The hardware ladder -- GSP, display, EDID, HDMI audio, the cursor, the
/// copy engine -- answers `NV_ERR_NOT_SUPPORTED`, because a test GPU never
/// reaches it. The value matters: `NV_OK` is zero, so a shim that fell
/// through to a default would be reporting success, and a driver step that
/// should have bailed would carry on into hardware that is not there.
#[test]
fn the_hardware_ladder_says_not_supported_and_never_ok() {
    let _g = LOCK.lock();
    reset_fake_rm();
    let mut sink = [0u8; 256];
    let p = sink.as_mut_ptr();
    let answers = [
        (
            "attach_gpu",
            eclipse_rm_attach_gpu(0, 0, 0, 0, p, 0, 0, 0, 0, 0, p),
        ),
        ("bench", eclipse_rm_bench(0, p)),
        ("ce_blit", eclipse_rm_ce_blit(0, 0, 0, 0, p)),
        ("ce_blit_p2p", eclipse_rm_ce_blit_p2p(0, 0, 0, 0, p)),
        (
            "ce_blit_p2p_2d",
            eclipse_rm_ce_blit_p2p_2d(0, 0, 0, 0, 0, 0, 0, p),
        ),
        ("ce_fill_fb", eclipse_rm_ce_fill_fb(0, 0, 0, 0)),
        ("ce_fill_fb_p2p", eclipse_rm_ce_fill_fb_p2p(0, 0, 0, 0)),
        ("ce_release_inflight", eclipse_rm_ce_release_inflight()),
        ("ce_wait", eclipse_rm_ce_wait(0, 0)),
        ("edid", eclipse_rm_edid(0, p)),
        ("get_gsp_info", eclipse_rm_get_gsp_info(0, p)),
        ("hdmi_audio", eclipse_rm_hdmi_audio(0, 0, 0, 0, p)),
        ("hwcursor_hide", eclipse_rm_hwcursor_hide(0)),
    ];
    for (name, got) in answers {
        assert_eq!(got, NV_ERR_NOT_SUPPORTED, "{name} did not refuse");
        assert_ne!(got, NV_OK, "{name} reported success");
    }
}

/// The error notifier is real memory the EXEC failure path reads through
/// `phys_to_virt` (the identity here), so its address has to be a live,
/// aligned, stable one. A fake that invented it would have that path
/// reading whatever is at that address.
#[test]
fn the_error_notifier_is_real_aligned_memory_that_does_not_move() {
    let _g = LOCK.lock();
    reset_fake_rm();
    let pa = notifier_pa();
    assert_ne!(pa, 0);
    assert_eq!(pa % 4, 0, "an NvNotification is read as words");
    assert_eq!(pa, notifier_pa(), "the notifier moved between calls");
    let words = pa as *const AtomicU32;
    for i in 0..4 {
        assert_eq!(unsafe { (*words.add(i)).load(Ordering::Relaxed) }, 0);
    }
    // And it survives a reset: it is memory, not ledger state.
    reset_fake_rm();
    assert_eq!(pa, notifier_pa());
}

/// There is a notifier to read only while step 17's channel stands, and
/// the fake refuses before it. A fake that handed the address out anyway
/// would let a test read an error notifier of a channel that does not
/// exist and find it conveniently zero.
#[test]
fn the_notifier_address_is_refused_before_a_channel_is_built() {
    let _g = LOCK.lock();
    reset_fake_rm();
    let mut pa = 0xffff_ffff_ffff_ffffu64;
    assert_ne!(eclipse_rm_chan_notifier_pa(0, &mut pa), NV_OK);
    assert_eq!(
        pa, 0xffff_ffff_ffff_ffff,
        "it wrote the out-parameter anyway"
    );

    FAKE_RM.lock().chan_built = true;
    assert_eq!(eclipse_rm_chan_notifier_pa(0, &mut pa), NV_OK);
    assert_eq!(pa, notifier_pa());
}

/// `ctx0_reset` is a no-op before step 17 -- as the C is -- and tears the
/// channel down after. The counter only moves when there was something to
/// tear down, which is what a test distinguishes "the driver reset a live
/// channel" from "the driver called reset into the void" with.
#[test]
fn resetting_context_zero_is_a_no_op_until_there_is_a_channel() {
    let _g = LOCK.lock();
    reset_fake_rm();

    assert_eq!(eclipse_rm_ctx0_reset(0), NV_OK);
    assert_eq!(
        FAKE_RM.lock().ctx0_resets,
        0,
        "it counted a reset of nothing"
    );

    ctx(0);
    ctx(1);
    FAKE_RM.lock().chan_built = true;
    assert_eq!(eclipse_rm_ctx0_reset(0), NV_OK);
    let f = FAKE_RM.lock();
    assert_eq!(f.ctx0_resets, 1);
    assert!(!f.chan_built, "the channel latch was not cleared");
    assert_eq!(f.ctxs, [1], "it took a context that was not zero");
}

/// Direct submit is behind a switch, and the switch is what the suites use
/// to drive the driver down the slow path. A dead switch would make every
/// "EXEC falls back" test exercise the fast path instead and pass.
#[test]
fn direct_submit_is_refused_until_the_switch_is_on() {
    let _g = LOCK.lock();
    reset_fake_rm();
    ctx(1);
    let mut out = ExecFast::default();
    assert_eq!(
        eclipse_rm_exec_fast_prepare(0, 1, &mut out),
        NV_ERR_NOT_SUPPORTED
    );

    FAKE_RM.lock().fast = true;
    assert_eq!(eclipse_rm_exec_fast_prepare(0, 1, &mut out), NV_OK);
    assert_eq!(out.status, NV_OK);
}

/// With the switch on but no context behind the index, the call still
/// answers `NV_OK` and puts the refusal in `status` -- the C side's shape
/// past its argument checks -- and hands out nothing.
#[test]
fn a_direct_submit_window_for_a_context_that_does_not_exist_is_refused_in_the_status() {
    let _g = LOCK.lock();
    reset_fake_rm();
    FAKE_RM.lock().fast = true;
    let mut out = ExecFast::default();
    assert_eq!(eclipse_rm_exec_fast_prepare(0, 9, &mut out), NV_OK);
    assert_eq!(out.status, NV_ERR_INVALID_STATE);
    assert_eq!(out.work_token, 0);
    assert!(FAKE_RM.lock().fast_ctxs.is_empty());
}

/// The window the fake hands out describes real memory: the three regions
/// it names (fence pushbuffer, semaphore landing zone, GPFIFO ring) have
/// to lie inside the 64 KiB buffer at the offsets it reports, and the ring
/// has to fit. The driver writes GP entries through these, so a region
/// that ran past the end would corrupt the heap and the test would see a
/// mystery.
#[test]
fn the_direct_submit_window_describes_memory_that_is_really_there() {
    let _g = LOCK.lock();
    reset_fake_rm();
    FAKE_RM.lock().fast = true;
    ctx(1);
    let mut out = ExecFast::default();
    assert_eq!(eclipse_rm_exec_fast_prepare(0, 1, &mut out), NV_OK);
    assert_eq!(out.status, NV_OK);

    assert_eq!(out.fence_pb_off, FAST_PB_OFF);
    assert_eq!(out.fence_sem_off, FAST_SEM_OFF);
    assert_eq!(out.gpfifo_off, FAST_GPFIFO_OFF);
    assert_eq!(out.gpfifo_entries, FAST_ENTRIES);
    assert_eq!(out.slot_bytes, FAST_SLOT_BYTES);
    assert_eq!(out.doorbell_reg, FAKE_DOORBELL);

    let base = out.fence_pb_phys - u64::from(FAST_PB_OFF);
    assert_eq!(out.fence_sem_phys, base + u64::from(FAST_SEM_OFF));
    assert_eq!(out.gpfifo_phys, base + u64::from(FAST_GPFIFO_OFF));
    // 8192 u64 words = 64 KiB, and the ring is 8 bytes an entry.
    const BUF_BYTES: u64 = 8192 * 8;
    let ring_end = u64::from(FAST_GPFIFO_OFF) + u64::from(FAST_ENTRIES) * 8;
    assert!(
        ring_end <= BUF_BYTES,
        "the GPFIFO ring runs past the buffer"
    );
    assert!(u64::from(FAST_SEM_OFF) > u64::from(FAST_PB_OFF));
    assert!(u64::from(FAST_GPFIFO_OFF) > u64::from(FAST_SEM_OFF));
    // The USERD window is its own allocation, and GPGet/GPPut lie in it.
    assert_ne!(out.userd_cpu, 0);
    assert!(out.userd_gpput_off < out.userd_size);
    assert!(out.userd_gpget_off < out.userd_size);
    assert_ne!(out.userd_gpget_off, out.userd_gpput_off);
}

/// Asking twice for the same context gives the SAME window: the RM does
/// not build a second channel behind one index. A fake that minted a new
/// one each time would hide a driver that re-prepares on every submit.
#[test]
fn asking_twice_for_the_same_context_gives_the_same_window() {
    let _g = LOCK.lock();
    reset_fake_rm();
    FAKE_RM.lock().fast = true;
    ctx(1);
    let mut a = ExecFast::default();
    let mut b = ExecFast::default();
    eclipse_rm_exec_fast_prepare(0, 1, &mut a);
    eclipse_rm_exec_fast_prepare(0, 1, &mut b);
    assert_eq!(a.work_token, b.work_token);
    assert_eq!(a.gpfifo_phys, b.gpfifo_phys);
    assert_eq!(a.userd_cpu, b.userd_cpu);
    assert_eq!(FAKE_RM.lock().fast_ctxs.len(), 1);
}

/// A window left over from a channel that was freed is refused rather than
/// handed back: poking the USERD of a dead channel is BAR1 the RM has
/// reclaimed. The build counter is what distinguishes them, and this is
/// the test that it is actually consulted.
#[test]
fn a_window_left_over_from_a_freed_channel_is_refused() {
    let _g = LOCK.lock();
    reset_fake_rm();
    FAKE_RM.lock().fast = true;
    ctx(1);
    let mut out = ExecFast::default();
    eclipse_rm_exec_fast_prepare(0, 1, &mut out);
    assert_eq!(out.status, NV_OK);

    // The channel goes, and a new one is built behind the same index.
    eclipse_rm_ctx_free(0, 1);
    ctx(1);
    eclipse_rm_exec_fast_prepare(0, 1, &mut out);
    assert_eq!(
        out.status, NV_ERR_INVALID_STATE,
        "a stale USERD window was handed back"
    );
}

/// Releasing a window the fake never handed out is a no-op and is not
/// recorded, so `fast_releases` counts real releases. A fake that logged
/// every call would make a driver that releases twice look correct.
#[test]
fn releasing_a_window_that_was_never_handed_out_is_not_recorded() {
    let _g = LOCK.lock();
    reset_fake_rm();
    FAKE_RM.lock().fast = true;
    ctx(1);
    let mut out = ExecFast::default();
    eclipse_rm_exec_fast_prepare(0, 1, &mut out);

    assert_eq!(eclipse_rm_exec_fast_release(0, 2), NV_OK);
    assert!(FAKE_RM.lock().fast_releases.is_empty());

    assert_eq!(eclipse_rm_exec_fast_release(0, 1), NV_OK);
    assert_eq!(FAKE_RM.lock().fast_releases, [1]);
    assert!(FAKE_RM.lock().fast_ctxs.is_empty());
    // And a second release of the same window finds nothing.
    eclipse_rm_exec_fast_release(0, 1);
    assert_eq!(
        FAKE_RM.lock().fast_releases,
        [1],
        "a double release counted twice"
    );
}

/// Every entry point records itself, in order, including the ones that
/// refuse. The suites above assert on `calls` to pin the order the driver
/// walks the RM in; a shim that forgot to record would make a missing step
/// invisible.
#[test]
fn the_call_trail_records_refusals_too_and_keeps_the_order() {
    let _g = LOCK.lock();
    reset_fake_rm();
    ctx(1);
    eclipse_rm_ctx_prime(0, 1);
    eclipse_rm_gem_free(0, 0xdead); // refused
    let h = gem(4096, true);
    bind(1, h, 0xb0_0000, 4096);
    submit(1, 0xb0_0000, 16);
    assert_eq!(
        FAKE_RM.lock().calls,
        [
            "ctx_alloc",
            "ctx_prime",
            "gem_free",
            "gem_alloc",
            "vm_bind_map",
            "exec_submit",
        ]
    );
}

/// `maps_of_ctx` is what the suites read a context's address space
/// through, so it has to answer that context's bindings and only those,
/// with the VA, size and PTE kind the driver asked for.
#[test]
fn the_address_space_of_a_context_is_its_own_bindings_with_the_kind_asked_for() {
    let _g = LOCK.lock();
    reset_fake_rm();
    ctx(1);
    ctx(2);
    let h = gem(0x1000, true);
    let mut out = zeroed::<VmBind>();
    assert_eq!(
        eclipse_rm_vm_bind_map(0, 1, h, 0x1000, 0xc0_0000, 0, 0x06, 0, &mut out),
        NV_OK
    );
    bind(2, h, 0xc1_0000, 0x1000);

    let f = FAKE_RM.lock();
    assert_eq!(f.maps_of_ctx(1), [(0xc0_0000, 0x1000, 0x06)]);
    assert_eq!(f.maps_of_ctx(2), [(0xc1_0000, 0x1000, 0)]);
    assert!(f.maps_of_ctx(3).is_empty());
}

const MIB: u64 = 1024 * 1024;
