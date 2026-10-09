use super::*;
use crate::sync::Mutex;
use alloc::{boxed::Box, vec::Vec};

/// The report descriptor of a garden-variety 5-button wheel mouse: a
/// 5-bit button block with 3 bits of padding, 12-bit relative X/Y, an
/// 8-bit wheel and an 8-bit Consumer "AC Pan". No report ID. This is the
/// shape almost every USB mouse on a desk actually ships, and it is NOT
/// the boot layout, so nothing works unless the descriptor parses.
const MOUSE_5BTN_12BIT: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x02, // Usage (Mouse)
    0xA1, 0x01, // Collection (Application)
    0x09, 0x01, //   Usage (Pointer)
    0xA1, 0x00, //   Collection (Physical)
    0x05, 0x09, //     Usage Page (Button)
    0x19, 0x01, //     Usage Minimum (1)
    0x29, 0x05, //     Usage Maximum (5)
    0x15, 0x00, //     Logical Minimum (0)
    0x25, 0x01, //     Logical Maximum (1)
    0x95, 0x05, //     Report Count (5)
    0x75, 0x01, //     Report Size (1)
    0x81, 0x02, //     Input (Data,Var,Abs)
    0x95, 0x01, //     Report Count (1)
    0x75, 0x03, //     Report Size (3)
    0x81, 0x01, //     Input (Const)            <- padding
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, //     Usage (X)
    0x09, 0x31, //     Usage (Y)
    0x16, 0x01, 0xF8, // Logical Minimum (-2047)
    0x26, 0xFF, 0x07, // Logical Maximum (2047)
    0x75, 0x0C, //     Report Size (12)
    0x95, 0x02, //     Report Count (2)
    0x81, 0x06, //     Input (Data,Var,Rel)
    0x15, 0x81, //     Logical Minimum (-127)
    0x25, 0x7F, //     Logical Maximum (127)
    0x75, 0x08, //     Report Size (8)
    0x95, 0x01, //     Report Count (1)
    0x09, 0x38, //     Usage (Wheel)
    0x81, 0x06, //     Input (Data,Var,Rel)
    0x05, 0x0C, //     Usage Page (Consumer)
    0x0A, 0x38, 0x02, // Usage (AC Pan)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x06, //     Input (Data,Var,Rel)
    0xC0, //         End Collection
    0xC0, //       End Collection
];

/// The boot-shaped descriptor, with a report ID in front and a consumer
/// report sharing the interface — the multi-report case.
const MOUSE_WITH_REPORT_IDS: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, //
    0x85, 0x01, //   Report ID (1)
    0x09, 0x01, 0xA1, 0x00, //
    0x05, 0x09, 0x19, 0x01, 0x29, 0x03, //
    0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, //
    0x95, 0x01, 0x75, 0x05, 0x81, 0x01, //   padding
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, //   X, Y, Wheel
    0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x03, 0x81, 0x06, //
    0xC0, 0xC0, //
    0x05, 0x0C, 0x09, 0x01, 0xA1, 0x01, //   Consumer Control
    0x85, 0x02, //   Report ID (2)
    0x19, 0x00, 0x2A, 0x3C, 0x02, //
    0x15, 0x00, 0x26, 0x3C, 0x02, 0x95, 0x01, 0x75, 0x10, 0x81, 0x00, //
    0xC0,
];

/// Boot-compatible: 3 buttons, 5 bits of padding, 8-bit X/Y/Wheel in
/// one Input item. QEMU's own `usb-mouse` and most cheap office mice.
const BOOT_SHAPED: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, //
    0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, //
    0x95, 0x03, 0x75, 0x01, 0x81, 0x02, //   3 buttons
    0x95, 0x01, 0x75, 0x05, 0x81, 0x01, //   padding
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, //   X, Y, Wheel
    0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x03, 0x81, 0x06, //
    0xC0, 0xC0,
];
/// Report ID, 5 buttons, 16-bit axes, wheel and AC Pan.
const WIDE_AXES_AND_PAN: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x01, 0xA1, 0x00, //
    0x05, 0x09, 0x19, 0x01, 0x29, 0x05, 0x15, 0x00, 0x25, 0x01, //
    0x95, 0x05, 0x75, 0x01, 0x81, 0x02, //
    0x95, 0x01, 0x75, 0x03, 0x81, 0x01, //
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, //
    0x16, 0x00, 0x80, 0x26, 0xFF, 0x7F, 0x75, 0x10, 0x95, 0x02, 0x81, 0x06, //
    0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, //
    0x05, 0x0C, 0x0A, 0x38, 0x02, 0x95, 0x01, 0x81, 0x06, //
    0xC0, 0xC0,
];
/// The high-resolution wheel pattern: the Wheel sits inside a Logical
/// Collection next to a Resolution Multiplier declared as a *Feature*.
/// A parser that let Feature items consume input bits would put the
/// wheel two bits late and decode garbage.
const HI_RES_WHEEL: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x02, 0x09, 0x01, 0xA1, 0x00, //
    0x05, 0x09, 0x19, 0x01, 0x29, 0x05, 0x15, 0x00, 0x25, 0x01, //
    0x95, 0x05, 0x75, 0x01, 0x81, 0x02, //
    0x95, 0x01, 0x75, 0x03, 0x81, 0x01, //
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, //
    0x16, 0x01, 0xF8, 0x26, 0xFF, 0x07, 0x75, 0x0C, 0x95, 0x02, 0x81, 0x06, //
    0xA1, 0x02, //   Collection (Logical)
    0x15, 0x00, 0x25, 0x01, 0x35, 0x01, 0x45, 0x0C, //
    0x75, 0x02, 0x95, 0x01, 0x09, 0x48, 0xB1, 0x02, //     Resolution Multiplier, Feature
    0x15, 0x81, 0x25, 0x7F, 0x35, 0x00, 0x45, 0x00, //
    0x75, 0x08, 0x95, 0x01, 0x09, 0x38, 0x81, 0x06, //     Wheel, Input
    0xC0, //   End Collection
    0x95, 0x01, 0x75, 0x04, 0xB1, 0x01, //   Feature padding
    0xC0, 0xC0,
];
/// Sixteen buttons declared as one bitmap, then 16-bit axes.
const SIXTEEN_BUTTONS: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, //
    0x05, 0x09, 0x19, 0x01, 0x29, 0x10, 0x15, 0x00, 0x25, 0x01, //
    0x95, 0x10, 0x75, 0x01, 0x81, 0x02, //
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, //
    0x16, 0x00, 0x80, 0x26, 0xFF, 0x7F, 0x75, 0x10, 0x95, 0x02, 0x81, 0x06, //
    0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, //
    0xC0, 0xC0,
];
/// The wheel declared BEFORE X and Y. Nothing forbids it.
const WHEEL_FIRST: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, //
    0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, //
    0x95, 0x03, 0x75, 0x01, 0x81, 0x02, //
    0x95, 0x01, 0x75, 0x05, 0x81, 0x01, //
    0x05, 0x01, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, //
    0x75, 0x08, 0x95, 0x01, 0x81, 0x06, //
    0x09, 0x30, 0x09, 0x31, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06, //
    0xC0, 0xC0,
];
/// Push/Pop with a Report Size change inside the pushed scope, and the
/// wheel declared *after* the Pop so its width comes from the restored
/// globals. A Pop that pops the stack without putting the saved values
/// back leaves the wheel 16 bits wide at the wrong size.
const PUSH_POP: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, //
    0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, //
    0x95, 0x03, 0x75, 0x01, 0x81, 0x02, //   3 buttons
    0x95, 0x01, 0x75, 0x05, 0x81, 0x01, //   padding
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F, //
    0x75, 0x08, 0x95, 0x02, 0x81, 0x06, //   X, Y at 8 bits
    0xA4, //   Push               (saves Report Size 8)
    0x75, 0x10, 0x95, 0x01, 0x81, 0x01, //     16 bits of vendor padding
    0xB4, //   Pop                (restores Report Size 8)
    0x95, 0x01, 0x09, 0x38, 0x81, 0x06, //   Wheel, at the restored size
    0xC0, 0xC0,
];
/// A wireless combo receiver: keyboard under report ID 1, mouse under
/// report ID 2, one interface and one interrupt endpoint.
const COMBO_RECEIVER: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x85, 0x01, //
    0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01, //
    0x75, 0x01, 0x95, 0x08, 0x81, 0x02, //
    0x95, 0x01, 0x75, 0x08, 0x81, 0x01, //
    0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x26, 0xFF, 0x00, //
    0x19, 0x00, 0x29, 0xFF, 0x81, 0x00, //
    0xC0, //
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x02, 0x09, 0x01, 0xA1, 0x00, //
    0x05, 0x09, 0x19, 0x01, 0x29, 0x05, 0x15, 0x00, 0x25, 0x01, //
    0x95, 0x05, 0x75, 0x01, 0x81, 0x02, //
    0x95, 0x01, 0x75, 0x03, 0x81, 0x01, //
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, //
    0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x03, 0x81, 0x06, //
    0xC0, 0xC0,
];
/// Microsoft's hi-res wheel sample: the input report (ID 2) is
/// interrupted by a Feature report (ID 3) for the Resolution Multiplier,
/// and the descriptor switches BACK to ID 2 to declare the Wheel and
/// again for AC Pan. Both belong to the same six-byte input report
/// `[02, buttons, X, Y, wheel, pan]`. Mice that copy this sample are why a
/// real wheel can be dead while QEMU's (no report IDs at all) works.
const SPLIT_REPORT_ID: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x05, 0x01, 0x09, 0x02, 0xA1, 0x02, //
    0x85, 0x02, 0x09, 0x01, 0xA1, 0x00, //       Report ID (2), Pointer
    0x05, 0x09, 0x19, 0x01, 0x29, 0x05, 0x15, 0x00, 0x25, 0x01, //
    0x75, 0x01, 0x95, 0x05, 0x81, 0x02, 0x95, 0x03, 0x81, 0x01, //
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F, //
    0x75, 0x08, 0x95, 0x02, 0x81, 0x06, //       X, Y
    0xA1, 0x02, 0x85, 0x03, 0x09, 0x48, //       Report ID (3), Res. Multiplier
    0x15, 0x00, 0x25, 0x01, 0x35, 0x01, 0x45, 0x04, //
    0x75, 0x02, 0x95, 0x01, 0xA4, 0xB1, 0x02, // Push, Feature
    0x85, 0x02, 0x09, 0x38, 0x35, 0x00, 0x45, 0x00, // back to ID 2, Wheel
    0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x81, 0x06, 0xC0, //
    0xA1, 0x02, 0x85, 0x03, 0x09, 0x48, 0xB4, 0xB1, 0x02, // ID 3, Pop, Feature
    0x35, 0x00, 0x45, 0x00, 0x75, 0x04, 0xB1, 0x03, //
    0x85, 0x02, 0x05, 0x0C, 0x0A, 0x38, 0x02, // back to ID 2, AC Pan
    0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, 0xC0, //
    0xC0, 0xC0, 0xC0,
];
/// Report ID is a global item, so Push saves it and Pop restores it.
/// Report 1 is a mouse; after Push a 16-bit consumer report 2 is
/// declared, and the Pop takes the descriptor back to report 1 for the
/// Wheel, with no Report ID item on the way back.
const POP_RESTORES_REPORT_ID: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x01, 0xA1, 0x00, //
    0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, //
    0x75, 0x01, 0x95, 0x03, 0x81, 0x02, 0x75, 0x05, 0x95, 0x01, 0x81, 0x01, //
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F, //
    0x75, 0x08, 0x95, 0x02, 0x81, 0x06, //       X, Y
    0xA4, //                                     Push (ID 1)
    0x85, 0x02, 0x05, 0x0C, 0x09, 0xE9, 0x15, 0x00, 0x26, 0xFF, 0x00, //
    0x75, 0x10, 0x95, 0x01, 0x81, 0x00, //       report 2: 16-bit consumer
    0xB4, //                                     Pop (back to ID 1)
    0x09, 0x38, 0x95, 0x01, 0x81, 0x06, //       Wheel
    0xC0, 0xC0,
];
/// Xiaomi's wireless mouse dongle (2717:003b), byte for byte as the
/// kernel's own HID selftests carry it (`MIDongleMIWirelessMouse` in
/// `tools/testing/selftests/hid/tests/test_mouse.py`). It spreads one
/// mouse over two report IDs: report 1 is `[01, buttons, wheel, pan]`,
/// report 2 is `[02, X12, Y12]`. Report 3 is the media keys.
const XIAOMI_SPLIT_MOUSE: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x95, 0x05, 0x75, 0x01,
    0x05, 0x09, 0x19, 0x01, 0x29, 0x05, 0x15, 0x00, 0x25, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x03,
    0x81, 0x01, 0x75, 0x08, 0x95, 0x01, 0x05, 0x01, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x81, 0x06,
    0x05, 0x0C, 0x0A, 0x38, 0x02, 0x95, 0x01, 0x81, 0x06, 0xC0, 0x85, 0x02, 0x09, 0x01, 0xA1, 0x00,
    0x75, 0x0C, 0x95, 0x02, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x16, 0x01, 0xF8, 0x26, 0xFF, 0x07,
    0x81, 0x06, 0xC0, 0xC0, 0x05, 0x0C, 0x09, 0x01, 0xA1, 0x01, 0x85, 0x03, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x01, 0x09, 0xCD, 0x81, 0x06, 0x0A, 0x83, 0x01, 0x81, 0x06, 0x09, 0xB5, 0x81,
    0x06, 0x09, 0xB6, 0x81, 0x06, 0x09, 0xEA, 0x81, 0x06, 0x09, 0xE9, 0x81, 0x06, 0x0A, 0x25, 0x02,
    0x81, 0x06, 0x0A, 0x24, 0x02, 0x81, 0x06, 0xC0,
];
/// Microsoft-style hi-res wheel AND pan with 16-bit fields, from the same
/// selftests (`ResolutionMultiplierHWheelMouse`): report 26 is
/// `[1a, buttons, X16, Y16, wheel16, pan16]`.
const SIXTEEN_BIT_WHEEL_AND_PAN: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x05, 0x01, 0x09, 0x02, 0xA1, 0x02, 0x85, 0x1A, 0x09, 0x01,
    0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x05, 0x95, 0x05, 0x75, 0x01, 0x15, 0x00, 0x25, 0x01,
    0x81, 0x02, 0x75, 0x03, 0x95, 0x01, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x95, 0x02,
    0x75, 0x10, 0x16, 0x01, 0x80, 0x26, 0xFF, 0x7F, 0x81, 0x06, 0xA1, 0x02, 0x85, 0x12, 0x09, 0x48,
    0x95, 0x01, 0x75, 0x02, 0x15, 0x00, 0x25, 0x01, 0x35, 0x01, 0x45, 0x0C, 0xB1, 0x02, 0x85, 0x1A,
    0x09, 0x38, 0x35, 0x00, 0x45, 0x00, 0x95, 0x01, 0x75, 0x10, 0x16, 0x01, 0x80, 0x26, 0xFF, 0x7F,
    0x81, 0x06, 0xC0, 0xA1, 0x02, 0x85, 0x12, 0x09, 0x48, 0x75, 0x02, 0x15, 0x00, 0x25, 0x01, 0x35,
    0x01, 0x45, 0x0C, 0xB1, 0x02, 0x35, 0x00, 0x45, 0x00, 0x75, 0x04, 0xB1, 0x01, 0x85, 0x1A, 0x05,
    0x0C, 0x95, 0x01, 0x75, 0x10, 0x16, 0x01, 0x80, 0x26, 0xFF, 0x7F, 0x0A, 0x38, 0x02, 0x81, 0x06,
    0xC0, 0xC0, 0xC0, 0xC0,
];
/// Five buttons, X/Y and a wheel in report 1, and buttons 6..13 in report
/// 2 on their own. Gaming mice with side buttons do this. Report 2's first
/// bit is button 6, not the left button.
const EXTRA_BUTTONS_REPORT: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x01, 0xA1, 0x00, //
    0x05, 0x09, 0x19, 0x01, 0x29, 0x05, 0x15, 0x00, 0x25, 0x01, //
    0x75, 0x01, 0x95, 0x05, 0x81, 0x02, 0x75, 0x03, 0x95, 0x01, 0x81, 0x01, //
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, //
    0x75, 0x08, 0x95, 0x03, 0x81, 0x06, //       X, Y, Wheel
    0x85, 0x02, 0x05, 0x09, 0x19, 0x06, 0x29, 0x0D, //   report 2, buttons 6..13
    0x75, 0x01, 0x95, 0x08, 0x81, 0x02, //
    0xC0, 0xC0,
];

/// A boot-shaped keyboard that also declares a Consumer Control
/// collection for its media keys, under its own report ID. This is the
/// commonest keyboard descriptor there is, and the one that used to
/// classify as `Skip` and never bind.
const KBD_WITH_CONSUMER: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x06, // Usage (Keyboard)
    0xA1, 0x01, // Collection (Application)
    0x85, 0x01, //   Report ID (1)
    0x05, 0x07, //   Usage Page (Keyboard)
    0x19, 0xE0, //   Usage Minimum (LeftControl)
    0x29, 0xE7, //   Usage Maximum (RightGUI)
    0x15, 0x00, 0x25, 0x01, //
    0x75, 0x01, //   Report Size (1)
    0x95, 0x08, //   Report Count (8)
    0x81, 0x02, //   Input (Data,Var,Abs)     <- modifier bitmap
    0x95, 0x01, 0x75, 0x08, 0x81, 0x01, //   reserved byte
    0x95, 0x06, //   Report Count (6)
    0x75, 0x08, //   Report Size (8)
    0x15, 0x00, 0x26, 0xFF, 0x00, //
    0x19, 0x00, //   Usage Minimum (0)
    0x29, 0xFF, //   Usage Maximum (255)
    0x81, 0x00, //   Input (Data,Array)       <- keycodes
    0xC0, //       End Collection
    0x05, 0x0C, // Usage Page (Consumer)
    0x09, 0x01, // Usage (Consumer Control)
    0xA1, 0x01, // Collection (Application)
    0x85, 0x02, //   Report ID (2)
    0x19, 0x00, 0x2A, 0x3C, 0x02, //
    0x15, 0x00, 0x26, 0x3C, 0x02, //
    0x95, 0x01, 0x75, 0x10, 0x81, 0x00, //
    0xC0,
];

fn capture(f: impl FnOnce(&EventListener<InputEvent>)) -> Vec<(u16, u16, i32)> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let lis = EventListener::<InputEvent>::new();
    let sink = seen.clone();
    lis.subscribe(
        Box::new(move |e: &InputEvent| sink.lock().push((e.event_type as u16, e.code, e.value))),
        false,
    );
    f(&lis);
    let out = seen.lock().clone();
    out
}

#[test]
fn a_plain_wheel_mouse_descriptor_yields_every_field() {
    let info = parse_hid_descriptor(MOUSE_5BTN_12BIT);
    let ml = info.mouse.primary().expect("a mouse layout");
    assert_eq!(ml.report_id, None);
    assert_eq!(ml.buttons, Some(BitField { off: 0, len: 5 }));
    assert_eq!(ml.x, Some(BitField { off: 8, len: 12 }));
    assert_eq!(ml.y, Some(BitField { off: 20, len: 12 }));
    assert_eq!(ml.wheel, Some(BitField { off: 32, len: 8 }));
    assert_eq!(ml.hwheel, Some(BitField { off: 40, len: 8 }));
    assert_eq!(ml.report_bytes, 6);
    assert_eq!(info.max_report_bytes, 6);
    assert_eq!(classify_hid_report(MOUSE_5BTN_12BIT), HidClass::Mouse);
}

#[test]
fn a_report_id_mouse_keeps_its_wheel_and_sizes_the_other_report() {
    let info = parse_hid_descriptor(MOUSE_WITH_REPORT_IDS);
    let ml = info.mouse.primary().expect("a mouse layout");
    assert_eq!(ml.report_id, Some(1));
    // Fields start after the report-ID byte.
    assert_eq!(ml.buttons, Some(BitField { off: 8, len: 3 }));
    assert_eq!(ml.x, Some(BitField { off: 16, len: 8 }));
    assert_eq!(ml.y, Some(BitField { off: 24, len: 8 }));
    assert_eq!(ml.wheel, Some(BitField { off: 32, len: 8 }));
    assert_eq!(ml.report_bytes, 5);
    // The consumer report (ID byte + 16 bits) must be counted too, or the
    // interrupt TD is armed too short and the endpoint babbles.
    assert_eq!(info.max_report_bytes, 5);
}

#[test]
fn a_mouse_we_parsed_a_layout_for_is_asked_for_report_protocol() {
    // The boot mouse report is three bytes -- buttons, X, Y -- and has no
    // wheel byte in it at all. Putting a boot-subclass mouse into boot
    // protocol while decoding its report-protocol layout is exactly why
    // the wheel was dead on real hardware: its Wheel field lives at byte 4
    // of a six-byte report the device then stops sending.
    let parsed = parse_hid_descriptor(MOUSE_5BTN_12BIT);
    assert_eq!(parsed.mouse.primary().unwrap().wheel.unwrap().off / 8, 4);
    assert_eq!(
        hid_protocol_request(HID_PROTO_MOUSE, &parsed),
        HID_PROTOCOL_REPORT
    );
    let kbd = parse_hid_descriptor(KBD_WITH_CONSUMER);
    assert_eq!(
        hid_protocol_request(HID_PROTO_KEY, &kbd),
        HID_PROTOCOL_REPORT
    );
}

#[test]
fn a_boot_mouse_reads_its_descriptor_and_is_put_in_report_protocol() {
    // Moebius's mouse, 30fa:0400: boot subclass, bInterfaceProtocol 2.
    // Its descriptor was never read, so it had no layout, was asked for
    // boot protocol and sent three-byte reports with no wheel in them.
    assert!(reads_report_descriptor(HID_PROTO_MOUSE, 0x30fa, 0x0400, 67));
    assert!(reads_report_descriptor(0, 0x30fa, 0x0400, 67));
    let info = iface_desc_info(
        classify_hid_report(MOUSE_5BTN_12BIT),
        parse_hid_descriptor(MOUSE_5BTN_12BIT),
        true,
    );
    assert!(info.mouse.has_wheel());
    assert_eq!(
        hid_protocol_request(HID_PROTO_MOUSE, &info),
        HID_PROTOCOL_REPORT
    );
}

#[test]
fn a_boot_keyboard_a_vm_tablet_and_no_descriptor_are_not_read() {
    assert!(!reads_report_descriptor(HID_PROTO_KEY, 0x30fa, 0x0400, 63));
    assert!(!reads_report_descriptor(0, QEMU_USB_VID, 0x0001, 74));
    assert!(!reads_report_descriptor(HID_PROTO_MOUSE, 0x30fa, 0x0400, 0));
}

#[test]
fn a_mouse_by_protocol_keeps_only_a_keyboard_report_with_its_own_id() {
    // A keyboard report with no Report ID would take every report on the
    // endpoint, the mouse's included.
    let mut parsed = parse_hid_descriptor(MOUSE_5BTN_12BIT);
    parsed.key = Some(BOOT_KEY_LAYOUT);
    let class = classify_hid_report(MOUSE_5BTN_12BIT);
    assert_eq!(iface_desc_info(class, parsed, true).key, None);
    assert_eq!(
        iface_desc_info(class, parsed, false).key,
        Some(BOOT_KEY_LAYOUT)
    );
    let combo = parse_hid_descriptor(COMBO_RECEIVER);
    assert!(combo.key.unwrap().report_id.is_some());
    let class = classify_hid_report(COMBO_RECEIVER);
    assert_eq!(iface_desc_info(class, combo, true).key, combo.key);
    // And the pointer layout still follows what the descriptor is.
    let kbd = parse_hid_descriptor(KBD_WITH_CONSUMER);
    let class = classify_hid_report(KBD_WITH_CONSUMER);
    assert!(iface_desc_info(class, kbd, true).mouse.is_empty());
}

#[test]
fn a_device_whose_descriptor_gave_nothing_is_asked_for_boot_protocol() {
    // With no layout, `dispatch_hid` decodes the fixed boot layout, which
    // is only true of a device that really is in boot protocol.
    let empty = HidDescInfo::default();
    assert_eq!(
        hid_protocol_request(HID_PROTO_MOUSE, &empty),
        HID_PROTOCOL_BOOT
    );
    assert_eq!(
        hid_protocol_request(HID_PROTO_KEY, &empty),
        HID_PROTOCOL_BOOT
    );
    // A parsed mouse layout says nothing about the keyboard role: a combo
    // receiver's keyboard interface must not be dragged into report
    // protocol by the mouse's descriptor.
    let mouse_only = parse_hid_descriptor(MOUSE_5BTN_12BIT);
    assert_eq!(
        hid_protocol_request(HID_PROTO_KEY, &mouse_only),
        HID_PROTOCOL_BOOT
    );
}

#[test]
fn the_wheel_byte_of_a_boot_report_is_simply_not_there() {
    // The regression in two assertions: the same layout over a full report
    // and over a boot report.
    let ml = parse_hid_descriptor(MOUSE_5BTN_12BIT)
        .mouse
        .primary()
        .unwrap();
    let wheel = ml.wheel.unwrap();
    let mut full = [0u8; 6];
    full[4] = 1;
    assert_eq!(read_signed_bits(&full, wheel.off, wheel.len), 1);
    let boot = [0x00u8, 0x00, 0x00];
    assert_eq!(read_signed_bits(&boot, wheel.off, wheel.len), 0);
    // ...and that short report is what tells us to stop using this layout.
    assert!(mouse_report_is_truncated(&ml, boot.len(), true));
    assert!(!mouse_report_is_truncated(&ml, full.len(), true));
}

#[test]
fn a_short_report_is_only_a_boot_report_when_the_interface_has_one_report() {
    // An interface that multiplexes Report IDs sends reports of several
    // lengths by design, so a short one there is a different report, not a
    // device stuck in boot protocol.
    let ids = parse_hid_descriptor(MOUSE_WITH_REPORT_IDS)
        .mouse
        .primary()
        .unwrap();
    assert!(!mouse_report_is_truncated(&ids, 3, true));
    // And an interface with no boot layout to fall back to (subclass 0,
    // bInterfaceProtocol 0) must keep decoding its descriptor, whatever
    // length its reports come in.
    let plain = parse_hid_descriptor(MOUSE_5BTN_12BIT)
        .mouse
        .primary()
        .unwrap();
    assert!(!mouse_report_is_truncated(&plain, 3, false));
}

/// Seven report descriptors in the shapes real mice actually ship, to pin
/// down where each one puts its wheel. Written when the wheel worked in
/// QEMU and not on real hardware: QEMU only ever attaches `usb-tablet`
/// (see `USB_POINTER` in `zCore/Makefile`), so the relative path had never
/// run anywhere and the parser was the first suspect. It is not: every one
/// of these yields a wheel. That is worth keeping as a test rather than
/// re-deriving the next time scrolling breaks.
#[test]
fn the_wheel_is_found_in_every_shape_a_real_mouse_ships() {
    // (descriptor, report id, wheel bit offset, report bytes).
    let cases: [(&str, &[u8], Option<u8>, usize, usize); 9] = [
        ("boot-shaped", BOOT_SHAPED, None, 24, 4),
        ("wide axes + pan", WIDE_AXES_AND_PAN, Some(1), 48, 8),
        ("hi-res wheel", HI_RES_WHEEL, Some(2), 40, 6),
        ("sixteen buttons", SIXTEEN_BUTTONS, None, 48, 7),
        ("wheel first", WHEEL_FIRST, None, 8, 4),
        ("push/pop", PUSH_POP, None, 40, 6),
        ("combo receiver", COMBO_RECEIVER, Some(2), 32, 5),
        ("split report id", SPLIT_REPORT_ID, Some(2), 32, 6),
        (
            "pop restores report id",
            POP_RESTORES_REPORT_ID,
            Some(1),
            32,
            5,
        ),
    ];
    for (name, desc, id, wheel_off, bytes) in cases {
        let info = parse_hid_descriptor(desc);
        let ml = info
            .mouse
            .primary()
            .unwrap_or_else(|| panic!("{}: no mouse layout at all", name));
        assert_eq!(ml.report_id, id, "{}: report id", name);
        assert_eq!(
            ml.wheel.map(|f| (f.off, f.len)),
            Some((wheel_off, 8)),
            "{}: wheel field",
            name
        );
        assert_eq!(ml.report_bytes, bytes, "{}: report size", name);
        assert_eq!(classify_hid_report(desc), HidClass::Mouse, "{}", name);
    }
    // The combo receiver must keep BOTH roles: pinning such an interface to
    // one of them is how a keyboard-and-mouse dongle loses half itself.
    let combo = parse_hid_descriptor(COMBO_RECEIVER);
    assert_eq!(combo.key.map(|k| k.report_id), Some(Some(1)));
    // ...and the endpoint must be armed for the LONGER of the two reports.
    assert_eq!(combo.max_report_bytes, 9);
    // The pan axis only exists where the descriptor declares AC Pan.
    assert!(parse_hid_descriptor(WIDE_AXES_AND_PAN)
        .mouse
        .primary()
        .unwrap()
        .hwheel
        .is_some());
    assert!(parse_hid_descriptor(BOOT_SHAPED)
        .mouse
        .primary()
        .unwrap()
        .hwheel
        .is_none());
}

/// A descriptor and the raw bytes of one report in, the evdev frame a
/// client would read out. This is the whole relative-mouse path -- the one
/// QEMU has never run, because it only ever attaches `usb-tablet`.
fn mouse_frame(desc: &[u8], report: &[u8]) -> Vec<(u16, u16, i32)> {
    let ml = parse_hid_descriptor(desc).mouse;
    let d = decode_mouse_report(&ml, report, report.len(), false)
        .expect("this report should belong to this mouse");
    capture(|lis| {
        emit_mouse(lis, d, 0);
    })
}

/// The same, for a device decoded with the fixed boot layout.
fn boot_frame(report: &[u8]) -> Vec<(u16, u16, i32)> {
    let d = decode_mouse_report(&MouseReports::default(), report, report.len(), true)
        .expect("a boot mouse report always decodes");
    capture(|lis| {
        emit_mouse(lis, d, 0);
    })
}

#[test]
fn one_detent_of_a_report_protocol_mouse_reaches_evdev() {
    // [buttons, dx, dy, wheel] with only the wheel moved: the frame must
    // carry the wheel and NOT a pair of zero-valued axes, which libinput
    // would read as motion.
    assert_eq!(
        mouse_frame(BOOT_SHAPED, &[0x00, 0x00, 0x00, 0x01]),
        alloc::vec![
            (EV_REL, REL_WHEEL, 1),
            (EV_REL, REL_WHEEL_HI_RES, 120),
            (EV_SYN, SYN_REPORT, 0),
        ]
    );
}

#[test]
fn scrolling_down_is_negative_the_whole_way_through() {
    // HID Wheel is positive away from the user and so is REL_WHEEL, so a
    // pull towards the user stays negative on both axes. Negating it here
    // is what made the desktop scroll backwards, which gets reported as a
    // wheel that does not work.
    assert_eq!(
        mouse_frame(BOOT_SHAPED, &[0x00, 0x00, 0x00, 0xFF]),
        alloc::vec![
            (EV_REL, REL_WHEEL, -1),
            (EV_REL, REL_WHEEL_HI_RES, -120),
            (EV_SYN, SYN_REPORT, 0),
        ]
    );
}

#[test]
fn a_fast_flick_reports_every_detent_it_carries() {
    // A mouse coalesces detents when the host is slow to poll, so a single
    // report can say 3. Clamping it to one loses scrolling speed.
    for n in [2i32, 3, 7, 127] {
        let frame = mouse_frame(BOOT_SHAPED, &[0x00, 0x00, 0x00, n as u8]);
        assert_eq!(
            frame,
            alloc::vec![
                (EV_REL, REL_WHEEL, n),
                (EV_REL, REL_WHEEL_HI_RES, n * 120),
                (EV_SYN, SYN_REPORT, 0),
            ],
            "{} detents",
            n
        );
    }
}

#[test]
fn the_pan_axis_comes_out_of_ac_pan_and_not_the_wheel() {
    // Report ID 1, 5 buttons + 3 pad, X16, Y16, wheel, pan.
    // Tilt right by one, nothing else.
    let report = [0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01];
    assert_eq!(
        mouse_frame(WIDE_AXES_AND_PAN, &report),
        alloc::vec![
            (EV_REL, REL_HWHEEL, 1),
            (EV_REL, REL_HWHEEL_HI_RES, 120),
            (EV_SYN, SYN_REPORT, 0),
        ]
    );
}

#[test]
fn a_wheel_and_a_pan_in_one_report_are_one_frame() {
    // Both axes moved in the same report: four events and exactly one
    // SYN_REPORT. Two frames here would make a diagonal scroll stutter.
    let report = [0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0x01];
    assert_eq!(
        mouse_frame(WIDE_AXES_AND_PAN, &report),
        alloc::vec![
            (EV_REL, REL_WHEEL, -1),
            (EV_REL, REL_WHEEL_HI_RES, -120),
            (EV_REL, REL_HWHEEL, 1),
            (EV_REL, REL_HWHEEL_HI_RES, 120),
            (EV_SYN, SYN_REPORT, 0),
        ]
    );
}

#[test]
fn the_wheel_of_a_twelve_bit_mouse_is_not_shifted_by_its_axes() {
    // MOUSE_5BTN_12BIT packs X and Y as 12 bits each, so the wheel starts
    // at byte 4 rather than byte 3. Reading it at the boot offset gives
    // the high nibble of Y: motion that looks like scrolling.
    // buttons=left, X=-1, Y=+1, wheel=+1, pan=0.
    let report = [0x01, 0xFF, 0x1F, 0x00, 0x01, 0x00];
    assert_eq!(
        mouse_frame(MOUSE_5BTN_12BIT, &report),
        alloc::vec![
            (EV_KEY, BTN_LEFT, 1),
            (EV_REL, REL_X, -1),
            (EV_REL, REL_Y, 1),
            (EV_REL, REL_WHEEL, 1),
            (EV_REL, REL_WHEEL_HI_RES, 120),
            (EV_SYN, SYN_REPORT, 0),
        ]
    );
}

#[test]
fn a_report_belonging_to_another_report_id_moves_nothing() {
    // The keyboard half of a combo receiver shares the endpoint. Decoding
    // its keycodes as buttons and deltas is how a keystroke used to move
    // the pointer.
    let ml = parse_hid_descriptor(COMBO_RECEIVER).mouse;
    assert_eq!(ml.primary().map(|m| m.report_id), Some(Some(2)));
    let keyboard_report = [0x01, 0x02, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00];
    assert_eq!(
        decode_mouse_report(&ml, &keyboard_report, keyboard_report.len(), false),
        None
    );
    // ...and its own report still decodes.
    let mouse_report = [0x02, 0x00, 0x00, 0x00, 0x01];
    assert_eq!(
        decode_mouse_report(&ml, &mouse_report, mouse_report.len(), false)
            .map(|d| (d.wheel, d.hwheel)),
        Some((1, 0))
    );
}

#[test]
fn a_three_byte_boot_report_has_no_wheel_to_find() {
    // The boot mouse report is buttons, X, Y and nothing else. This is the
    // shape a device stuck in boot protocol sends, and the reason the
    // wheel could not work there however well the descriptor parsed.
    assert_eq!(
        boot_frame(&[0x00, 0x00, 0x00]),
        alloc::vec![(EV_SYN, SYN_REPORT, 0)]
    );
    // A device that does send a fourth byte gets its wheel decoded.
    assert_eq!(
        boot_frame(&[0x00, 0x00, 0x00, 0xFF]),
        alloc::vec![
            (EV_REL, REL_WHEEL, -1),
            (EV_REL, REL_WHEEL_HI_RES, -120),
            (EV_SYN, SYN_REPORT, 0),
        ]
    );
}

#[test]
fn an_interface_with_no_layout_and_no_boot_layout_dispatches_nothing() {
    // A report-protocol interface whose descriptor we could not parse: its
    // report ID would decode as a stuck button and its payload as motion.
    assert_eq!(
        decode_mouse_report(
            &MouseReports::default(),
            &[0x01, 0x7F, 0x7F, 0x01],
            4,
            false
        ),
        None
    );
    // And a report too short to be one at all.
    assert_eq!(
        decode_mouse_report(&MouseReports::default(), &[0x00, 0x00], 2, true),
        None
    );
}

#[test]
fn buttons_are_edges_while_the_wheel_is_not() {
    // Holding the left button down across two scrolled reports must send
    // one press, not one per report -- but every detent must still come
    // through.
    let ml = parse_hid_descriptor(BOOT_SHAPED).mouse;
    let first = [0x01u8, 0x00, 0x00, 0x01];
    let second = [0x01u8, 0x00, 0x00, 0x01];
    let d1 = decode_mouse_report(&ml, &first, first.len(), false).unwrap();
    let d2 = decode_mouse_report(&ml, &second, second.len(), false).unwrap();
    let mut held = 0u8;
    let frame1 = capture(|lis| held = emit_mouse(lis, d1, 0));
    assert_eq!(held, 1);
    assert!(frame1.contains(&(EV_KEY, BTN_LEFT, 1)));
    let frame2 = capture(|lis| {
        emit_mouse(lis, d2, held);
    });
    assert!(
        !frame2.iter().any(|&(t, _, _)| t == EV_KEY),
        "the button was already down: {:?}",
        frame2
    );
    assert_eq!(
        frame2,
        alloc::vec![
            (EV_REL, REL_WHEEL, 1),
            (EV_REL, REL_WHEEL_HI_RES, 120),
            (EV_SYN, SYN_REPORT, 0),
        ]
    );
    // Releasing it sends the up edge.
    let up = [0x00u8, 0x00, 0x00, 0x00];
    let d3 = decode_mouse_report(&ml, &up, up.len(), false).unwrap();
    let frame3 = capture(|lis| {
        emit_mouse(lis, d3, held);
    });
    assert_eq!(
        frame3,
        alloc::vec![(EV_KEY, BTN_LEFT, 0), (EV_SYN, SYN_REPORT, 0)]
    );
}

#[test]
fn every_shape_a_real_mouse_ships_can_actually_scroll() {
    // The parser finding a wheel field is not the same as a detent coming
    // out the other end. Put one detent through each descriptor and demand
    // the frame.
    let want = alloc::vec![
        (EV_REL, REL_WHEEL, 1),
        (EV_REL, REL_WHEEL_HI_RES, 120),
        (EV_SYN, SYN_REPORT, 0),
    ];
    for (name, desc, report) in [
        (
            "boot-shaped",
            BOOT_SHAPED,
            alloc::vec![0x00, 0x00, 0x00, 0x01],
        ),
        (
            "wide axes + pan",
            WIDE_AXES_AND_PAN,
            alloc::vec![0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00],
        ),
        (
            "hi-res wheel",
            HI_RES_WHEEL,
            alloc::vec![0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
        ),
        (
            "sixteen buttons",
            SIXTEEN_BUTTONS,
            alloc::vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01],
        ),
        (
            "wheel first",
            WHEEL_FIRST,
            alloc::vec![0x00, 0x01, 0x00, 0x00],
        ),
        (
            "push/pop",
            PUSH_POP,
            alloc::vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x01],
        ),
        (
            "combo receiver",
            COMBO_RECEIVER,
            alloc::vec![0x02, 0x00, 0x00, 0x00, 0x01],
        ),
        (
            "split report id",
            SPLIT_REPORT_ID,
            alloc::vec![0x02, 0x00, 0x00, 0x00, 0x01, 0x00],
        ),
        (
            "pop restores report id",
            POP_RESTORES_REPORT_ID,
            alloc::vec![0x01, 0x00, 0x00, 0x00, 0x01],
        ),
        (
            "xiaomi split mouse",
            XIAOMI_SPLIT_MOUSE,
            alloc::vec![0x01, 0x00, 0x01, 0x00],
        ),
        (
            "sixteen-bit wheel and pan",
            SIXTEEN_BIT_WHEEL_AND_PAN,
            alloc::vec![0x1A, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00],
        ),
    ] {
        assert_eq!(mouse_frame(desc, &report), want, "{}", name);
    }
}

#[test]
fn a_report_id_that_comes_back_continues_its_report() {
    // The Wheel and AC Pan are declared after a detour through Feature
    // report 3. Starting a fresh report at bit 8 on the way back put them
    // in a report of their own that never had X/Y, so the mouse layout
    // came out with no wheel and no pan, four bytes long instead of six.
    let info = parse_hid_descriptor(SPLIT_REPORT_ID);
    let ml = info.mouse.primary().expect("a mouse layout");
    assert_eq!(ml.x, Some(BitField { off: 16, len: 8 }));
    assert_eq!(ml.y, Some(BitField { off: 24, len: 8 }));
    assert_eq!(ml.wheel, Some(BitField { off: 32, len: 8 }));
    assert_eq!(ml.hwheel, Some(BitField { off: 40, len: 8 }));
    assert_eq!(ml.report_bytes, 6);
    // The endpoint is armed for the largest report: split in two, report
    // 2 counted as four bytes and the TD could not hold a whole one.
    assert_eq!(info.max_report_bytes, 6);
    // And the pan really comes out of byte 5, one frame with the wheel.
    assert_eq!(
        mouse_frame(SPLIT_REPORT_ID, &[0x02, 0x00, 0x00, 0x00, 0xFF, 0x01]),
        alloc::vec![
            (EV_REL, REL_WHEEL, -1),
            (EV_REL, REL_WHEEL_HI_RES, -120),
            (EV_REL, REL_HWHEEL, 1),
            (EV_REL, REL_HWHEEL_HI_RES, 120),
            (EV_SYN, SYN_REPORT, 0),
        ]
    );
}

#[test]
fn a_pop_takes_the_walk_back_to_the_report_id_it_restores() {
    // Without the Report ID on the stack the Wheel after the Pop was
    // walked as part of the consumer report 2, and the mouse had none.
    let info = parse_hid_descriptor(POP_RESTORES_REPORT_ID);
    let ml = info.mouse.primary().expect("a mouse layout");
    assert_eq!(ml.report_id, Some(1));
    assert_eq!(ml.wheel, Some(BitField { off: 32, len: 8 }));
    assert_eq!(ml.report_bytes, 5);
    // The largest report is the mouse's five bytes. Walked into report 2,
    // the wheel grew that one to four and left the mouse at four.
    assert_eq!(info.max_report_bytes, 5);
}

#[test]
fn a_mouse_split_across_report_ids_is_still_a_mouse() {
    // Buttons, wheel and pan in report 1, X and Y in report 2. Asking for
    // one report with all three found no mouse here, and a boot-subclass
    // interface then fell back to boot protocol: three-byte reports, a
    // pointer that moves and a wheel that never does, and nothing logged.
    let info = parse_hid_descriptor(XIAOMI_SPLIT_MOUSE);
    assert_eq!(classify_hid_report(XIAOMI_SPLIT_MOUSE), HidClass::Mouse);
    let r: Vec<_> = info.mouse.iter().collect();
    assert_eq!(r.len(), 2, "{:?}", r);
    assert_eq!(r[0].report_id, Some(1));
    assert_eq!(r[0].buttons, Some(BitField { off: 8, len: 5 }));
    assert_eq!(r[0].wheel, Some(BitField { off: 16, len: 8 }));
    assert_eq!(r[0].hwheel, Some(BitField { off: 24, len: 8 }));
    assert_eq!((r[0].x, r[0].y), (None, None));
    assert_eq!(r[1].report_id, Some(2));
    assert_eq!(r[1].x, Some(BitField { off: 8, len: 12 }));
    assert_eq!(r[1].y, Some(BitField { off: 20, len: 12 }));
    assert_eq!(r[1].buttons, None);
    assert!(info.mouse.has_wheel());
    assert_eq!(
        hid_protocol_request(HID_PROTO_MOUSE, &info),
        HID_PROTOCOL_REPORT
    );
}

#[test]
fn a_split_mouse_scrolls_from_one_report_and_moves_from_the_other() {
    assert_eq!(
        mouse_frame(XIAOMI_SPLIT_MOUSE, &[0x01, 0x00, 0xFF, 0x01]),
        alloc::vec![
            (EV_REL, REL_WHEEL, -1),
            (EV_REL, REL_WHEEL_HI_RES, -120),
            (EV_REL, REL_HWHEEL, 1),
            (EV_REL, REL_HWHEEL_HI_RES, 120),
            (EV_SYN, SYN_REPORT, 0),
        ]
    );
    // X = -1 and Y = +1, twelve bits each.
    assert_eq!(
        mouse_frame(XIAOMI_SPLIT_MOUSE, &[0x02, 0xFF, 0x1F, 0x00]),
        alloc::vec![
            (EV_REL, REL_X, -1),
            (EV_REL, REL_Y, 1),
            (EV_SYN, SYN_REPORT, 0),
        ]
    );
    // The media-key report is not the mouse's.
    let ml = parse_hid_descriptor(XIAOMI_SPLIT_MOUSE).mouse;
    assert_eq!(
        decode_mouse_report(&ml, &[0x03, 0xFF, 0x00], 3, false),
        None
    );
}

#[test]
fn a_report_without_buttons_does_not_release_the_held_ones() {
    // Dragging with the split mouse: the left button goes down in report
    // 1, the motion arrives in report 2, which has no buttons. Reading
    // that as "all released" dropped every drag after its first step.
    let ml = parse_hid_descriptor(XIAOMI_SPLIT_MOUSE).mouse;
    let press = decode_mouse_report(&ml, &[0x01, 0x01, 0x00, 0x00], 4, false).unwrap();
    let mut held = 0u8;
    let frame = capture(|lis| held = emit_mouse(lis, press, 0));
    assert_eq!(
        frame,
        alloc::vec![(EV_KEY, BTN_LEFT, 1), (EV_SYN, SYN_REPORT, 0)]
    );
    let motion = decode_mouse_report(&ml, &[0x02, 0x05, 0x00, 0x00], 4, false).unwrap();
    assert_eq!(motion.buttons, None);
    let frame = capture(|lis| held = emit_mouse(lis, motion, held));
    assert_eq!(held, 1);
    assert_eq!(
        frame,
        alloc::vec![(EV_REL, REL_X, 5), (EV_SYN, SYN_REPORT, 0)]
    );
    let release = decode_mouse_report(&ml, &[0x01, 0x00, 0x00, 0x00], 4, false).unwrap();
    let frame = capture(|lis| held = emit_mouse(lis, release, held));
    assert_eq!(held, 0);
    assert_eq!(
        frame,
        alloc::vec![(EV_KEY, BTN_LEFT, 0), (EV_SYN, SYN_REPORT, 0)]
    );
}

#[test]
fn buttons_of_another_report_are_not_left_and_right() {
    // Report 2 carries buttons 6..13. Its first bit is button 6, so it
    // must not come out as BTN_LEFT; it has no other pointer field, so it
    // is not one of the mouse's reports at all.
    let info = parse_hid_descriptor(EXTRA_BUTTONS_REPORT);
    assert_eq!(info.mouse.iter().count(), 1);
    let ml = info.mouse.primary().unwrap();
    assert_eq!(ml.report_id, Some(1));
    assert_eq!(ml.buttons, Some(BitField { off: 8, len: 5 }));
    assert_eq!(ml.wheel, Some(BitField { off: 32, len: 8 }));
    assert_eq!(
        decode_mouse_report(&info.mouse, &[0x02, 0xFF, 0x00], 3, false),
        None
    );
}

#[test]
fn sixteen_bit_wheel_and_pan_come_out_whole() {
    let ml = parse_hid_descriptor(SIXTEEN_BIT_WHEEL_AND_PAN)
        .mouse
        .primary()
        .unwrap();
    assert_eq!(ml.report_id, Some(26));
    assert_eq!(ml.wheel, Some(BitField { off: 48, len: 16 }));
    assert_eq!(ml.hwheel, Some(BitField { off: 64, len: 16 }));
    assert_eq!(ml.report_bytes, 10);
    // Wheel -1 is 0xFFFF; reading eight of its sixteen bits would give 255.
    assert_eq!(
        mouse_frame(
            SIXTEEN_BIT_WHEEL_AND_PAN,
            &[0x1A, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0x02, 0x00]
        ),
        alloc::vec![
            (EV_REL, REL_WHEEL, -1),
            (EV_REL, REL_WHEEL_HI_RES, -120),
            (EV_REL, REL_HWHEEL, 2),
            (EV_REL, REL_HWHEEL_HI_RES, 240),
            (EV_SYN, SYN_REPORT, 0),
        ]
    );
}

#[test]
fn a_pan_is_not_a_wheel_and_axes_without_buttons_are_not_a_mouse() {
    // Buttons, X, Y and AC Pan, no Wheel: a mouse, but one whose page
    // scrolling cannot work, which is what the boot-time error says.
    const PAN_ONLY: &[u8] = &[
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, //
        0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, //
        0x75, 0x01, 0x95, 0x03, 0x81, 0x02, 0x75, 0x05, 0x95, 0x01, 0x81, 0x01, //
        0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F, //
        0x75, 0x08, 0x95, 0x02, 0x81, 0x06, //
        0x05, 0x0C, 0x0A, 0x38, 0x02, 0x95, 0x01, 0x81, 0x06, //
        0xC0, 0xC0,
    ];
    let pan = parse_hid_descriptor(PAN_ONLY).mouse;
    assert!(!pan.is_empty());
    assert!(!pan.has_wheel());
    // Relative X and Y with no button anywhere is not a mouse to bind.
    const AXES_ONLY: &[u8] = &[
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, //
        0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F, //
        0x75, 0x08, 0x95, 0x02, 0x81, 0x06, //
        0xC0, 0xC0,
    ];
    assert!(parse_hid_descriptor(AXES_ONLY).mouse.is_empty());
}

#[test]
fn twelve_bit_axes_sign_extend() {
    // buttons=0b00001 (left), X = -1 (0xFFF), Y = +1.
    let report = [0x01, 0xFF, 0x1F, 0x00, 0x00, 0x00];
    let ml = parse_hid_descriptor(MOUSE_5BTN_12BIT)
        .mouse
        .primary()
        .unwrap();
    let (b, x, y) = (ml.buttons.unwrap(), ml.x.unwrap(), ml.y.unwrap());
    assert_eq!(read_bits(&report, b.off, b.len), 1);
    assert_eq!(read_signed_bits(&report, x.off, x.len), -1);
    assert_eq!(read_signed_bits(&report, y.off, y.len), 1);
}

#[test]
fn scroll_keeps_the_hid_sign_and_carries_the_hi_res_axis() {
    // HID Wheel is positive away from the user, and so is REL_WHEEL:
    // the value must reach evdev unchanged, with 120 per detent on the
    // high-resolution axis beside it.
    let events = capture(|lis| emit_scroll(lis, 1, 0));
    assert_eq!(
        events,
        alloc::vec![(EV_REL, REL_WHEEL, 1), (EV_REL, REL_WHEEL_HI_RES, 120),]
    );

    let down = capture(|lis| emit_scroll(lis, -2, 0));
    assert_eq!(
        down,
        alloc::vec![(EV_REL, REL_WHEEL, -2), (EV_REL, REL_WHEEL_HI_RES, -240),]
    );
}

#[test]
fn horizontal_scroll_uses_its_own_axes() {
    let events = capture(|lis| emit_scroll(lis, 0, 1));
    assert_eq!(
        events,
        alloc::vec![(EV_REL, REL_HWHEEL, 1), (EV_REL, REL_HWHEEL_HI_RES, 120),]
    );
}

#[test]
fn endpoint_interval_uses_the_xhci_exponent_at_every_speed() {
    // High speed (id 3): bInterval is already an exponent, M = N - 1.
    assert_eq!(xhci_endpoint_interval(3, 1), 0);
    assert_eq!(xhci_endpoint_interval(3, 8), 7);
    assert_eq!(xhci_endpoint_interval(3, 16), 15);
    // Out-of-range values clamp rather than wrap.
    assert_eq!(xhci_endpoint_interval(3, 0), 0);
    assert_eq!(xhci_endpoint_interval(3, 255), 15);

    // Full/low speed (ids 1 and 2): bInterval counts 1 ms frames and the
    // field wants log2 of the microframe count, never the frame count.
    assert_eq!(xhci_endpoint_interval(2, 1), 3); // 1 ms  -> 8 microframes
    assert_eq!(xhci_endpoint_interval(2, 2), 4); // 2 ms  -> 16
    assert_eq!(xhci_endpoint_interval(1, 8), 6); // 8 ms  -> 64
    assert_eq!(xhci_endpoint_interval(1, 10), 6); // 10 ms -> 80, floor(log2)=6
                                                  // And it stays inside the legal 3..=10 whatever the device asks for.
    for b in 1..=255u8 {
        let m = xhci_endpoint_interval(1, b);
        assert!((3..=10).contains(&m), "bInterval {} gave {}", b, m);
    }
}

#[test]
fn the_interval_never_reaches_max_esit_payload_hi() {
    // DW0 bits 31:24 are Max ESIT Payload Hi and RsvdZ for us; the old code
    // shifted by 24 and put the interval there, leaving Interval itself 0.
    // (Interval 0 IS legal at high speed -- bInterval 1 -- so the only
    // invariant to assert is that nothing lands above bit 23.)
    for speed in [1u8, 2, 3, 4] {
        for b in 1..=255u8 {
            let dw0 = xhci_endpoint_interval(speed, b) << 16;
            assert_eq!(dw0 & 0xff00_0000, 0, "speed {} bInterval {}", speed, b);
        }
    }
    // A full-speed endpoint always lands in the legal 3..=10, so its
    // Interval field is never zero.
    assert_ne!(xhci_endpoint_interval(2, 10) << 16 & 0x00ff_0000, 0);
}

#[test]
fn a_still_wheel_emits_nothing() {
    assert!(capture(|lis| emit_scroll(lis, 0, 0)).is_empty());
}

#[test]
fn a_keyboard_with_media_keys_behind_it_still_classifies_as_a_keyboard() {
    // The Consumer collection comes LAST. Keeping only the last
    // application collection made this `Skip`, so the interface was never
    // bound and the keyboard was dead.
    assert_eq!(classify_hid_report(KBD_WITH_CONSUMER), HidClass::Key);
}

#[test]
fn a_report_id_keyboard_does_not_decode_its_id_as_modifiers() {
    let info = parse_hid_descriptor(KBD_WITH_CONSUMER);
    let kl = info.key.expect("a keyboard layout");
    assert_eq!(kl.report_id, Some(1));
    // Report ID byte first, then the modifier bitmap, then the reserved
    // byte, then six keycodes.
    assert_eq!(kl.mods, BitField { off: 8, len: 8 });
    assert_eq!(kl.keys, BitField { off: 24, len: 8 });
    assert_eq!(kl.key_count, 6);
    assert_eq!(kl.report_bytes, 9);

    // Report 1, no modifiers, "A" held.
    let report = [0x01, 0x00, 0x00, 0x04, 0, 0, 0, 0, 0];
    let (mods, keys) = decode_keyboard(&report, &kl);
    assert_eq!(mods, 0, "the report ID must not read as a modifier bitmap");
    assert_eq!(keys, [0x04, 0, 0, 0, 0, 0]);

    // The boot layout on the same bytes is what the old code did.
    let (boot_mods, _) = decode_keyboard(&report, &BOOT_KEY_LAYOUT);
    assert_eq!(boot_mods, 0x01, "left Ctrl, latched by the report ID");
}

#[test]
fn the_boot_layout_is_just_another_key_layout() {
    // [mods, reserved, k0..k5]: left Shift held, "B" and "C" down.
    let report = [0x02, 0x00, 0x05, 0x06, 0, 0, 0, 0];
    let (mods, keys) = decode_keyboard(&report, &BOOT_KEY_LAYOUT);
    assert_eq!(mods, 0x02);
    assert_eq!(keys, [0x05, 0x06, 0, 0, 0, 0]);
}

#[test]
fn a_rollover_report_keeps_the_held_keys_down() {
    let held = [0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
    let rollover = [0x01; 6];
    let mut latched = [0u8; 6];
    let evs = capture(|lis| {
        latched = emit_keyboard_delta(lis, 0, 0, &held, &rollover);
    });
    assert_eq!(latched, held, "a rollover must not release what is held");
    // Only the SYN; no key went up.
    assert!(
        evs.iter().all(|&(t, _, _)| t == InputEventType::Syn as u16),
        "{:?}",
        evs
    );
}

#[test]
fn a_normal_report_still_presses_and_releases() {
    let evs = capture(|lis| {
        let out = emit_keyboard_delta(lis, 0, 0, &[0x04, 0, 0, 0, 0, 0], &[0x05, 0, 0, 0, 0, 0]);
        assert_eq!(out, [0x05, 0, 0, 0, 0, 0]);
    });
    let keys: Vec<_> = evs
        .iter()
        .filter(|&&(t, _, _)| t == InputEventType::Key as u16)
        .map(|&(_, c, v)| (c, v))
        .collect();
    assert_eq!(keys.as_slice(), &[(KEY_B, 1), (KEY_A, 0)]);
}

#[test]
fn the_non_us_hash_key_is_backslash_and_102nd_is_its_own_usage() {
    // HID 0x32 ("Non-US # and ~") sits where Backslash does; 0x64
    // ("Non-US \\ and |") is the extra ISO key. Mapping both to 102ND
    // made the `#` key of every UK/DE/ES board type the wrong character.
    assert_eq!(hid_usage_to_linux(0x32), Some(KEY_BACKSLASH));
    assert_eq!(hid_usage_to_linux(0x64), Some(KEY_102ND));
    assert_eq!(hid_usage_to_linux(0x65), Some(KEY_COMPOSE));
    assert_eq!(hid_usage_to_linux(0x68), Some(KEY_F13));
    assert_eq!(hid_usage_to_linux(0x73), Some(KEY_F24));
    assert_eq!(hid_usage_to_linux(0x87), Some(KEY_RO));
    // The error indicators are not keys.
    for u in 0x01..=0x03 {
        assert_eq!(hid_usage_to_linux(u), None, "usage {:#x}", u);
    }
}

#[test]
fn a_hostile_report_count_does_not_hang_the_parser() {
    // Report Size 32, Report Count 0xffffffff, then an Input item. The
    // unbounded walk this used to do is 2^32 iterations inside the IRQ
    // handler: the machine never finishes booting.
    let desc: &[u8] = &[
        0x05, 0x01, // Usage Page (Generic Desktop)
        0x09, 0x02, // Usage (Mouse)
        0xA1, 0x01, // Collection (Application)
        0x75, 0x20, //   Report Size (32)
        0x97, 0xFF, 0xFF, 0xFF, 0xFF, //   Report Count (0xffffffff)
        0x09, 0x30, //   Usage (X)
        0x81, 0x06, //   Input (Data,Var,Rel)
        0xC0,
    ];
    let info = parse_hid_descriptor(desc);
    // Whatever it decides, it has to come back, and with a sane size.
    assert!(info.max_report_bytes <= HID_MAX_REPORT_BITS / 8);
}

#[test]
fn a_short_report_does_not_resurrect_the_previous_one() {
    // `dispatch_hid` zeroes `tmp` past the actual length, so a field that
    // falls outside a short report reads 0. This is that invariant on the
    // decoder itself: byte 5 of a 3-byte report is not the wheel.
    let ml = parse_hid_descriptor(MOUSE_5BTN_12BIT)
        .mouse
        .primary()
        .expect("a mouse layout");
    let mut tmp = [0u8; 64];
    // Full 6-byte report with a wheel detent, then only the first 3 bytes
    // of the next one survive.
    tmp[..6].copy_from_slice(&[0x01, 0x10, 0x00, 0x00, 0x01, 0x00]);
    assert_eq!(
        read_signed_bits(&tmp, ml.wheel.unwrap().off, 8),
        1,
        "the full report does carry a detent"
    );
    tmp[3..].fill(0);
    assert_eq!(read_signed_bits(&tmp, ml.wheel.unwrap().off, 8), 0);
}

#[test]
fn a_control_event_only_answers_its_own_transfer() {
    // Three TRBs, contiguous, plus VirtualBox's off-by-one on each.
    let (setup, data, status) = (0x1000, 0x1010, 0x1020);
    for p in [setup, data, status, 0x1030] {
        assert!(ep0_event_belongs(setup, data, status, p), "{:#x}", p);
    }
    // The TRB before the Setup stage, and anything past the transfer,
    // belong to some other request. The old "[min, max+16)" window let a
    // wrapped transfer (status below setup) span the whole ring.
    for p in [0xff0u64, 0x1040, 0x2000] {
        assert!(!ep0_event_belongs(setup, data, status, p), "{:#x}", p);
    }
    // No data stage: a zero address matches nothing, not address zero.
    assert!(!ep0_event_belongs(setup, 0, status, 0));
    assert!(ep0_event_belongs(setup, 0, status, status));
}

#[test]
fn a_dci_names_the_endpoint_clear_feature_has_to_address() {
    assert_eq!(ep_addr_from_dci(1), 0x80, "EP0 IN");
    assert_eq!(
        ep_addr_from_dci(3),
        0x81,
        "EP1 IN, where HID reports come from"
    );
    assert_eq!(ep_addr_from_dci(2), 0x01, "EP1 OUT");
    assert_eq!(ep_addr_from_dci(9), 0x84);
}

#[test]
fn a_missed_service_error_does_not_halt_the_endpoint() {
    // The one a real xHCI produces on a busy bus, and the reason a mouse
    // went quiet on metal but never under QEMU.
    assert!(!cc_halts_endpoint(23));
    // Stall Error and Babble do halt it.
    assert!(cc_halts_endpoint(6));
    assert!(cc_halts_endpoint(3));
}

/// Every event CODE a device advertises has to have its event TYPE in the
/// `EVIOCGBIT(0)` bitmap. libevdev asks for a type's code bitmap only when
/// that type is set, and neither X11 nor Wayland will deliver an event of
/// a type the device never declared -- so a code advertised under a
/// missing type is not merely untidy, it is silently dropped input.
fn assert_types_cover_codes(vm_tablet: bool, rel_mouse: bool, tablet: bool) {
    let types = hid_capability(CapabilityType::Event, vm_tablet, rel_mouse, tablet);
    for (ev, cap_type) in [
        (EV_KEY, CapabilityType::Key),
        (EV_REL, CapabilityType::RelAxis),
        (EV_ABS, CapabilityType::AbsAxis),
    ] {
        let codes = hid_capability(cap_type, vm_tablet, rel_mouse, tablet);
        let any_code = (0u16..1024).any(|c| codes.contains(c));
        if any_code {
            assert!(
                types.contains(ev),
                "vm_tablet={} rel_mouse={} tablet={}: codes for ev {:#x} \
                 advertised without the type",
                vm_tablet,
                rel_mouse,
                tablet,
                ev
            );
        }
    }
}

#[test]
fn the_type_bitmap_covers_every_code_bitmap_in_every_configuration() {
    for vm_tablet in [false, true] {
        for rel_mouse in [false, true] {
            for tablet in [false, true] {
                assert_types_cover_codes(vm_tablet, rel_mouse, tablet);
            }
        }
    }
}

#[test]
fn a_vm_tablet_still_advertises_a_wheel_it_can_use() {
    // `-device usb-tablet` is what every QEMU run uses, and the tablet arm
    // of `dispatch_hid` does emit REL_WHEEL. EV_REL used to be missing
    // here, so those events were dropped: the wheel could not work in the
    // one configuration anyone ever ran.
    let types = hid_capability(CapabilityType::Event, true, false, true);
    assert!(types.contains(EV_REL), "a wheel needs EV_REL");
    assert!(types.contains(EV_ABS));
    let rel = hid_capability(CapabilityType::RelAxis, true, false, true);
    assert!(rel.contains(REL_WHEEL) && rel.contains(REL_WHEEL_HI_RES));
    // The pointer axes stay off, so libinput still classifies it absolute.
    assert!(!rel.contains(REL_X) && !rel.contains(REL_Y));
}

#[test]
fn a_gamepad_does_not_take_the_pointer_away_on_real_hardware() {
    // A gamepad, a digitizer or a touchscreen all declare absolute X/Y, so
    // `tablet` is true for them on metal -- but none of them owns the
    // pointer, so `USB_ABS_POINTER` (vm_tablet) is false. The node must
    // still be a relative pointer, or the cursor is dead in X11 and
    // Wayland at once while the keyboard keeps working.
    let types = hid_capability(CapabilityType::Event, false, false, true);
    assert!(types.contains(EV_REL), "the pointer must survive a gamepad");
    let rel = hid_capability(CapabilityType::RelAxis, false, false, true);
    assert!(rel.contains(REL_X) && rel.contains(REL_Y));
    let key = hid_capability(CapabilityType::Key, false, false, true);
    assert!(
        key.contains(BTN_LEFT),
        "a pointer with no buttons is rejected"
    );
}

#[test]
fn a_plain_mouse_and_keyboard_box_advertises_both_roles() {
    let types = hid_capability(CapabilityType::Event, false, true, false);
    assert!(types.contains(EV_SYN) && types.contains(EV_KEY) && types.contains(EV_REL));
    assert!(!types.contains(EV_ABS));
    let rel = hid_capability(CapabilityType::RelAxis, false, true, false);
    for code in [
        REL_X,
        REL_Y,
        REL_WHEEL,
        REL_HWHEEL,
        REL_WHEEL_HI_RES,
        REL_HWHEEL_HI_RES,
    ] {
        assert!(rel.contains(code), "missing rel code {:#x}", code);
    }
    let key = hid_capability(CapabilityType::Key, false, true, false);
    assert!(key.contains(KEY_A) && key.contains(KEY_Z) && key.contains(BTN_LEFT));
}

#[test]
fn the_relative_axes_never_appear_without_each_other() {
    // libinput's `evdev_reject_device` throws the WHOLE device away when
    // REL_X and REL_Y disagree, or ABS_X and ABS_Y do -- keyboard
    // included.
    for vm_tablet in [false, true] {
        for rel_mouse in [false, true] {
            for tablet in [false, true] {
                let rel = hid_capability(CapabilityType::RelAxis, vm_tablet, rel_mouse, tablet);
                assert_eq!(rel.contains(REL_X), rel.contains(REL_Y));
                let abs = hid_capability(CapabilityType::AbsAxis, vm_tablet, rel_mouse, tablet);
                assert_eq!(abs.contains(ABS_X), abs.contains(ABS_Y));
            }
        }
    }
}
