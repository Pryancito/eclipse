//! `nouveau_exec_ioctl` and `nouveau_job_add_deps`: what EXEC and VM_BIND
//! read from their wait and sig arrays and when they refuse. A child of
//! `nouveau_bookkeeping_tests` so it has the same harness (`LOCK`,
//! `gpu_rm`, `call`, `wr`, the syncobj helpers); a file of its own because
//! the parent is at its size ceiling (`xtask tamano`).

use super::*;

#[test]
fn an_exec_or_vm_bind_wait_on_a_point_nobody_submitted_is_einval_before_anything() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let ch = client_with_pushbuf(&gpu, A);
    let never = syncobj::create(false);
    let done = syncobj::create(true);
    let tl = syncobj::create(false);
    assert!(syncobj::timeline_signal(tl, 3));
    let out = syncobj::create(false);
    let out_tl = syncobj::create(false);
    let waited = nv::EXEC_WAIT_US.load(Ordering::Relaxed);
    let before = FAKE_RM.lock().calls.len();
    // 1 ms per clock read: a wait that spun would reach its deadline
    // in 10 000 reads, and be accounted.
    crate::nvme::nvme_queue::test_clock::set_auto_advance(1000);
    for (waits, why) in [
        (alloc::vec![sync(never)], "a binary syncobj with no fence"),
        (
            alloc::vec![sync_tl(tl, 4)],
            "a timeline point past the chain",
        ),
        (
            alloc::vec![sync_tl(never, 1)],
            "a timeline point on an empty chain",
        ),
        (
            alloc::vec![sync(done), sync_tl(tl, 3), sync(never)],
            "the third of three, behind two that pass",
        ),
    ] {
        assert_eq!(
            exec(
                &gpu,
                A,
                ch,
                &[push(PUSH_VA, 16)],
                &waits,
                &[sync(out), sync_tl(out_tl, 5)]
            ),
            Err(nv::EINVAL),
            "{why}"
        );
        assert_eq!(
            exec(&gpu, A, ch, &[], &waits, &[sync(out)]),
            Err(nv::EINVAL),
            "the health probe builds the same job: {why}"
        );
        assert_eq!(rm_calls_since(before), [] as [&str; 0], "{why}");
        assert_eq!(syncobj::query(out), Some(0), "not signaled: {why}");
        assert_eq!(syncobj::query(out_tl), Some(0), "{why}");
    }
    crate::nvme::nvme_queue::test_clock::set_auto_advance(0);
    assert_eq!(
        nv::EXEC_WAIT_US.load(Ordering::Relaxed),
        waited,
        "no deadline spent"
    );
    // A binary syncobj RESET has no fence any more; point 0 asks for
    // the chain's head, which the timeline has.
    assert!(syncobj::reset(done));
    assert_eq!(
        exec(&gpu, A, ch, &[], &[sync(done)], &[sync(out)]),
        Err(nv::EINVAL)
    );
    assert_eq!(
        exec(&gpu, A, ch, &[], &[sync_tl(tl, 0)], &[sync(out)]),
        Ok(0),
        "point 0 is the last fence"
    );
    assert_eq!(syncobj::query(out), Some(1));
    // The waits are read in order, and all of them before the sigs
    // (`nouveau_job_add_deps` runs before
    // `nouveau_job_fence_attach_prepare`): an unknown type ahead is
    // EOPNOTSUPP, an unknown handle ahead ENOENT, and an unsubmitted
    // wait beats an unknown sig.
    let odd = || nv::DrmNouveauSync {
        flags: 0x7,
        handle: never,
        timeline_value: 0,
    };
    assert_eq!(
        exec(&gpu, A, ch, &[], &[odd(), sync(never)], &[sync(out)]),
        Err(nv::EOPNOTSUPP)
    );
    assert_eq!(
        exec(&gpu, A, ch, &[], &[sync(never), odd()], &[sync(out)]),
        Err(nv::EINVAL)
    );
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[],
            &[sync(0xdead_0007), sync(never)],
            &[sync(out)]
        ),
        Err(nv::ENOENT)
    );
    assert_eq!(
        exec(
            &gpu,
            A,
            ch,
            &[],
            &[sync(never), sync(0xdead_0007)],
            &[sync(out)]
        ),
        Err(nv::EINVAL)
    );
    assert_eq!(
        exec(&gpu, A, ch, &[], &[sync(never)], &[sync(0xdead_0007)]),
        Err(nv::EINVAL),
        "the waits before the sigs"
    );
    // VM_BIND builds its job the same way: EINVAL before the ops,
    // nothing bound, nothing signaled, no deadline spent.
    let h = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    const VA: u64 = 0x1_8000_0000;
    let maps = driver_maps(&gpu, A).len();
    let before = FAKE_RM.lock().calls.len();
    crate::nvme::nvme_queue::test_clock::set_auto_advance(1000);
    assert_eq!(
        vm_bind_sync(
            &gpu,
            A,
            &mut [map(h, VA, 65536)],
            nv::VM_BIND_RUN_ASYNC,
            &[sync(never)],
            &[sync_tl(out_tl, 5)]
        ),
        Err(nv::EINVAL)
    );
    assert_eq!(
        vm_bind_sync(
            &gpu,
            A,
            &mut [map(h, VA, 65536)],
            nv::VM_BIND_RUN_ASYNC,
            &[sync_tl(tl, 3), sync_tl(tl, 4)],
            &[sync_tl(out_tl, 5)]
        ),
        Err(nv::EINVAL)
    );
    crate::nvme::nvme_queue::test_clock::set_auto_advance(0);
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    assert_eq!(driver_maps(&gpu, A).len(), maps, "nothing bound");
    assert_eq!(syncobj::query(out_tl), Some(0), "nothing signaled");
    assert_eq!(nv::EXEC_WAIT_US.load(Ordering::Relaxed), waited);
    // Once the point is submitted -- signaled outright here -- the same
    // waits pass.
    assert!(syncobj::signal(never));
    assert_eq!(
        vm_bind_sync(
            &gpu,
            A,
            &mut [map(h, VA, 65536)],
            nv::VM_BIND_RUN_ASYNC,
            &[sync(never), sync_tl(tl, 3)],
            &[sync_tl(out_tl, 5)]
        ),
        Ok(0)
    );
    assert_eq!(syncobj::query(out_tl), Some(5));
    assert_eq!(driver_maps(&gpu, A).len(), maps + 1);
    assert_eq!(vm_bind_ops(&gpu, A, &mut [unmap(VA, 65536)]), Ok(0));
    for s in [never, done, tl, out, out_tl] {
        assert!(syncobj::destroy(s));
    }
    gpu.nouveau_release_process(A);
}

/// `without_fence` resolves the pending list (a read of every in-flight
/// fence's landing zone) under the table lock. Asked once per wait, a full
/// EXEC paid one resolution per wait where one answers the whole list, and
/// the answer (the first wait without a fence, in order) is the same.
#[test]
fn the_waits_of_an_exec_are_looked_up_in_one_resolution_of_the_table() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let ch = client_with_pushbuf(&gpu, A);
    let done = syncobj::create(true);
    let never = syncobj::create(false);
    let out = syncobj::create(false);
    // `NVKMD_NOUVEAU_MAX_SYNCS`, the most NVK batches into one EXEC and the
    // cap the ioctl enforces (`MAX_EXEC_SYNC`).
    let full = 256;
    let mut waits: Vec<nv::DrmNouveauSync> = (1..full).map(|_| sync(done)).collect();
    waits.push(sync(never));
    assert_eq!(waits.len(), full);
    let before = syncobj::RESOLVE_WALKS.load(Ordering::Relaxed);
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &waits, &[sync(out)]),
        Err(nv::EINVAL),
        "the last wait, behind {} that pass",
        full - 1
    );
    assert_eq!(
        syncobj::RESOLVE_WALKS.load(Ordering::Relaxed) - before,
        1,
        "one resolution for {full} waits"
    );
    assert_eq!(syncobj::query(out), Some(0), "nothing signaled");
    for s in [done, never, out] {
        assert!(syncobj::destroy(s));
    }
    gpu.nouveau_release_process(A);
}
