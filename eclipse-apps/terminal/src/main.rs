
#![cfg_attr(target_vendor = "eclipse", no_std)]

#[cfg(target_vendor = "eclipse")]
extern crate alloc;
#[cfg(target_vendor = "eclipse")]
extern crate eclipse_std as std;

#[cfg(target_vendor = "eclipse")]
use alloc::boxed::Box;

use os_terminal::{DrawTarget, Terminal, Rgb, KeyboardEvent};
use os_terminal::font::{BitmapFont, TrueTypeFont};
use sidewind::{discover_composer, MSG_TYPE_WAYLAND};
use libc::{eclipse_send as send, receive, receive_fast, open, mmap, close, PROT_READ, PROT_WRITE, MAP_SHARED, O_RDWR, O_CREAT};

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
            // Fill alpha channel with 0xFF (fully opaque). 
            // Lunas (the compositor) expects 0xFFRRGGBB for opaque pixels.
            let value = 0xFF000000u32 | (color.0 as u32) << 16 | (color.1 as u32) << 8 | color.2 as u32;
            unsafe {
                core::ptr::write_volatile(self.vaddr.add(y * self.width + x), value);
            }
        }
    }
}

const FONT_DATA: &[u8] = include_bytes!("../../../libcosmic/res/noto/NotoSansMono-Regular.ttf");

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

    // 3.2 Create Surface
    let surface_id = 0x2001u32;
    let mut surf_msg = WaylandMsgCreateSurface::default();
    surf_msg.header = WaylandHeader::new(ID_COMPOSITOR, 1, 16); // Opcode 1: CreateSurface
    surf_msg.new_id = surface_id;
    surf_msg.width = width as u16;
    surf_msg.height = height as u16;

    // Pack both messages into a single IPC send to prevent Lunas from missing the first
    // when reading pending messages.
    let mut buf = [0u8; 40]; // 4 (WAYL) + 20 (CreatePool) + 16 (CreateSurface)
    buf[0..4].copy_from_slice(b"WAYL");
    unsafe {
        core::ptr::write_unaligned(buf[4..24].as_mut_ptr() as *mut WaylandMsgCreatePool, pool_msg);
        core::ptr::write_unaligned(buf[24..40].as_mut_ptr() as *mut WaylandMsgCreateSurface, surf_msg);
        let _ = send(composer_pid, MSG_TYPE_WAYLAND, buf.as_ptr() as *const core::ffi::c_void, 40, 0);
    }
    println!("[TERM] Sent CreatePool id={} and CreateSurface id={}", pool_id, surface_id);

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
    
    // Clear buffer to opaque black so window does not appear transparent-black
    let num_pixels = (width * height) as isize;
    unsafe {
        for i in 0..num_pixels {
            *buffer_ptr.offset(i) = 0xFF000000;
        }
    }
    
    let mut font_size = 14.0f32;
    let mut terminal = Terminal::new(display, Box::new(TrueTypeFont::new(font_size, FONT_DATA)));
    terminal.set_crnl_mapping(true);

    // Set up PTY and launch shell
    let master_fd = eclipse_syscall::call::open("pty:master", libc::O_RDWR as usize).unwrap_or(0);
    let mut pty_num: usize = 0;
    let _ = eclipse_syscall::call::ioctl(master_fd, 1, &mut pty_num as *mut usize as usize);
    let slave_path = alloc::format!("pty:slave/{}\0", pty_num);
    let slave_fd = eclipse_syscall::call::open(&slave_path, libc::O_RDWR as usize).unwrap_or(0);

    let mut cmd = std::process::Command::new("/bin/sh\0");
    if let Ok(_) = cmd.spawn_with_stdio(slave_fd, slave_fd, slave_fd) {
        println!("[TERM] Spawned /bin/sh on pty:slave/{}", pty_num);
    } else {
        println!("[TERM] Failed to spawn /bin/sh!");
    }

    terminal.set_pty_writer(Box::new(move |s: &str| {
        let _ = eclipse_syscall::call::write(master_fd, s.as_bytes());
    }));

    // Helper to send frame commit
    let send_commit = |composer_pid: u32, surface_id: u32, pool_id: u32, buf_vaddr: u64| {
        let mut commit = WaylandMsgCommitFrame::default();
        commit.header = WaylandHeader::new(surface_id, 1, 32); // CommitFrame
        commit.pool_id = pool_id;
        commit.offset = 0;
        commit.width = width as u16;
        commit.height = height as u16;
        commit.stride = (width * 4) as u16;
        commit.format = 0;
        commit.vaddr = buf_vaddr;

        let mut buf = [0u8; 36];
        buf[0..4].copy_from_slice(b"WAYL");
        unsafe { core::ptr::write_unaligned(buf[4..36].as_mut_ptr() as *mut WaylandMsgCommitFrame, commit); }
        unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, buf.as_ptr() as *const core::ffi::c_void, 36, 0); }
    };

    send_commit(composer_pid, surface_id, pool_id, buffer_ptr as u64);

    loop {
        let mut dirty = false;

        // Drain IPC (Wayland events)
        loop {
            let mut buffer = [0u8; 128];
            let mut sender: u32 = 0;
            let len = unsafe { receive(buffer.as_mut_ptr(), buffer.len(), &mut sender) };
            if len == 0 { break; }
            if sender == composer_pid && &buffer[0..4] == b"SWND" {
                let event = unsafe { core::ptr::read_unaligned(buffer[4..].as_ptr() as *const sidewind::SideWindEvent) };
                if event.event_type == sidewind::SWND_EVENT_TYPE_KEY {
                    let scancode = event.data1 as u8;
                    let kb_event = terminal.handle_keyboard(scancode);
                    if let Some(KeyboardEvent::FontSize(delta)) = kb_event {
                        font_size = (font_size + delta as f32).max(6.0).min(72.0);
                        terminal.set_font_manager(Box::new(TrueTypeFont::new(font_size, FONT_DATA)));
                        unsafe { core::ptr::write_bytes(buffer_ptr, 0, size_bytes); }
                        terminal.flush();
                        dirty = true;
                    }
                }
            }
        }

        // Drain PTY Master (messages from shell)
        loop {
            let mut available: usize = 0;
            let res = eclipse_syscall::call::ioctl(master_fd, 2, &mut available as *mut usize as usize);
            if res.is_err() || available == 0 { break; }

            let mut pty_buf = [0u8; 512];
            if let Ok(n) = eclipse_syscall::call::read(master_fd, &mut pty_buf) {
                if n > 0 {
                    terminal.process(&pty_buf[..n]);
                    dirty = true;
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        if dirty {
            send_commit(composer_pid, surface_id, pool_id, buffer_ptr as u64);
        }

        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

#[cfg(not(target_vendor = "eclipse"))]
fn main() { println!("Eclipse OS only."); }
