//! The two `struct flock`, by their bytes.
//!
//! What can go wrong here is not arithmetic, it is picking the wrong of two
//! layouts, so every test states the width it means and reads the fields at
//! the offsets that width gives them.

use super::*;

/// A 24-byte `struct flock64` with a distinct value in each field.
fn wide_bytes(l_type: i16, l_whence: i16, l_start: i64, l_len: i64, l_pid: i32) -> [u8; 24] {
    let mut b = [0u8; 24];
    b[..2].copy_from_slice(&l_type.to_le_bytes());
    b[2..4].copy_from_slice(&l_whence.to_le_bytes());
    b[4..12].copy_from_slice(&l_start.to_le_bytes());
    b[12..20].copy_from_slice(&l_len.to_le_bytes());
    b[20..24].copy_from_slice(&l_pid.to_le_bytes());
    b
}

/// A 16-byte `struct flock`, the same way.
fn narrow_bytes(l_type: i16, l_whence: i16, l_start: i32, l_len: i32, l_pid: i32) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..2].copy_from_slice(&l_type.to_le_bytes());
    b[2..4].copy_from_slice(&l_whence.to_le_bytes());
    b[4..8].copy_from_slice(&l_start.to_le_bytes());
    b[8..12].copy_from_slice(&l_len.to_le_bytes());
    b[12..16].copy_from_slice(&l_pid.to_le_bytes());
    b
}

/// `struct flock64`: 24 bytes, offsets 0, 2, 4, 12, 20.
#[test]
fn the_wide_struct_is_read_at_the_offsets_four_byte_alignment_gives_it() {
    let b = wide_bytes(1, 2, 0x0000_0005_dead_beef, 0x0000_0003_0000_0007, 4321);
    let fl = flock_from_bytes(&b, FLOCK64_SIZE).unwrap();
    assert_eq!(fl.l_type, 1);
    assert_eq!(fl.l_whence, 2);
    assert_eq!(fl.l_start, 0x0000_0005_dead_beef);
    assert_eq!(fl.l_len, 0x0000_0003_0000_0007);
    assert_eq!(fl.l_pid, 4321);
}

/// `struct flock`: 16 bytes, and `l_pid` at offset 12 rather than 20.
///
/// Reading the narrow form at the wide form's offsets would take `l_pid` from
/// four bytes past the end of the structure, so this is the test that the
/// width actually selects a layout.
#[test]
fn the_narrow_struct_is_a_different_layout_and_not_a_prefix_of_the_wide_one() {
    let b = narrow_bytes(1, 2, 100, 200, 4321);
    let fl = flock_from_bytes(&b, FLOCK32_SIZE).unwrap();
    assert_eq!(fl.l_start, 100);
    assert_eq!(fl.l_len, 200);
    assert_eq!(fl.l_pid, 4321, "l_pid is at offset 12, not 20");

    // And decoding the same sixteen bytes as the wide form does not merely
    // give a different `l_pid`: it reads past them.
    assert_eq!(
        flock_from_bytes(&b, FLOCK64_SIZE).unwrap_err(),
        LxError::EFAULT,
        "twenty-four bytes cannot be read out of sixteen"
    );
}

/// A negative `l_len` survives, in both widths.
///
/// `fcntl(2)` gives a negative length a meaning --- the range *before*
/// `l_start` --- so these two fields are sign-extended where the split
/// offsets in `super::wide` are zero-extended. A zero-extended `-1` is
/// 4294967295, which is a range of four gigabytes rather than one byte
/// backwards, and nothing reports it: the lock is simply somewhere else.
#[test]
fn a_negative_length_stays_negative_in_both_widths() {
    let narrow = narrow_bytes(1, 0, -4096, -8192, 1);
    let fl = flock_from_bytes(&narrow, FLOCK32_SIZE).unwrap();
    assert_eq!(fl.l_start, -4096);
    assert_eq!(fl.l_len, -8192);

    let wide = wide_bytes(1, 0, -4096, -8192, 1);
    let fl = flock_from_bytes(&wide, FLOCK64_SIZE).unwrap();
    assert_eq!(fl.l_start, -4096);
    assert_eq!(fl.l_len, -8192);
}

/// Writing back, and the round trip through both widths.
#[test]
fn what_goes_in_comes_back_out_in_the_same_shape() {
    for (width, size) in [(FLOCK32_SIZE, 16usize), (FLOCK64_SIZE, 24usize)] {
        let fl = Flock {
            l_type: 2,
            l_whence: 0,
            l_start: 1 << 20,
            l_len: 4096,
            l_pid: 777,
        };
        let mut raw = [0u8; FLOCK64_SIZE];
        flock_to_bytes(&fl, width, &mut raw).unwrap();
        let back = flock_from_bytes(&raw[..size], width).unwrap();
        assert_eq!(back.l_type, fl.l_type);
        assert_eq!(back.l_whence, fl.l_whence);
        assert_eq!(back.l_start, fl.l_start);
        assert_eq!(back.l_len, fl.l_len);
        assert_eq!(back.l_pid, fl.l_pid);
    }
}

/// An `F_GETLK` answer that does not fit in the narrow structure is
/// `EOVERFLOW`, not a truncated offset.
///
/// `F_GETLK` reports somebody else's lock, so the offset is not one this
/// program chose and can be anywhere in the file. Truncating it would answer
/// "the conflict is at byte 0" for a conflict two gigabytes in, and the
/// program would retry forever against a range that is free. Linux answers
/// `EOVERFLOW` here for the same reason.
#[test]
fn an_answer_too_big_for_the_narrow_struct_is_eoverflow() {
    let fl = Flock {
        l_type: 1,
        l_whence: 0,
        l_start: 1 << 40,
        l_len: 1,
        l_pid: 9,
    };
    let mut raw = [0u8; FLOCK64_SIZE];
    assert_eq!(
        flock_to_bytes(&fl, FLOCK32_SIZE, &mut raw).unwrap_err(),
        LxError::EOVERFLOW
    );
    // The same answer fits the wide one.
    assert!(flock_to_bytes(&fl, FLOCK64_SIZE, &mut raw).is_ok());
}

/// Each command picks its structure, and the ones that are not lock commands
/// pick none.
///
/// The `F_OFD_*` row is the one worth asserting: those three are numbered
/// well above the `*64` commands and were added long after, so there is no
/// 16-byte form of them to pick by mistake --- but nothing about their
/// numbers says so, and a `match` arm that grouped them with `F_GETLK`
/// because both are "the non-64 spelling" would read 16 bytes of a 24-byte
/// structure.
#[test]
fn the_command_number_decides_the_width_and_nothing_else_does() {
    assert_eq!(flock_width(F_GETLK), Some(FLOCK32_SIZE));
    assert_eq!(flock_width(F_SETLK), Some(FLOCK32_SIZE));
    assert_eq!(flock_width(F_SETLKW), Some(FLOCK32_SIZE));
    assert_eq!(flock_width(F_GETLK64), Some(FLOCK64_SIZE));
    assert_eq!(flock_width(F_SETLK64), Some(FLOCK64_SIZE));
    assert_eq!(flock_width(F_SETLKW64), Some(FLOCK64_SIZE));
    assert_eq!(flock_width(F_OFD_GETLK), Some(FLOCK64_SIZE));
    assert_eq!(flock_width(F_OFD_SETLK), Some(FLOCK64_SIZE));
    assert_eq!(flock_width(F_OFD_SETLKW), Some(FLOCK64_SIZE));

    // F_DUPFD(0), F_GETFD(1), F_SETFD(2), F_GETFL(3), F_SETFL(4),
    // F_SETOWN(8), F_GETOWN(9), F_DUPFD_CLOEXEC(1030), F_ADD_SEALS(1033):
    // every one of these takes a scalar.
    for cmd in [0, 1, 2, 3, 4, 8, 9, 10, 11, 1030, 1031, 1032, 1033, 1034] {
        assert_eq!(flock_width(cmd), None, "command {} takes a scalar", cmd);
    }
}

/// The two syscall numbers are the same call.
///
/// Written the other way round first, on the belief that `fcntl`(55) refuses
/// the wide commands. Measuring the Linux of the build container said
/// otherwise --- 55 with `F_SETLK64` takes the lock and 55 with `F_GETLK64`
/// writes the 24-byte answer back --- so there is one handler and this test
/// pins the decision, because "the number does not matter, only the command"
/// is exactly the kind of thing a later reader would un-simplify.
#[test]
fn the_width_comes_from_the_command_and_never_from_the_syscall_number() {
    // Every lock command has a width, and it is the same width whichever of
    // the two numbers carried it: `flock_width` never sees the number.
    for cmd in [
        F_GETLK,
        F_SETLK,
        F_SETLKW,
        F_GETLK64,
        F_SETLK64,
        F_SETLKW64,
        F_OFD_GETLK,
        F_OFD_SETLK,
        F_OFD_SETLKW,
    ] {
        assert!(
            flock_width(cmd).is_some(),
            "command {} is a lock command on both numbers",
            cmd
        );
    }
}
