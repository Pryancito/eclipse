use super::*;
use crate::scheme::drm::GemAperture;

/// `nouveau_gem_new`: `NOUVEAU_GEM_DOMAIN_NO_SHARE` needs the client's
/// uvmm (EINVAL without a VM_INIT) and makes the object unexportable
/// (`nouveau_gem_prime_export`: EPERM); GEM_INFO never reports the bit.
/// This driver ignored it.
#[test]
fn gem_new_no_share_needs_a_uvmm_and_marks_the_object_unexportable() {
    let _g = LOCK.lock();
    let _live = LiveBytes::hold();
    let gpu = gpu_rm();
    let gart = nv::NOUVEAU_GEM_DOMAIN_GART;
    assert_eq!(
        gem_new_rm(&gpu, 4096, gart | nv::NOUVEAU_GEM_DOMAIN_NO_SHARE, A).map(|i| i.handle),
        Err(nv::EINVAL),
        "no VM_INIT yet"
    );
    assert_eq!(
        gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_NO_SHARE, A).map(|i| i.handle),
        Err(nv::EINVAL),
        "before the domain is looked at"
    );
    with_uvmm(&gpu, A);
    let private = gem_new_rm(&gpu, 4096, gart | nv::NOUVEAU_GEM_DOMAIN_NO_SHARE, A).unwrap();
    assert_eq!(private.domain, gart, "the bit is not a domain");
    let shared = gem_new_rm(&gpu, 4096, gart, A).unwrap();
    assert!(crate::scheme::gem_mmap::no_share(private.handle, A));
    assert!(
        !crate::scheme::gem_mmap::no_share(private.handle, B),
        "its creator's alone"
    );
    assert!(!crate::scheme::gem_mmap::no_share(shared.handle, A));
    assert_eq!(
        crate::scheme::gem_mmap::lookup_for(private.handle, A).map(|(_, size)| size),
        Some(4096),
        "the process itself still maps it"
    );
    // A VRAM-only object has no host address and no mapping entry; the
    // record does not depend on one.
    let vram = gem_new_rm(
        &gpu,
        4096,
        nv::NOUVEAU_GEM_DOMAIN_VRAM | nv::NOUVEAU_GEM_DOMAIN_NO_SHARE,
        A,
    )
    .unwrap();
    assert_eq!(vram.map_handle, 0, "not CPU-mappable");
    assert!(crate::scheme::gem_mmap::lookup_for(vram.handle, A).is_none());
    // The aperture is told to its owner and to the kernel; to another
    // process the object does not exist (`drm_gem_object_lookup` is per
    // file), which is what keeps its PRIME export ENOENT there.
    assert_eq!(gpu.gem_aperture(vram.handle, A), GemAperture::Vram);
    assert_eq!(gpu.gem_aperture(vram.handle, 0), GemAperture::Vram);
    assert_eq!(
        gpu.gem_aperture(vram.handle, B),
        GemAperture::Unknown,
        "another process's object is no object"
    );
    assert!(
        crate::scheme::gem_mmap::no_share(vram.handle, A),
        "marked all the same"
    );
    // Its one holder closing it takes the record with the object.
    assert!(gpu.nouveau_gem_close(vram.handle, A));
    assert!(!crate::scheme::gem_mmap::no_share(vram.handle, A));
    gpu.nouveau_release_process(A);
    assert!(
        !crate::scheme::gem_mmap::no_share(private.handle, A),
        "gone with the object"
    );
}

/// `nouveau_bo_fixup_align` / `nouveau_gem_info`: the object is as big
/// as its GPU page (4 KiB host-backed; 64 KiB or 2 MiB in VRAM once the
/// request fills one), that size is what the RM allocates, what the
/// quotas count, what the mmap registry covers and what `GEM_NEW` and
/// `GEM_INFO` report; and `tile_flags` comes back as NONCONTIG | kind,
/// the kind being the uncompressed pair on a 4 KiB page. This allocated
/// and reported the request byte for byte, `tile_flags` verbatim.
#[test]
fn with_the_rm_gem_new_rounds_the_size_to_the_gpu_page_and_reports_the_kind_it_programs() {
    let _g = LOCK.lock();
    let live = LiveBytes::hold();
    let gpu = gpu_rm();
    let gart = nv::NOUVEAU_GEM_DOMAIN_GART;
    let vram = nv::NOUVEAU_GEM_DOMAIN_VRAM;

    let small = gem_new_rm_tiled(&gpu, 100, gart, 0x00ff_08ff, A).unwrap();
    assert_eq!(small.size, 4096, "a host page");
    assert_eq!(
        small.tile_flags, 0x0608,
        "NONCONTIG kept, kind 0x08 uncompressed to 0x06 on 4 KiB pages, the rest gone"
    );
    let h = FAKE_RM.lock().gems[0].0;
    assert_eq!(
        FAKE_RM.lock().gems[0],
        (h, 4096, true),
        "allocated at that size"
    );
    assert_eq!(
        crate::scheme::gem_mmap::lookup(small.handle).map(|(_, len)| len),
        Some(4096),
        "and registered at it"
    );
    assert_eq!(live.delta(), 4096, "and counted at it");
    assert_eq!(
        gem_info(&gpu, small.handle, A).map(|i| (i.size, i.tile_flags)),
        Ok((4096, 0x0608)),
        "GEM_INFO says the same"
    );

    // VRAM: the page the request fills.
    let big = gem_new_rm_tiled(&gpu, 3 * MIB, vram, 0x0800, A).unwrap();
    assert_eq!(big.size, 4 * MIB, "2 MiB pages");
    assert_eq!(big.tile_flags, 0x0800, "compression stays on a 2 MiB page");
    assert_eq!(FAKE_RM.lock().gems[1].1, 4 * MIB);
    let mid = gem_new_rm_tiled(&gpu, 65537, vram, 0x0800, A).unwrap();
    assert_eq!(mid.size, 131072, "64 KiB pages");
    assert_eq!(mid.tile_flags, 0x0800);
    let tiny = gem_new_rm_tiled(&gpu, 4096, vram, 0x0800, A).unwrap();
    assert_eq!(
        tiny.size, 65536,
        "a compressed object stays off 4 KiB pages"
    );
    assert_eq!(tiny.tile_flags, 0x0800, "so its compression stays too");
    let plain = gem_new_rm_tiled(&gpu, 4096, vram, 0x0600, A).unwrap();
    assert_eq!(
        plain.size, 4096,
        "an uncompressed kind takes the page it fills"
    );
    assert_eq!(plain.tile_flags, 0x0600);
    // GART|VRAM is host-backed here: host pages.
    let both = gem_new_rm_tiled(&gpu, 100_000, gart | vram, 0, A).unwrap();
    assert_eq!(both.size, 102_400);
    assert_eq!(both.tile_flags, 0);
    assert_eq!(
        live.delta(),
        4096 + 4 * MIB as i64 + 131072 + 65536 + 4096 + 102_400
    );
    for h in [
        small.handle,
        big.handle,
        mid.handle,
        tiny.handle,
        plain.handle,
        both.handle,
    ] {
        assert!(gpu.nouveau_gem_close(h, A));
    }
    assert_eq!(live.delta(), 0, "and given back at that size");
}

/// `nouveau_bo_alloc` once more, for a compressible kind: a client
/// without a uvmm gets a `comp` object, which in VRAM goes on the 64 KiB
/// page (the smallest with compression) however small the request, kind
/// kept, and on host pages loses the compression, kind downgraded. A
/// client with a uvmm (`VM_INIT`) is walked without compression: the
/// page is the one the request fills and the kind comes back as given,
/// in VRAM and on host pages alike. This put both clients' small
/// requests on 4 KiB pages with the kind downgraded.
#[test]
fn with_the_rm_a_compressible_kind_takes_a_64_kib_vram_page_and_a_uvmm_client_keeps_its_kind() {
    let _g = LOCK.lock();
    let live = LiveBytes::hold();
    let gpu = gpu_rm();
    let gart = nv::NOUVEAU_GEM_DOMAIN_GART;
    let vram = nv::NOUVEAU_GEM_DOMAIN_VRAM;

    // No uvmm: `comp`.
    let legacy = gem_new_rm_tiled(&gpu, 100, vram, 0x0b00, A).unwrap();
    assert_eq!(legacy.size, 65536, "the 64 KiB page, not the 4 KiB one");
    assert_eq!(legacy.tile_flags, 0x0b00, "and the kind stays compressed");
    assert_eq!(FAKE_RM.lock().gems[0].1, 65536, "allocated at that size");
    assert_eq!(live.delta(), 65536, "and counted at it");
    let host = gem_new_rm_tiled(&gpu, 100, gart, 0x0b00, A).unwrap();
    assert_eq!(host.size, 4096, "host pages are 4 KiB whatever the kind");
    assert_eq!(host.tile_flags, 0x0100, "and carry no compression: Z16");
    let plain = gem_new_rm_tiled(&gpu, 100, vram, 0x0100, A).unwrap();
    assert_eq!(plain.size, 4096, "an uncompressed kind is not `comp`");
    assert_eq!(plain.tile_flags, 0x0100);

    // A uvmm client: the walk without compression.
    with_uvmm(&gpu, B);
    let uvmm = gem_new_rm_tiled(&gpu, 100, vram, 0x0b00, B).unwrap();
    assert_eq!(uvmm.size, 4096, "the page the request fills");
    assert_eq!(uvmm.tile_flags, 0x0b00, "kind as given");
    let uvmm_mid = gem_new_rm_tiled(&gpu, 65537, vram, 0x0b00, B).unwrap();
    assert_eq!(uvmm_mid.size, 131072);
    assert_eq!(uvmm_mid.tile_flags, 0x0b00);
    let uvmm_host = gem_new_rm_tiled(&gpu, 100, gart, 0x0b00, B).unwrap();
    assert_eq!(uvmm_host.size, 4096);
    assert_eq!(
        uvmm_host.tile_flags, 0x0b00,
        "no downgrade on host pages either: nothing was compressed"
    );
    assert_eq!(
        gem_info(&gpu, uvmm.handle, B).map(|i| (i.size, i.tile_flags)),
        Ok((4096, 0x0b00)),
        "GEM_INFO says the same"
    );
    // The other client's uvmm is not this one's.
    let again = gem_new_rm_tiled(&gpu, 100, vram, 0x0b00, A).unwrap();
    assert_eq!((again.size, again.tile_flags), (65536, 0x0b00));

    for (h, pid) in [
        (legacy.handle, A),
        (host.handle, A),
        (plain.handle, A),
        (uvmm.handle, B),
        (uvmm_mid.handle, B),
        (uvmm_host.handle, B),
        (again.handle, A),
    ] {
        assert!(gpu.nouveau_gem_close(h, pid));
    }
    assert_eq!(live.delta(), 0);
}
