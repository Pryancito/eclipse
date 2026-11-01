//! `exec_cpu_waits`: the waits of an `EXEC` that its arm would do on the CPU,
//! for the async pre-wait in `linux-object` to sleep through first. A child
//! of `nouveau_bookkeeping_tests` for the harness; a file of its own because
//! the parent is at its size ceiling (`xtask tamano`).
//!
//! What matters is that the set is the arm's own: a wait the GPU takes as an
//! ACQUIRE must not be named (the submission would be held back on the CPU
//! for nothing), a wait the arm does on the CPU must be (or it spins), and a
//! request the arm refuses before waiting must not be waited for at all.

use super::*;

fn cpu_waits(
    gpu: &NvidiaGpu,
    pid: u64,
    channel: u32,
    pushes: &[nv::DrmNouveauExecPush],
    waits: &[nv::DrmNouveauSync],
    sigs: &[nv::DrmNouveauSync],
) -> Option<(Vec<u32>, Vec<u64>)> {
    let r = nv::DrmNouveauExec {
        channel,
        push_count: pushes.len() as u32,
        wait_count: waits.len() as u32,
        sig_count: sigs.len() as u32,
        wait_ptr: ptr_of(waits),
        sig_ptr: ptr_of(sigs),
        push_ptr: ptr_of(pushes),
    };
    with_uvmm(gpu, pid);
    gpu.exec_cpu_waits(&r as *const nv::DrmNouveauExec as usize, pid)
}

#[test]
fn a_wait_the_gpu_acquires_is_not_waited_for_on_the_cpu() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch = client_with_pushbuf(&gpu, A);
    let rel = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(rel)]),
        Ok(0)
    );
    assert_eq!(syncobj::query(rel), Some(0), "still in flight");
    let out = syncobj::create(false);
    assert_eq!(
        cpu_waits(
            &gpu,
            A,
            ch,
            &[push(PUSH_VA, 16)],
            &[sync(rel)],
            &[sync(out)]
        ),
        None,
        "a fence on the submitter's own ring is an ACQUIRE: nothing to sleep on"
    );
    run_gpu(1);
    syncobj::destroy(rel);
    syncobj::destroy(out);
}

#[test]
fn an_empty_exec_with_nothing_to_attach_waits_for_everything_on_the_cpu() {
    // The health probe NVK sends after each syncobj wait: no pushes and no
    // fence to hang an ACQUIRE on, so its arm CPU-waits the whole set.
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch = client_with_pushbuf(&gpu, A);
    let rel = syncobj::create(false);
    assert_eq!(
        exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(rel)]),
        Ok(0)
    );
    let tl = syncobj::create(false);
    assert!(syncobj::timeline_signal(tl, 3));
    assert_eq!(
        cpu_waits(&gpu, A, ch, &[], &[sync(rel), sync_tl(tl, 2)], &[]),
        Some((alloc::vec![rel, tl], alloc::vec![1, 2]))
    );
    // Once the fence lands the set is ready on the first look, which is
    // what the arm then finds.
    run_gpu(1);
    assert!(matches!(
        syncobj::wait_ready(&[rel, tl], Some(&[1, 2]), true, u64::MAX),
        Some(Ok(_))
    ));
    syncobj::destroy(rel);
    syncobj::destroy(tl);
}

#[test]
fn a_request_the_arm_refuses_before_waiting_is_not_waited_for() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let ch = client_with_pushbuf(&gpu, A);
    let never = syncobj::create(false);
    let done = syncobj::create(true);
    const NOBODYS: u32 = 0xdead_0139;
    assert_eq!(syncobj::query(NOBODYS), None);
    let odd = nv::DrmNouveauSync {
        flags: 0x7,
        handle: done,
        timeline_value: 0,
    };
    let p = [push(PUSH_VA, 16)];
    for (waits, why) in [
        (alloc::vec![], "no waits"),
        (
            alloc::vec![sync(never)],
            "a point nobody submitted (EINVAL)",
        ),
        (
            alloc::vec![sync(NOBODYS)],
            "a handle that is nobody's (ENOENT)",
        ),
        (alloc::vec![odd], "a sync type nobody knows (EOPNOTSUPP)"),
    ] {
        assert_eq!(cpu_waits(&gpu, A, ch, &p, &waits, &[]), None, "{why}");
    }
    assert_eq!(
        cpu_waits(&gpu, A, ch + 7, &p, &[sync(done)], &[]),
        None,
        "a channel that is not the caller's (ENOENT)"
    );
    let too_many: Vec<_> = (0..65).map(|_| push(PUSH_VA, 16)).collect();
    assert_eq!(
        cpu_waits(&gpu, A, ch, &too_many, &[sync(done)], &[]),
        None,
        "more pushes than an EXEC may carry (EINVAL)"
    );
    syncobj::destroy(never);
    syncobj::destroy(done);
}
