use super::*;

/// What the flag is for, and the shape Moebius actually types.
#[test]
fn a_full_bdf_pins_that_card() {
    assert_eq!(
        parse_compute_bdf_in("LOG=warn nvidia.compute=65.00.0"),
        Some((0x65, 0x00, 0))
    );
    // Hex, not decimal: bus 0x65 is 101, and reading it as decimal would name
    // a different slot.
    assert_eq!(
        parse_compute_bdf_in("nvidia.compute=0b.00.0"),
        Some((0x0b, 0, 0))
    );
    // `:` is the cmdline's own separator, so the flag survives being wedged
    // between two others with no spaces.
    assert_eq!(
        parse_compute_bdf_in("a=1:nvidia.compute=01.00.1:b=2"),
        Some((0x01, 0x00, 1))
    );
}

/// The function field is optional, because a GPU is function 0.
#[test]
fn a_bdf_without_a_function_means_function_zero() {
    assert_eq!(
        parse_compute_bdf_in("nvidia.compute=65.00"),
        Some((0x65, 0, 0))
    );
}

/// The regression. An unparsable function used to fall back to 0, so
/// `nvidia.compute=65.00.zz` silently pinned compute to `65.00.0` -- a pin
/// nobody asked for. A pin is what someone reaches for when the automatic
/// choice already went wrong, so it either means what it says or says nothing.
#[test]
fn a_function_that_does_not_parse_is_a_rejection_not_a_zero() {
    assert_eq!(parse_compute_bdf_in("nvidia.compute=65.00.zz"), None);
    assert_eq!(parse_compute_bdf_in("nvidia.compute=65.00."), None);
    // Out of range for a u8 is the same case, not a wrap.
    assert_eq!(parse_compute_bdf_in("nvidia.compute=65.00.1ff"), None);
}

/// A fourth segment is a BDF the writer did not mean either way.
#[test]
fn an_extra_segment_is_a_rejection() {
    assert_eq!(parse_compute_bdf_in("nvidia.compute=65.00.0.0"), None);
}

/// A bad bus or device was already a rejection, and stays one.
#[test]
fn a_bad_bus_or_device_is_still_a_rejection() {
    assert_eq!(parse_compute_bdf_in("nvidia.compute=zz.00.0"), None);
    assert_eq!(parse_compute_bdf_in("nvidia.compute=65.zz.0"), None);
    assert_eq!(parse_compute_bdf_in("nvidia.compute=65"), None);
    assert_eq!(parse_compute_bdf_in("nvidia.compute="), None);
}

/// No flag, no pin -- the overwhelmingly common boot.
#[test]
fn no_flag_means_no_pin() {
    assert_eq!(parse_compute_bdf_in("LOG=warn smp=off"), None);
    assert_eq!(parse_compute_bdf_in(""), None);
}
