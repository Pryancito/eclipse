use super::*;

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
    assert!(crate::scheme::gem_mmap::no_share(private.handle));
    assert!(!crate::scheme::gem_mmap::no_share(shared.handle));
    assert_eq!(
        crate::scheme::gem_mmap::lookup_for(private.handle, A).map(|(_, size)| size),
        Some(4096),
        "the process itself still maps it"
    );
    gpu.nouveau_release_process(A);
    assert!(
        !crate::scheme::gem_mmap::no_share(private.handle),
        "gone with the object"
    );
}
