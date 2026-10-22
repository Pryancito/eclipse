//! `Mouse` on top of an input scheme: what makes a device eligible to be one,
//! and what reaches a `/dev/input/mice` client once it is.
//!
//! [`MouseState`] is tested on its own in `mouse_tests`, with no device in
//! sight. This is the other half: the wiring. `compatible_with` is what
//! decides whether a device becomes a mouse at all, and it answers from the
//! capability bitmap alone -- a device missing one bit there is never offered
//! as a pointer however well it works, which is the shape of #1401. And
//! `new`'s subscription is the only path by which an input event becomes a
//! packet: without it the device exists and nothing ever moves.

use super::*;
use crate::scheme::input::{CapabilityType, InputCapability, InputEvent, InputEventType};
use crate::scheme::Scheme;
use crate::utils::EventListener;
use alloc::vec::Vec;

use crate::input::input_event_codes::{ev::*, key::*, rel::*, syn::*};

/// An input device whose capability bitmaps the test writes, and that can be
/// made to report an event as a real one would.
struct FakeInput {
    listener: EventListener<InputEvent>,
    /// The codes of each bitmap, as lists: [`InputCapability`] is the wire
    /// format and is neither `Clone` nor `Copy`, so each call builds its own.
    key: Vec<u16>,
    rel: Vec<u16>,
    abs: Vec<u16>,
    ev: Vec<u16>,
}

impl FakeInput {
    /// A device that looks exactly like a two-axis, three-button mouse.
    fn mouse() -> Self {
        Self {
            listener: EventListener::new(),
            key: alloc::vec![BTN_LEFT, BTN_RIGHT, BTN_MIDDLE],
            rel: alloc::vec![REL_X, REL_Y, REL_WHEEL],
            abs: Vec::new(),
            ev: alloc::vec![EV_KEY, EV_REL],
        }
    }

    fn report(&self, event_type: InputEventType, code: u16, value: i32) {
        self.listener.trigger(InputEvent {
            event_type,
            code,
            value,
        });
    }
}

fn caps(codes: &[u16]) -> InputCapability {
    let mut c = InputCapability::empty();
    for &code in codes {
        c.set(code);
    }
    c
}

impl Scheme for FakeInput {
    fn name(&self) -> &str {
        "fake-input"
    }
}

crate::scheme::impl_event_scheme!(FakeInput, InputEvent);

impl InputScheme for FakeInput {
    fn capability(&self, cap_type: CapabilityType) -> InputCapability {
        match cap_type {
            CapabilityType::Key => caps(&self.key),
            CapabilityType::RelAxis => caps(&self.rel),
            CapabilityType::AbsAxis => caps(&self.abs),
            _ => caps(&self.ev),
        }
    }
}

/// `compatible_with` needs an `Arc<dyn InputScheme>`, so every case goes
/// through one.
fn as_scheme(dev: FakeInput) -> Arc<dyn InputScheme> {
    Arc::new(dev)
}

// --- what makes a device a mouse ------------------------------------------

#[test]
fn a_two_axis_three_button_device_is_a_mouse() {
    assert!(Mouse::compatible_with(&as_scheme(FakeInput::mouse())));
}

/// A device that reports no `EV_REL` at all is an absolute pointer or a
/// keyboard, not a mouse: taking it for one leaves the cursor pinned.
#[test]
fn a_device_that_reports_no_relative_events_is_not_a_mouse() {
    let mut dev = FakeInput::mouse();
    dev.ev = alloc::vec![EV_KEY];
    assert!(!Mouse::compatible_with(&as_scheme(dev)));
}

/// Nor is one that reports no keys: `EV_REL` alone is a dial or a volume
/// wheel.
#[test]
fn a_device_that_reports_no_keys_is_not_a_mouse() {
    let mut dev = FakeInput::mouse();
    dev.ev = alloc::vec![EV_REL];
    assert!(!Mouse::compatible_with(&as_scheme(dev)));
}

/// `EV_KEY` in the event bitmap is not enough: the left button has to be in
/// the key bitmap too. A pointer with only side buttons is not what
/// `/dev/input/mice` describes.
#[test]
fn a_device_without_a_left_button_is_not_a_mouse() {
    let mut dev = FakeInput::mouse();
    dev.key = alloc::vec![BTN_RIGHT, BTN_MIDDLE];
    assert!(!Mouse::compatible_with(&as_scheme(dev)));
}

/// And both axes are required, not either: a one-axis device is a wheel.
#[test]
fn a_device_with_only_one_axis_is_not_a_mouse() {
    let mut dev = FakeInput::mouse();
    dev.rel = alloc::vec![REL_X];
    assert!(!Mouse::compatible_with(&as_scheme(dev)));

    let mut dev = FakeInput::mouse();
    dev.rel = alloc::vec![REL_Y];
    assert!(!Mouse::compatible_with(&as_scheme(dev)));
}

/// Subscribes to `mouse` and collects every packet it hands a client.
fn escuchar(mouse: &Arc<Mouse>) -> Arc<Mutex<Vec<MouseState>>> {
    let seen: Arc<Mutex<Vec<MouseState>>> = Arc::new(Mutex::new(Vec::new()));
    let cloned = seen.clone();
    crate::scheme::EventScheme::subscribe(
        &**mouse,
        Box::new(move |s| cloned.lock().push(*s)),
        false,
    );
    seen
}

// --- what reaches a client -------------------------------------------------

/// The subscription is the whole point of `new`: an event reported by the
/// device has to come out of the mouse as a packet, and only at `SYN_REPORT`.
/// The Y axis flips on the way: `REL_Y` grows downward, the packet upward.
#[test]
fn a_report_from_the_device_reaches_a_client_at_syn_report() {
    let dev = Arc::new(FakeInput::mouse());
    let mouse = Mouse::new(dev.clone());
    let seen = escuchar(&mouse);

    dev.report(InputEventType::RelAxis, REL_X, 7);
    dev.report(InputEventType::RelAxis, REL_Y, 3);
    assert!(
        seen.lock().is_empty(),
        "a packet came out before SYN_REPORT"
    );

    dev.report(InputEventType::Syn, SYN_REPORT, 0);

    let packets = seen.lock().clone();
    assert_eq!(packets.len(), 1, "one report, {} packets", packets.len());
    assert_eq!((packets[0].dx, packets[0].dy), (7, -3));
}

/// A button press and its release are two reports, and the second one has to
/// say the button is up: a release swallowed here is a cursor that drags.
#[test]
fn a_press_and_its_release_are_two_packets() {
    let dev = Arc::new(FakeInput::mouse());
    let mouse = Mouse::new(dev.clone());
    let seen = escuchar(&mouse);

    dev.report(InputEventType::Key, BTN_LEFT, 1);
    dev.report(InputEventType::Syn, SYN_REPORT, 0);
    dev.report(InputEventType::Key, BTN_LEFT, 0);
    dev.report(InputEventType::Syn, SYN_REPORT, 0);

    let packets = seen.lock().clone();
    assert_eq!(
        packets.len(),
        2,
        "{} packets for two reports",
        packets.len()
    );
    assert!(
        packets[0].buttons.contains(MouseFlags::LEFT_BTN),
        "the press did not reach the client"
    );
    assert!(
        !packets[1].buttons.contains(MouseFlags::LEFT_BTN),
        "the release did not reach the client"
    );
}

/// Every `SYN_REPORT` is a packet, empty one included: that is how a client
/// knows a report ended, and a device at rest reports nothing at all rather
/// than empty reports.
#[test]
fn an_empty_report_is_a_packet_too() {
    let dev = Arc::new(FakeInput::mouse());
    let mouse = Mouse::new(dev.clone());
    let seen = escuchar(&mouse);

    dev.report(InputEventType::Syn, SYN_REPORT, 0);

    let packets = seen.lock().clone();
    assert_eq!(packets.len(), 1);
    assert_eq!((packets[0].dx, packets[0].dy, packets[0].dz), (0, 0, 0));
}

/// A report drains the deltas and keeps the buttons: carrying a delta into
/// the next report doubles every movement, and dropping a button state ends
/// a drag the user is still holding.
#[test]
fn a_report_drains_the_deltas_and_keeps_the_buttons() {
    let dev = Arc::new(FakeInput::mouse());
    let mouse = Mouse::new(dev.clone());
    let seen = escuchar(&mouse);

    dev.report(InputEventType::Key, BTN_RIGHT, 1);
    dev.report(InputEventType::RelAxis, REL_X, 5);
    dev.report(InputEventType::RelAxis, REL_WHEEL, 1);
    dev.report(InputEventType::Syn, SYN_REPORT, 0);
    dev.report(InputEventType::Syn, SYN_REPORT, 0);

    let packets = seen.lock().clone();
    assert_eq!(packets.len(), 2);
    // The wheel flips on the way out too, just like the Y axis.
    assert_eq!((packets[0].dx, packets[0].dz), (5, -1));
    assert_eq!(
        (packets[1].dx, packets[1].dz),
        (0, 0),
        "the deltas carried into the next report"
    );
    assert!(
        packets[1].buttons.contains(MouseFlags::RIGHT_BTN),
        "the button the user is still holding was dropped"
    );
}
