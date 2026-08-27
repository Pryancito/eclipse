use lock::Mutex;
use x86_64::instructions::port::Port;

use crate::prelude::{CapabilityType, InputCapability, InputEvent, InputEventType};
use crate::scheme::{impl_event_scheme, InputScheme, Scheme};
use crate::utils::EventListener;

pub struct Ps2Input {
    listener: EventListener<InputEvent>,
    extended: Mutex<bool>,
    mouse_state: Mutex<MouseState>,
}

#[derive(Default)]
struct MouseState {
    phase: u8,
    bytes: [u8; 3],
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

            // 5. Restore defaults, then enable streaming packet reports.
            let _ = write_aux(&mut data_port, &mut status_port, 0xF6);
            let _ = write_aux(&mut data_port, &mut status_port, 0xF4);
        }

        Self {
            listener: EventListener::new(),
            extended: Mutex::new(false),
            mouse_state: Mutex::new(MouseState::default()),
        }
    }
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

                    if state.phase == 3 {
                        let flags = state.bytes[0];
                        let dx_raw = state.bytes[1];
                        let dy_raw = state.bytes[2];
                        state.phase = 0;

                        // Signature bit (bit 3 of byte 0) is guaranteed set now (the
                        // phase-0 resync above rejects any first byte without it), so
                        // this is belt-and-braces rather than a filter.
                        if (flags & 0x08) != 0 {
                            // Translate relative coordinates
                            let x_neg = (flags & 0x10) != 0;
                            let y_neg = (flags & 0x20) != 0;

                            let dx = if x_neg {
                                (dx_raw as i16 - 256) as i32
                            } else {
                                dx_raw as i32
                            };

                            let dy = if y_neg {
                                (dy_raw as i16 - 256) as i32
                            } else {
                                dy_raw as i32
                            };

                            // RelAxis X/Y
                            self.listener.trigger(InputEvent {
                                event_type: InputEventType::RelAxis,
                                code: 0, // REL_X
                                value: dx,
                            });
                            self.listener.trigger(InputEvent {
                                event_type: InputEventType::RelAxis,
                                code: 1,    // REL_Y
                                value: -dy, // Invert Y delta for standard mouse behavior
                            });

                            // Buttons
                            let left = (flags & 0x01) != 0;
                            let right = (flags & 0x02) != 0;
                            let middle = (flags & 0x04) != 0;

                            self.listener.trigger(InputEvent {
                                event_type: InputEventType::Key,
                                code: 0x110, // BTN_LEFT
                                value: if left { 1 } else { 0 },
                            });
                            self.listener.trigger(InputEvent {
                                event_type: InputEventType::Key,
                                code: 0x111, // BTN_RIGHT
                                value: if right { 1 } else { 0 },
                            });
                            self.listener.trigger(InputEvent {
                                event_type: InputEventType::Key,
                                code: 0x112, // BTN_MIDDLE
                                value: if middle { 1 } else { 0 },
                            });

                            // Sync
                            self.listener.trigger(InputEvent {
                                event_type: InputEventType::Syn,
                                code: 0,
                                value: 0,
                            });
                        }
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
        match cap_type {
            CapabilityType::Event => {
                cap.set(crate::input::input_event_codes::ev::EV_SYN);
                cap.set(crate::input::input_event_codes::ev::EV_KEY);
                cap.set(crate::input::input_event_codes::ev::EV_REL);
            }
            CapabilityType::Key => {
                for i in 0..0x120 {
                    cap.set(i);
                }
            }
            CapabilityType::RelAxis => {
                cap.set(0); // REL_X
                cap.set(1); // REL_Y
            }
            _ => {}
        }
        cap
    }
}
