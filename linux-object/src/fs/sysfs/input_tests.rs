//! `/sys/class/input`: how a device is classified for libinput (the
//! `ID_INPUT_*` properties udev's `input_id` builtin derives from the
//! evdev bitmaps), and what its `eventN` directory says.

use super::tree_tests::contents;
use super::*;

/// The properties of a device with exactly `keys`, `rels` and `abses`.
fn props(keys: &[u16], rels: &[u16], abses: &[u16]) -> Vec<String> {
    let s = input_class_props(&|c| keys.contains(&c), &|c| rels.contains(&c), &|c| {
        abses.contains(&c)
    });
    assert!(s.ends_with('\n'));
    s.lines().map(String::from).collect()
}

const KEY_ESC: u16 = 1;
const KEY_A: u16 = 30;
const BTN_LEFT: u16 = 0x110;
const BTN_TOOL_PEN: u16 = 0x140;
const BTN_TOUCH: u16 = 0x14a;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;

fn has(p: &[String], line: &str) -> bool {
    p.iter().any(|l| l == line)
}

/// Every input device is `ID_INPUT=1` on `seat0`, whatever else it is.
#[test]
fn every_input_device_is_an_input_of_seat0() {
    for p in [
        props(&[], &[], &[]),
        props(&[KEY_ESC], &[], &[]),
        props(&[BTN_LEFT], &[REL_X, REL_Y], &[]),
    ] {
        assert_eq!(&p[..2], ["ID_INPUT=1", "ID_SEAT=seat0"]);
    }
}

/// A keyboard (`KEY_ESC` among its keys) is `ID_INPUT_KEYBOARD` and
/// `ID_INPUT_KEY`, and nothing else.
#[test]
fn a_keyboard_is_a_keyboard() {
    let p = props(&[KEY_ESC, KEY_A], &[], &[]);
    assert!(has(&p, "ID_INPUT_KEYBOARD=1") && has(&p, "ID_INPUT_KEY=1"));
    assert!(!has(&p, "ID_INPUT_MOUSE=1"));
    assert_eq!(p.len(), 4);
}

/// `REL_X` and `REL_Y` make a mouse, buttons or not.
#[test]
fn relative_x_and_y_are_a_mouse() {
    let p = props(&[BTN_LEFT], &[REL_X, REL_Y], &[]);
    assert!(has(&p, "ID_INPUT_MOUSE=1"));
    assert!(!has(&p, "ID_INPUT_KEYBOARD=1"));
    // One axis alone is not.
    assert!(!has(&props(&[BTN_LEFT], &[REL_X], &[]), "ID_INPUT_MOUSE=1"));
}

/// QEMU's `usb-tablet`: absolute X and Y with `BTN_LEFT` and no pen is an
/// absolute mouse (`input_id` calls it a mouse), not a tablet.
#[test]
fn an_absolute_pointer_with_a_left_button_is_a_mouse() {
    let p = props(&[BTN_LEFT], &[], &[ABS_X, ABS_Y]);
    assert!(has(&p, "ID_INPUT_MOUSE=1"));
    assert!(!has(&p, "ID_INPUT_TABLET=1") && !has(&p, "ID_INPUT_TOUCHSCREEN=1"));
}

/// `BTN_TOOL_PEN` with absolute axes is a tablet, and only that.
#[test]
fn a_pen_on_absolute_axes_is_a_tablet() {
    let p = props(&[BTN_LEFT, BTN_TOOL_PEN, BTN_TOUCH], &[], &[ABS_X, ABS_Y]);
    assert!(has(&p, "ID_INPUT_TABLET=1"));
    assert!(!has(&p, "ID_INPUT_MOUSE=1") && !has(&p, "ID_INPUT_TOUCHSCREEN=1"));
}

/// `BTN_TOUCH` with absolute axes and no pen is a touchscreen.
#[test]
fn a_touch_on_absolute_axes_is_a_touchscreen() {
    let p = props(&[BTN_TOUCH], &[], &[ABS_X, ABS_Y]);
    assert!(has(&p, "ID_INPUT_TOUCHSCREEN=1"));
    assert!(!has(&p, "ID_INPUT_MOUSE=1") && !has(&p, "ID_INPUT_TABLET=1"));
}

/// A device with no capability matched is still an input, and nothing more.
#[test]
fn a_device_of_nothing_is_only_an_input() {
    assert_eq!(props(&[], &[], &[]).len(), 2);
    assert_eq!(props(&[KEY_A], &[], &[]).len(), 2, "keys, but no KEY_ESC");
}

/// `/sys/class/input/eventN`: `dev` is `13:(64+N)`, the evdev major and
/// minor; `uevent` says them again with the node's name under `/dev`; and
/// `subsystem` leads to the class.
#[test]
fn an_event_device_says_its_number_and_its_name() {
    for id in [0usize, 3] {
        let node = SysInputEventINode { id };
        let minor = 64 + id;
        let dev = contents(&node.find("dev").unwrap());
        assert_eq!(dev, format!("13:{}\n", minor).as_bytes());
        let uevent = String::from_utf8(contents(&node.find("uevent").unwrap())).unwrap();
        assert!(uevent.starts_with(&format!(
            "MAJOR=13\nMINOR={}\nDEVNAME=input/event{}\nID_INPUT=1\n",
            minor, id
        )));
        let sub = node.find("subsystem").unwrap();
        assert_eq!(sub.metadata().unwrap().type_, FileType::SymLink);
        assert_eq!(contents(&sub), b"../../../class/input");
        assert_eq!(node.metadata().unwrap().inode, ino::INPUT_EVENT + id);
        assert_eq!(
            node.find("..").unwrap().metadata().unwrap().inode,
            ino::CLASS_INPUT
        );
        assert!(node.find("name").is_err());
    }
}
