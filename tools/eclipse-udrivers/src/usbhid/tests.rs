use super::*;

/// QEMU's `usb-tablet`: three buttons and padding, X and Y absolute from 0
/// to 0x7fff in 16 bits each, a relative wheel.
const TABLET: [u8; 74] = [
    0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03,
    0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26, 0xff, 0x7f, 0x35, 0x00, 0x46, 0xff, 0x7f,
    0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0x05, 0x01, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x35, 0x00,
    0x45, 0x00, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, 0xc0, 0xc0,
];

/// QEMU's `usb-mouse`: five buttons and padding, then X, Y and a wheel as
/// signed bytes, all relative.
const MOUSE: [u8; 52] = [
    0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x05,
    0x15, 0x00, 0x25, 0x01, 0x95, 0x05, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x03, 0x81, 0x01,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x03,
    0x81, 0x06, 0xc0, 0xc0,
];

#[test]
fn a_boot_keyboard_says_each_key_that_goes_down_or_up() {
    let mut k = Keyboard::default();
    // Shift, then A with it.
    assert_eq!(k.report(&[2, 0, 0, 0, 0, 0, 0, 0]), [(EV_KEY, 42, 1)]);
    assert_eq!(k.report(&[2, 0, 4, 0, 0, 0, 0, 0]), [(EV_KEY, 30, 1)]);
    assert!(k.held());
    // B joins A; then both let go, and Shift.
    assert_eq!(k.report(&[2, 0, 4, 5, 0, 0, 0, 0]), [(EV_KEY, 48, 1)]);
    let up = k.report(&[0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(up, [(EV_KEY, 42, 0), (EV_KEY, 30, 0), (EV_KEY, 48, 0)]);
    assert!(!k.held());
}

#[test]
fn a_rollover_report_leaves_the_keys_held() {
    let mut k = Keyboard::default();
    k.report(&[0, 0, 4, 0, 0, 0, 0, 0]);
    assert_eq!(k.report(&[0, 0, 1, 1, 1, 1, 1, 1]), []);
    // A is still down: the next report that drops it lets it go.
    assert_eq!(k.report(&[0, 0, 0, 0, 0, 0, 0, 0]), [(EV_KEY, 30, 0)]);
}

#[test]
fn keyboard_usages_are_linux_keys() {
    assert_eq!(usage_key(0x04), Some(30)); // A
    assert_eq!(usage_key(0x28), Some(28)); // Enter
    assert_eq!(usage_key(0x32), Some(43)); // non-US # is Backslash
    assert_eq!(usage_key(0x64), Some(86)); // the ISO key
    assert_eq!(usage_key(0xe1), Some(42)); // left Shift
    assert_eq!(usage_key(0x01), None);
    assert_eq!(usage_key(0xff), None);
}

#[test]
fn leds_go_as_bits_of_the_report() {
    assert_eq!(led_report(&[false, true, false]), 0b010);
    assert_eq!(led_report(&[true, true, true]), 0b111);
}

#[test]
fn a_tablet_is_absolute_from_its_descriptor() {
    let p = parse_pointer(&TABLET).expect("an X");
    assert!(p.absolute());
    assert_eq!(p.id, None);
    assert_eq!(p.buttons.len(), 3);
    assert_eq!(
        p.x,
        Some(Field {
            at: 8,
            size: 16,
            min: 0,
            max: 0x7fff,
            abs: true
        })
    );
    assert_eq!(p.y.map(|f| f.at), Some(24));
    assert_eq!(p.wheel.map(|f| (f.at, f.abs)), Some((40, false)));
    // Left button, at (0x1234, 0x0100), the wheel one notch down.
    let ev = p.report(&[1, 0x34, 0x12, 0x00, 0x01, 0xff]);
    assert_eq!(
        ev,
        [
            (EV_ABS, ABS_X, 0x1234),
            (EV_ABS, ABS_Y, 0x100),
            (EV_KEY, BTN_LEFT, 1),
            (EV_KEY, BTN_LEFT + 1, 0),
            (EV_KEY, BTN_LEFT + 2, 0),
            (EV_REL, REL_WHEEL, -1),
        ]
    );
    let info = p.info(
        "QEMU QEMU USB Tablet",
        "usb-0000:00:04.0-1/input0",
        [0x627, 1, 0],
    );
    assert_eq!(info.abs.get(&ABS_X), Some(&[0, 0, 0x7fff, 0, 0, 0]));
    assert!(info.sends(EV_REL, REL_WHEEL) && !info.sends(EV_REL, REL_X));
    assert!(info.sends(EV_KEY, BTN_LEFT));
}

#[test]
fn a_mouse_moves_by_signed_bytes() {
    let p = parse_pointer(&MOUSE).expect("an X");
    assert!(!p.absolute());
    assert_eq!(p.buttons.len(), 5);
    // Right button, 3 left, 2 down, no wheel.
    let ev = p.report(&[2, 0xfd, 2, 0]);
    assert_eq!(&ev[..2], [(EV_REL, REL_X, -3), (EV_REL, REL_Y, 2)]);
    assert_eq!(ev[3], (EV_KEY, BTN_LEFT + 1, 1));
    assert_eq!(ev.len(), 2 + 5);
    let info = p.info("QEMU QEMU USB Mouse", "", [0x627, 1, 0]);
    assert!(info.sends(EV_REL, REL_X) && info.sends(EV_REL, REL_WHEEL));
    assert!(info.abs.is_empty());
    // The boot layout is the same report, three buttons of it.
    assert_eq!(boot_mouse().report(&[2, 0xfd, 2, 0])[..2], ev[..2]);
}

#[test]
fn numbered_reports_start_with_their_id() {
    // Report 2: one button and X, Y as signed bytes.
    let desc = [
        0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x85, 0x02, 0x05, 0x09, 0x09, 0x01, 0x15, 0x00, 0x25,
        0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0x75, 0x07, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30,
        0x09, 0x31, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06, 0xc0,
    ];
    let p = parse_pointer(&desc).expect("an X");
    assert_eq!(p.id, Some(2));
    assert_eq!(p.report(&[2, 1, 5, 0])[0], (EV_REL, REL_X, 5));
    assert_eq!(p.report(&[3, 1, 5, 0]), []);
}

#[test]
fn a_descriptor_with_no_x_is_no_pointer() {
    // A keyboard's: modifiers and keys, nothing else.
    let desc = [
        0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00, 0x25,
        0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0xc0,
    ];
    assert_eq!(parse_pointer(&desc), None);
    assert_eq!(parse_pointer(&[0x05]), None, "cut short");
}
