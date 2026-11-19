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

/// QEMU's `usb-storage`: one bulk-only SCSI interface, bulk IN 1 and bulk
/// OUT 2.
const DISK_CONFIG: [u8; 32] = [
    9, 2, 32, 0, 1, 1, 0, 0xc0, 1, // configuration 1
    9, 4, 0, 0, 2, 8, 6, 0x50, 0, // interface 0: storage, SCSI, bulk-only
    7, 5, 0x81, 2, 0, 2, 0, // bulk IN 1, 512 bytes
    7, 5, 0x02, 2, 0, 2, 0, // bulk OUT 2
];

#[test]
fn a_configuration_says_its_disk() {
    assert_eq!(
        disk_interface(&DISK_CONFIG),
        Some(DiskIface {
            number: 0,
            bulk_in: (0x81, 512),
            bulk_out: (0x02, 512),
        })
    );
    assert_eq!(hid_interfaces(&DISK_CONFIG).1, []);
    // A keyboard is no disk; a disk with one endpoint is none either.
    assert_eq!(disk_interface(&KEYBOARD_CONFIG), None);
    assert_eq!(disk_interface(&DISK_CONFIG[..25]), None);
}

#[test]
fn a_command_block_and_its_status() {
    let b = cbw(7, 4096, true, &scsi_rw(false, 0x1234, 8));
    assert_eq!(&b[0..4], b"USBC");
    assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 7);
    assert_eq!(u32::from_le_bytes(b[8..12].try_into().unwrap()), 4096);
    assert_eq!((b[12], b[13], b[14]), (0x80, 0, 10));
    assert_eq!(&b[15..25], [0x28, 0, 0, 0, 0x12, 0x34, 0, 0, 8, 0]);
    let mut csw = [0u8; 13];
    csw[0..4].copy_from_slice(b"USBS");
    csw[4..8].copy_from_slice(&7u32.to_le_bytes());
    csw[12] = 1;
    assert_eq!(csw_status(&csw, 7), Some(1));
    assert_eq!(csw_status(&csw, 8), None, "another command's");
    assert_eq!(csw_status(&csw[..12], 7), None, "cut short");
}

#[test]
fn a_far_lba_takes_the_sixteen_byte_command() {
    let c = scsi_rw(true, 1 << 33, 16);
    assert_eq!(c.len(), 16);
    assert_eq!(c[0], 0x8a);
    assert_eq!(&c[2..10], (1u64 << 33).to_be_bytes());
    assert_eq!(&c[10..14], 16u32.to_be_bytes());
    assert_eq!(scsi_rw(true, 5, 1)[0], 0x2a);
}

#[test]
fn a_transfer_is_cut_at_each_64k_boundary() {
    assert_eq!(pieces(0x1_0000, 4096), [(0x1_0000, 4096)]);
    assert_eq!(
        pieces(0xf000, 0x2_0000),
        [(0xf000, 0x1000), (0x1_0000, 0x1_0000), (0x2_0000, 0xf000)]
    );
    assert_eq!(pieces(0, 0), []);
}

/// QEMU's `usb-hub`: one hub interface, its status change endpoint 1 IN.
const HUB_CONFIG: [u8; 25] = [
    9, 2, 25, 0, 1, 1, 0, 0xe0, 0, // configuration 1
    9, 4, 0, 0, 1, 9, 0, 0, 0, // interface 0: hub
    7, 5, 0x81, 3, 1, 0, 255, // interrupt IN 1, 1 byte
];

#[test]
fn a_hub_says_its_status_change_endpoint() {
    assert_eq!(class_interrupt_in(&HUB_CONFIG, CLASS_HUB), Some((0x81, 1, 255)));
    assert_eq!(class_interrupt_in(&HUB_CONFIG, CLASS_HID), None);
    assert_eq!(class_interrupt_in(&KEYBOARD_CONFIG, CLASS_HID), Some((0x81, 8, 7)));
}

#[test]
fn a_hub_port_is_a_tier_of_the_route() {
    assert_eq!(child_route(0, 0, 3), Some(3));
    assert_eq!(child_route(0x3, 1, 2), Some(0x23));
    assert_eq!(child_route(0, 0, 20), Some(15), "a port past 15 says 15");
    assert_eq!(child_route(0x1_1111, 5, 1), None, "five tiers at most");
}

#[test]
fn a_status_change_bitmap_names_ports_from_bit_one() {
    assert_eq!(changed_ports(&[0b0000_0110], 4), [1, 2]);
    assert_eq!(changed_ports(&[0b0000_0001], 4), [], "bit 0 is the hub");
    assert_eq!(changed_ports(&[0, 0b1], 8), [8]);
    assert_eq!(changed_ports(&[0xff], 3), [1, 2, 3]);
}

#[test]
fn a_hub_port_says_its_speed() {
    assert_eq!(hub_port_speed(0x0103), SPEED_FULL);
    assert_eq!(hub_port_speed(0x0303), SPEED_LOW);
    assert_eq!(hub_port_speed(0x0503), SPEED_HIGH);
}
