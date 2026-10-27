//! What `nvidia.monitor=BB.DD.F` does to the answer, with registered cards.
//!
//! The parsing and the match live next to themselves in `compute_bdf_tests`.
//! What is here is the effect: that the pin overrules the automatic choice,
//! that it reaches a card the automatic choice would never have picked, and
//! that a pin naming nothing leaves the automatic choice alone instead of
//! leaving the box with no display card.
//!
//! Through [`display_driver_pinned`] and not [`get_display_driver`], because
//! the real pin comes off the bootloader's command line and `boot::cmdline()`
//! is the empty string in a host test build. The split is the same one
//! `kernel_hal::cmdline::pci_bdf` already uses for the same reason.

use super::*;
use crate::fs::devfs::kms_emu::{self, EmuGpu};

/// **The pin reaches the card the automatic choice refuses.** This is the
/// whole point of the flag: on a box where the monitor is on the card that
/// `is_compute_gpu()` is true for -- the firmware scanned out of the other
/// one, or `drives_boot_display()` got it wrong -- there is no automatic
/// answer that names it, so without the flag the desktop has nowhere to go.
#[test]
fn the_pin_names_a_card_the_automatic_choice_would_never_pick() {
    let screen = kms_emu::attach(64, 16);
    let _console = screen.attach_gpu(EmuGpu::new("emu-console").console_at(0x01, 0x00));
    let _compute = screen.attach_gpu(EmuGpu::new("emu-compute").compute_at(0x65, 0x00));

    let automatic = display_driver_pinned(None).expect("a display driver");
    assert_eq!(
        automatic.pci_bdf(),
        Some((0, 0x01, 0x00, 0)),
        "without the flag it is the card that is not a compute GPU"
    );
    let pinned = display_driver_pinned(Some((0x65, 0x00, 0))).expect("a display driver");
    assert_eq!(
        pinned.pci_bdf(),
        Some((0, 0x65, 0x00, 0)),
        "the pin has to win, or it is not an override"
    );
}

/// The pin on the card the automatic choice already picks is not a no-op worth
/// warning about: it answers the same card, which is what someone who read
/// `dmesg` and typed what it said should get.
#[test]
fn the_pin_on_the_automatic_card_answers_that_same_card() {
    let screen = kms_emu::attach(64, 16);
    let _console = screen.attach_gpu(EmuGpu::new("emu-console").console_at(0x01, 0x00));
    let _compute = screen.attach_gpu(EmuGpu::new("emu-compute").compute_at(0x65, 0x00));

    let pinned = display_driver_pinned(Some((0x01, 0x00, 0))).expect("a display driver");
    assert_eq!(pinned.pci_bdf(), Some((0, 0x01, 0x00, 0)));
}

/// **A pin that names no registered card falls back to the automatic answer.**
/// A typo in a cmdline flag must not leave the box with no display card: that
/// would turn `software_kms_active()` around and take the panel down, which is
/// strictly worse than the state the flag was reached for.
#[test]
fn a_pin_that_names_nothing_leaves_the_automatic_answer_alone() {
    let screen = kms_emu::attach(64, 16);
    let _console = screen.attach_gpu(EmuGpu::new("emu-console").console_at(0x01, 0x00));
    let _compute = screen.attach_gpu(EmuGpu::new("emu-compute").compute_at(0x65, 0x00));

    let pinned = display_driver_pinned(Some((0x42, 0x00, 0))).expect("a display driver");
    assert_eq!(
        pinned.pci_bdf(),
        Some((0, 0x01, 0x00, 0)),
        "a pin naming no card must not answer None, and must not answer a neighbour"
    );
}

/// One card, and the pin names it or names nothing: either way that card is
/// the answer. This is the box that comes up today, so the flag must not be
/// able to break it.
#[test]
fn a_one_card_box_answers_with_its_one_card_however_the_pin_reads() {
    let screen = kms_emu::attach(64, 16);
    let _only = screen.attach_gpu(EmuGpu::hardware_kms("emu-only").console_at(0x01, 0x00));

    for pin in [None, Some((0x01, 0x00, 0)), Some((0x65, 0x00, 0))] {
        assert_eq!(
            display_driver_pinned(pin).and_then(|d| d.pci_bdf()),
            Some((0, 0x01, 0x00, 0)),
            "pin {:x?}",
            pin
        );
    }
}
