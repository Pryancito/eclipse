//! TTY (TeleTYpewriter) Scheme
//!
//! Provides a character-based interface for standard I/O,
//! multiplexing keyboard input and serial console output.

use alloc::vec::Vec;
use spin::Mutex;
use crate::scheme::{Scheme, error, Stat};
use crate::interrupts;
use crate::serial;

pub struct TtyScheme {
    /// Internal buffer for character-encoded input
    char_buffer: Mutex<Vec<u8>>,
}

impl TtyScheme {
    pub fn new() -> Self {
        Self {
            char_buffer: Mutex::new(Vec::with_capacity(1024)),
        }
    }

    /// Poll the kernel keyboard buffer and translate to ASCII
    fn poll_keyboard(&self) {
        loop {
            let scancode = interrupts::read_key();
            if scancode == 0 { break; }
            
            if let Some(c) = interrupts::scancode_to_ascii(scancode) {
                let mut buf = self.char_buffer.lock();
                buf.push(c as u8);
                
                // Optional: Basic echo (Standard for TTY canonical mode)
                // In a full implementation, this would be controlled by LFLAG ECHO
                serial::serial_print_char(c);
            }
        }
    }
}

impl Scheme for TtyScheme {
    fn open(&self, _path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        Ok(0) // Single resource for the console TTY
    }

    fn read(&self, _id: usize, buffer: &mut [u8], _offset: u64) -> Result<usize, usize> {
        if buffer.is_empty() { return Ok(0); }

        loop {
            self.poll_keyboard();
            
            let mut buf = self.char_buffer.lock();
            if !buf.is_empty() {
                let to_copy = core::cmp::min(buffer.len(), buf.len());
                for (i, b) in buf.drain(..to_copy).enumerate() {
                    buffer[i] = b;
                }
                return Ok(to_copy);
            }
            drop(buf);

            // Blocking read: yield if no characters available
            crate::scheduler::yield_cpu();
            
            // Check for signals to allow interrupting a blocked read
            if let Some(pid) = crate::process::current_process_id() {
                if let Some(p) = crate::process::get_process(pid) {
                    if p.pending_signals != 0 {
                        return Err(4); // EINTR
                    }
                }
            }
        }
    }

    fn write(&self, _id: usize, buffer: &[u8], _offset: u64) -> Result<usize, usize> {
        if let Ok(s) = core::str::from_utf8(buffer) {
            serial::serial_print(s);
            Ok(buffer.len())
        } else {
            for &b in buffer {
                serial::serial_print_char(b as char);
            }
            Ok(buffer.len())
        }
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize, _current_offset: u64) -> Result<usize, usize> {
        Err(error::ESPIPE) // TTY is not seekable
    }

    fn close(&self, _id: usize) -> Result<usize, usize> {
        Ok(0)
    }

    fn fstat(&self, _id: usize, stat: &mut Stat) -> Result<usize, usize> {
        stat.mode = 0o620 | 0x2000; // Character device
        stat.size = 0;
        Ok(0)
    }

    fn ioctl(&self, _id: usize, request: usize, arg: usize) -> Result<usize, usize> {
        // Basic ioctl support could be added here (e.g. TCGETS, TCSETS)
        match request {
             0x5401 => { // TCGETS
                #[repr(C)]
                struct Termios {
                    c_iflag: u32, c_oflag: u32, c_cflag: u32, c_lflag: u32,
                    c_line: u8, c_cc: [u8; 32], c_ispeed: u32, c_ospeed: u32,
                }
                let t = unsafe { &mut *(arg as *mut Termios) };
                t.c_iflag = 0;
                t.c_oflag = 0x5; // ONLCR | OPOST
                t.c_cflag = 0xBF; // B38400 | CS8 | CREAD
                t.c_lflag = 0x8A03; // ISIG | ICANON | ECHO | IEXTEN
                Ok(0)
             },
             0x5413 => { // TIOCGWINSZ
                #[repr(C)]
                struct WinSize {
                    row: u16, col: u16, xpixel: u16, ypixel: u16,
                }
                let w = unsafe { &mut *(arg as *mut WinSize) };
                w.row = 25;
                w.col = 80;
                Ok(0)
             },
             _ => Err(error::ENOSYS),
        }
    }
}
