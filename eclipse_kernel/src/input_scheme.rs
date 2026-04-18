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
                        // Non-blocking for now, or we could yield
                        return Err(scheme_error::EAGAIN);
                    }
                    buffer[count] = key;
                    count += 1;
                }
                Ok(count)
            }
            InputType::Mouse => {
                // Return 4-byte packed packets: buttons | dx<<8 | dy<<16 | 0<<24
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
