use super::*;

#[test]
fn a_producer_ring_links_back_and_flips_its_cycle() {
    let mut p = Producer::new(4);
    assert_eq!(p.advance(), (0, true, None));
    assert_eq!(p.advance(), (1, true, None));
    // Entry 3 is the link: after entry 2 the ring wraps, the link handed
    // over with the cycle of that lap.
    assert_eq!(p.advance(), (2, true, Some(true)));
    assert_eq!(p.advance(), (0, false, None));
}

#[test]
fn the_event_ring_takes_only_entries_of_its_cycle() {
    let mut c = Consumer {
        len: 2,
        deq: 0,
        cycle: true,
    };
    assert!(!c.take(false));
    assert!(c.take(true));
    assert!(c.take(true));
    assert_eq!((c.deq, c.cycle), (0, false));
    assert!(!c.take(true));
}

#[test]
fn a_setup_trb_carries_the_request_and_its_data_stage() {
    let get = Setup::get_descriptor(DESC_DEVICE, 0, 0, 18);
    let t = get.trb();
    assert_eq!(t[0], 0x0100_0680);
    assert_eq!(t[1], 18 << 16);
    assert_eq!(t[2], 8);
    assert_eq!(t[3], TRB_SETUP << 10 | TRB_IDT | 3 << 16);
    let set = Setup {
        request_type: 0,
        request: REQ_SET_CONFIGURATION,
        value: 1,
        index: 0,
        length: 0,
    };
    assert_eq!(set.trb()[3] >> 16 & 3, 0);
}

#[test]
fn an_interval_is_an_exponent_of_125_us() {
    // 10 ms at full speed: 80 microframes, 2^6 is the most under it.
    assert_eq!(interval(SPEED_FULL, 10), 6);
    assert_eq!(interval(SPEED_LOW, 1), 3);
    assert_eq!(interval(SPEED_FULL, 255), 10);
    // High speed says the exponent plus one.
    assert_eq!(interval(SPEED_HIGH, 4), 3);
    assert_eq!(interval(SPEED_HIGH, 0), 0);
}

/// QEMU's `usb-kbd` configuration: one boot keyboard interface.
const KEYBOARD_CONFIG: [u8; 34] = [
    9, 2, 34, 0, 1, 1, 6, 0xa0, 50, // configuration 1
    9, 4, 0, 0, 1, 3, 1, 1, 0, // interface 0: HID, boot, keyboard
    9, 0x21, 0x11, 1, 0, 1, 0x22, 63, 0, // HID: a report of 63 bytes
    7, 5, 0x81, 3, 8, 0, 7, // endpoint 1 IN, interrupt, 8 bytes, 7 ms
];

#[test]
fn a_configuration_says_its_hid_interfaces() {
    let (value, ifaces) = hid_interfaces(&KEYBOARD_CONFIG);
    assert_eq!(value, 1);
    assert_eq!(
        ifaces,
        [HidIface {
            number: 0,
            subclass: 1,
            protocol: 1,
            endpoint: (0x81, 8, 7),
            report_len: 63,
        }]
    );
    // A mass storage interface after it is no HID; one with no interrupt
    // IN endpoint is left out.
    let mut more = KEYBOARD_CONFIG.to_vec();
    more.extend([9, 4, 1, 0, 2, 8, 6, 0x50, 0, 7, 5, 0x82, 2, 0, 2, 0]);
    more.extend([9, 4, 2, 0, 0, 3, 0, 0, 0]);
    assert_eq!(hid_interfaces(&more).1.len(), 1);
    // Cut short, it says what it had.
    assert_eq!(hid_interfaces(&KEYBOARD_CONFIG[..20]).1, []);
}

#[test]
fn a_string_descriptor_is_utf16() {
    let d = [12, 3, b'Q', 0, b'E', 0, b'M', 0, b'U', 0, b' ', 0];
    assert_eq!(string_text(&d), "QEMU");
    assert_eq!(string_text(&[]), "");
    assert_eq!(string_text(&[40, 3, b'A', 0]), "A", "a length past the end");
}
