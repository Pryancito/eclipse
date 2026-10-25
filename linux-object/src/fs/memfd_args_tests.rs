//! What `memfd_create(2)` accepts. The flag word was read for three bits
//! and the rest ignored, and the name was never measured at all.

use super::*;

/// The `uapi` values, and the length the name has to leave room for.
#[test]
fn the_memfd_flags_have_their_linux_values() {
    assert_eq!(MFD_CLOEXEC, 0x0001);
    assert_eq!(MFD_ALLOW_SEALING, 0x0002);
    assert_eq!(MFD_HUGETLB, 0x0004);
    assert_eq!(MFD_NOEXEC_SEAL, 0x0008);
    assert_eq!(MFD_EXEC, 0x0010);
    // NAME_MAX (255) less "memfd:", which Linux puts in front of the name.
    assert_eq!(MFD_NAME_MAX_LEN, 249);
}

/// The four that this kernel can honour come through; `MFD_HUGETLB` and
/// anything else is a caller asking for a feature, and answering
/// "granted" to a feature this kernel does not have is the failure mode
/// the check exists to stop.
#[test]
fn a_bit_memfd_create_does_not_have_is_einval() {
    let all = MFD_CLOEXEC | MFD_ALLOW_SEALING | MFD_NOEXEC_SEAL;
    assert_eq!(memfd_args("x", 0), Ok(0));
    assert_eq!(memfd_args("x", all), Ok(all));
    assert_eq!(memfd_args("x", MFD_EXEC), Ok(MFD_EXEC));
    for bad in [MFD_HUGETLB, 0x20usize, 0x40, 0x80, 0x100, 1 << 20, 1 << 25] {
        assert_eq!(memfd_args("x", bad), Err(LxError::EINVAL), "{bad:#x}");
    }
}

/// `MFD_HUGETLB` (and its size encoding) used to be accepted and then
/// ignored — a normal ramfs memfd answered success. No hugepages here.
#[test]
fn hugetlb_is_einval_not_a_silent_normal_memfd() {
    let size_2mb = 21usize << 26;
    assert_eq!(memfd_args("x", MFD_HUGETLB), Err(LxError::EINVAL));
    assert_eq!(
        memfd_args("x", MFD_HUGETLB | size_2mb),
        Err(LxError::EINVAL)
    );
    assert_eq!(memfd_args("x", size_2mb), Err(LxError::EINVAL));
    assert_eq!(new_memfd("huge", MFD_HUGETLB).err(), Some(LxError::EINVAL));
}

/// "executable" and "sealed shut against ever becoming executable" are not
/// two requests that can both be granted.
#[test]
fn exec_and_noexec_seal_cannot_both_be_asked_for() {
    assert_eq!(
        memfd_args("x", MFD_EXEC | MFD_NOEXEC_SEAL),
        Err(LxError::EINVAL)
    );
    assert_eq!(memfd_args("x", MFD_NOEXEC_SEAL), Ok(MFD_NOEXEC_SEAL));
}

/// The name is built into a directory entry AND into the file's path, and
/// it arrives through a C string bounded only by `MAX_C_STR_LEN` -- so
/// without this the caller picked the size of two kernel allocations.
#[test]
fn a_name_longer_than_name_max_less_the_prefix_is_refused() {
    let ok = "a".repeat(MFD_NAME_MAX_LEN);
    assert_eq!(memfd_args(&ok, 0), Ok(0));
    let one_too_many = "a".repeat(MFD_NAME_MAX_LEN + 1);
    assert_eq!(memfd_args(&one_too_many, 0), Err(LxError::EINVAL));
    let absurd = "a".repeat(1 << 20);
    assert_eq!(memfd_args(&absurd, 0), Err(LxError::EINVAL));
}

/// The syscall's argument is an `unsigned int`, so the bits above it never
/// reached `memfd_create` on Linux and cannot be refused here either.
#[test]
fn the_flag_word_is_an_unsigned_int() {
    assert_eq!(memfd_args("x", 1 << 32), Ok(0));
    assert_eq!(
        memfd_args("x", (1usize << 40) | MFD_CLOEXEC),
        Ok(MFD_CLOEXEC)
    );
    // ...but a bad bit inside the low word still is.
    assert_eq!(memfd_args("x", (1usize << 32) | 0x20), Err(LxError::EINVAL));
}

/// And the refusal reaches the caller, not just the helper.
#[test]
fn new_memfd_is_held_to_the_same_answers() {
    assert_eq!(new_memfd("x", 0x20).err(), Some(LxError::EINVAL));
    assert_eq!(
        new_memfd(&"a".repeat(MFD_NAME_MAX_LEN + 1), 0).err(),
        Some(LxError::EINVAL)
    );
    let f = new_memfd("keymap", MFD_CLOEXEC).unwrap();
    assert!(f.flags().contains(OpenFlags::CLOEXEC));
    assert!(new_memfd("keymap", 0)
        .unwrap()
        .flags()
        .contains(OpenFlags::RDWR));
}
