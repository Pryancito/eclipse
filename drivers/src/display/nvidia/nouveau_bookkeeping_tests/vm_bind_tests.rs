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
