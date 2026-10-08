//! `GEM_CLOSE` and `DESTROY_DUMB` are where a file lets go of a handle:
//! what it imports after that is a new reference, as in Linux, where
//! `drm_gem_handle_delete` drops the `drm_prime_file_private` entry with
//! the handle on both paths.
use super::gl_client_sequence_tests::Client;
use super::*;

#[test]
fn closing_the_handle_forgets_the_import_so_the_next_one_counts_again() {
    let _serialised = drm::test_globals::lock();
    let c = Client::open(0);
    let by_gem_close = c.create_dumb(16, 16);
    let by_destroy_dumb = c.create_dumb(16, 16);
    for h in [by_gem_close.handle, by_destroy_dumb.handle] {
        assert!(c.file_state().note_prime_import(h));
        assert!(c.file_state().holds_prime_import(h));
    }
    let mut h = by_gem_close.handle;
    assert_eq!(c.ioctl(DRM_IOCTL_GEM_CLOSE, &mut h), Ok(0));
    assert!(
        !c.file_state().holds_prime_import(by_gem_close.handle),
        "GEM_CLOSE left the import on record"
    );
    assert!(
        c.file_state().holds_prime_import(by_destroy_dumb.handle),
        "closing one handle forgot the other"
    );
    assert_eq!(c.destroy_dumb(by_destroy_dumb.handle), Ok(0));
    assert!(
        !c.file_state().holds_prime_import(by_destroy_dumb.handle),
        "DESTROY_DUMB left the import on record"
    );
    for h in [by_gem_close.handle, by_destroy_dumb.handle] {
        assert!(c.file_state().note_prime_import(h), "counts again");
        c.file_state().forget_prime_import(h);
    }
    // A close that fails (handle already gone) forgets nothing, and says so.
    assert!(c.file_state().note_prime_import(by_gem_close.handle));
    let mut h = by_gem_close.handle;
    assert_eq!(
        c.ioctl(DRM_IOCTL_GEM_CLOSE, &mut h),
        Err(FsError::InvalidParam)
    );
    assert!(c.file_state().holds_prime_import(by_gem_close.handle));
    c.file_state().forget_prime_import(by_gem_close.handle);
}
