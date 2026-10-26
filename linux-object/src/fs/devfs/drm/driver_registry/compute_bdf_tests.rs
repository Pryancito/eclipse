use super::*;

/// What the flag is for, and the shape Moebius actually types.
#[test]
fn a_full_bdf_pins_that_card() {
    assert_eq!(
        parse_bdf_flag_in("LOG=warn nvidia.compute=65.00.0", "nvidia.compute"),
        Some((0x65, 0x00, 0))
    );
    // Hex, not decimal: bus 0x65 is 101, and reading it as decimal would name
    // a different slot.
    assert_eq!(
        parse_bdf_flag_in("nvidia.compute=0b.00.0", "nvidia.compute"),
        Some((0x0b, 0, 0))
    );
    // `:` is the cmdline's own separator, so the flag survives being wedged
    // between two others with no spaces.
    assert_eq!(
        parse_bdf_flag_in("a=1:nvidia.compute=01.00.1:b=2", "nvidia.compute"),
        Some((0x01, 0x00, 1))
    );
}

/// The function field is optional, because a GPU is function 0.
#[test]
fn a_bdf_without_a_function_means_function_zero() {
    assert_eq!(
        parse_bdf_flag_in("nvidia.compute=65.00", "nvidia.compute"),
        Some((0x65, 0, 0))
    );
}

/// The regression. An unparsable function used to fall back to 0, so
/// `nvidia.compute=65.00.zz` silently pinned compute to `65.00.0` -- a pin
/// nobody asked for. A pin is what someone reaches for when the automatic
/// choice already went wrong, so it either means what it says or says nothing.
#[test]
fn a_function_that_does_not_parse_is_a_rejection_not_a_zero() {
    assert_eq!(
        parse_bdf_flag_in("nvidia.compute=65.00.zz", "nvidia.compute"),
        None
    );
    assert_eq!(
        parse_bdf_flag_in("nvidia.compute=65.00.", "nvidia.compute"),
        None
    );
    // Out of range for a u8 is the same case, not a wrap.
    assert_eq!(
        parse_bdf_flag_in("nvidia.compute=65.00.1ff", "nvidia.compute"),
        None
    );
}

/// A fourth segment is a BDF the writer did not mean either way.
#[test]
fn an_extra_segment_is_a_rejection() {
    assert_eq!(
        parse_bdf_flag_in("nvidia.compute=65.00.0.0", "nvidia.compute"),
        None
    );
}

/// A bad bus or device was already a rejection, and stays one.
#[test]
fn a_bad_bus_or_device_is_still_a_rejection() {
    assert_eq!(
        parse_bdf_flag_in("nvidia.compute=zz.00.0", "nvidia.compute"),
        None
    );
    assert_eq!(
        parse_bdf_flag_in("nvidia.compute=65.zz.0", "nvidia.compute"),
        None
    );
    assert_eq!(
        parse_bdf_flag_in("nvidia.compute=65", "nvidia.compute"),
        None
    );
    assert_eq!(parse_bdf_flag_in("nvidia.compute=", "nvidia.compute"), None);
}

/// No flag, no pin -- the overwhelmingly common boot.
#[test]
fn no_flag_means_no_pin() {
    assert_eq!(
        parse_bdf_flag_in("LOG=warn smp=off", "nvidia.compute"),
        None
    );
    assert_eq!(parse_bdf_flag_in("", "nvidia.compute"), None);
}

/// The pin names a card that **scans out the boot console**, and it is
/// obeyed.
///
/// This is the one that changed. The old code found the card, saw it was the
/// console GPU, logged «ignoring pin» and fell through to the automatic
/// choice -- so the knob that exists for when the automatic choice went wrong
/// deferred to the automatic choice. On a one-card box the pin could never
/// name anything else, which made it unusable rather than merely overridden.
///
/// It no longer even knows which card scans out: `nvidia.monitor` resolves
/// through the same function, and there "the card that scans out" is the thing
/// being named rather than a thing to look past.
#[test]
fn a_pin_on_the_card_that_scans_out_is_obeyed() {
    // One card, at 65:00.0, and it is the one scanning out.
    let cards = [Some((0u32, 0x65u8, 0x00u8, 0u8))];
    assert_eq!(
        resolve_bdf_pin(cards.iter().copied(), (0x65, 0x00, 0)),
        Some(0)
    );
}

/// The flag is not a tie-break either: with two cards, the pin picks the one
/// it names, scanning out or not.
#[test]
fn the_pin_picks_the_card_it_names_whichever_one_that_is() {
    let cards = [
        Some((0u32, 0x01u8, 0x00u8, 0u8)),
        Some((0u32, 0x65u8, 0x00u8, 0u8)),
    ];
    assert_eq!(
        resolve_bdf_pin(cards.iter().copied(), (0x65, 0x00, 0)),
        Some(1)
    );
    assert_eq!(
        resolve_bdf_pin(cards.iter().copied(), (0x01, 0x00, 0)),
        Some(0)
    );
}

/// A pin that matches nothing is still nothing: the caller says so and falls
/// back, rather than picking a neighbour.
#[test]
fn a_pin_that_matches_no_card_resolves_to_nothing() {
    let cards = [
        Some((0u32, 0x01u8, 0x00u8, 0u8)),
        Some((0u32, 0x65u8, 0x00u8, 0u8)),
    ];
    assert_eq!(
        resolve_bdf_pin(cards.iter().copied(), (0x42, 0x00, 0)),
        None
    );
    // The function field is part of the match, not decoration.
    assert_eq!(
        resolve_bdf_pin(cards.iter().copied(), (0x65, 0x00, 1)),
        None
    );
    // A driver with no BDF at all (not a PCI GPU) is never the pin's card.
    assert_eq!(
        resolve_bdf_pin([None].iter().copied(), (0x65, 0x00, 0)),
        None
    );
    assert_eq!(resolve_bdf_pin([].iter().copied(), (0x65, 0x00, 0)), None);
}

/// **`nvidia.monitor` reads its own value and nothing else's.** The two flags
/// share one parser, so the failure to guard against is one flag answering
/// with the other's BDF -- which on a two-card box would put `card0` on the
/// compute card, the exact shape that cost the desktop.
#[test]
fn the_two_pins_cannot_read_each_others_value() {
    let both = "nvidia.compute=65.00.0:nvidia.monitor=01.00.0";
    assert_eq!(
        parse_bdf_flag_in(both, "nvidia.monitor"),
        Some((0x01, 0x00, 0))
    );
    assert_eq!(
        parse_bdf_flag_in(both, "nvidia.compute"),
        Some((0x65, 0x00, 0))
    );
    // One set and not the other is not a reason to borrow.
    assert_eq!(
        parse_bdf_flag_in("nvidia.compute=65.00.0", "nvidia.monitor"),
        None
    );
    assert_eq!(
        parse_bdf_flag_in("nvidia.monitor=01.00.0", "nvidia.compute"),
        None
    );
}

/// The flag name is matched as a whole token: a longer name that merely starts
/// with it is a different flag, not this one with a strange value.
#[test]
fn a_flag_whose_name_is_only_a_prefix_is_a_different_flag() {
    assert_eq!(
        parse_bdf_flag_in("nvidia.monitors=01.00.0", "nvidia.monitor"),
        None
    );
    assert_eq!(
        parse_bdf_flag_in("nvidia.monitor_bdf=01.00.0", "nvidia.monitor"),
        None
    );
    // And the flag without a value at all is not the flag.
    assert_eq!(parse_bdf_flag_in("nvidia.monitor", "nvidia.monitor"), None);
}

/// `nvidia.monitor=BB.DD.F` accepts exactly what `nvidia.compute=` accepts,
/// which is the reason the parser is shared rather than copied: a rejection
/// rule fixed in one would otherwise be missing from the other.
#[test]
fn the_monitor_pin_accepts_and_rejects_the_same_shapes() {
    for (cmdline, want) in [
        ("nvidia.monitor=01.00.0", Some((0x01, 0x00, 0))),
        ("LOG=warn nvidia.monitor=0b.00", Some((0x0b, 0x00, 0))),
        ("a=1:nvidia.monitor=65.00.1:b=2", Some((0x65, 0x00, 1))),
        ("nvidia.monitor=01.00.zz", None),
        ("nvidia.monitor=01.00.0.0", None),
        ("nvidia.monitor=zz.00.0", None),
        ("nvidia.monitor=", None),
        ("smp=off", None),
    ] {
        assert_eq!(
            parse_bdf_flag_in(cmdline, "nvidia.monitor"),
            want,
            "cmdline {:?}",
            cmdline
        );
    }
}
