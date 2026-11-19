use std::collections::VecDeque;

use super::*;

/// A mouse behind the fake controller, up to the protocol of id `max`.
struct FakeMouse {
    max: u8,
    id: u8,
    rates: Vec<u8>,
    rate_next: bool,
}

impl FakeMouse {
    fn new(max: u8) -> FakeMouse {
        FakeMouse {
            max,
            id: 0,
            rates: Vec::new(),
            rate_next: false,
        }
    }

    fn take(&mut self, byte: u8) -> Vec<u8> {
        if std::mem::take(&mut self.rate_next) {
            self.rates.push(byte);
            let tail = &self.rates[self.rates.len().saturating_sub(3)..];
            if self.id == 0 && self.max >= 3 && tail == [200, 100, 80] {
                self.id = 3;
            } else if self.id == 3 && self.max >= 4 && tail == [200, 200, 80] {
                self.id = 4;
            }
            return vec![ACK];
        }
        match byte {
            0xFF => vec![ACK, 0xAA, self.id],
            0xF2 => vec![ACK, self.id],
            0xF3 => {
                self.rate_next = true;
                vec![ACK]
            }
            0xF4..=0xF6 => vec![ACK],
            _ => vec![RESEND],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum W {
    Cmd(u8),
    Data(u8),
}

/// An i8042: bytes to read, each with whether it is the aux port's, and
/// all that was written. Each look at the status is a millisecond.
struct Fake {
    inbox: VecDeque<(u8, bool)>,
    wrote: Vec<W>,
    mouse: Option<FakeMouse>,
    config: u8,
    config_written: Option<u8>,
    to_aux: bool,
    config_next: bool,
    /// Whether it never takes a byte.
    stuck: bool,
    clock: u64,
}

impl Fake {
    fn new(mouse: Option<u8>) -> Fake {
        Fake {
            inbox: VecDeque::new(),
            wrote: Vec::new(),
            mouse: mouse.map(FakeMouse::new),
            config: 0x30 | 0x04,
            config_written: None,
            to_aux: false,
            config_next: false,
            stuck: false,
            clock: 0,
        }
    }

    fn aux_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut armed = false;
        for w in &self.wrote {
            match *w {
                W::Cmd(c) => armed = c == 0xD4,
                W::Data(d) if armed => {
                    out.push(d);
                    armed = false;
                }
                W::Data(_) => {}
            }
        }
        out
    }

    fn commands(&self) -> Vec<u8> {
        self.wrote
            .iter()
            .filter_map(|w| match w {
                W::Cmd(c) => Some(*c),
                W::Data(_) => None,
            })
            .collect()
    }
}

impl Io for Fake {
    fn status(&mut self) -> u8 {
        self.clock += 1_000_000;
        if self.stuck {
            return INPUT_FULL;
        }
        match self.inbox.front() {
            Some((_, true)) => OUTPUT_FULL | AUX_DATA,
            Some((_, false)) => OUTPUT_FULL,
            None => 0,
        }
    }

    fn read(&mut self) -> u8 {
        self.inbox.pop_front().map_or(0xFF, |(b, _)| b)
    }

    fn write_data(&mut self, value: u8) {
        self.wrote.push(W::Data(value));
        if std::mem::take(&mut self.config_next) {
            self.config_written = Some(value);
        } else if std::mem::take(&mut self.to_aux) {
            let reply = self
                .mouse
                .as_mut()
                .map(|m| m.take(value))
                .unwrap_or_default();
            self.inbox.extend(reply.into_iter().map(|b| (b, true)));
        }
    }

    fn write_cmd(&mut self, value: u8) {
        self.wrote.push(W::Cmd(value));
        match value {
            0xD4 => self.to_aux = true,
            0x20 => self.inbox.push_back((self.config, false)),
            0x60 => self.config_next = true,
            _ => {}
        }
    }

    fn now_ns(&mut self) -> u64 {
        self.clock
    }
}

#[test]
fn init_turns_both_ports_on_before_it_reads_the_configuration() {
    let mut io = Fake::new(Some(0));
    init(&mut io);
    assert_eq!(&io.commands()[..3], &[0xAE, 0xA8, 0x20]);
}

#[test]
fn init_leaves_both_interrupts_and_translation_on_and_the_rest_alone() {
    let mut io = Fake::new(Some(0));
    init(&mut io);
    // Clocks on (0x30 cleared), the system flag (0x04) kept.
    assert_eq!(io.config_written, Some(0x01 | 0x02 | 0x04 | 0x40));
}

#[test]
fn init_resets_the_mouse_first_and_starts_it_last() {
    let mut io = Fake::new(Some(0));
    init(&mut io);
    let aux = io.aux_bytes();
    assert_eq!(&aux[..2], &[0xFF, 0xF6]);
    assert_eq!(aux.last(), Some(&0xF4));
    assert!(io.inbox.is_empty(), "every answer read: {:?}", io.inbox);
}

#[test]
fn a_plain_mouse_stays_plain() {
    let mut io = Fake::new(Some(0));
    assert_eq!(init(&mut io), Proto::Ps2);
    assert_eq!(io.mouse.unwrap().rates.last(), Some(&100));
}

#[test]
fn an_intellimouse_answers_the_first_knock() {
    assert_eq!(init(&mut Fake::new(Some(3))), Proto::Imps);
}

#[test]
fn an_explorer_answers_both() {
    let mut io = Fake::new(Some(4));
    assert_eq!(init(&mut io), Proto::Imex);
    let rates = io.mouse.unwrap().rates;
    assert_eq!(rates, [200, 100, 80, 200, 200, 80, 100]);
}

#[test]
fn without_a_mouse_init_ends_and_finds_no_wheel() {
    let mut io = Fake::new(None);
    assert_eq!(init(&mut io), Proto::Ps2);
}

#[test]
fn a_controller_that_takes_nothing_does_not_hang_init() {
    let mut io = Fake::new(Some(4));
    io.stuck = true;
    assert_eq!(init(&mut io), Proto::Ps2);
    assert!(io.wrote.is_empty());
    // Each wait gave up after its timeout, not after forever.
    assert!(io.clock < 60 * TIMEOUT_NS, "{}", io.clock);
}

#[test]
fn leds_are_a_command_and_a_mask() {
    let mut io = Fake::new(None);
    set_leds(&mut io, led_mask(true, true, false));
    assert_eq!(io.wrote, [W::Data(0xED), W::Data(0b110)]);
    assert_eq!(led_mask(false, false, true), 1);
}

#[test]
fn take_keeps_whose_byte_each_is_in_order() {
    let mut io = Fake::new(None);
    io.inbox
        .extend([(0x1e, false), (0x08, true), (0x9e, false)]);
    let mut got = Vec::new();
    take(&mut io, &mut got);
    assert_eq!(got, [(false, 0x1e), (true, 0x08), (false, 0x9e)]);
}

#[test]
fn a_scancode_is_its_key_down_and_up() {
    let mut k = Keyboard::default();
    assert_eq!(k.byte(0x1e), Some((30, true))); // KEY_A
    assert_eq!(k.byte(0x9e), Some((30, false)));
    assert_eq!(k.byte(0x58), Some((88, true))); // KEY_F12
}

#[test]
fn e0_gives_the_grey_keys_and_is_spent_on_one() {
    let mut k = Keyboard::default();
    assert_eq!(k.byte(0xE0), None);
    assert_eq!(k.byte(0x48), Some((103, true))); // KEY_UP
    assert_eq!(k.byte(0x48), Some((72, true))); // KEY_KP8
    assert_eq!(k.byte(0xE0), None);
    assert_eq!(k.byte(0xB8), Some((100, false))); // KEY_RIGHTALT up
}

#[test]
fn the_fake_shifts_and_unknown_codes_give_nothing() {
    let mut k = Keyboard::default();
    assert_eq!(k.byte(0xE0), None);
    assert_eq!(k.byte(0x2A), None);
    assert_eq!(k.byte(0xE0), None);
    assert_eq!(k.byte(0x7a), None);
    assert_eq!(k.byte(0x59), None);
    // And the next byte is plain again.
    assert_eq!(k.byte(0x1c), Some((28, true)));
}

#[test]
fn acks_of_led_commands_are_not_keys() {
    let mut k = Keyboard::default();
    assert_eq!(k.byte(ACK), None);
    assert_eq!(k.byte(RESEND), None);
}

#[test]
fn pause_is_six_bytes_down_then_up() {
    let mut k = Keyboard::default();
    let got: Vec<_> = [0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5]
        .into_iter()
        .filter_map(|b| k.byte(b))
        .collect();
    assert_eq!(got, [(KEY_PAUSE, true), (KEY_PAUSE, false)]);
}

#[test]
fn every_key_given_is_one_the_keyboard_says_it_has() {
    let info = keyboard_info();
    let mut k = Keyboard::default();
    for b in 0..=0xffu8 {
        for prefix in [None, Some(0xE0)] {
            if let Some(p) = prefix {
                k.byte(p);
            }
            if let Some((key, _)) = k.byte(b) {
                assert!(info.sends(EV_KEY, key), "{prefix:?} {b:#x} = {key}");
            }
            k = Keyboard::default();
        }
    }
    assert!(info.sends(EV_LED, LED_CAPSL) && info.bits.contains_key(&EV_REP));
}

#[test]
fn a_packet_moves_with_y_down() {
    let mut m = Mouse::new(Proto::Ps2);
    assert_eq!(m.byte(0x08 | 0x20 | 0x01), None);
    assert_eq!(m.byte(5), None);
    let p = m.byte(0xfe).unwrap();
    assert_eq!((p.dx, p.dy, p.buttons), (5, 2, 1));
}

#[test]
fn a_first_byte_without_bit_3_is_dropped_not_shifted_in() {
    let mut m = Mouse::new(Proto::Ps2);
    // An id, and a byte of a packet half lost.
    assert_eq!(m.byte(0x00), None);
    assert_eq!(m.byte(0x41), None);
    m.byte(0x08);
    m.byte(1);
    assert_eq!(m.byte(1).map(|p| (p.dx, p.dy)), Some((1, -1)));
}

#[test]
fn the_wheels_are_negated_as_linux_does() {
    let imps = decode(Proto::Imps, &[0x08, 0, 0, 0xff]);
    assert_eq!(imps.wheel, 1);
    let imex = decode(Proto::Imex, &[0x08, 0, 0, 0x0f | 0x10 | 0x20]);
    assert_eq!((imex.wheel, imex.buttons), (1, 0b11000));
    let tilt = decode(Proto::Imex, &[0x08, 0, 0, 0x40 | 0x01]);
    assert_eq!((tilt.wheel, tilt.hwheel), (0, -1));
}

#[test]
fn a_four_byte_mouse_waits_for_the_fourth() {
    let mut m = Mouse::new(Proto::Imps);
    for b in [0x08, 0, 0] {
        assert_eq!(m.byte(b), None);
    }
    assert_eq!(m.byte(1).map(|p| p.wheel), Some(-1));
}

#[test]
fn a_packet_is_its_moves_every_button_and_both_wheels() {
    let p = Packet {
        dx: 3,
        dy: 0,
        wheel: -1,
        hwheel: 0,
        buttons: 0b10,
    };
    let ev = packet_events(&p);
    assert_eq!(ev[0], (EV_REL, REL_X, 3));
    assert!(ev.contains(&(EV_KEY, BTN_LEFT + 1, 1)));
    assert!(ev.contains(&(EV_KEY, BTN_LEFT, 0)));
    assert!(ev.ends_with(&[(EV_REL, REL_WHEEL, -1), (EV_REL, REL_WHEEL_HI_RES, -120)]));
}

#[test]
fn the_mouse_says_only_what_its_protocol_has() {
    let plain = mouse_info(Proto::Ps2);
    assert!(plain.sends(EV_REL, REL_X) && !plain.sends(EV_REL, REL_WHEEL));
    assert!(plain.sends(EV_KEY, BTN_LEFT + 2) && !plain.sends(EV_KEY, BTN_LEFT + 3));
    let imex = mouse_info(Proto::Imex);
    assert!(imex.sends(EV_REL, REL_HWHEEL_HI_RES) && imex.sends(EV_KEY, BTN_LEFT + 4));
    assert_eq!(imex.id, [0x11, 2, 4, 0]);
}
