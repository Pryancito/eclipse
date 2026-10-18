extern crate std;

use super::*;
use std::sync::{Mutex, MutexGuard};

/// `IN_CALL`, `CALLER_GPUS` and `USER_RANGE_OK` are process-globals and
/// `cargo test` runs in parallel: every test that reads or sets them holds
/// this, so "outside a call" really is outside every test's call.
fn turnstile() -> MutexGuard<'static, ()> {
    static T: Mutex<()> = Mutex::new(());
    T.lock().unwrap_or_else(|e| e.into_inner())
}

fn version_of(s: &[u8]) -> [u8; VERSION_STRING_LENGTH] {
    let mut v = [0u8; VERSION_STRING_LENGTH];
    v[..s.len()].copy_from_slice(s);
    v
}

const RM: &[u8] = b"580.178.04";

#[test]
fn the_same_version_passes_and_is_left_alone() {
    let mut v = version_of(RM);
    assert!(version_check(0, &mut v, RM));
    assert_eq!(v, version_of(RM));
}

/// libnvidia-ml from another release must be refused: a user-mode driver
/// talking to an RM of another version is exactly what `CHECK_VERSION_STR`
/// exists to stop, and nvidia.ko's comparison is byte for byte.
#[test]
fn another_version_is_refused() {
    let mut v = version_of(b"580.178.05");
    assert!(!version_check(0, &mut v, RM));
    let mut v = version_of(b"580.178.0");
    assert!(!version_check(0, &mut v, RM), "a prefix is not a match");
    let mut v = version_of(b"580.178.040");
    assert!(!version_check(0, &mut v, RM), "nor is a longer string");
}

/// The relaxed form compares up to the first `.`, and no further.
#[test]
fn relaxed_compares_the_major_only() {
    let mut v = version_of(b"580.95.05");
    assert!(version_check(VERSION_CMD_RELAXED, &mut v, RM));
    let mut v = version_of(b"575.178.04");
    assert!(!version_check(VERSION_CMD_RELAXED, &mut v, RM));
    let mut v = version_of(b"580.95.05");
    assert!(
        !version_check(0, &mut v, RM),
        "the strict form must not inherit the relaxed answer"
    );
}

#[test]
fn a_query_answers_with_the_rm_string() {
    let mut v = version_of(b"whatever");
    assert!(version_check(VERSION_CMD_QUERY, &mut v, RM));
    assert_eq!(v, version_of(RM));
}

/// A client string with no NUL in 64 bytes is terminated at the last one
/// before anything prints it, as `RmPerformVersionCheck` does.
#[test]
fn an_unterminated_client_string_is_terminated_on_mismatch() {
    let mut v = [b'9'; VERSION_STRING_LENGTH];
    assert!(!version_check(0, &mut v, RM));
    assert_eq!(v[VERSION_STRING_LENGTH - 1], 0);
    assert!(v[..VERSION_STRING_LENGTH - 1].iter().all(|&b| b == b'9'));
}

/// With no RM (an empty string) nothing but an empty client string matches;
/// a hosted kernel never says yes to a real driver.
#[test]
fn no_rm_matches_nothing_real() {
    let mut v = version_of(RM);
    assert!(!version_check(0, &mut v, b""));
}

#[test]
fn copy_version_always_leaves_room_for_the_nul() {
    let mut v = [0xffu8; VERSION_STRING_LENGTH];
    copy_version(&mut v, &[b'7'; 100]);
    assert_eq!(v[VERSION_STRING_LENGTH - 1], 0);
    assert!(v[..VERSION_STRING_LENGTH - 1].iter().all(|&b| b == b'7'));
    let mut v = [0xffu8; VERSION_STRING_LENGTH];
    copy_version(&mut v, RM);
    assert_eq!(&v[..RM.len()], RM);
    assert_eq!(v[RM.len()], 0);
}

#[test]
fn the_mask_names_instances_bit_by_bit() {
    assert!(instance_in_mask(0, 0b1));
    assert!(!instance_in_mask(1, 0b1));
    assert!(instance_in_mask(1, 0b10));
    assert!(instance_in_mask(31, 1 << 31));
    // Past the mask: never accessible, never a wrapped bit 0.
    assert!(!instance_in_mask(32, u32::MAX));
    assert!(!instance_in_mask(u32::MAX, u32::MAX));
}

fn accept_all(_addr: usize, _len: usize) -> bool {
    true
}

fn refuse_all(_addr: usize, _len: usize) -> bool {
    false
}

/// Outside a front-end call nothing in this port hands the RM a user
/// pointer, so the copies keep answering what they always did.
#[test]
fn user_copies_are_refused_outside_a_call() {
    let _t = turnstile();
    set_user_range_validator(accept_all);
    let src = [1u8, 2, 3, 4];
    let mut dst = [0u8; 4];
    assert_eq!(
        copy_from_user(dst.as_mut_ptr(), src.as_ptr(), 4),
        NV_ERR_NOT_SUPPORTED
    );
    assert_eq!(
        copy_to_user(dst.as_mut_ptr(), src.as_ptr(), 4),
        NV_ERR_NOT_SUPPORTED
    );
    assert_eq!(dst, [0; 4]);
}

#[test]
fn user_copies_ask_the_kernel_inside_a_call() {
    let _t = turnstile();
    let src = [1u8, 2, 3, 4];
    let mut dst = [0u8; 4];
    {
        let _scope = CallScope::enter(0, false);
        set_user_range_validator(refuse_all);
        assert_eq!(
            copy_from_user(dst.as_mut_ptr(), src.as_ptr(), 4),
            NV_ERR_INVALID_ADDRESS
        );
        assert_eq!(
            copy_to_user(dst.as_mut_ptr(), src.as_ptr(), 4),
            NV_ERR_INVALID_ADDRESS
        );
        assert_eq!(dst, [0; 4], "a refused range copies nothing");

        set_user_range_validator(accept_all);
        assert_eq!(copy_from_user(dst.as_mut_ptr(), src.as_ptr(), 4), NV_OK);
        assert_eq!(dst, src);
        let mut back = [0u8; 4];
        assert_eq!(copy_to_user(back.as_mut_ptr(), dst.as_ptr(), 4), NV_OK);
        assert_eq!(back, src);
    }
    // And the scope's end closes the window again.
    let mut after = [0u8; 4];
    assert_eq!(
        copy_from_user(after.as_mut_ptr(), src.as_ptr(), 4),
        NV_ERR_NOT_SUPPORTED
    );
}

#[test]
fn the_call_scope_sets_and_clears_the_caller() {
    let _t = turnstile();
    assert!(!IN_CALL.load(Ordering::SeqCst));
    {
        let _scope = CallScope::enter(0b101, true);
        assert!(IN_CALL.load(Ordering::SeqCst));
        assert_eq!(CALLER_GPUS.load(Ordering::SeqCst), 0b101);
        assert!(is_administrator());
    }
    assert!(!IN_CALL.load(Ordering::SeqCst));
    assert_eq!(CALLER_GPUS.load(Ordering::SeqCst), 0);
    assert!(
        !is_administrator(),
        "nobody is an administrator outside a call"
    );
    {
        let _scope = CallScope::enter(0, false);
        assert!(!is_administrator());
    }
}

#[test]
fn no_gpu_is_accessible_outside_a_call() {
    let _t = turnstile();
    let mut fake = 0u8;
    assert!(!gpu_accessible(&mut fake as *mut u8 as *mut c_void));
    let _scope = CallScope::enter(u32::MAX, true);
    assert!(
        !gpu_accessible(core::ptr::null_mut()),
        "a null OBJGPU is never accessible"
    );
}

/// Hosted builds have no RM: every escape is refused and no open can be
/// confused for another (a zero token is refused before the RM sees it).
#[test]
fn hosted_escapes_are_refused() {
    let mut payload = [0u8; 32];
    let caller = Caller {
        os_info: 0x1000,
        ctl: true,
        admin: false,
        gpu_mask: 1,
    };
    assert_eq!(rm_escape(0x2A, &mut payload, &caller), NV_ERR_NOT_SUPPORTED);
    let nobody = Caller {
        os_info: 0,
        ..caller
    };
    assert_eq!(
        rm_escape(0x2A, &mut payload, &nobody),
        NV_ERR_INVALID_ARGUMENT
    );
    assert!(rm_version().is_empty());
    assert_eq!(gpu_id(0), u32::MAX);
}

#[test]
fn a_hosted_build_agrees_with_its_own_layout() {
    assert_eq!(layout_mismatch(), None);
}
