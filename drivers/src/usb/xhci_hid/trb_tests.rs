//! The command TRBs the driver builds, and the bounds on its wait loops.
//!
//! The rest of this driver's arithmetic already has tests next to it
//! (`portsc_tests.rs`, `hub_tests.rs`, `ring_tests.rs`, `disk_tests.rs` and
//! `tests.rs`); what was left without any are the TRB builders and the two
//! functions that bound a wait.
//!
//! Both are the kind of code where a mistake produces a plausible result
//! rather than a failure: a slot id written into the endpoint field addresses
//! another endpoint of slot 0, and a wait bounded only by a clock that is not
//! moving never ends. Neither needs a controller to check.
//!
//! Separate file rather than a `mod tests` inside `xhci_hid.rs` because that
//! file is already within a hundred lines of its size ceiling (`xtask
//! tamano`), which is also why its other test modules live out here.

use super::*;

// ─── TRB builders ───────────────────────────────────────────────────────────

/// The TRB type lives in bits 15:10 of the control dword.
fn trb_type(t: &Trb) -> u32 {
    (t.ctrl >> 10) & 0x3f
}

/// The slot id lives in bits 31:24 of a command TRB's control dword, and the
/// endpoint id (DCI) in bits 20:16. Every command that names a slot has to
/// put it in the same place: a slot written into the endpoint field addresses
/// another endpoint of slot 0.
#[test]
fn every_command_trb_carries_its_own_type_and_puts_the_slot_where_it_belongs() {
    assert_eq!(trb_type(&trb_enable_slot()), 9);
    assert_eq!(trb_type(&trb_disable_slot(1)), 10);
    assert_eq!(trb_type(&trb_address_device(0, 1)), 11);
    assert_eq!(trb_type(&trb_configure_endpoint(0, 1)), 12);
    assert_eq!(trb_type(&trb_evaluate_context(0, 1)), 13);
    assert_eq!(trb_type(&trb_reset_endpoint(1, 2)), 14);
    assert_eq!(trb_type(&trb_stop_endpoint(1, 2)), 15);
    assert_eq!(trb_type(&trb_set_tr_dequeue_pointer(0, false, 1, 2)), 16);
    assert_eq!(trb_type(&trb_link(0, false)), 6);

    for t in [
        trb_disable_slot(0xab),
        trb_address_device(0, 0xab),
        trb_configure_endpoint(0, 0xab),
        trb_evaluate_context(0, 0xab),
        trb_stop_endpoint(0xab, 3),
        trb_reset_endpoint(0xab, 3),
        trb_set_tr_dequeue_pointer(0, false, 0xab, 3),
    ] {
        assert_eq!(t.ctrl >> 24, 0xab, "the slot id is not in bits 31:24");
    }

    for t in [
        trb_stop_endpoint(1, 0x1f),
        trb_reset_endpoint(1, 0x1f),
        trb_set_tr_dequeue_pointer(0, false, 1, 0x1f),
    ] {
        assert_eq!(
            (t.ctrl >> 16) & 0x1f,
            0x1f,
            "the endpoint id is not in bits 20:16"
        );
    }
}

/// Evaluate Context and Configure Endpoint are different commands on purpose
/// (4.6.7 vs 4.6.6): a controller that enforces the distinction answers
/// Context State Error if the wrong one is used to change EP0's packet size,
/// which leaves EP0 at the guessed size.
#[test]
fn evaluate_context_and_configure_endpoint_are_not_the_same_command() {
    assert_ne!(
        trb_type(&trb_evaluate_context(0x1000, 1)),
        trb_type(&trb_configure_endpoint(0x1000, 1))
    );
    // Both carry the input context pointer in the parameter, unmodified.
    assert_eq!(trb_evaluate_context(0xdead_0000, 1).p, 0xdead_0000);
    assert_eq!(trb_configure_endpoint(0xdead_0000, 1).p, 0xdead_0000);
}

/// A Link TRB closes a ring segment, so it always has Toggle Cycle set and
/// carries the cycle bit the producer is on. A Link without Toggle leaves the
/// consumer's cycle state wrong for the whole next lap of the ring.
#[test]
fn a_link_trb_always_toggles_the_cycle_and_carries_the_producers_bit() {
    let off = trb_link(0x2000, false);
    assert_eq!(off.p, 0x2000);
    assert_ne!(off.ctrl & (1 << 1), 0, "Toggle Cycle is not set");
    assert_eq!(off.ctrl & 1, 0, "the cycle bit should be 0");

    let on = trb_link(0x2000, true);
    assert_ne!(on.ctrl & (1 << 1), 0);
    assert_eq!(on.ctrl & 1, 1, "the cycle bit should be 1");
}

/// A Set TR Dequeue Pointer carries its Dequeue Cycle State in bit 0 of the
/// address, which is why the address is 16-byte aligned first: a stray low
/// bit would be read as DCS and the ring would restart on the wrong cycle.
#[test]
fn the_dequeue_pointer_keeps_its_cycle_state_in_the_low_bit_of_an_aligned_address() {
    let t = trb_set_tr_dequeue_pointer(0x3001, true, 1, 2);
    assert_eq!(t.p & !0xf, 0x3000, "the address was not aligned down");
    assert_eq!(t.p & 1, 1, "DCS is not in bit 0");

    let t = trb_set_tr_dequeue_pointer(0x300f, false, 1, 2);
    assert_eq!(t.p, 0x3000, "a low nibble leaked into the pointer");
}

/// Enable Slot and Disable Slot both set the Cycle bit themselves (they are
/// pushed with it already on), and Enable Slot names no slot.
#[test]
fn the_slot_commands_set_their_own_cycle_bit() {
    assert_eq!(trb_enable_slot().ctrl & 1, 1);
    assert_eq!(trb_disable_slot(3).ctrl & 1, 1);
    assert_eq!(trb_enable_slot().ctrl >> 24, 0, "Enable Slot named a slot");
    assert_eq!(trb_enable_slot().p, 0);
}

/// A Setup Stage TRB carries the eight bytes of the control request in its
/// parameter, little-endian in the order the wire has them, and its transfer
/// length is always 8.
#[test]
fn a_setup_trb_lays_the_eight_request_bytes_out_in_wire_order() {
    // GET_DESCRIPTOR(Device), 18 bytes, to the device.
    let t = trb_setup(0x80, 0x06, 0x0100, 0x0000, 18, 3);
    assert_eq!(trb_type(&t), 2);
    assert_eq!(t.status, 8, "the setup stage is not 8 bytes");
    assert_eq!(t.p.to_le_bytes(), [0x80, 0x06, 0x00, 0x01, 0, 0, 18, 0]);
    // Immediate Data (bit 6) must be set: the request is IN the TRB, not
    // pointed to by it, so a controller without it reads the parameter as an
    // address.
    assert_ne!(t.ctrl & (1 << 6), 0, "Immediate Data is not set");
    assert_eq!((t.ctrl >> 16) & 3, 3, "the transfer type is not in 17:16");
}

/// A Data Stage TRB's direction is bit 16, and its length field is 17 bits:
/// a longer length must be masked rather than spill into the next field.
#[test]
fn a_data_trb_carries_its_direction_and_a_seventeen_bit_length() {
    let out = trb_data(0x4000, 64, false);
    assert_eq!(trb_type(&out), 3);
    assert_eq!(out.p, 0x4000);
    assert_eq!(out.status, 64);
    assert_eq!(out.ctrl & (1 << 16), 0, "an OUT stage claims IN");

    let inn = trb_data(0x4000, 64, true);
    assert_ne!(inn.ctrl & (1 << 16), 0, "an IN stage claims OUT");

    // 0x1_ffff is the widest length the field holds; anything above it is
    // masked, and must not reach bit 17 and beyond.
    assert_eq!(trb_data(0, 0x1_ffff, true).status, 0x1_ffff);
    assert_eq!(trb_data(0, 0x2_0000, true).status, 0);
    assert_eq!(trb_data(0, u32::MAX, true).status, 0x1_ffff);
}

/// A Status Stage TRB has the opposite direction to the data it follows, and
/// Interrupt On Completion (bit 5) is what makes the transfer report at all.
#[test]
fn a_status_trb_can_ask_for_an_interrupt_on_completion() {
    let quiet = trb_status(true, false);
    assert_eq!(trb_type(&quiet), 4);
    assert_eq!(quiet.ctrl & (1 << 5), 0);
    let loud = trb_status(true, true);
    assert_ne!(loud.ctrl & (1 << 5), 0, "IOC is not set");
    assert_eq!(trb_status(false, true).ctrl & (1 << 16), 0);
}

/// A Normal TRB is what carries interrupt and bulk data. Its length is a
/// `u16` at the call site, so the whole range must survive the 17-bit mask --
/// a request over 65535 bytes is split by the caller, which only works if a
/// 65535-byte one is passed through whole.
#[test]
fn a_normal_trb_carries_the_whole_sixteen_bit_length() {
    let t = trb_normal(0x5000, 64, true);
    assert_eq!(trb_type(&t), 1);
    assert_eq!(t.p, 0x5000);
    assert_eq!(t.status, 64);
    assert_ne!(t.ctrl & (1 << 5), 0);
    assert_eq!(trb_normal(0, u16::MAX, false).status, u16::MAX as u32);
    assert_eq!(trb_normal(0, 0, false).ctrl & (1 << 5), 0);
}

// ─── Wait loop ──────────────────────────────────────────────────────────────

/// A wait is bounded by BOTH a deadline and a spin count, because the clock
/// the driver reads is not trustworthy in every context it waits in: a wait
/// that trusted only the clock would spin forever if the clock did not move,
/// which is what a stuck TSC looks like during boot.
#[test]
fn a_wait_ends_on_the_spin_bound_when_the_clock_does_not_move() {
    let clock = crate::nvme::nvme_queue::test_clock::now();
    let limit = xhci_wait_spin_limit(1_000);

    // The clock is where it was, so only the spin count can end this wait.
    assert!(
        !xhci_wait_expired(clock, 1_000, limit - 1),
        "a wait ended before either bound"
    );
    assert!(
        xhci_wait_expired(clock, 1_000, limit),
        "a stopped clock would wait forever"
    );
}

/// And on the deadline when it does move, which is the ordinary case.
#[test]
fn a_wait_ends_on_its_deadline_when_the_clock_moves() {
    let clock = crate::nvme::nvme_queue::test_clock::now();
    assert!(!xhci_wait_expired(clock, 1_000, 0));
    crate::nvme::nvme_queue::test_clock::advance(999);
    assert!(!xhci_wait_expired(clock, 1_000, 0), "ended one tick early");
    crate::nvme::nvme_queue::test_clock::advance(1);
    assert!(xhci_wait_expired(clock, 1_000, 0), "the deadline passed");
}

/// The elapsed time is a `wrapping_sub`, so a clock that wrapped between the
/// start and the check must not read as an enormous elapsed time and cut
/// every wait short.
#[test]
fn a_clock_that_wrapped_does_not_cut_the_wait_short() {
    crate::nvme::nvme_queue::test_clock::set(u64::MAX - 10);
    let start = crate::nvme::nvme_queue::test_clock::now();
    crate::nvme::nvme_queue::test_clock::set(5);
    // 16 microseconds elapsed across the wrap, well inside the timeout.
    assert!(!xhci_wait_expired(start, 1_000, 0));
    crate::nvme::nvme_queue::test_clock::set(0);
}

/// The spin limit has a FLOOR, not just a slope: a short timeout still gets
/// enough spins that a controller answering in microseconds is not cut off by
/// the spin bound before its deadline.
#[test]
fn the_spin_limit_has_a_floor_and_then_grows_with_the_timeout() {
    let floor = xhci_wait_spin_limit(0);
    assert!(floor >= 500_000_000, "the floor is only {} spins", floor);
    for timeout in [1u64, 1_000, 100_000, 1_000_000] {
        assert_eq!(
            xhci_wait_spin_limit(timeout),
            floor,
            "a timeout of {} us dropped below the floor",
            timeout
        );
    }
    // Past a second the slope takes over, so a genuinely long wait is not
    // ended by the spin bound either.
    assert!(xhci_wait_spin_limit(10_000_000) > floor);
    // And it saturates rather than overflowing on an absurd timeout.
    assert_eq!(xhci_wait_spin_limit(u64::MAX), u64::MAX);
}
