
#![cfg_attr(target_vendor = "eclipse", no_std)]

#[cfg(target_vendor = "eclipse")]
extern crate alloc;
#[cfg(target_vendor = "eclipse")]
extern crate eclipse_std as std;

#[cfg(target_vendor = "eclipse")]
use alloc::boxed::Box;

use os_terminal::{DrawTarget, Terminal, Rgb};
use os_terminal::font::BitmapFont;
use sidewind::{discover_composer, MSG_TYPE_WAYLAND};
use libc::{eclipse_send as send, receive, receive_fast, sleep_ms, open, mmap, close, PROT_READ, PROT_WRITE, MAP_SHARED, O_RDWR, O_CREAT};

// Custom DrawTarget that draws directly into our SHM region
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

// Scancode mapping (same as before)
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

    // 1. Discover Compositor (prefer Lunas)
    let mut composer_pid = discover_composer().unwrap_or(0);
    let mut proc_list = [eclipse_syscall::ProcessInfo::new(); 32];
    if let Ok(count) = eclipse_syscall::get_process_list(&mut proc_list) {
        println!("[TERM] Scanning {} processes for Lunas...", count);
        for proc in &proc_list[..count] {
            let name_len = proc.name.iter().position(|&b| b == 0).unwrap_or(proc.name.len());
            let name = core::str::from_utf8(&proc.name[..name_len]).unwrap_or("");
            
            // Print all processes so we can see what lunas might be called
            println!("[TERM] PID {}: '{}'", proc.pid, name);
            
            // Case-insensitive contains check for "lunas" or "gui" (fallback for gui_service)
            let mut is_lunas = false;
            let target = "lunas";
            let fallback = "gui";
            
            if name.len() >= target.len() {
                for i in 0..=(name.len() - target.len()) {
                    if name[i..i+target.len()].eq_ignore_ascii_case(target) {
                        is_lunas = true;
                        break;
                    }
                }
            }
            
            if !is_lunas && name.eq_ignore_ascii_case(fallback) {
                is_lunas = true;
            }

            if is_lunas {
                composer_pid = proc.pid as u32;
                println!("[TERM] Selected Lunas as compositor (PID {}, name '{}')", composer_pid, name);
                break;
            }
        }
    }
    if composer_pid == 0 { panic!("No composer found"); }

    // 2. Prepare SHM File
    let width = 640;
    let height = 480;
    let size_bytes = width * height * 4;

    let path = b"/tmp/Terminal\0";
    let fd = unsafe { open(path.as_ptr() as *const core::ffi::c_char, O_RDWR | O_CREAT, 0o666) };
    if fd < 0 { panic!("Failed to create /tmp/Terminal"); }
    let _ = eclipse_syscall::call::ftruncate(fd as usize, size_bytes);
    
    let vaddr = unsafe { mmap(core::ptr::null_mut(), size_bytes, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0) };
    if vaddr.is_null() || vaddr == (-1isize as *mut core::ffi::c_void) { panic!("Failed to mmap"); }
    let buffer_ptr = vaddr as *mut u32;

    // 3. Wayland Handshake (EDP Style - Simplified)
    println!("[TERM] Initializing Wayland session with PID {}...", composer_pid);
    
    use sidewind::wayland::{ID_COMPOSITOR, ID_SHM, WaylandHeader, WaylandMsgCreatePool, WaylandMsgCreateSurface, WaylandMsgCommitFrame};

    // 3.1 Create SHM Pool
    let pool_id = 0x1001u32;
    let mut pool_msg = WaylandMsgCreatePool::default();
    pool_msg.header = WaylandHeader::new(ID_SHM, 1, 20); // Opcode 1: CreatePool
    pool_msg.new_id = pool_id;
    pool_msg.size = size_bytes as u32;

    let mut buf = [0u8; 24];
    buf[0..4].copy_from_slice(b"WAYL");
    unsafe { core::ptr::write_unaligned(buf[4..24].as_mut_ptr() as *mut WaylandMsgCreatePool, pool_msg); }
    unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, buf.as_ptr() as *const core::ffi::c_void, 24, 0); }
    println!("[TERM] Sent CreatePool id={}", pool_id);

    // 3.2 Create Surface
    let surface_id = 0x2001u32;
    let mut surf_msg = WaylandMsgCreateSurface::default();
    surf_msg.header = WaylandHeader::new(ID_COMPOSITOR, 1, 16); // Opcode 1: CreateSurface
    surf_msg.new_id = surface_id;
    surf_msg.width = width as u16;
    surf_msg.height = height as u16;

    let mut buf = [0u8; 20];
    buf[0..4].copy_from_slice(b"WAYL");
    unsafe { core::ptr::write_unaligned(buf[4..20].as_mut_ptr() as *mut WaylandMsgCreateSurface, surf_msg); }
    unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, buf.as_ptr() as *const core::ffi::c_void, 20, 0); }
    println!("[TERM] Sent CreateSurface id={} ({}x{})", surface_id, width, height);

    // 4. Initialization
    let display = SidewindDrawTarget { vaddr: buffer_ptr, width: width as usize, height: height as usize };
    let mut terminal = Terminal::new(display, Box::new(BitmapFont));
    terminal.process(b"\x1b[1;32mEclipse OS Terminal (Wayland EDP Optimized)\x1b[0m\r\n\n$ ");

    // Helper to send frame commit
    let send_commit = |composer_pid: u32, surface_id: u32, pool_id: u32| {
        let mut commit = WaylandMsgCommitFrame::default();
        commit.header = WaylandHeader::new(surface_id, 1, 24); // Opcode 1 for Surface: CommitFrame
        commit.pool_id = pool_id;
        commit.offset = 0;
        commit.width = width as u16;
        commit.height = height as u16;
        commit.stride = (width * 4) as u16;
        commit.format = 0; // ARGB8888

        let mut buf = [0u8; 28];
        buf[0..4].copy_from_slice(b"WAYL");
        unsafe { core::ptr::write_unaligned(buf[4..28].as_mut_ptr() as *mut WaylandMsgCommitFrame, commit); }
        unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, buf.as_ptr() as *const core::ffi::c_void, 28, 0); }
    };

    // Initial commit
    send_commit(composer_pid, surface_id, pool_id);

    loop {
        let mut buffer = [0u8; 128];
        let mut sender: u32 = 0;
        let len = unsafe { receive(buffer.as_mut_ptr(), buffer.len(), &mut sender) };
        if len > 0 && sender == composer_pid {
            if &buffer[0..4] == b"SWND" {
                let event = unsafe { core::ptr::read_unaligned(buffer[4..].as_ptr() as *const sidewind::SideWindEvent) };
                if event.event_type == sidewind::SWND_EVENT_TYPE_KEY {
                    if let Some(c) = scancode_to_char(event.data1 as u16) {
                        terminal.process(&[c as u8]);
                        send_commit(composer_pid, surface_id, pool_id);
                    }
                }
            }
        }
        unsafe { sleep_ms(16); }
    }
}

#[cfg(not(target_vendor = "eclipse"))]
fn main() { println!("Eclipse OS only."); }
