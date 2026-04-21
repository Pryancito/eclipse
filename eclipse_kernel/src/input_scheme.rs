use crate::scheme::{Scheme, error as scheme_error, Stat};
use crate::interrupts;
use alloc::vec::Vec;
use spin::Mutex;

/// Input Scheme implementation (input:)
/// Provides raw keyboard scancodes and PS/2 mouse packets
pub struct InputScheme;

#[derive(Clone, Copy, PartialEq)]
enum InputType {
    Keyboard,
    Mouse,
    EvdevKeyboard,
    EvdevMouse,
}

#[repr(C)]
struct InputEvent {
    time_sec: u64,
    time_usec: u64,
    kind: u16,
    code: u16,
    value: i32,
}

struct InputResource {
    kind: InputType,
}

static OPEN_RESOURCES: Mutex<Vec<Option<InputResource>>> = Mutex::new(Vec::new());

impl InputScheme {
    pub const fn new() -> Self {
        Self
    }
}

impl Scheme for InputScheme {
    fn open(&self, path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        let kind = if path == "keyboard" || path == "/keyboard" {
            InputType::Keyboard
        } else if path == "mouse" || path == "/mouse" {
            InputType::Mouse
        } else if path == "event0" || path == "/event0" {
            InputType::EvdevKeyboard
        } else if path == "event1" || path == "/event1" {
            InputType::EvdevMouse
        } else {
            return Err(scheme_error::ENOENT);
        };

        let resource = InputResource { kind };
        let mut resources = OPEN_RESOURCES.lock();
        for (i, slot) in resources.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(resource);
                return Ok(i);
            }
        }
        let id = resources.len();
        resources.push(Some(resource));
        Ok(id)
    }

    fn read(&self, id: usize, buffer: &mut [u8], _offset: u64) -> Result<usize, usize> {
        let resources = OPEN_RESOURCES.lock();
        let resource = resources.get(id).and_then(|s| s.as_ref()).ok_or(scheme_error::EBADF)?;
        let kind = resource.kind;
        drop(resources); // Unlock before potentially spinning or calling scheduler

        if buffer.is_empty() { return Ok(0); }

        match kind {
            InputType::Keyboard => {
                let mut count = 0;
                while count < buffer.len() {
                    let key = interrupts::read_key();
                    if key == 0 {
                        if count > 0 { break; }
                        return Err(scheme_error::EAGAIN);
                    }
                    buffer[count] = key;
                    count += 1;
                }
                Ok(count)
            }
            InputType::Mouse => {
                let mut count = 0;
                while count + 4 <= buffer.len() {
                    let packet = interrupts::read_mouse_packet();
                    if packet == 0xFFFFFFFF {
                        if count > 0 { break; }
                        return Err(scheme_error::EAGAIN);
                    }
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            &packet as *const u32 as *const u8,
                            buffer.as_mut_ptr().add(count),
                            4
                        );
                    }
                    count += 4;
                }
                Ok(count)
            }
            InputType::EvdevKeyboard => {
                let mut count = 0;
                let event_size = core::mem::size_of::<InputEvent>();
                while count + event_size <= buffer.len() {
                    let key = interrupts::read_key();
                    if key == 0 {
                        if count > 0 { break; }
                        return Err(scheme_error::EAGAIN);
                    }
                    
                    let ticks = interrupts::ticks();
                    let event = InputEvent {
                        time_sec: ticks / 1000,
                        time_usec: (ticks % 1000) * 1000,
                        kind: 1, // EV_KEY
                        code: key as u16,
                        value: 1, // Press (simplified, we don't have release yet in kernel buffer)
                    };
                    
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            &event as *const _ as *const u8,
                            buffer.as_mut_ptr().add(count),
                            event_size
                        );
                    }
                    count += event_size;
                }
                Ok(count)
            }
            InputType::EvdevMouse => {
                let mut count = 0;
                let event_size = core::mem::size_of::<InputEvent>();
                // A single mouse packet can generate multiple evdev events (X, Y, Buttons, SYN)
                while count + event_size * 4 <= buffer.len() {
                    let packet = interrupts::read_mouse_packet();
                    if packet == 0xFFFFFFFF {
                        if count > 0 { break; }
                        return Err(scheme_error::EAGAIN);
                    }
                    
                    let buttons = (packet & 0xFF) as u8;
                    let dx = ((packet >> 8) & 0xFF) as i8;
                    let dy = ((packet >> 16) & 0xFF) as i8;
                    
                    let ticks = interrupts::ticks();
                    let mut events = [
                        InputEvent { time_sec: ticks/1000, time_usec: (ticks%1000)*1000, kind: 2 /* EV_REL */, code: 0 /* REL_X */, value: dx as i32 },
                        InputEvent { time_sec: ticks/1000, time_usec: (ticks%1000)*1000, kind: 2 /* EV_REL */, code: 1 /* REL_Y */, value: -dy as i32 },
                        InputEvent { time_sec: ticks/1000, time_usec: (ticks%1000)*1000, kind: 1 /* EV_KEY */, code: 0x110 /* BTN_LEFT */, value: (buttons & 1) as i32 },
                        InputEvent { time_sec: ticks/1000, time_usec: (ticks%1000)*1000, kind: 0 /* EV_SYN */, code: 0, value: 0 },
                    ];
                    
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            events.as_ptr() as *const u8,
                            buffer.as_mut_ptr().add(count),
                            event_size * 4
                        );
                    }
                    count += event_size * 4;
                }
                Ok(count)
            }
        }
    }

    fn ioctl(&self, _id: usize, request: usize, arg: usize) -> Result<usize, usize> {
        // Minimal evdev ioctls to satisfy libinput
        match request {
            0x80044501 => { // EVIOCGVERSION
                unsafe { *(arg as *mut u32) = 0x010001; }
                Ok(0)
            }
            0x80084502 => { // EVIOCGID
                unsafe { core::ptr::write_bytes(arg as *mut u8, 0, 8); }
                Ok(0)
            }
            _ => {
                // Return success for most BIT queries to let libinput proceed
                if (request >> 8) & 0xFF == 0x45 { // 'E'
                    return Ok(0);
                }
                Err(scheme_error::ENOSYS)
            }
        }
    }

    fn write(&self, _id: usize, _buffer: &[u8], _offset: u64) -> Result<usize, usize> {
        Err(scheme_error::EBADF)
    }

    fn close(&self, id: usize) -> Result<usize, usize> {
        let mut resources = OPEN_RESOURCES.lock();
        if id < resources.len() {
            resources[id] = None;
            Ok(0)
        } else {
            Err(scheme_error::EBADF)
        }
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize, _current_offset: u64) -> Result<usize, usize> {
        Err(scheme_error::ESPIPE)
    }

    fn fstat(&self, id: usize, stat: &mut Stat) -> Result<usize, usize> {
        let resources = OPEN_RESOURCES.lock();
        let _ = resources.get(id).and_then(|s| s.as_ref()).ok_or(scheme_error::EBADF)?;
        stat.mode = 0o444 | 0x2000; // Character device, read-only
        stat.size = 0;
        Ok(0)
    }
}
