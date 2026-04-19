//! ABI de `mmap` / `mprotect` y arena de usuario — `eclipse_kernel/src/syscalls.rs` (`linux_mmap_abi`).

use kernel_host_tests::policy::*;

#[test]
fn user_arena_is_ordered_and_sane() {
    assert!(MMAP_USER_ARENA_LO < MMAP_USER_ARENA_HI);
    assert!(MMAP_USER_ARENA_HI <= 0x8000_0000u64);
}

#[test]
fn anon_slack_is_page_multiple() {
    assert_eq!(MMAP_ANON_SLACK_BYTES % 4096, 0);
    assert!(MMAP_ANON_SLACK_BYTES >= 4096);
}

#[test]
fn prot_mask_covers_rwx() {
    assert_eq!(MMAP_PROT_MASK, 7);
    assert!(MMAP_PROT_EXEC <= MMAP_PROT_MASK);
}

#[test]
fn map_flags_bits_distinct() {
    assert_ne!(MMAP_MAP_ANONYMOUS, MMAP_MAP_SHARED);
    assert_ne!(MMAP_MAP_FIXED, MMAP_MAP_POPULATE);
}
