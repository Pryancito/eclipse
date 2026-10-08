extern crate std;

use super::*;
use zcore_drivers::display::edid;

const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];

/// A whole block with a correct header and checksum, the way a monitor sends
/// one.
fn good_block() -> [u8; edid::BLOCK_LEN] {
    let mut b = [0u8; edid::BLOCK_LEN];
    b[..8].copy_from_slice(&HEADER);
    b[18] = 1;
    b[19] = 4;
    b[21] = 60;
    b[22] = 34;
    let sum = b[..edid::BLOCK_LEN - 1]
        .iter()
        .fold(0u8, |s, x| s.wrapping_add(*x));
    b[edid::BLOCK_LEN - 1] = sum.wrapping_neg();
    b
}

/// The defect, reproduced: 32 real bytes zero-padded to 128. This is exactly
/// what `NvidiaGpu::get_connector_edid` used to return when the RM's head did
/// not match the firmware capture, and what the DRM core used to serve on a
/// length check alone.
fn zero_padded_head() -> [u8; edid::BLOCK_LEN] {
    let mut b = [0u8; edid::BLOCK_LEN];
    b[..8].copy_from_slice(&HEADER);
    b[18] = 1;
    b[21] = 60;
    b
}

#[test]
fn a_real_block_is_served() {
    assert_eq!(edid_refusal_reason(&good_block()), None);
}

/// The whole point of the batch. A zero-padded head is refused, and the
/// reason names the checksum rather than the header -- because the header IS
/// right, which is exactly why a length check let it through.
#[test]
fn the_zero_padded_head_is_refused_on_its_checksum() {
    let padded = zero_padded_head();
    assert_eq!(
        &padded[..8],
        &HEADER,
        "the header is the part that was fine"
    );
    assert_eq!(edid_refusal_reason(&padded), Some("checksum"));
}

/// And the driver no longer produces one: the same 32 bytes, completed,
/// pass. The two halves of the fix have to agree, or refusing in the core
/// just loses the monitor's identity instead of repairing it.
#[test]
fn the_completed_head_the_driver_now_reports_is_served() {
    let padded = zero_padded_head();
    let completed = edid::finish_partial_block(&padded[..32]).expect("32 real bytes complete");
    assert_eq!(edid_refusal_reason(&completed), None);
    // And the bytes that were real are the bytes served: byte for byte, the
    // head goes through untouched, so the make, the model and the date the
    // RM gave us are still there to be read.
    assert_eq!(&completed[..32], &padded[..32]);
    // A completed block states no timing, which is the honest answer -- the
    // RM never gave us one -- and the client falls back to the mode.
    assert_eq!(edid::preferred_timing(&completed), None);
}

/// A bad pointer and a corrupt read are different things to go and look at,
/// so they are told apart rather than both coming out as "invalid".
#[test]
fn a_wrong_header_and_a_wrong_checksum_are_named_separately() {
    let mut garbage = good_block();
    garbage[3] = 0x00;
    assert_eq!(edid_refusal_reason(&garbage), Some("header"));

    let mut corrupt = good_block();
    corrupt[40] ^= 0xFF;
    assert_eq!(
        edid_refusal_reason(&corrupt),
        Some("checksum"),
        "the header is untouched, so this must not come out as a header fault"
    );
}

/// Short of a whole block is its own reason: nothing was decoded, so neither
/// the header nor the checksum is the thing to report.
#[test]
fn a_block_short_of_a_whole_one_says_so() {
    let b = good_block();
    assert_eq!(edid_refusal_reason(&b[..127]), Some("short"));
    assert_eq!(edid_refusal_reason(&[]), Some("short"));
}

/// The klog budget, same shape and same reason as everywhere else here: a
/// connector is probed on every `GETCONNECTOR`, and a compositor that
/// re-enumerates in a loop would pin the UART.
#[test]
fn the_refusal_budget_is_bounded() {
    assert!(MAX_EDID_REFUSALS > 0 && MAX_EDID_REFUSALS <= 8);
}

/// What a refused connector reports instead: nothing. Which is the honest
/// answer and the one every compositor already handles, because it is what
/// it gets on any machine whose firmware captured no EDID at all.
#[test]
fn a_connector_with_no_usable_edid_reports_none_rather_than_a_stub() {
    let _g = super::test_globals::lock();
    // No driver is registered in a host test and no firmware EDID was
    // captured, so this is the fallback path every such machine takes.
    assert_eq!(get_connector_edid(SYNTH_CONNECTOR_ID), None);
    assert_eq!(boot_edid_block(), None);
}
