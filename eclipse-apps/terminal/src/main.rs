#![cfg_attr(target_vendor = "eclipse", no_std)]

#[cfg(target_vendor = "eclipse")]
extern crate alloc;
#[cfg(target_vendor = "eclipse")]
extern crate eclipse_std as std;

#[cfg(target_vendor = "eclipse")]
use alloc::boxed::Box;

use os_terminal::{DrawTarget, Terminal, Rgb};
use os_terminal::font::BitmapFont;
use sidewind::{SideWindSurface, discover_composer};

// Custom DrawTarget that uses the raw pointer from SideWindSurface
// This allows the Terminal to own the drawer while we still have access to the surface.
struct SidewindDrawTarget {
    vaddr: *mut u32,
    width: usize,
    height: usize,
}

impl DrawTarget for SidewindDrawTarget {
    fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn draw_pixel(&mut self, x: usize, y: usize, color: Rgb) {
        if x < self.width && y < self.height {
            let pixel = 0xFF00_0000u32 | ((color.0 as u32) << 16) | ((color.1 as u32) << 8) | (color.2 as u32);
            unsafe {
                core::ptr::write_volatile(self.vaddr.add(y * self.width + x), pixel);
            }
        }
    }
}

// Scancode mapping (limited set for demo)
#[cfg(target_vendor = "eclipse")]
fn scancode_to_char(code: u16) -> Option<char> {
    match code {
        0x1E => Some('a'), 0x30 => Some('b'), 0x2E => Some('c'), 0x20 => Some('d'),
        0x12 => Some('e'), 0x21 => Some('f'), 0x22 => Some('g'), 0x23 => Some('h'),
        0x17 => Some('i'), 0x24 => Some('j'), 0x25 => Some('k'), 0x26 => Some('l'),
        0x32 => Some('m'), 0x31 => Some('n'), 0x18 => Some('o'), 0x19 => Some('p'),
        0x10 => Some('q'), 0x13 => Some('r'), 0x1F => Some('s'), 0x14 => Some('t'),
        0x16 => Some('u'), 0x2F => Some('v'), 0x11 => Some('w'), 0x2D => Some('x'),
        0x15 => Some('y'), 0x2C => Some('z'), 0x1C => Some('\r'), 0x39 => Some(' '),
        0x0E => Some('\x08'), // Backspace
        _ => None,
    }
}

#[cfg(target_vendor = "eclipse")]
fn main() {
    use std::prelude::v1::*;

    // Intentamos descubrir el compositor.
    // Damos prioridad a "lunas" si existe, de lo contrario usamos el display por defecto.
    let mut composer_pid = discover_composer().unwrap_or(0);
    let mut proc_list = [eclipse_syscall::ProcessInfo::default(); 32];
    if let Ok(count) = eclipse_syscall::get_process_list(&mut proc_list) {
        for proc in &proc_list[..count] {
            let name = core::str::from_utf8(&proc.name).unwrap_or("");
            if name.starts_with("lunas") {
                composer_pid = proc.pid as u32;
                break;
            }
        }
    }

    if composer_pid == 0 {
        panic!("No se encontró ningún compositor (Lunas o Smithay) activo.");
    }
    
    let width = 640;
    let height = 480;
    
    let mut surface = SideWindSurface::new(composer_pid, 120, 100, width, height, "Terminal")
        .expect("Failed to create SideWind surface");

    let buffer_ptr = surface.buffer().as_mut_ptr();

    let display = SidewindDrawTarget { 
        vaddr: buffer_ptr, 
        width: width as usize, 
        height: height as usize 
    };
    
    // os-terminal 0.7.3: Terminal::new(display, font_manager)
    let mut terminal = Terminal::new(display, Box::new(BitmapFont));

    // Welcome message via process()
    terminal.process(b"\x1b[1;36mEclipse OS Terminal (os-terminal 0.7.3)\x1b[0m\r\n");
    terminal.process(b"Ready.\r\n\n$ ");

    loop {
        let mut event_received = false;
        while let Some(event) = surface.poll_event() {
            match event.event_type {
                sidewind::SWND_EVENT_TYPE_KEY => {
                    let scancode = event.data1 as u16;
                    if let Some(c) = scancode_to_char(scancode) {
                        terminal.process(&[c as u8]);
                        event_received = true;
                    }
                }
                _ => {}
            }
        }

        // Always commit at the end of the frame if something changed
        // In os-terminal, pixels are drawn eagerly during process().
        surface.commit();
        
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

#[cfg(not(target_vendor = "eclipse"))]
fn main() {
    println!("Terminal app requires Eclipse OS target.");
}
