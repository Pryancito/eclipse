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

/// A fence that never lands is given up on after `FENCE_TIMEOUT_US`:
/// syncobj advances the point so its CPU waiters do not park forever.
/// EXEC's contract on a wait that never arrives is EIO (NVK: device
/// lost), and that deadline is the same 10 s -- so whether the client's
/// push was refused or SUBMITTED behind a release that never happened
/// depended on which of the two clocks ticked first, i.e. on one extra
/// read of the clock, such as a QUERY between the submit and the EXEC.
#[test]
fn an_exec_wait_reached_only_by_a_timed_out_fence_is_eio_whichever_clock_ticks_first() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch_a = client_with_pushbuf(&gpu, A);
    let ch_b = client_with_pushbuf(&gpu, B);
    // The plain case first: A's fence timed out long ago. Userspace's
    // QUERY reads it as reached (Linux: a signaled fence, error and all).
    let rel = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(rel)]),
        Ok(0)
    );
    test_clock::advance(syncobj::FENCE_TIMEOUT_US + 1);
    assert_eq!(syncobj::query(rel), Some(1));
    let out = syncobj::create(false);
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA, 16)],
            &[sync(rel)],
            &[sync(out)]
        ),
        Err(nv::EIO),
        "the release never happened: not submitting"
    );
    assert!(
        !has_chan(2),
        "nothing on B's ring: it was never even prepared"
    );
    assert_eq!(syncobj::query(out), Some(0));
    // The same through a merge whose other half is genuine.
    let ok = syncobj::create(false);
    assert_eq!(
        exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[sync(ok)]),
        Ok(0)
    );
    assert_eq!(run_gpu(2).len(), 2);
    let b = chan(2);
    assert_eq!(userd(&b), (2, 2));
    assert_eq!(syncobj::query(ok), Some(1));
    let merged = syncobj::merge_fences(&[(rel, 1), (ok, 1)]);
    assert_eq!(syncobj::query(merged), Some(1));
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA, 16)],
            &[sync(merged)],
            &[sync(out)]
        ),
        Err(nv::EIO)
    );
    assert_eq!(userd(&b), (2, 2));
    // And the health probe: it signals nothing on a wait that died.
    assert_eq!(
        exec(&gpu, B, ch_b, &[], &[sync(rel)], &[sync(out)]),
        Err(nv::EIO)
    );
    assert_eq!(syncobj::query(out), Some(0));
    // The probe with a half the GPU could ACQUIRE (B's own fence in
    // flight): the dead half still decides, before anything is queued.
    let ok2 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[sync(ok2)]),
        Ok(0)
    );
    assert_eq!(userd(&b), (2, 4));
    assert_eq!(
        exec(&gpu, B, ch_b, &[], &[sync(ok2), sync(rel)], &[sync(out)]),
        Err(nv::EIO)
    );
    assert_eq!(
        userd(&b),
        (2, 4),
        "no acquire and no fence went onto the ring"
    );
    assert_eq!(syncobj::query(out), Some(0));
    assert_eq!(run_gpu(2).len(), 2);
    assert_eq!(syncobj::query(ok2), Some(1));
    // A point reached for real before the one that timed out stays good:
    // 1 landed, 2 never did.
    let tl = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync_tl(tl, 1)]),
        Ok(0)
    );
    assert_eq!(run_gpu(1).len(), 4, "A's two submits land");
    assert_eq!(syncobj::query(tl), Some(1));
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync_tl(tl, 2)]),
        Ok(0)
    );
    test_clock::advance(syncobj::FENCE_TIMEOUT_US + 1);
    assert_eq!(syncobj::query(tl), Some(2));
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA, 16)],
            &[sync_tl(tl, 1)],
            &[sync(out)]
        ),
        Ok(0),
        "point 1 was the GPU's word"
    );
    assert_eq!(userd(&b), (4, 6));
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA, 16)],
            &[sync_tl(tl, 2)],
            &[sync(out)]
        ),
        Err(nv::EIO)
    );
    assert_eq!(userd(&b), (4, 6));
    // Userspace signaling the point itself supersedes the dead fence.
    assert!(syncobj::timeline_signal(tl, 2));
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA, 16)],
            &[sync_tl(tl, 2)],
            &[sync(out)]
        ),
        Ok(0)
    );
    assert_eq!(userd(&b), (4, 8));
    assert_eq!(run_gpu(2).len(), 4);
    assert_eq!(syncobj::query(out), Some(1));
    // The race itself: B waits while the fence is still in flight, one
    // read of the clock per millisecond. The QUERY in between is the
    // read that used to make the fence timeout win over EXEC's own
    // deadline, and turned the EIO into a submit.
    let rel2 = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(rel2)]),
        Ok(0)
    );
    let out2 = syncobj::create(false);
    test_clock::set_auto_advance(1_000);
    assert_eq!(syncobj::query(rel2), Some(0));
    assert_eq!(
        exec(
            &gpu,
            B,
            ch_b,
            &[push(PUSH_VA, 16)],
            &[sync(rel2)],
            &[sync(out2)]
        ),
        Err(nv::EIO)
    );
    test_clock::set_auto_advance(0);
    assert_eq!(userd(&b), (8, 8), "still nothing new on B's ring");
    assert_eq!(syncobj::query(out2), Some(0));
    assert_eq!(syncobj::query(rel2), Some(1), "given up on, as before");
    for h in [rel, out, ok, ok2, merged, tl, rel2, out2] {
        assert!(syncobj::destroy(h));
    }
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// `nouveau_exec_ucopy` copies the three arrays, `nouveau_exec_job_init`
/// measures every push while it builds the job, and only
/// `nouveau_job_submit` looks the syncs up: a push that is empty, not a
/// dword stream or longer than a GPFIFO entry can name is EINVAL ahead of
/// a sig nobody owns (ENOENT) and of a wait of a type nobody knows
/// (EOPNOTSUPP), and a count with no array is EFAULT ahead of both. This
/// measured the pushes after the syncs, so a bad push behind a stranger's
/// sig was ENOENT.
#[test]
fn exec_measures_the_pushes_before_it_looks_the_syncs_up() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let ch = client_with_pushbuf(&gpu, A);
    const NOBODYS: u32 = 0xdead_0138;
    assert_eq!(syncobj::query(NOBODYS), None);
    let mine = syncobj::create(false);
    let odd = || nv::DrmNouveauSync {
        flags: 0x7,
        handle: mine,
        timeline_value: 0,
    };
    let before = FAKE_RM.lock().calls.len();
    for (len, why) in [
        (0, "empty"),
        (6, "not a dword stream"),
        (
            nv::EXEC_PUSH_MAX_LENGTH + 1,
            "longer than a GPFIFO entry can name",
        ),
    ] {
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA, len)], &[], &[sync(NOBODYS)]),
            Err(nv::EINVAL),
            "{why}: ahead of a sig nobody owns"
        );
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA, len)], &[odd()], &[sync(mine)]),
            Err(nv::EINVAL),
            "{why}: ahead of a wait of an unknown type"
        );
        assert_eq!(
            exec(
                &gpu,
                A,
                ch,
                &[push(PUSH_VA, 16), push(PUSH_VA, len)],
                &[],
                &[sync(NOBODYS)]
            ),
            Err(nv::EINVAL),
            "{why}: as the second push, behind a sound one"
        );
    }
    // The copy comes first: a count with no array faults before the bad
    // push is measured, and before the stranger's sig is looked up.
    let bad = [push(PUSH_VA, 0)];
    let nobodys = [sync(NOBODYS)];
    let mut r = nv::DrmNouveauExec {
        channel: ch,
        push_count: 1,
        wait_count: 0,
        sig_count: 1,
        wait_ptr: 0,
        sig_ptr: 0,
        push_ptr: ptr_of(&bad),
    };
    assert_eq!(
        exec_raw(&gpu, A, &mut r),
        Err(nv::EFAULT),
        "sigs with no array, ahead of the bad push"
    );
    let mut r = nv::DrmNouveauExec {
        channel: ch,
        push_count: 1,
        wait_count: 0,
        sig_count: 1,
        wait_ptr: 0,
        sig_ptr: ptr_of(&nobodys),
        push_ptr: 0,
    };
    assert_eq!(
        exec_raw(&gpu, A, &mut r),
        Err(nv::EFAULT),
        "pushes with no array, ahead of the sig nobody owns"
    );
    // With the pushes sound, the syncs decide as before.
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(NOBODYS)]),
        Err(nv::ENOENT)
    );
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[odd()], &[sync(mine)]),
        Err(nv::EOPNOTSUPP)
    );
    assert_eq!(
        rm_calls_since(before),
        [] as [&str; 0],
        "nothing reached the ring"
    );
    assert_eq!(syncobj::query(mine), Some(0), "nothing signaled");
    syncobj::destroy(mine);
}

/// NVK batches up to 256 waits and 256 signals per EXEC and 64 pushes
/// (`GETPARAM_EXEC_PUSH_MAX`), and every same-channel wait is a GPFIFO
/// ACQUIRE entry here, next to the pushes and the fence. The ring has
/// 128 entries with one kept free, so a batch of 64 pushes with 63 or
/// more such waits asked for room the ring never has: `fast_submit`
/// spun 10 s, latched the channel WEDGED and every EXEC after it was
/// EIO (VK_ERROR_DEVICE_LOST) until the process died. Linux has no such
/// ceiling: its waits are scheduler dependencies, not ring entries.
#[test]
fn an_exec_with_more_acquires_than_the_ring_holds_waits_for_the_rest_on_the_cpu_and_is_not_a_hang()
{
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch = client_with_pushbuf(&gpu, A);
    // One submit signals 100 semaphores with its one fence (65 or more
    // was EOPNOTSUPP before): 100 waits on them are 100 ACQUIREs.
    let sems: Vec<u32> = (0..100).map(|_| syncobj::create(false)).collect();
    let sigs: Vec<_> = sems.iter().map(|&s| sync(s)).collect();
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &sigs),
        Ok(0),
        "100 signals in one EXEC"
    );
    let c = chan(1);
    assert_eq!(userd(&c), (0, 2));
    assert!(sems.iter().all(|&s| syncobj::query(s) == Some(0)));
    // 100 waits + 64 pushes + the fence would be 165 entries.
    let waits: Vec<_> = sems.iter().map(|&s| sync(s)).collect();
    let pushes: Vec<_> = (0..64)
        .map(|i| push(PUSH_VA + 0x100 + i * 16, 16))
        .collect();
    let out = syncobj::create(false);
    let now = test_clock::now();
    let tick = AutoAdvance::of(1);
    // The GPU runs on its own: it lands the first fence, which satisfies
    // the waits that did not fit as ACQUIREs, and drains the ring for the
    // submission itself. It starts once the EXEC has parked in its CPU
    // wait (the wait's first look at the syncobj table), so the fence is
    // still pending when the EXEC decides what rides as an ACQUIRE, and
    // it polls the ring until the submission is in (`submitted`),
    // yielding between looks: no wall-clock budget and no sleep, so a run
    // with the suite oversubscribed (`--test-threads=32` on four cores)
    // cannot run the GPU out of time while the EXEC is still waiting --
    // it fetched 67 of 129 entries in 30 s there.
    let submitted = std::sync::atomic::AtomicBool::new(false);
    let probes = syncobj::wait_table_probes();
    let mut fetched = std::thread::scope(|s| {
        let t = s.spawn(|| {
            test_clock::set(now);
            while syncobj::wait_table_probes() == probes {
                std::thread::yield_now();
            }
            let mut got = Vec::new();
            while !submitted.load(Ordering::Acquire) {
                got.extend(run_gpu(1));
                std::thread::yield_now();
            }
            got
        });
        assert_eq!(
            exec(&gpu, A, ch, &pushes, &waits, &[sync(out)]),
            Ok(0),
            "62 ACQUIREs in the ring, 38 waits on the CPU"
        );
        submitted.store(true, Ordering::Release);
        t.join().unwrap()
    });
    drop(tick);
    // Whatever the GPU had not fetched when the EXEC returned.
    fetched.extend(run_gpu(1));
    assert!(!nv::ctx_is_wedged(1));
    assert_eq!(fetched.len(), 2 + 127);
    let acquires = fetched
        .iter()
        .filter(|f| matches!(f, Fetched::Acquire { payload: 1, .. }))
        .count();
    assert_eq!(
        acquires,
        127 - 64 - 1,
        "as many ACQUIREs as the ring holds next to 64 pushes and the fence"
    );
    assert_eq!(
        fetched
            .iter()
            .filter(|f| matches!(f, Fetched::Push { .. }))
            .count(),
        65
    );
    assert_eq!(
        fetched
            .iter()
            .filter(|f| matches!(f, Fetched::Release { .. }))
            .count(),
        2
    );
    assert!(
        matches!(fetched[2], Fetched::Acquire { .. }),
        "ACQUIREs first"
    );
    assert!(
        matches!(fetched[128], Fetched::Release { .. }),
        "the fence last"
    );
    assert_eq!(syncobj::query(out), Some(1));
    assert!(sems.iter().all(|&s| syncobj::query(s) == Some(1)));
    assert_eq!(userd(&c), (1, 1), "129 entries: wrapped once");
    // Not wedged: the channel goes on.
    assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
    // A submission larger than the ring on its own is refused at once,
    // not after 10 s, and does not wedge the channel either.
    let big: Vec<_> = (0..128).map(|i| push(PUSH_VA + i * 16, 16)).collect();
    // (1 ms per clock read: a submit that still spun would give up
    // after its 10 s instead of hanging the test.)
    let tick = AutoAdvance::of(1000);
    assert!(matches!(
        gpu.fast_submit(1, &big, false, &[]),
        Err(nv::FastSubmitError::TooLarge {
            needed: 128,
            entries: 128
        })
    ));
    assert!(!nv::ctx_is_wedged(1));
    // Through EXEC's direct path the answer is EINVAL, and still no
    // WEDGED latch: the next submission is welcome.
    let r = nv::DrmNouveauExec {
        channel: ch,
        push_count: big.len() as u32,
        wait_count: 0,
        sig_count: 0,
        wait_ptr: 0,
        sig_ptr: 0,
        push_ptr: big.as_ptr() as u64,
    };
    assert_eq!(gpu.exec_fast(1, A, &r, &big, &[]), Err(nv::EINVAL));
    drop(tick);
    assert!(!nv::ctx_is_wedged(1));
    assert_eq!(userd(&c), (1, 2), "nothing was written over the ring");
    assert_eq!(run_gpu(1).len(), 1);
    for s in sems {
        assert!(syncobj::destroy(s));
    }
    assert!(syncobj::destroy(out));
    gpu.nouveau_release_process(A);
    assert_eq!(FAKE_RM.lock().bad, 0);
}
