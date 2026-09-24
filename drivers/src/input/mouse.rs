use alloc::{boxed::Box, sync::Arc};

use crate::sync::Mutex;

use crate::prelude::{CapabilityType, InputEvent, InputEventType};
use crate::scheme::{impl_event_scheme, InputScheme};
use crate::utils::EventListener;

bitflags::bitflags! {
    #[derive(Default)]
    pub struct MouseFlags: u8 {
        /// Whether or not the left mouse button is pressed.
        const LEFT_BTN = 1 << 0;
        /// Whether or not the right mouse button is pressed.
        const RIGHT_BTN = 1 << 1;
        /// Whether or not the middle mouse button is pressed.
        const MIDDLE_BTN = 1 << 2;
        /// Whether or not the packet is valid or not.
        const ALWAYS_ONE = 1 << 3;
        /// Whether or not the x delta is negative.
        const X_SIGN = 1 << 4;
        /// Whether or not the y delta is negative.
        const Y_SIGN = 1 << 5;
    }
}

#[derive(Default, Debug, Clone, Copy)]
pub struct MouseState {
    pub dx: i32,
    pub dy: i32,
    pub dz: i32,
    pub buttons: MouseFlags,
}

/// Which of the three PS/2 mouse protocols a `/dev/input/mice` client has
/// asked for.
///
/// A client picks one by writing a sample-rate sequence, exactly as it would
/// to a real mouse; `mousedev.c` calls these `MOUSEDEV_EMUL_*`. The plain
/// protocol has no room for a wheel at all, which is why the other two exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ps2Mode {
    /// Three bytes, three buttons, no wheel. What every client gets until it
    /// asks for something else.
    #[default]
    Ps2,
    /// IntelliMouse: a fourth byte carrying a signed 8-bit wheel movement.
    ImPs2,
    /// IntelliMouse Explorer: a fourth byte carrying a signed 4-bit wheel
    /// movement and two more buttons.
    ImEx,
}

impl MouseState {
    /// Take one packet's worth of movement, leaving the rest here.
    ///
    /// Returns the bytes and how many of them this protocol sends. One PS/2
    /// packet carries a signed byte per axis, so a movement larger than that
    /// takes more than one packet: `mousedev_packet` subtracts what it sent
    /// and leaves the remainder queued, and so does this. Clamping and
    /// throwing the rest away -- which is what this did -- turns a fast flick
    /// of the wrist into a short one, and the faster the flick the more of it
    /// is lost.
    pub fn take_ps2_packet(&mut self, mode: Ps2Mode) -> ([u8; 4], usize) {
        let dx = self.dx.clamp(-127, 127);
        self.dx -= dx;
        let dy = self.dy.clamp(-127, 127);
        self.dy -= dy;
        let mut flags = self.buttons | MouseFlags::ALWAYS_ONE;
        if dx < 0 {
            flags |= MouseFlags::X_SIGN;
        }
        if dy < 0 {
            flags |= MouseFlags::Y_SIGN;
        }
        let mut out = [flags.bits(), dx as u8, dy as u8, 0];
        let len = match mode {
            Ps2Mode::Ps2 => {
                // There is nowhere to put it, so it is dropped rather than
                // held: kept, it would keep this state from ever draining and
                // every scroll would queue a packet that says nothing.
                self.dz = 0;
                3
            }
            Ps2Mode::ImPs2 => {
                let dz = self.dz.clamp(-127, 127);
                self.dz -= dz;
                out[3] = dz as u8;
                4
            }
            Ps2Mode::ImEx => {
                // Four bits of wheel; the top nibble is the two buttons this
                // mouse does not have.
                let dz = self.dz.clamp(-7, 7);
                self.dz -= dz;
                out[3] = dz as u8 & 0x0f;
                4
            }
        };
        (out, len)
    }

    /// Whether every movement in this state has been sent.
    pub fn is_drained(&self) -> bool {
        self.dx == 0 && self.dy == 0 && self.dz == 0
    }
}

impl MouseState {
    fn update(&mut self, e: &InputEvent) -> Option<MouseState> {
        match e.event_type {
            InputEventType::Syn => {
                use super::input_event_codes::syn::*;
                if e.code == SYN_REPORT {
                    let saved = *self;
                    self.dx = 0;
                    self.dy = 0;
                    self.dz = 0;
                    return Some(saved);
                }
            }
            InputEventType::Key => {
                use super::input_event_codes::key::*;
                let btn = match e.code {
                    BTN_LEFT => MouseFlags::LEFT_BTN,
                    BTN_RIGHT => MouseFlags::RIGHT_BTN,
                    BTN_MIDDLE => MouseFlags::MIDDLE_BTN,
                    _ => return None,
                };
                if e.value == 0 {
                    self.buttons -= btn;
                } else {
                    self.buttons |= btn;
                }
            }
            InputEventType::RelAxis => {
                use super::input_event_codes::rel::*;
                match e.code {
                    REL_X => self.dx += e.value,
                    REL_Y => self.dy -= e.value,
                    REL_WHEEL => self.dz -= e.value,
                    _ => {}
                }
            }
            _ => {}
        }
        None
    }
}

pub struct Mouse {
    listener: EventListener<MouseState>,
    state: Mutex<MouseState>,
}

impl_event_scheme!(Mouse, MouseState);

impl Mouse {
    pub fn new(input: Arc<dyn InputScheme>) -> Arc<Self> {
        let ret = Arc::new(Self {
            listener: EventListener::new(),
            state: Mutex::new(MouseState::default()),
        });
        let cloned = ret.clone();
        input.subscribe(Box::new(move |e| cloned.handle_input_event(e)), false);
        ret
    }

    fn handle_input_event(&self, e: &InputEvent) {
        if let Some(p) = self.state.lock().update(e) {
            self.listener.trigger(p);
        }
    }

    pub fn compatible_with(input: &Arc<dyn InputScheme>) -> bool {
        // A mouse like device, at least one button, two relative axes.
        use super::input_event_codes::{ev::*, key::*, rel::*};
        let ev = input.capability(CapabilityType::Event);
        let key = input.capability(CapabilityType::Key);
        let rel = input.capability(CapabilityType::RelAxis);
        if !ev.contains_all(&[EV_KEY, EV_REL]) {
            return false;
        }
        if !key.contains(BTN_LEFT) {
            return false;
        }
        if !rel.contains_all(&[REL_X, REL_Y]) {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod mouse_tests {
    //! `MouseState` is the whole of the PS/2 side of the mouse: every packet
    //! `/dev/input/mice` hands a client comes out of `take_ps2_packet`, and
    //! every button and every axis a client sees was put there by `update`.
    //! Neither needs a device, a scheme or a listener.
    use super::*;
    use crate::prelude::{InputEvent, InputEventType};

    fn ev(event_type: InputEventType, code: u16, value: i32) -> InputEvent {
        InputEvent {
            event_type,
            code,
            value,
        }
    }

    /// Decode a packet the way a client does: the byte is the low eight bits
    /// of a nine-bit two's-complement number whose top bit is in the flags.
    fn decode(byte: u8, negative: bool) -> i32 {
        byte as i32 - if negative { 256 } else { 0 }
    }

    // ---- what one packet carries ----------------------------------------

    #[test]
    fn a_packet_says_what_a_client_will_read_back() {
        let mut s = MouseState {
            dx: 5,
            dy: -3,
            dz: 0,
            buttons: MouseFlags::LEFT_BTN,
        };
        let (p, len) = s.take_ps2_packet(Ps2Mode::Ps2);
        assert_eq!(len, 3);
        let flags = MouseFlags::from_bits_truncate(p[0]);
        // Bit 3 is how a client that has lost its place finds it again.
        assert!(flags.contains(MouseFlags::ALWAYS_ONE));
        assert!(flags.contains(MouseFlags::LEFT_BTN));
        assert!(!flags.contains(MouseFlags::X_SIGN));
        assert!(flags.contains(MouseFlags::Y_SIGN));
        assert_eq!(decode(p[1], flags.contains(MouseFlags::X_SIGN)), 5);
        assert_eq!(decode(p[2], flags.contains(MouseFlags::Y_SIGN)), -3);
    }

    /// One packet carries a signed byte per axis. A movement larger than that
    /// takes more than one packet, and the rest waits rather than being
    /// thrown away -- which is what turned a fast flick of the wrist into a
    /// short one, the faster the flick the more of it lost.
    #[test]
    fn a_movement_larger_than_one_packet_is_split_and_not_truncated() {
        let mut s = MouseState {
            dx: 300,
            dy: -300,
            dz: 0,
            buttons: MouseFlags::empty(),
        };
        let mut moved_x = 0;
        let mut moved_y = 0;
        let mut packets = 0;
        while !s.is_drained() {
            let (p, _) = s.take_ps2_packet(Ps2Mode::Ps2);
            let flags = MouseFlags::from_bits_truncate(p[0]);
            moved_x += decode(p[1], flags.contains(MouseFlags::X_SIGN));
            moved_y += decode(p[2], flags.contains(MouseFlags::Y_SIGN));
            packets += 1;
            assert!(packets < 10, "not draining");
        }
        assert_eq!(moved_x, 300, "every step of the movement arrives");
        assert_eq!(moved_y, -300);
        assert_eq!(packets, 3, "127 + 127 + 46");
    }

    #[test]
    fn the_largest_step_one_packet_can_say_is_the_one_linux_sends() {
        let mut s = MouseState {
            dx: 1000,
            dy: 0,
            dz: 0,
            buttons: MouseFlags::empty(),
        };
        let (p, _) = s.take_ps2_packet(Ps2Mode::Ps2);
        assert_eq!(p[1], 127);
        assert_eq!(s.dx, 873);
        let mut back = MouseState {
            dx: -1000,
            dy: 0,
            dz: 0,
            buttons: MouseFlags::empty(),
        };
        let (p, _) = back.take_ps2_packet(Ps2Mode::Ps2);
        let flags = MouseFlags::from_bits_truncate(p[0]);
        assert_eq!(decode(p[1], flags.contains(MouseFlags::X_SIGN)), -127);
        assert_eq!(back.dx, -873);
    }

    // ---- the wheel -------------------------------------------------------

    #[test]
    fn plain_ps2_has_nowhere_to_put_the_wheel_so_it_drops_it() {
        let mut s = MouseState {
            dx: 0,
            dy: 0,
            dz: 5,
            buttons: MouseFlags::empty(),
        };
        let (_, len) = s.take_ps2_packet(Ps2Mode::Ps2);
        assert_eq!(len, 3);
        // Held instead of dropped, this state would never drain and every
        // scroll would queue a packet for ever.
        assert!(s.is_drained());
    }

    #[test]
    fn intellimouse_carries_the_wheel_in_a_fourth_byte() {
        let mut s = MouseState {
            dx: 0,
            dy: 0,
            dz: -3,
            buttons: MouseFlags::empty(),
        };
        let (p, len) = s.take_ps2_packet(Ps2Mode::ImPs2);
        assert_eq!(len, 4);
        assert_eq!(p[3] as i8, -3);
        assert!(s.is_drained());
    }

    #[test]
    fn explorer_carries_four_bits_of_wheel_and_keeps_the_rest() {
        let mut s = MouseState {
            dx: 0,
            dy: 0,
            dz: -7,
            buttons: MouseFlags::empty(),
        };
        let (p, len) = s.take_ps2_packet(Ps2Mode::ImEx);
        assert_eq!(len, 4);
        // Four-bit two's complement, and nothing above it.
        assert_eq!(p[3], 0x09);
        assert_eq!(p[3] & 0xf0, 0, "the top nibble is the buttons we lack");
        // A larger scroll takes more than one packet, as the axes do.
        let mut big = MouseState {
            dx: 0,
            dy: 0,
            dz: 20,
            buttons: MouseFlags::empty(),
        };
        let (p, _) = big.take_ps2_packet(Ps2Mode::ImEx);
        assert_eq!(p[3], 7);
        assert_eq!(big.dz, 13);
    }

    #[test]
    fn a_report_with_nothing_in_it_is_spent_at_once() {
        let mut s = MouseState::default();
        assert!(s.is_drained());
        let (p, len) = s.take_ps2_packet(Ps2Mode::Ps2);
        assert_eq!(len, 3);
        assert_eq!(p[1], 0);
        assert_eq!(p[2], 0);
        assert!(s.is_drained());
    }

    // ---- what the evdev stream becomes ----------------------------------

    /// A terminal's y grows downwards and a mouse's grows upwards, so the two
    /// disagree by a sign. Getting it wrong is a pointer that moves the wrong
    /// way, which is the first thing anybody notices.
    #[test]
    fn the_vertical_axis_is_turned_over_and_the_horizontal_one_is_not() {
        use super::super::input_event_codes::{rel::*, syn::*};
        let mut s = MouseState::default();
        assert!(s.update(&ev(InputEventType::RelAxis, REL_X, 4)).is_none());
        assert!(s.update(&ev(InputEventType::RelAxis, REL_Y, 4)).is_none());
        let done = s
            .update(&ev(InputEventType::Syn, SYN_REPORT, 0))
            .expect("a report ends at SYN_REPORT");
        assert_eq!(done.dx, 4);
        assert_eq!(done.dy, -4);
    }

    #[test]
    fn the_wheel_is_turned_over_too() {
        use super::super::input_event_codes::{rel::*, syn::*};
        let mut s = MouseState::default();
        s.update(&ev(InputEventType::RelAxis, REL_WHEEL, 1));
        let done = s.update(&ev(InputEventType::Syn, SYN_REPORT, 0)).unwrap();
        assert_eq!(done.dz, -1);
    }

    #[test]
    fn a_report_ends_at_syn_report_and_the_next_one_starts_at_zero() {
        use super::super::input_event_codes::{rel::*, syn::*};
        let mut s = MouseState::default();
        s.update(&ev(InputEventType::RelAxis, REL_X, 7));
        let first = s.update(&ev(InputEventType::Syn, SYN_REPORT, 0)).unwrap();
        assert_eq!(first.dx, 7);
        let second = s.update(&ev(InputEventType::Syn, SYN_REPORT, 0)).unwrap();
        assert_eq!(second.dx, 0, "the movement was reported once");
    }

    /// Movement is a difference and a button is a state: one is cleared by a
    /// report and the other is not.
    #[test]
    fn a_button_stays_down_across_reports_and_a_movement_does_not() {
        use super::super::input_event_codes::{key::*, syn::*};
        let mut s = MouseState::default();
        s.update(&ev(InputEventType::Key, BTN_LEFT, 1));
        let first = s.update(&ev(InputEventType::Syn, SYN_REPORT, 0)).unwrap();
        assert!(first.buttons.contains(MouseFlags::LEFT_BTN));
        let second = s.update(&ev(InputEventType::Syn, SYN_REPORT, 0)).unwrap();
        assert!(second.buttons.contains(MouseFlags::LEFT_BTN), "still down");
        s.update(&ev(InputEventType::Key, BTN_LEFT, 0));
        let third = s.update(&ev(InputEventType::Syn, SYN_REPORT, 0)).unwrap();
        assert!(!third.buttons.contains(MouseFlags::LEFT_BTN));
    }

    #[test]
    fn the_three_buttons_land_on_the_three_bits_a_client_reads() {
        use super::super::input_event_codes::{key::*, syn::*};
        for (code, bit) in [
            (BTN_LEFT, MouseFlags::LEFT_BTN),
            (BTN_RIGHT, MouseFlags::RIGHT_BTN),
            (BTN_MIDDLE, MouseFlags::MIDDLE_BTN),
        ] {
            let mut s = MouseState::default();
            s.update(&ev(InputEventType::Key, code, 1));
            let done = s.update(&ev(InputEventType::Syn, SYN_REPORT, 0)).unwrap();
            assert_eq!(done.buttons, bit, "{:#x}", code);
            let (p, _) = { done }.take_ps2_packet(Ps2Mode::Ps2);
            assert_eq!(p[0] & 0x07, bit.bits(), "the low three bits are buttons");
        }
    }

    #[test]
    fn a_key_that_is_not_one_of_the_three_buttons_is_ignored() {
        use super::super::input_event_codes::{key::*, syn::*};
        let mut s = MouseState::default();
        s.update(&ev(InputEventType::Key, BTN_SIDE, 1));
        s.update(&ev(InputEventType::Key, KEY_A, 1));
        let done = s.update(&ev(InputEventType::Syn, SYN_REPORT, 0)).unwrap();
        assert_eq!(done.buttons, MouseFlags::empty());
    }

    /// `SYN_REPORT` is the end of a report; the other `SYN_*` codes are not,
    /// and treating one of them as the end would cut a report in two.
    #[test]
    fn only_syn_report_ends_a_report() {
        use super::super::input_event_codes::syn::*;
        let mut s = MouseState::default();
        assert!(s.update(&ev(InputEventType::Syn, SYN_DROPPED, 0)).is_none());
        assert!(s.update(&ev(InputEventType::Syn, SYN_REPORT, 0)).is_some());
    }
}
