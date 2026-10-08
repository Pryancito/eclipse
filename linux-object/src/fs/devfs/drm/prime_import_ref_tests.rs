//! One PRIME reference per importing file, as `drm_prime_lookup_buf_handle`
//! gives one handle per (file, dma-buf) and no reference for a repeat.
use super::*;
use zcore_drivers::scheme::gem_mmap::{self, DecRef};

/// A nouveau-range handle no GPU slice hands out in these tests.
const H: u32 = 0xbfff_0007;

#[test]
fn a_repeat_import_by_the_same_file_adds_no_reference_and_another_file_does() {
    let _serialised = test_globals::lock();
    let pid = current_pid();
    gem_mmap::register(H, 0x1000_0000, 4096, pid);
    assert_eq!(gem_mmap::ref_count(H), Some(1), "the creator's own");
    let (f1, f2) = (DrmFileState::new(), DrmFileState::new());

    assert_eq!(nouveau_gem_import_ref(H, &f1), Some(2));
    assert_eq!(
        nouveau_gem_import_ref(H, &f1),
        Some(2),
        "a second import by the same file is the same handle, no new reference"
    );
    assert_eq!(nouveau_gem_import_ref(H, &f1), Some(2));
    assert_eq!(
        nouveau_gem_import_ref(H, &f2),
        Some(3),
        "another file of the process is another holder"
    );
    assert!(f1.holds_prime_import(H) && f2.holds_prime_import(H));

    // f1's one GEM_CLOSE: its one reference goes, and it forgets the
    // handle, so an import after that counts again.
    assert!(f1.forget_prime_import(H));
    assert!(!f1.forget_prime_import(H), "forgotten once");
    assert_eq!(gem_mmap::dec_ref(H, pid), DecRef::StillReferenced(2));
    assert_eq!(nouveau_gem_import_ref(H, &f1), Some(3));

    // An untracked handle: no reference to take, and the file does not
    // keep a record of it either way.
    assert_eq!(nouveau_gem_import_ref(H + 1, &f1), None);
    assert!(!f1.holds_prime_import(H + 1));
    assert!(gem_mmap::unregister(H));
}
