use super::*;

#[test]
fn vm_bind_stays_inside_the_va_space_and_off_the_range_vm_init_reserved() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let init = |pid: u64, addr: u64, size: u64| {
        let mut r = nv::DrmNouveauVmInit {
            kernel_managed_addr: addr,
            kernel_managed_size: size,
        };
        call(
            &gpu,
            wr::<nv::DrmNouveauVmInit>(nv::NR_VM_INIT),
            &mut r,
            pid,
        )
    };
    let bo = |pid: u64| {
        gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, pid)
            .unwrap()
            .handle
    };
    channel_alloc(&gpu, A).unwrap();
    channel_alloc(&gpu, B).unwrap();
    channel_alloc(&gpu, STRANGER).unwrap();
    // A reserves 1 GiB at 1 TiB for the kernel; the helpers' own
    // VM_INIT (the low 4 GiB) comes second and does not replace it.
    const KSTART: u64 = 1 << 40;
    const KSIZE: u64 = 1 << 30;
    const KEND: u64 = KSTART + KSIZE;
    const END: u64 = nv::NOUVEAU_VA_SPACE_END;
    assert_eq!(init(A, KSTART, KSIZE), Ok(0));
    let ha = bo(A);
    // Past the VA space (without wrapping): EINVAL, MAP and UNMAP alike.
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(ha, END - 4096, 8192)]),
        Err(nv::EINVAL),
        "a page past the end"
    );
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(ha, END, 4096)]),
        Err(nv::EINVAL),
        "starting at the end"
    );
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [unmap(END, 4096)]),
        Err(nv::EINVAL),
        "an UNMAP past the end"
    );
    // Touching the reservation anywhere: EINVAL.
    for (what, op) in [
        ("at its start", map(ha, KSTART, 4096)),
        ("its last page", map(ha, KEND - 4096, 4096)),
        ("straddling its start", map(ha, KSTART - 4096, 8192)),
        ("straddling its end", map(ha, KEND - 4096, 8192)),
        ("an UNMAP inside it", unmap(KSTART + 4096, 4096)),
    ] {
        assert_eq!(vm_bind_ops(&gpu, A, &mut [op]), Err(nv::EINVAL), "{what}");
    }
    // A batch is checked whole: a good op ahead of a bad one binds nothing.
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(ha, KEND, 4096), unmap(KSTART, 4096)]),
        Err(nv::EINVAL)
    );
    assert!(driver_maps(&gpu, A).is_empty(), "nothing reached the RM");
    // Ending exactly at the reservation, starting exactly at its end,
    // and the last page of the VA space: bound.
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(ha, KSTART - 4096, 4096)]),
        Ok(0)
    );
    assert_eq!(vm_bind_ops(&gpu, A, &mut [map(bo(A), KEND, 4096)]), Ok(0));
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(bo(A), END - 4096, 4096)]),
        Ok(0)
    );
    assert_eq!(driver_maps(&gpu, A).len(), 3);
    // The reservation is the client's own: B, with the helpers' low
    // 4 GiB, may bind where A may not, and not below 4 GiB.
    let hb = bo(B);
    assert_eq!(
        vm_bind_ops(&gpu, B, &mut [map(hb, 0x1000_0000, 4096)]),
        Err(nv::EINVAL)
    );
    assert_eq!(vm_bind_ops(&gpu, B, &mut [map(hb, KSTART, 4096)]), Ok(0));
    // An empty reservation blocks nothing, not even the op that
    // straddles its address.
    assert_eq!(init(STRANGER, KSTART + 4096, 0), Ok(0));
    assert_eq!(
        vm_bind_ops(&gpu, STRANGER, &mut [map(bo(STRANGER), KSTART, 8192)]),
        Ok(0)
    );
    // Process exit drops the reservation with the uvmm: A starts over.
    gpu.nouveau_release_process(A);
    assert_eq!(init(A, 0, 0), Ok(0));
    assert_eq!(vm_bind_ops(&gpu, A, &mut [map(bo(A), KSTART, 4096)]), Ok(0));
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
    gpu.nouveau_release_process(STRANGER);
}

/// `nouveau_uvmm_bind_job_submit` looks every MAP's handle up in the
/// caller's file (`drm_gem_object_lookup`) before anything is validated
/// or applied, and handle 0 names no GEM object: ENOENT, with the batch
/// untouched. Here a MAP with handle 0 skipped the lookup and was applied
/// as "unmap the range, keep the reservation", draining whatever the
/// caller had mapped there -- a shape no client sends Linux (NVK unbinds
/// with UNMAP, and its sparse regions are MAP with the SPARSE flag,
/// refused here as EOPNOTSUPP), so a stray handle took a live mapping
/// with it instead of an error, and in a batch did so after the ops
/// ahead of it had been applied.
#[test]
fn a_vm_bind_map_with_no_object_is_enoent_and_unmaps_nothing() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let _ch_a = client_with_pushbuf(&gpu, A);
    let ha = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    const VA: u64 = 0x1_9000_0000;
    let bound = driver_maps(&gpu, A).len();
    assert_eq!(vm_bind_ops(&gpu, A, &mut [map(ha, VA, 65536)]), Ok(0));
    assert_eq!(driver_maps(&gpu, A).len(), bound + 1);
    let before = FAKE_RM.lock().calls.len();
    // Over the live mapping, beside it, and as pitch: no object, ENOENT.
    for bad in [
        map(0, VA, 65536),
        map(0, VA + 0x10_0000, 4096),
        op(nv::VM_BIND_OP_MAP, 0, 0, VA, 4096),
    ] {
        assert_eq!(vm_bind_ops(&gpu, A, &mut [bad]), Err(nv::ENOENT));
    }
    assert_eq!(driver_maps(&gpu, A).len(), bound + 1, "still mapped");
    assert_eq!(
        rm_calls_since(before),
        [] as [&str; 0],
        "nothing reached the RM"
    );
    // In a batch it is found before anything is applied: the good MAP
    // ahead of it does not run, nor the UNMAP behind it.
    let hb = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        vm_bind_ops(
            &gpu,
            A,
            &mut [
                map(hb, VA + 0x20_0000, 4096),
                map(0, VA + 0x30_0000, 4096),
                unmap(VA, 65536),
            ]
        ),
        Err(nv::ENOENT)
    );
    assert_eq!(driver_maps(&gpu, A).len(), bound + 1);
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    // A sparse region is what it was, and UNMAP is the way to unmap.
    assert_eq!(
        vm_bind_ops(
            &gpu,
            A,
            &mut [op(
                nv::VM_BIND_OP_MAP,
                nv::VM_BIND_SPARSE,
                0,
                VA + 0x30_0000,
                4096
            )]
        ),
        Err(nv::EOPNOTSUPP)
    );
    assert_eq!(vm_bind_ops(&gpu, A, &mut [unmap(VA, 65536)]), Ok(0));
    assert_eq!(driver_maps(&gpu, A).len(), bound);
    gpu.nouveau_release_process(A);
}

/// `nouveau_uvmm_bind_job_submit` runs `bind_validate_op` over every
/// op of a `VM_BIND` when the job is submitted, before the job waits
/// its in-syncs or touches the VA space, so a batch with a bad op in
/// it does nothing at all. Here the ops were checked as they were
/// applied: op[0]'s UNMAP was already in the RM when op[1] was
/// refused -- the caller told "nothing happened" had lost a live
/// mapping -- and a bad op behind an unsignaled wait was heard of only
/// after the wait had cost its whole deadline.
#[test]
fn a_vm_bind_batch_with_a_bad_op_binds_nothing_and_unbinds_nothing() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let _ch_a = client_with_pushbuf(&gpu, A);
    let ha = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    const VA: u64 = 0x7f_3000_0000;
    let bound = driver_maps(&gpu, A).len();
    assert_eq!(vm_bind_ops(&gpu, A, &mut [map(ha, VA, 65536)]), Ok(0));
    // A's mappings in this test's stretch of VA: (va, size).
    let live = || {
        driver_maps(&gpu, A)
            .iter()
            .filter(|m| (VA..VA + 0x100_0000).contains(&m.1))
            .map(|m| (m.1, m.2))
            .collect::<Vec<_>>()
    };
    assert_eq!(live(), [(VA, 65536)]);
    let bad: [(nv::DrmNouveauVmBindOp, i32, &str); 6] = [
        (
            map(ha + 1000, VA + 0x10_0000, 4096),
            nv::ENOENT,
            "a handle A does not hold",
        ),
        (
            op(
                nv::VM_BIND_OP_MAP,
                nv::VM_BIND_SPARSE,
                0,
                VA + 0x10_0000,
                4096,
            ),
            nv::EOPNOTSUPP,
            "a sparse region",
        ),
        (
            map(ha, VA + 0x10_0001, 4096),
            nv::EINVAL,
            "a VA off the page",
        ),
        (map(ha, VA + 0x10_0000, 0), nv::EINVAL, "an empty range"),
        (
            map_at(ha, VA + 0x10_0000, 4096, 65536),
            nv::EINVAL,
            "a window past the object",
        ),
        (
            op(7, 0, ha, VA + 0x10_0000, 4096),
            nv::EINVAL,
            "an op nobody knows",
        ),
    ];
    for (op_bad, errno, what) in bad {
        // Two good ops first -- an UNMAP of the live mapping and a MAP
        // of a fresh page -- then the bad one; and the bad one first.
        let again = || nv::DrmNouveauVmBindOp { ..op_bad };
        for (place, mut ops) in [
            (
                "last",
                [unmap(VA, 65536), map(ha, VA + 0x20_0000, 4096), again()],
            ),
            (
                "first",
                [again(), unmap(VA, 65536), map(ha, VA + 0x20_0000, 4096)],
            ),
        ] {
            let before = FAKE_RM.lock().calls.len();
            assert_eq!(
                vm_bind_ops(&gpu, A, &mut ops),
                Err(errno),
                "{} ({})",
                what,
                place
            );
            assert_eq!(
                live(),
                [(VA, 65536)],
                "{} ({}): the UNMAP in the same batch did not run, nor the MAP",
                what,
                place
            );
            assert_eq!(
                rm_calls_since(before),
                [] as [&str; 0],
                "{} ({}): nothing reached the RM",
                what,
                place
            );
        }
    }
    // Behind a wait nobody submitted: refused at once, not after the
    // deadline the wait would have cost, and the sig stays unsignaled.
    // The wait is looked up first (`nouveau_job_add_deps` runs before
    // `ops->submit` validates the ops), so it is its EINVAL.
    let never = syncobj::create(false);
    let late = syncobj::create(false);
    let _advance = AutoAdvance::of(1_000);
    let t0 = test_clock::now();
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(
        vm_bind_sync(
            &gpu,
            A,
            &mut [unmap(VA, 65536), map(ha + 1000, VA + 0x10_0000, 4096)],
            nv::VM_BIND_RUN_ASYNC,
            &[sync(never)],
            &[sync(late)]
        ),
        Err(nv::EINVAL),
        "the unsubmitted wait, not EIO for it"
    );
    assert!(
        test_clock::now() - t0 < 1_000_000,
        "refused without waiting ({} us)",
        test_clock::now() - t0
    );
    assert_eq!(live(), [(VA, 65536)]);
    assert_eq!(syncobj::query(late), Some(0), "not signaled");
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    // The same two good ops on their own: both apply.
    assert_eq!(
        vm_bind_ops(
            &gpu,
            A,
            &mut [unmap(VA, 65536), map(ha, VA + 0x20_0000, 4096)]
        ),
        Ok(0)
    );
    assert_eq!(live(), [(VA + 0x20_0000, 4096)]);
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [unmap(VA + 0x20_0000, 4096)]),
        Ok(0)
    );
    assert_eq!(driver_maps(&gpu, A).len(), bound);
}

#[test]
fn an_empty_vm_bind_is_a_job_that_does_nothing_and_a_list_with_no_array_is_a_fault() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 0);
    // B has a uvmm and no context yet. `nouveau_uvmm_bind_job_init`
    // takes an empty op list and `nouveau_job_submit` runs a job that
    // does nothing: 0, no RM call, and no context built for it.
    let before = FAKE_RM.lock().calls.len();
    assert_eq!(vm_bind_ops(&gpu, B, &mut []), Ok(0), "no ops, no syncs");
    assert_eq!(
        vm_bind_sync(&gpu, B, &mut [], nv::VM_BIND_RUN_ASYNC, &[], &[]),
        Ok(0),
        "asynchronous and empty"
    );
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    assert_eq!(ctx_of(&gpu, B), None, "nothing to bind builds no context");
    assert_eq!(driver_maps(&gpu, B), []);
    // `nouveau_uvmm_vm_bind_ucopy` copies each list before anything is
    // looked at (`u_memcpya`): a count with no array is EFAULT, for
    // the ops as for the waits and the sigs.
    let raw = |op_count: u32, wait_count: u32, sig_count: u32| {
        let mut r = nv::DrmNouveauVmBind {
            op_count,
            flags: nv::VM_BIND_RUN_ASYNC,
            wait_count,
            sig_count,
            wait_ptr: 0,
            sig_ptr: 0,
            op_ptr: 0,
        };
        call(&gpu, wr::<nv::DrmNouveauVmBind>(nv::NR_VM_BIND), &mut r, B)
    };
    assert_eq!(raw(1, 0, 0), Err(nv::EFAULT), "ops with no array");
    assert_eq!(raw(0, 1, 0), Err(nv::EFAULT), "waits with no array");
    assert_eq!(raw(0, 0, 1), Err(nv::EFAULT), "sigs with no array");
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    assert_eq!(ctx_of(&gpu, B), None);
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
    assert_eq!(FAKE_RM.lock().bad, 0);
}

/// `nouveau_uvmm_bind_job_submit` walks a `VM_BIND`'s ops twice: the
/// first walk looks a `MAP`'s object up (`drm_gem_object_lookup`,
/// ENOENT) and only then measures its window (`bind_validate_op`) and
/// its range (`nouveau_uvmm_validate_range`, EINVAL), op by op; the
/// second refuses an op it does not know (EINVAL), once every op was
/// looked up and measured. Here the range came first, so a `MAP` of no
/// object off the page, past the VA space or inside the kernel's
/// reservation was EINVAL, and an unknown op was refused before the
/// lookups, hiding the ENOENT of a `MAP` behind it.
#[test]
fn a_vm_bind_map_names_its_object_before_its_window_and_an_unknown_op_fails_after_every_lookup() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm_fast();
    let _ch_a = client_with_pushbuf(&gpu, A);
    let _ch_b = client_with_pushbuf(&gpu, B);
    let ha = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    let hb = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, B)
        .unwrap()
        .handle;
    const VA: u64 = 0x1_9000_0000;
    const END: u64 = nv::NOUVEAU_VA_SPACE_END;
    assert_eq!(vm_bind_ops(&gpu, A, &mut [map(ha, VA, 65536)]), Ok(0));
    let live = driver_maps(&gpu, A);
    let before = FAKE_RM.lock().calls.len();
    // No object of A's (handle 0, or B's): ENOENT whatever the window
    // and the range say.
    for (what, bad) in [
        ("off the page", map(0, VA + 1, 4096)),
        ("an empty range", map(0, VA, 0)),
        ("a range off the page", map(0, VA, 100)),
        ("a range that wraps", map(0, u64::MAX - 4095, 8192)),
        ("past the VA space", map(0, END, 4096)),
        ("inside the kernel's reservation", map(0, 0x1000_0000, 4096)),
        ("an offset off the page", map_at(0, VA, 4096, 1)),
        ("B's object, off the page", map(hb, VA + 1, 4096)),
        ("B's object, past the VA space", map(hb, END - 4096, 8192)),
    ] {
        assert_eq!(vm_bind_ops(&gpu, A, &mut [bad]), Err(nv::ENOENT), "{what}");
    }
    // With the object in hand, the window and the range are measured;
    // an UNMAP names no object, so its range is all there is.
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [map(ha, VA + 1, 4096)]),
        Err(nv::EINVAL)
    );
    assert_eq!(
        vm_bind_ops(&gpu, A, &mut [unmap(VA + 1, 4096)]),
        Err(nv::EINVAL)
    );
    // An op nobody knows is refused after every op was looked up and
    // measured: behind it, a MAP of no object is still ENOENT and a
    // good MAP leaves it EINVAL; its own range is measured in the
    // first walk, like any op's; and ahead of a good MAP it binds
    // nothing.
    let unknown = |addr: u64| op(7, 0, ha, addr, 4096);
    assert_eq!(
        vm_bind_ops(
            &gpu,
            A,
            &mut [unknown(VA + 0x10_0000), map(0, VA + 0x20_0000, 4096)]
        ),
        Err(nv::ENOENT)
    );
    assert_eq!(
        vm_bind_ops(
            &gpu,
            A,
            &mut [unknown(VA + 0x10_0000), map(ha, VA + 0x20_0000, 4096)]
        ),
        Err(nv::EINVAL)
    );
    assert_eq!(
        vm_bind_ops(
            &gpu,
            A,
            &mut [unknown(VA + 0x10_0001), map(0, VA + 0x20_0000, 4096)]
        ),
        Err(nv::EINVAL)
    );
    assert_eq!(
        vm_bind_ops(
            &gpu,
            A,
            &mut [map(ha, VA + 0x20_0000, 4096), unknown(VA + 0x10_0000)]
        ),
        Err(nv::EINVAL)
    );
    assert_eq!(driver_maps(&gpu, A), live, "nothing bound, nothing lost");
    assert_eq!(
        rm_calls_since(before),
        [] as [&str; 0],
        "nothing reached the RM"
    );
    gpu.nouveau_release_process(A);
    gpu.nouveau_release_process(B);
}

/// `nouveau_uvmm_vm_bind_ucopy` copies the op, wait and sig lists before
/// `nouveau_uvmm_vm_bind` builds the job, and it is `nouveau_job_init`,
/// building it, that refuses syncs on a synchronous bind (no RUN_ASYNC):
/// a count with no array is EFAULT whether or not the bind is
/// asynchronous, and the EINVAL comes once the lists are in. This applied
/// the rule before the copy, so a synchronous bind naming a sync list it
/// had no array for was EINVAL.
#[test]
fn vm_bind_copies_its_lists_before_the_rule_against_syncs_on_a_synchronous_bind() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    // A has an RM channel (or VM_BIND is ENODEV before anything) and one
    // mapping of its own, which the binds below never add to.
    let _ch = client_with_pushbuf(&gpu, A);
    let h = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
        .unwrap()
        .handle;
    // A wait the job could take (signaled) and a sig it could deliver: the
    // rule, not the lookups, is what refuses them.
    let done = syncobj::create(true);
    let out = syncobj::create(false);
    let waits = [sync(done)];
    let sigs = [sync(out)];
    let mut ops = [map(h, 0x2_0000_0000, 65536)];
    let (ops_p, waits_p, sigs_p) = (ops.as_mut_ptr() as u64, ptr_of(&waits), ptr_of(&sigs));
    let before = FAKE_RM.lock().calls.len();
    let had = driver_maps(&gpu, A).len();
    let raw = |op_ptr: u64, waits: (u32, u64), sigs: (u32, u64)| {
        let mut r = nv::DrmNouveauVmBind {
            op_count: 1,
            flags: 0,
            wait_count: waits.0,
            sig_count: sigs.0,
            wait_ptr: waits.1,
            sig_ptr: sigs.1,
            op_ptr,
        };
        call(&gpu, wr::<nv::DrmNouveauVmBind>(nv::NR_VM_BIND), &mut r, A)
    };
    assert_eq!(
        raw(ops_p, (1, 0), (0, 0)),
        Err(nv::EFAULT),
        "waits with no array: the copy faults first"
    );
    assert_eq!(
        raw(ops_p, (0, 0), (1, 0)),
        Err(nv::EFAULT),
        "sigs with no array"
    );
    assert_eq!(
        raw(0, (1, waits_p), (0, 0)),
        Err(nv::EFAULT),
        "ops with no array, the waits with one"
    );
    assert_eq!(
        raw(ops_p, (1, waits_p), (0, 0)),
        Err(nv::EINVAL),
        "the lists in: a synchronous bind carries no waits"
    );
    assert_eq!(raw(ops_p, (0, 0), (1, sigs_p)), Err(nv::EINVAL), "nor sigs");
    assert_eq!(
        raw(ops_p, (1, waits_p), (1, sigs_p)),
        Err(nv::EINVAL),
        "nor both"
    );
    assert_eq!(rm_calls_since(before), [] as [&str; 0]);
    assert_eq!(driver_maps(&gpu, A).len(), had, "nothing bound");
    assert_eq!(syncobj::query(out), Some(0), "nothing signaled");
    syncobj::destroy(done);
    syncobj::destroy(out);
}
