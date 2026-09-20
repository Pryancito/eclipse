use lock::Mutex;
use x86_64::instructions::port::Port;

use crate::input::input_event_codes::{ev, key, rel};
use crate::prelude::{CapabilityType, InputCapability, InputEvent, InputEventType};
use crate::scheme::{impl_event_scheme, InputScheme, Scheme};
use crate::utils::EventListener;

pub struct Ps2Input {
    listener: EventListener<InputEvent>,
    extended: Mutex<bool>,
    mouse_state: Mutex<MouseState>,
    /// Which aux protocol the mouse was talked into at init. Decides both the
    /// packet length and whether this device has a wheel at all.
    proto: Ps2MouseProto,
}

/// The three PS/2 aux protocols worth supporting. A plain mouse sends 3-byte
/// packets and has no wheel; the two Microsoft extensions add a fourth byte.
/// Which one is live is decided at init by the "magic knock" (a sequence of
/// SET_SAMPLE_RATE values the device recognises), exactly as Linux's
/// `psmouse` does, and the device then reports a new id from GET_INFO (0xF2).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Ps2MouseProto {
    /// Plain 3-byte PS/2 mouse, no wheel.
    #[default]
    Ps2,
    /// IntelliMouse (id 3): 4 bytes, byte 3 is a signed 8-bit wheel.
    Imps,
    /// IntelliMouse Explorer (id 4): 4 bytes, byte 3 packs a 4-bit wheel plus
    /// buttons 4 and 5 (and, on Explorer 4.0, a horizontal wheel).
    Imex,
}

impl Ps2MouseProto {
    /// Bytes per aux packet.
    fn packet_len(self) -> usize {
        match self {
            Ps2MouseProto::Ps2 => 3,
            _ => 4,
        }
    }

    fn has_wheel(self) -> bool {
        self != Ps2MouseProto::Ps2
    }
}

#[derive(Default)]
struct MouseState {
    phase: u8,
    bytes: [u8; 4],
    /// Button bits (left/right/middle/side/extra) as of the last packet, so a
    /// key event is only emitted when one actually changes. Linux's evdev core
    /// drops a repeated value; this kernel's `/dev/input/eventN` does not, so
    /// filtering here is what keeps a moving mouse from burying every other
    /// event under a stream of `BTN_LEFT 0` in a 64-entry ring.
    last_buttons: u8,
}

fn usb_tablet_owns_pointer() -> bool {
    #[cfg(all(
        any(feature = "xhci-usb-hid", feature = "legacy-usb-hid"),
        not(feature = "mock"),
        not(feature = "no-pci")
    ))]
    {
        crate::usb::xhci_hid::usb_abs_pointer_active()
    }
    #[cfg(not(all(
        any(feature = "xhci-usb-hid", feature = "legacy-usb-hid"),
        not(feature = "mock"),
        not(feature = "no-pci")
    )))]
    {
        false
    }
}

fn wait_write() -> bool {
    let mut status_port = Port::<u8>::new(0x64);
    let mut timeout = 100_000;
    unsafe {
        while (status_port.read() & 0x02) != 0 && timeout > 0 {
            timeout -= 1;
            core::hint::spin_loop();
        }
        timeout > 0
    }
}

fn wait_read() -> bool {
    let mut status_port = Port::<u8>::new(0x64);
    let mut timeout = 100_000;
    unsafe {
        while (status_port.read() & 0x01) == 0 && timeout > 0 {
            timeout -= 1;
            core::hint::spin_loop();
        }
        timeout > 0
    }
}

fn drain_output(data_port: &mut Port<u8>, status_port: &mut Port<u8>) {
    unsafe {
        while (status_port.read() & 0x01) != 0 {
            let _ = data_port.read();
        }
    }
}

fn read_data(data_port: &mut Port<u8>) -> Option<u8> {
    if wait_read() {
        Some(unsafe { data_port.read() })
    } else {
        None
    }
}

fn write_aux(data_port: &mut Port<u8>, status_port: &mut Port<u8>, cmd: u8) -> bool {
    unsafe {
        if !wait_write() {
            return false;
        }
        status_port.write(0xD4);
        if !wait_write() {
            return false;
        }
        data_port.write(cmd);
    }
    matches!(read_data(data_port), Some(0xFA))
}

impl Default for Ps2Input {
    fn default() -> Self {
        Self::new()
    }
}

impl Ps2Input {
    pub fn new() -> Self {
        let proto;
        // Initialize PS/2 controller
        unsafe {
            let mut data_port = Port::<u8>::new(0x60);
            let mut status_port = Port::<u8>::new(0x64);

            drain_output(&mut data_port, &mut status_port);

            // 1. Enable ports (keyboard & mouse)
            if wait_write() {
                status_port.write(0xAE); // Enable keyboard port
            }
            if wait_write() {
                status_port.write(0xA8); // Enable mouse port
            }

            // 2. Read Controller Configuration Byte
            if wait_write() {
                status_port.write(0x20);
            }
            let mut config = 0;
            if let Some(v) = read_data(&mut data_port) {
                config = v;
            }

            // 3. Update Configuration Byte:
            // - Bit 0: Enable keyboard interrupt
            // - Bit 1: Enable mouse interrupt
            // - Bit 4: Clear disable keyboard clock
            // - Bit 5: Clear disable mouse clock
            // - Bit 6: Enable translation to Scan Code Set 1
            config |= 0x01;
            config |= 0x02;
            config &= !0x10;
            config &= !0x20;
            config |= 0x40;

            if wait_write() {
                status_port.write(0x60);
                if wait_write() {
                    data_port.write(config);
                }
            }

            drain_output(&mut data_port, &mut status_port);

            // 4. Reset the mouse first: real hardware often ignores follow-up
            // commands until BAT/device-id have completed.
            if write_aux(&mut data_port, &mut status_port, 0xFF) {
                let _ = read_data(&mut data_port); // BAT result (usually 0xAA)
                let _ = read_data(&mut data_port); // Device ID (usually 0x00)
            }

            // 5. Restore defaults, negotiate a wheel protocol, then enable
            // streaming packet reports. The knock MUST happen while the device
            // is still disabled (0xF4 comes last): a mouse in stream mode
            // interleaves motion packets with command replies and the id read
            // below would pick up a motion byte instead.
            let _ = write_aux(&mut data_port, &mut status_port, 0xF6);
            proto = negotiate_wheel(&mut data_port, &mut status_port);
            let _ = write_aux(&mut data_port, &mut status_port, 0xF4);
        }

        info!("[ps2] aux protocol: {:?}", proto);

        Self {
            listener: EventListener::new(),
            extended: Mutex::new(false),
            mouse_state: Mutex::new(MouseState::default()),
            proto,
        }
    }
}

/// Ask the mouse for its device id (GET_INFO, 0xF2). `None` if it does not
/// answer, which is the normal outcome when there is no aux device at all.
fn mouse_id(data_port: &mut Port<u8>, status_port: &mut Port<u8>) -> Option<u8> {
    if !write_aux(data_port, status_port, 0xF2) {
        return None;
    }
    read_data(data_port)
}

/// Set the reporting rate (SET_SAMPLE_RATE, 0xF3 + value). Used both for its
/// own sake and as the "magic knock" the Microsoft wheel extensions listen for.
fn set_sample_rate(data_port: &mut Port<u8>, status_port: &mut Port<u8>, rate: u8) -> bool {
    write_aux(data_port, status_port, 0xF3) && write_aux(data_port, status_port, rate)
}

/// Talk the mouse into the richest protocol it admits to, exactly the sequence
/// Linux's `psmouse` uses (`intellimouse_detect` / `im_explorer_detect`):
///
/// * rates 200, 100, 80 -> id 3 = IntelliMouse, a 4-byte packet whose last
///   byte is a signed wheel delta;
/// * then rates 200, 200, 80 -> id 4 = IntelliMouse Explorer, whose last byte
///   packs a 4-bit wheel plus buttons 4 and 5.
///
/// A device that does not implement the extension simply keeps answering id 0,
/// so a failed knock costs nothing and leaves the plain 3-byte protocol.
/// Without this the mouse never sends a fourth byte at all: the wheel is not
/// "ignored" downstream, it is never transmitted.
fn negotiate_wheel(data_port: &mut Port<u8>, status_port: &mut Port<u8>) -> Ps2MouseProto {
    let mut proto = Ps2MouseProto::Ps2;
    for rate in [200u8, 100, 80] {
        if !set_sample_rate(data_port, status_port, rate) {
            return proto;
        }
    }
    if mouse_id(data_port, status_port) != Some(3) {
        // Leave the device at a sane rate even when the knock did not take.
        let _ = set_sample_rate(data_port, status_port, 100);
        return proto;
    }
    proto = Ps2MouseProto::Imps;

    for rate in [200u8, 200, 80] {
        if !set_sample_rate(data_port, status_port, rate) {
            let _ = set_sample_rate(data_port, status_port, 100);
            return proto;
        }
    }
    if mouse_id(data_port, status_port) == Some(4) {
        proto = Ps2MouseProto::Imex;
    }
    // 100 reports/s is the psmouse default; the knock left it at 80.
    let _ = set_sample_rate(data_port, status_port, 100);
    proto
}

/// What one aux packet means, in evdev terms. Split out from the IRQ handler
/// so the bit twiddling can be unit-tested without an i8042.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct AuxPacket {
    pub dx: i32,
    /// Already flipped into the evdev convention (positive = down).
    pub dy: i32,
    pub wheel: i32,
    pub hwheel: i32,
    /// bit0 left, bit1 right, bit2 middle, bit3 side, bit4 extra.
    pub buttons: u8,
}

/// Decode a PS/2 aux packet. `bytes` holds `proto.packet_len()` valid bytes.
///
/// The wheel sign conventions are Linux's, verbatim (`psmouse_process_byte`):
/// every PS/2 wheel field is NEGATED into `REL_WHEEL`/`REL_HWHEEL`, because
/// a PS/2 mouse counts Z positive toward the user while evdev counts
/// `REL_WHEEL` positive away from it. Linux spells the Explorer's cases as
/// `-sign_extend32(packet[3], 3)` and `-sign_extend32(packet[3], 5)`; the
/// bit arithmetic below is those expressions with the negation folded in.
/// Copying the kernel is the point: these are the numbers every Linux
/// desktop's scroll direction was calibrated against.
pub fn decode_aux_packet(proto: Ps2MouseProto, bytes: &[u8; 4]) -> AuxPacket {
    let flags = bytes[0];
    let dx = if flags & 0x10 != 0 {
        bytes[1] as i32 - 256
    } else {
        bytes[1] as i32
    };
    let dy = if flags & 0x20 != 0 {
        bytes[2] as i32 - 256
    } else {
        bytes[2] as i32
    };

    let mut pkt = AuxPacket {
        dx,
        // PS/2 reports Y up-positive; evdev's REL_Y is down-positive.
        dy: -dy,
        buttons: flags & 0x07,
        ..Default::default()
    };

    match proto {
        Ps2MouseProto::Ps2 => {}
        Ps2MouseProto::Imps => pkt.wheel = -(bytes[3] as i8 as i32),
        Ps2MouseProto::Imex => {
            let z = bytes[3];
            match z & 0xC0 {
                // Explorer 4.0 tilts the wheel: bits 0..5 are a 6-bit signed
                // delta, and bits 6/7 say which axis it belongs to.
                // `(z & 32) - (z & 31)` is `-sign_extend32(z, 5)`, negation
                // included; bits 6/7 fall outside the field and drop out.
                0x80 => pkt.wheel = ((z & 32) as i32) - ((z & 31) as i32),
                0x40 => pkt.hwheel = ((z & 32) as i32) - ((z & 31) as i32),
                _ => {
                    // Likewise `-sign_extend32(z, 3)` over the low nibble.
                    pkt.wheel = ((z & 8) as i32) - ((z & 7) as i32);
                    pkt.buttons |= ((z >> 4) & 1) << 3; // BTN_SIDE
                    pkt.buttons |= ((z >> 5) & 1) << 4; // BTN_EXTRA
                }
            }
        }
    }
    pkt
}

impl_event_scheme!(Ps2Input, InputEvent);

impl Scheme for Ps2Input {
    fn name(&self) -> &str {
        "ps2-input"
    }

    fn handle_irq(&self, _irq_num: usize) {
        let mut data_port = Port::<u8>::new(0x60);
        let mut status_port = Port::<u8>::new(0x64);

        unsafe {
            loop {
                let status = status_port.read();
                if (status & 0x01) == 0 {
                    break;
                }

                let is_aux = (status & 0x20) != 0;
                let code = data_port.read();

                if is_aux {
                    // Handle mouse data
                    let mut state = self.mouse_state.lock();
                    // Packet resync. Byte 0 of every PS/2 mouse packet has bit 3
                    // (the "always one" signature) SET. If we are at the START of a
                    // packet and this byte does not, the 3-byte stream has slipped --
                    // a dropped/extra byte from an i8042 output-buffer overflow under
                    // load, or the 0xAA (BAT) / 0x00 (device id) a real mouse streams
                    // ~500 ms after its power-on reset, long after our short init
                    // read timed out. Drop the stray byte and STAY at phase 0 rather
                    // than latch a SHIFTED packet: a shifted packet decodes dx/dy in
                    // place of the flags byte, so motion becomes garbage and the
                    // pointer sticks in place ("raton en estatico") until reboot,
                    // even though it is still drawn. Skipping mis-first bytes here
                    // realigns to the next real packet boundary on its own. On an
                    // already aligned stream (QEMU) byte 0 always has bit 3, so this
                    // never fires there -- no behaviour change on the working path.
                    if state.phase == 0 && (code & 0x08) == 0 {
                        continue;
                    }
                    let phase = state.phase as usize;
                    state.bytes[phase] = code;
                    state.phase += 1;

                    if state.phase as usize == self.proto.packet_len() {
                        state.phase = 0;
                        let bytes = state.bytes;

                        // A USB tablet already owns the pointer (absolute).
                        // Emitting PS/2 relative packets on top makes the
                        // cursor jump in VirtualBox (`--mouse usbtablet`).
                        if usb_tablet_owns_pointer() {
                            continue;
                        }

                        let pkt = decode_aux_packet(self.proto, &bytes);
                        let changed = pkt.buttons ^ state.last_buttons;
                        state.last_buttons = pkt.buttons;
                        drop(state);

                        if pkt.dx != 0 {
                            self.listener.trigger(InputEvent {
                                event_type: InputEventType::RelAxis,
                                code: rel::REL_X,
                                value: pkt.dx,
                            });
                        }
                        if pkt.dy != 0 {
                            self.listener.trigger(InputEvent {
                                event_type: InputEventType::RelAxis,
                                code: rel::REL_Y,
                                value: pkt.dy,
                            });
                        }
                        // Only the buttons that actually changed, so an idle
                        // hand on a moving mouse does not push six events per
                        // packet through a 64-deep evdev ring.
                        for (bit, code) in [
                            (0u8, key::BTN_LEFT),
                            (1, key::BTN_RIGHT),
                            (2, key::BTN_MIDDLE),
                            (3, key::BTN_SIDE),
                            (4, key::BTN_EXTRA),
                        ] {
                            if changed & (1 << bit) != 0 {
                                self.listener.trigger(InputEvent {
                                    event_type: InputEventType::Key,
                                    code,
                                    value: ((pkt.buttons >> bit) & 1) as i32,
                                });
                            }
                        }
                        // Wheel, low- and high-resolution. Linux has emitted
                        // both since 5.0 and libinput prefers the hi-res axis
                        // when the device advertises it; one detent is 120.
                        if pkt.wheel != 0 {
                            self.listener.trigger(InputEvent {
                                event_type: InputEventType::RelAxis,
                                code: rel::REL_WHEEL,
                                value: pkt.wheel,
                            });
                            self.listener.trigger(InputEvent {
                                event_type: InputEventType::RelAxis,
                                code: rel::REL_WHEEL_HI_RES,
                                value: pkt.wheel * 120,
                            });
                        }
                        if pkt.hwheel != 0 {
                            self.listener.trigger(InputEvent {
                                event_type: InputEventType::RelAxis,
                                code: rel::REL_HWHEEL,
                                value: pkt.hwheel,
                            });
                            self.listener.trigger(InputEvent {
                                event_type: InputEventType::RelAxis,
                                code: rel::REL_HWHEEL_HI_RES,
                                value: pkt.hwheel * 120,
                            });
                        }

                        // Sync
                        self.listener.trigger(InputEvent {
                            event_type: InputEventType::Syn,
                            code: 0,
                            value: 0,
                        });
                    }
                } else {
                    // Handle keyboard data
                    if code == 0xE0 {
                        *self.extended.lock() = true;
                        continue;
                    }

                    let is_extended = {
                        let mut ext = self.extended.lock();
                        let was_ext = *ext;
                        *ext = false;
                        was_ext
                    };

                    let pressed = (code & 0x80) == 0;
                    let scancode = code & 0x7F;

                    let keycode = if is_extended {
                        match scancode {
                            0x48 => 103, // Up
                            0x50 => 108, // Down
                            0x4B => 105, // Left
                            0x4D => 106, // Right
                            0x1D => 97,  // RCtrl
                            0x38 => 100, // RAlt / AltGr
                            0x35 => 98,  // KP_Divide
                            0x1C => 96,  // KP_Enter
                            0x53 => 111, // Delete
                            _ => scancode as u16,
                        }
                    } else {
                        scancode as u16
                    };

                    self.listener.trigger(InputEvent {
                        event_type: InputEventType::Key,
                        code: keycode,
                        value: if pressed { 1 } else { 0 },
                    });

                    self.listener.trigger(InputEvent {
                        event_type: InputEventType::Syn,
                        code: 0,
                        value: 0,
                    });
                }
            }
        }
    }
}

impl InputScheme for Ps2Input {
    fn capability(&self, cap_type: CapabilityType) -> InputCapability {
        let mut cap = InputCapability::empty();
        // VirtualBox ICH9 still exposes i8042 even with `--mouse usbtablet`.
        // If we advertise REL_X/Y + BTN_LEFT, libinput opens a second (silent)
        // relative pointer that fights the USB tablet. Keyboard bits stay.
        let tablet = usb_tablet_owns_pointer();
        let wheel = self.proto.has_wheel();
        match cap_type {
            CapabilityType::Event => {
                cap.set(ev::EV_SYN);
                cap.set(ev::EV_KEY);
                if !tablet {
                    cap.set(ev::EV_REL);
                }
            }
            CapabilityType::Key => {
                let end = if tablet { 0x110 } else { 0x120 };
                for i in 0..end {
                    cap.set(i);
                }
            }
            CapabilityType::RelAxis if !tablet => {
                cap.set(rel::REL_X);
                cap.set(rel::REL_Y);
                // Only when the device answered the IntelliMouse knock. A
                // pointer that advertises REL_WHEEL and never sends one makes
                // libinput treat it as a wheel mouse whose wheel is broken.
                if wheel {
                    cap.set(rel::REL_WHEEL);
                    cap.set(rel::REL_WHEEL_HI_RES);
                }
                if self.proto == Ps2MouseProto::Imex {
                    cap.set(rel::REL_HWHEEL);
                    cap.set(rel::REL_HWHEEL_HI_RES);
                }
            }
            _ => {}
        }
        cap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(bytes: [u8; 4]) -> [u8; 4] {
        bytes
    }

    #[test]
    fn plain_ps2_packet_has_no_wheel() {
        // flags=0x08 (signature only), dx=+5, dy=+3 (PS/2 up-positive).
        let p = decode_aux_packet(Ps2MouseProto::Ps2, &pkt([0x08, 5, 3, 0xff]));
        assert_eq!(p.dx, 5);
        // evdev REL_Y is down-positive, so PS/2 "+3 up" becomes -3.
        assert_eq!(p.dy, -3);
        assert_eq!(p.wheel, 0);
        assert_eq!(p.hwheel, 0);
        // Byte 3 is not part of a 3-byte packet and must not leak in.
        assert_eq!(p.buttons, 0);
    }

    #[test]
    fn negative_deltas_sign_extend() {
        // X_SIGN|Y_SIGN set, magnitudes 0xfb = -5 and 0xfd = -3.
        let p = decode_aux_packet(Ps2MouseProto::Ps2, &pkt([0x38, 0xfb, 0xfd, 0]));
        assert_eq!(p.dx, -5);
        assert_eq!(p.dy, 3);
    }

    #[test]
    fn intellimouse_wheel_matches_linux() {
        // psmouse: REL_WHEEL = -(signed char) packet[3].
        let up = decode_aux_packet(Ps2MouseProto::Imps, &pkt([0x08, 0, 0, 0xff]));
        assert_eq!(up.wheel, 1);
        let down = decode_aux_packet(Ps2MouseProto::Imps, &pkt([0x08, 0, 0, 0x01]));
        assert_eq!(down.wheel, -1);
        assert_eq!(
            decode_aux_packet(Ps2MouseProto::Imps, &pkt([0x08, 0, 0, 0])).wheel,
            0
        );
    }

    #[test]
    fn explorer_wheel_and_extra_buttons() {
        // 4-bit wheel, negated like every other PS/2 wheel field:
        // `-sign_extend32(0x01, 3)` is -1 and `-sign_extend32(0x0f, 3)` is +1.
        let a = decode_aux_packet(Ps2MouseProto::Imex, &pkt([0x08, 0, 0, 0x01]));
        assert_eq!(a.wheel, -1);
        let b = decode_aux_packet(Ps2MouseProto::Imex, &pkt([0x08, 0, 0, 0x0f]));
        assert_eq!(b.wheel, 1);
        // Both protocols agree on direction for the same physical motion:
        // one detent of IntelliMouse z=0x01 is also -1.
        assert_eq!(
            decode_aux_packet(Ps2MouseProto::Imps, &pkt([0x08, 0, 0, 0x01])).wheel,
            a.wheel
        );
        // Buttons 4 and 5 ride in bits 4 and 5 of the same byte.
        let c = decode_aux_packet(Ps2MouseProto::Imex, &pkt([0x08, 0, 0, 0x10]));
        assert_eq!(c.buttons, 1 << 3);
        let d = decode_aux_packet(Ps2MouseProto::Imex, &pkt([0x08, 0, 0, 0x20]));
        assert_eq!(d.buttons, 1 << 4);
    }

    #[test]
    fn explorer_4_tilt_is_horizontal() {
        // 0x40 selects the horizontal axis, 6-bit signed payload in bits 0..5,
        // negated: `-sign_extend32(0x41, 5)` is -1.
        let a = decode_aux_packet(Ps2MouseProto::Imex, &pkt([0x08, 0, 0, 0x40 | 0x01]));
        assert_eq!(a.hwheel, -1);
        assert_eq!(a.wheel, 0);
        let b = decode_aux_packet(Ps2MouseProto::Imex, &pkt([0x08, 0, 0, 0x40 | 0x3f]));
        assert_eq!(b.hwheel, 1);
        // 0x80 is the vertical one, and must not set any extra button even
        // though bits 4 and 5 of the payload are inside its 6-bit field.
        let vert = decode_aux_packet(Ps2MouseProto::Imex, &pkt([0x08, 0, 0, 0x80 | 0x3f]));
        assert_eq!(vert.wheel, 1);
        assert_eq!(vert.buttons, 0);
    }

    #[test]
    fn buttons_come_from_the_flags_byte() {
        let p = decode_aux_packet(Ps2MouseProto::Ps2, &pkt([0x0f, 0, 0, 0]));
        assert_eq!(p.buttons, 0x07);
    }

    #[test]
    fn packet_len_follows_the_protocol() {
        assert_eq!(Ps2MouseProto::Ps2.packet_len(), 3);
        assert_eq!(Ps2MouseProto::Imps.packet_len(), 4);
        assert_eq!(Ps2MouseProto::Imex.packet_len(), 4);
        assert!(!Ps2MouseProto::Ps2.has_wheel());
        assert!(Ps2MouseProto::Imps.has_wheel());
        assert!(Ps2MouseProto::Imex.has_wheel());
    }
}
