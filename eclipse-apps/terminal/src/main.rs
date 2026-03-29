
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

/// DrawTarget implementation for an ARGB8888 shared-memory pixel buffer.
///
/// Each pixel is written as `0x00_RR_GG_BB` (no forced alpha byte) using
/// volatile stores so the compositor can observe every frame update via the
/// shared mapping without the compiler eliding the writes.
struct SidewindDrawTarget {
    vaddr: *mut u32,
    width: usize,
    height: usize,
}

impl DrawTarget for SidewindDrawTarget {
    fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    #[inline(always)]
    fn draw_pixel(&mut self, x: usize, y: usize, color: Rgb) {
        if x < self.width && y < self.height {
            let value = (color.0 as u32) << 16 | (color.1 as u32) << 8 | color.2 as u32;
            unsafe {
                core::ptr::write_volatile(self.vaddr.add(y * self.width + x), value);
            }
        }
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

    // 4. Initialization — build the terminal and configure it for bare-metal use.
    //
    // `set_crnl_mapping(true)` makes the terminal convert incoming \r (Enter key)
    // to \n and outgoing \n to \r\n, just like a real TTY line discipline would.
    // This is necessary because Eclipse OS has no kernel TTY layer.
    //
    // A PTY loopback writer is registered so that keys processed by
    // `handle_keyboard` (which generates ANSI escape sequences) are immediately
    // fed back into `terminal.process()`.  This produces the expected echo
    // behaviour without requiring a real shell back-end.
    let display = SidewindDrawTarget { vaddr: buffer_ptr, width: width as usize, height: height as usize };
    let mut terminal = Terminal::new(display, Box::new(BitmapFont));
    terminal.set_crnl_mapping(true);

    // Static ring-buffer that collects ANSI bytes emitted by handle_keyboard so
    // they can be fed back to the terminal after each event.
    //
    // SAFETY: Eclipse OS runs this terminal as a single-threaded process with no
    // signal handlers or interrupt-driven code that could race on PTY_OUTPUT.
    // Every access is sequenced: the PTY writer closure fills the buffer, and the
    // event loop drains it immediately after handle_keyboard returns.
    static mut PTY_OUTPUT: heapless::Vec<u8, 512> = heapless::Vec::new();
    terminal.set_pty_writer(Box::new(|s: &str| {
        // SAFETY: single-threaded; no concurrent access possible.
        unsafe {
            for b in s.bytes() {
                let _ = PTY_OUTPUT.push(b);
            }
        }
    }));

    terminal.process(b"\x1b[1;32mEclipse OS Terminal (Wayland EDP Optimized)\x1b[0m\r\n\n$ ");

    // Helper to send frame commit (includes direct vaddr for flat-memory compositing)
    let send_commit = |composer_pid: u32, surface_id: u32, pool_id: u32, buf_vaddr: u64| {
        let mut commit = WaylandMsgCommitFrame::default();
        commit.header = WaylandHeader::new(surface_id, 1, 32); // Opcode 1 for Surface: CommitFrame (32-byte)
        commit.pool_id = pool_id;
        commit.offset = 0;
        commit.width = width as u16;
        commit.height = height as u16;
        commit.stride = (width * 4) as u16;
        commit.format = 0; // ARGB8888
        commit.vaddr = buf_vaddr; // direct buffer address for compositor

        let mut buf = [0u8; 36];
        buf[0..4].copy_from_slice(b"WAYL");
        unsafe { core::ptr::write_unaligned(buf[4..36].as_mut_ptr() as *mut WaylandMsgCommitFrame, commit); }
        unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, buf.as_ptr() as *const core::ffi::c_void, 36, 0); }
    };

    // Initial commit
    send_commit(composer_pid, surface_id, pool_id, buffer_ptr as u64);

    loop {
        let mut buffer = [0u8; 128];
        let mut sender: u32 = 0;
        let len = unsafe { receive(buffer.as_mut_ptr(), buffer.len(), &mut sender) };
        if len > 0 && sender == composer_pid {
            if &buffer[0..4] == b"SWND" {
                let event = unsafe { core::ptr::read_unaligned(buffer[4..].as_ptr() as *const sidewind::SideWindEvent) };
                if event.event_type == sidewind::SWND_EVENT_TYPE_KEY {
                    // Pass the raw scancode (Scan Code Set 1) to the terminal's
                    // keyboard handler.  It recognises make and break codes,
                    // modifier keys, and generates the correct ANSI sequences
                    // via the PTY writer registered above.
                    let scancode = event.data1 as u8;
                    let kb_event = terminal.handle_keyboard(scancode);

                    // FontSize events need application-level handling; ignore
                    // others since they are processed internally by the terminal.
                    let _ = kb_event;

                    // Drain the PTY loopback buffer and feed it back into the
                    // terminal so keyboard output (echoed characters, ANSI
                    // cursor moves, etc.) becomes visible immediately.
                    // SAFETY: single-threaded; no concurrent access possible.
                    let pending = unsafe {
                        let copy = PTY_OUTPUT.clone();
                        PTY_OUTPUT.clear();
                        copy
                    };
                    if !pending.is_empty() {
                        terminal.process(&pending);
                        send_commit(composer_pid, surface_id, pool_id, buffer_ptr as u64);
                    }
                }
            }
        }
        unsafe { sleep_ms(16); }
    }
}

#[cfg(not(target_vendor = "eclipse"))]
fn main() { println!("Eclipse OS only."); }
