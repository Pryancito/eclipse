//! The XDR codec: what it writes, what it refuses to read, and the two
//! boundaries where a wrong answer is a kernel bug rather than a parse error.
//!
//! The encoder's tests assert bytes, not round trips. A round trip through
//! this module's own decoder passes for any pair of functions that agree with
//! each other, including a pair that both get the format wrong — and the
//! whole point of XDR here is to agree with a *server*. So the expected bytes
//! are written out by hand from RFC 4506, and the round-trip tests are kept
//! for the properties a byte assertion cannot state.
//!
//! The two that matter most are at the end: a length near `usize::MAX` must
//! not wrap (on a release build, where overflow does not panic, a wrap turns
//! a bounds check that should fail into one that passes), and no length a
//! server sends may cause an allocation.

use super::*;
use alloc::vec;

// ---------------------------------------------------------------- encoding

#[test]
fn u32_is_four_big_endian_bytes() {
    let mut enc = XdrEncoder::new();
    enc.u32(0x0102_0304);
    assert_eq!(enc.as_bytes(), &[0x01, 0x02, 0x03, 0x04]);
}

#[test]
fn u64_is_eight_big_endian_bytes() {
    let mut enc = XdrEncoder::new();
    enc.u64(0x0102_0304_0506_0708);
    assert_eq!(
        enc.as_bytes(),
        &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]
    );
}

#[test]
fn negative_i32_is_twos_complement_big_endian() {
    let mut enc = XdrEncoder::new();
    enc.i32(-1);
    assert_eq!(enc.as_bytes(), &[0xff, 0xff, 0xff, 0xff]);
}

#[test]
fn negative_i64_is_twos_complement_big_endian() {
    let mut enc = XdrEncoder::new();
    enc.i64(-2);
    assert_eq!(
        enc.as_bytes(),
        &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe]
    );
}

#[test]
fn bool_is_a_four_byte_zero_or_one() {
    let mut enc = XdrEncoder::new();
    enc.bool(true).bool(false);
    assert_eq!(enc.as_bytes(), &[0, 0, 0, 1, 0, 0, 0, 0]);
}

/// The padding rule, at every remainder: one byte of payload takes four
/// bytes on the wire, two take four, three take four, four take four, and
/// five take eight.
#[test]
fn opaque_pads_every_length_to_a_multiple_of_four() {
    for (len, expected_total) in [(0, 4), (1, 8), (2, 8), (3, 8), (4, 8), (5, 12)] {
        let mut enc = XdrEncoder::new();
        enc.opaque(&vec![0xaa; len]);
        assert_eq!(
            enc.len(),
            expected_total,
            "{} payload bytes should take {} on the wire (4 of length + {} padded)",
            len,
            expected_total,
            expected_total - 4
        );
    }
}

/// The length ahead of a variable-length opaque counts the payload and NOT
/// the padding. A client that counted the padding would hand the server a
/// filename three bytes longer than the name.
#[test]
fn opaque_length_excludes_the_padding() {
    let mut enc = XdrEncoder::new();
    enc.opaque(b"abc");
    assert_eq!(
        enc.as_bytes(),
        &[0, 0, 0, 3, b'a', b'b', b'c', 0],
        "length 3, three bytes, one zero byte of padding"
    );
}

/// A fixed-length opaque carries no length, which is the whole difference
/// from [`XdrEncoder::opaque`] and the thing a caller can mix up.
#[test]
fn opaque_fixed_writes_no_length() {
    let mut enc = XdrEncoder::new();
    enc.opaque_fixed(b"abc");
    assert_eq!(enc.as_bytes(), &[b'a', b'b', b'c', 0]);
}

#[test]
fn padding_bytes_are_zero_not_leftovers() {
    let mut enc = XdrEncoder::new();
    enc.opaque(&[0xff, 0xff, 0xff, 0xff, 0xff]);
    assert_eq!(
        &enc.as_bytes()[9..],
        &[0, 0, 0],
        "padding must be zero even when the payload is all ones"
    );
}

#[test]
fn string_is_encoded_as_an_opaque_of_its_bytes() {
    let mut enc = XdrEncoder::new();
    enc.string("hola");
    let mut other = XdrEncoder::new();
    other.opaque(b"hola");
    assert_eq!(enc.as_bytes(), other.as_bytes());
}

#[test]
fn optional_writes_only_the_boolean_when_none() {
    let mut enc = XdrEncoder::new();
    enc.optional(None::<u32>, |e, v| e.u32(v));
    assert_eq!(enc.as_bytes(), &[0, 0, 0, 0]);
}

#[test]
fn optional_writes_the_boolean_then_the_value_when_some() {
    let mut enc = XdrEncoder::new();
    enc.optional(Some(7u32), |e, v| e.u32(v));
    assert_eq!(enc.as_bytes(), &[0, 0, 0, 1, 0, 0, 0, 7]);
}

// ---------------------------------------------------------------- decoding

#[test]
fn decoder_reads_back_what_the_encoder_wrote() {
    let mut enc = XdrEncoder::new();
    enc.u32(1).i32(-2).u64(3).i64(-4).bool(true).opaque(b"hola");
    let bytes = enc.into_bytes();

    let mut dec = XdrDecoder::new(&bytes);
    assert_eq!(dec.u32(), Ok(1));
    assert_eq!(dec.i32(), Ok(-2));
    assert_eq!(dec.u64(), Ok(3));
    assert_eq!(dec.i64(), Ok(-4));
    assert_eq!(dec.bool(), Ok(true));
    assert_eq!(dec.opaque(), Ok(&b"hola"[..]));
    assert!(dec.is_empty(), "the whole message should be consumed");
}

/// The padding is consumed, so the field after an unaligned one is read from
/// the right offset. A decoder that skipped the padding would be three bytes
/// out of step for the rest of the message, and the first symptom would be a
/// nonsense value somewhere far from the cause.
#[test]
fn decoder_consumes_padding_so_the_next_field_is_aligned() {
    let mut enc = XdrEncoder::new();
    enc.opaque(b"a").u32(0xdead_beef);
    let bytes = enc.into_bytes();

    let mut dec = XdrDecoder::new(&bytes);
    assert_eq!(dec.opaque(), Ok(&b"a"[..]));
    assert_eq!(
        dec.u32(),
        Ok(0xdead_beef),
        "the u32 after a 1-byte opaque starts 3 padding bytes later"
    );
}

#[test]
fn truncated_u32_is_truncated_not_a_short_read() {
    let mut dec = XdrDecoder::new(&[0, 0, 0]);
    assert_eq!(dec.u32(), Err(XdrError::Truncated));
}

#[test]
fn truncated_u64_is_truncated() {
    let mut dec = XdrDecoder::new(&[0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(dec.u64(), Err(XdrError::Truncated));
}

/// A length that names more bytes than arrived. The commonest shape of a
/// malformed reply, and the one that must not allocate.
#[test]
fn opaque_longer_than_the_buffer_is_truncated() {
    // Length 16, but only four bytes follow.
    let bytes = [0, 0, 0, 16, 1, 2, 3, 4];
    let mut dec = XdrDecoder::new(&bytes);
    assert_eq!(dec.opaque(), Err(XdrError::Truncated));
}

/// The padding is present but the payload's last byte is missing: the field
/// is still truncated, and the decoder must not read into the padding to
/// make up the difference.
#[test]
fn opaque_with_payload_short_by_one_is_truncated() {
    // Length 4, three bytes.
    let bytes = [0, 0, 0, 4, 1, 2, 3];
    let mut dec = XdrDecoder::new(&bytes);
    assert_eq!(dec.opaque(), Err(XdrError::Truncated));
}

/// The payload is all there but its padding is not. Still truncated: the
/// field occupies its padded length, and a decoder that accepted this would
/// hand the next field three bytes that are not there.
#[test]
fn opaque_whose_padding_is_missing_is_truncated() {
    // Length 1, the byte, no padding.
    let bytes = [0, 0, 0, 1, 0xaa];
    let mut dec = XdrDecoder::new(&bytes);
    assert_eq!(dec.opaque(), Err(XdrError::Truncated));
}

#[test]
fn non_zero_padding_is_refused() {
    let bytes = [0, 0, 0, 1, 0xaa, 0, 0, 1];
    let mut dec = XdrDecoder::new(&bytes);
    assert_eq!(dec.opaque(), Err(XdrError::BadPadding));
}

/// Every padding position is checked, not just the first.
#[test]
fn non_zero_padding_is_refused_in_any_of_the_three_positions() {
    for bad in 0..3 {
        let mut bytes = vec![0, 0, 0, 1, 0xaa, 0, 0, 0];
        bytes[5 + bad] = 0xff;
        let mut dec = XdrDecoder::new(&bytes);
        assert_eq!(
            dec.opaque(),
            Err(XdrError::BadPadding),
            "a non-zero byte in padding position {} must be refused",
            bad
        );
    }
}

#[test]
fn a_boolean_that_is_neither_zero_nor_one_is_refused() {
    let bytes = [0, 0, 0, 2];
    let mut dec = XdrDecoder::new(&bytes);
    assert_eq!(dec.bool(), Err(XdrError::BadDiscriminant(2)));
}

#[test]
fn opaque_max_refuses_a_length_over_the_cap() {
    let mut enc = XdrEncoder::new();
    enc.opaque(&[0xaa; 8]);
    let bytes = enc.into_bytes();

    let mut dec = XdrDecoder::new(&bytes);
    assert_eq!(dec.opaque_max(4), Err(XdrError::LengthTooLarge));

    let mut dec = XdrDecoder::new(&bytes);
    assert_eq!(
        dec.opaque_max(8),
        Ok(&[0xaa; 8][..]),
        "a length exactly at the cap is allowed"
    );
}

#[test]
fn optional_reads_nothing_past_a_false_boolean() {
    let bytes = [0, 0, 0, 0, 0xde, 0xad, 0xbe, 0xef];
    let mut dec = XdrDecoder::new(&bytes);
    assert_eq!(dec.optional(|d| d.u32()), Ok(None));
    assert_eq!(
        dec.remaining(),
        4,
        "a false optional consumes only its boolean"
    );
}

#[test]
fn optional_reads_the_value_past_a_true_boolean() {
    let bytes = [0, 0, 0, 1, 0, 0, 0, 9];
    let mut dec = XdrDecoder::new(&bytes);
    assert_eq!(dec.optional(|d| d.u32()), Ok(Some(9)));
    assert!(dec.is_empty());
}

#[test]
fn position_and_remaining_track_what_was_read() {
    let bytes = [0u8; 12];
    let mut dec = XdrDecoder::new(&bytes);
    assert_eq!((dec.position(), dec.remaining()), (0, 12));
    dec.u32().unwrap();
    assert_eq!((dec.position(), dec.remaining()), (4, 8));
    dec.skip(8).unwrap();
    assert_eq!((dec.position(), dec.remaining()), (12, 0));
    assert!(dec.is_empty());
}

#[test]
fn skipping_more_than_remains_is_truncated() {
    let mut dec = XdrDecoder::new(&[0u8; 4]);
    assert_eq!(dec.skip(5), Err(XdrError::Truncated));
}

// ------------------------------------------------- the boundaries that bite

/// `padded_len` must never wrap. On a release build — which is what ships —
/// `len + pad` for a `len` near `usize::MAX` wraps silently to a small
/// number, and a small number *passes* the bounds check that the real length
/// should have failed.
#[test]
fn padded_len_refuses_lengths_that_would_overflow() {
    assert_eq!(padded_len(0), Ok(0));
    assert_eq!(padded_len(1), Ok(4));
    assert_eq!(padded_len(4), Ok(4));
    assert_eq!(padded_len(5), Ok(8));
    // A length already aligned needs no padding, so even `usize::MAX`
    // rounded down to a multiple of four is fine.
    let aligned_max = usize::MAX - (usize::MAX % 4);
    assert_eq!(padded_len(aligned_max), Ok(aligned_max));
    // These need padding, and the sum does not fit.
    for len in [usize::MAX, usize::MAX - 1, usize::MAX - 2] {
        assert_eq!(
            padded_len(len),
            Err(XdrError::LengthTooLarge),
            "padded_len({}) must refuse rather than wrap",
            len
        );
    }
}

/// The same boundary reached through the decoder, which is how a server would
/// reach it: a 32-bit length of `0xffff_ffff` with no bytes behind it.
///
/// It must be an error, and it must not be [`XdrError::BadPadding`] — that
/// would mean the decoder had already read past the end to look at padding.
#[test]
fn a_length_of_all_ones_is_refused_without_reading_past_the_end() {
    let bytes = [0xff, 0xff, 0xff, 0xff];
    let mut dec = XdrDecoder::new(&bytes);
    let err = dec.opaque().unwrap_err();
    assert!(
        matches!(err, XdrError::Truncated | XdrError::LengthTooLarge),
        "a 4-gigabyte length over an empty buffer must be refused, got {:?}",
        err
    );
}

/// No length from the wire may cause an allocation. The proof here is
/// structural rather than measured: [`XdrDecoder::opaque`] returns a slice
/// borrowed from the input, so there is nowhere for four gigabytes to go, and
/// a decoder built over a four-byte buffer cannot touch more than four bytes.
///
/// A test that merely asserted the error would pass for an implementation
/// that allocated the buffer first and then failed — which is the bug. This
/// one asserts the returned bytes alias the input, which only a non-copying
/// decoder can satisfy.
#[test]
fn a_decoded_opaque_borrows_the_input_and_never_copies_it() {
    let mut enc = XdrEncoder::new();
    enc.opaque(b"abcd");
    let bytes = enc.into_bytes();
    let input_range = bytes.as_ptr_range();

    let mut dec = XdrDecoder::new(&bytes);
    let out = dec.opaque().unwrap();
    assert!(
        input_range.contains(&out.as_ptr()),
        "the decoded bytes must point into the caller's buffer, not a copy"
    );
}

/// A length of all ones *at every* decoder entry point, so a new method
/// cannot be added without this boundary being considered.
#[test]
fn every_length_taking_reader_refuses_all_ones() {
    let bytes = [0xff, 0xff, 0xff, 0xff];

    let mut dec = XdrDecoder::new(&bytes);
    assert!(dec.opaque().is_err());

    let mut dec = XdrDecoder::new(&bytes);
    assert!(dec.string_bytes().is_err());

    let mut dec = XdrDecoder::new(&bytes);
    assert_eq!(dec.opaque_max(64), Err(XdrError::LengthTooLarge));

    let mut dec = XdrDecoder::new(&bytes);
    assert_eq!(dec.opaque_fixed(usize::MAX), Err(XdrError::LengthTooLarge));
}
