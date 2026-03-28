
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
use libc::{eclipse_send as send, receive, sleep_ms, open, mmap, close, PROT_READ, PROT_WRITE, MAP_SHARED, O_RDWR, O_CREAT};

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

    // 3. Wayland Handshake
    println!("[TERM] Starting Wayland Handshake with PID {}...", composer_pid);
    
    // get_registry(id=2) from display(id=1)
    let registry_id = 2u32;
    let mut msg = [0u8; 16];
    msg[0..4].copy_from_slice(b"WAYL");
    msg[4..8].copy_from_slice(&1u32.to_le_bytes()); // object_id = 1 (wl_display)
    msg[8..12].copy_from_slice(&((12u32 << 16) | 1u32).to_le_bytes()); // get_registry
    msg[12..16].copy_from_slice(&registry_id.to_le_bytes());
    unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, msg.as_ptr() as *const core::ffi::c_void, 16, 0); }

    let mut compositor_id = 0u32;
    let mut shm_id = 0u32;
    let mut shell_id = 0u32;

    // Wait for globals (Lunas sends all three in one IPC message)
    for _ in 0..100 {
        let mut buffer = [0u8; 256];
        let mut sender: u32 = 0;
        let len = unsafe { receive(buffer.as_mut_ptr(), buffer.len(), &mut sender) };
        if len > 8 && sender == composer_pid && &buffer[0..4] == b"WAYL" {
            let mut offset = 4;
            while offset + 8 <= len {
                let p = &buffer[offset..];
                let obj_id = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
                let size_op = u32::from_le_bytes([p[4], p[5], p[6], p[7]]);
                let size = (size_op >> 16) as usize;
                let opcode = (size_op & 0xFFFF) as u16;

                if obj_id == registry_id && opcode == 0 && offset + 16 <= len {
                    let name = u32::from_le_bytes([p[8], p[9], p[10], p[11]]);
                    let if_len = u32::from_le_bytes([p[12], p[13], p[14], p[15]]) as usize;
                    if offset + 16 + if_len <= len {
                        let interface = unsafe { core::str::from_utf8_unchecked(&p[16..16+if_len-1]) };
                        if interface == "wl_compositor" { compositor_id = name; }
                        if interface == "wl_shm" { shm_id = name; }
                        if interface == "wl_shell" { shell_id = name; }
                    }
                }
                if size == 0 { break; }
                offset += size;
            }
        }
        if compositor_id != 0 && shm_id != 0 && shell_id != 0 { break; }
        unsafe { sleep_ms(10); }
    }

    if compositor_id == 0 || shm_id == 0 || shell_id == 0 { panic!("Failed to find Wayland globals"); }

    // Bindings (Object IDs: Compositor=4, SHM=5, Shell=6)
    let bound_compositor_id = 4u32;
    let bound_shm_id = 5u32;
    let bound_shell_id = 6u32;

    // Bind Compositor
    let mut msg = [0u8; 44];
    msg[0..4].copy_from_slice(b"WAYL");
    msg[4..8].copy_from_slice(&registry_id.to_le_bytes());
    msg[8..12].copy_from_slice(&((44u32 << 16) | 0u32).to_le_bytes()); // bind
    msg[12..16].copy_from_slice(&compositor_id.to_le_bytes());
    let ifname = b"wl_compositor\0";
    msg[16..20].copy_from_slice(&(ifname.len() as u32).to_le_bytes());
    msg[20..34].copy_from_slice(b"wl_compositor\0\0\0");
    msg[36..40].copy_from_slice(&4u32.to_le_bytes()); // version
    msg[40..44].copy_from_slice(&bound_compositor_id.to_le_bytes());
    unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, msg.as_ptr() as *const core::ffi::c_void, 44, 0); }

    // Bind SHM
    let mut msg = [0u8; 36];
    msg[0..4].copy_from_slice(b"WAYL");
    msg[4..8].copy_from_slice(&registry_id.to_le_bytes());
    msg[8..12].copy_from_slice(&((36u32 << 16) | 0u32).to_le_bytes()); 
    msg[12..16].copy_from_slice(&shm_id.to_le_bytes());
    let ifname = b"wl_shm\0";
    msg[16..20].copy_from_slice(&(ifname.len() as u32).to_le_bytes());
    msg[20..27].copy_from_slice(b"wl_shm\0");
    msg[28..32].copy_from_slice(&1u32.to_le_bytes());
    msg[32..36].copy_from_slice(&bound_shm_id.to_le_bytes());
    unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, msg.as_ptr() as *const core::ffi::c_void, 36, 0); }

    // Bind Shell
    let mut msg = [0u8; 40];
    msg[0..4].copy_from_slice(b"WAYL");
    msg[4..8].copy_from_slice(&registry_id.to_le_bytes());
    msg[8..12].copy_from_slice(&((40u32 << 16) | 0u32).to_le_bytes());
    msg[12..16].copy_from_slice(&shell_id.to_le_bytes());
    let ifname = b"wl_shell\0";
    msg[16..20].copy_from_slice(&(ifname.len() as u32).to_le_bytes());
    msg[20..30].copy_from_slice(b"wl_shell\0\0");
    msg[32..36].copy_from_slice(&1u32.to_le_bytes());
    msg[36..40].copy_from_slice(&bound_shell_id.to_le_bytes());
    unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, msg.as_ptr() as *const core::ffi::c_void, 40, 0); }

    // Create Surface (ID 7)
    let surface_id = 7u32;
    let mut msg = [0u8; 16];
    msg[0..4].copy_from_slice(b"WAYL");
    msg[4..8].copy_from_slice(&bound_compositor_id.to_le_bytes());
    msg[8..12].copy_from_slice(&((12u32 << 16) | 0u32).to_le_bytes()); // create_surface
    msg[12..16].copy_from_slice(&surface_id.to_le_bytes());
    unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, msg.as_ptr() as *const core::ffi::c_void, 16, 0); }

    // Create Shell Surface (ID 8)
    let shell_surface_id = 8u32;
    let mut msg = [0u8; 20];
    msg[0..4].copy_from_slice(b"WAYL");
    msg[4..8].copy_from_slice(&bound_shell_id.to_le_bytes());
    msg[8..12].copy_from_slice(&((16u32 << 16) | 0u32).to_le_bytes()); // get_shell_surface
    msg[12..16].copy_from_slice(&shell_surface_id.to_le_bytes());
    msg[16..20].copy_from_slice(&surface_id.to_le_bytes());
    unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, msg.as_ptr() as *const core::ffi::c_void, 20, 0); }

    // Set Toplevel
    let mut msg = [0u8; 12];
    msg[0..4].copy_from_slice(b"WAYL");
    msg[4..8].copy_from_slice(&shell_surface_id.to_le_bytes());
    msg[8..12].copy_from_slice(&((8u32 << 16) | 1u32).to_le_bytes()); 
    unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, msg.as_ptr() as *const core::ffi::c_void, 12, 0); }

    // Create SHM Pool (ID 9)
    let pool_id = 9u32;
    let mut msg = [0u8; 20];
    msg[0..4].copy_from_slice(b"WAYL");
    msg[4..8].copy_from_slice(&bound_shm_id.to_le_bytes());
    msg[8..12].copy_from_slice(&((16u32 << 16) | 0u32).to_le_bytes()); // create_pool
    msg[12..16].copy_from_slice(&pool_id.to_le_bytes());
    msg[16..20].copy_from_slice(&(fd as u32).to_le_bytes()); // Pass target_fd (handle)
    msg[20..24].copy_from_slice(&(size_bytes as i32).to_le_bytes());
    unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, msg.as_ptr() as *const core::ffi::c_void, 24, 0); }

    // Create Buffer (ID 10)
    let buffer_id = 10u32;
    let mut msg = [0u8; 32];
    msg[0..4].copy_from_slice(b"WAYL");
    msg[4..8].copy_from_slice(&pool_id.to_le_bytes());
    msg[8..12].copy_from_slice(&((28u32 << 16) | 0u32).to_le_bytes()); // create_buffer
    msg[12..16].copy_from_slice(&buffer_id.to_le_bytes());
    msg[16..20].copy_from_slice(&0u32.to_le_bytes()); // offset
    msg[20..24].copy_from_slice(&(width as i32).to_le_bytes());
    msg[24..28].copy_from_slice(&(height as i32).to_le_bytes());
    msg[28..32].copy_from_slice(&(width as i32 * 4).to_le_bytes()); // stride
    msg[32..36].copy_from_slice(&0u32.to_le_bytes()); // format (0=ARGB)
    unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, msg.as_ptr() as *const core::ffi::c_void, 36, 0); }

    // 4. Initialization
    let display = SidewindDrawTarget { vaddr: buffer_ptr, width: width as usize, height: height as usize };
    let mut terminal = Terminal::new(display, Box::new(BitmapFont));
    terminal.process(b"\x1b[1;32mEclipse OS Terminal (Wayland Enhanced)\x1b[0m\r\n\n$ ");

    // Initial attach & commit
    let mut msg = [0u8; 20];
    msg[0..4].copy_from_slice(b"WAYL");
    msg[4..8].copy_from_slice(&surface_id.to_le_bytes());
    msg[8..12].copy_from_slice(&((16u32 << 16) | 1u32).to_le_bytes()); // attach
    msg[12..16].copy_from_slice(&buffer_id.to_le_bytes());
    msg[16..20].copy_from_slice(&0u32.to_le_bytes()); // x
    msg[20..24].copy_from_slice(&0u32.to_le_bytes()); // y
    unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, msg.as_ptr() as *const core::ffi::c_void, 24, 0); }

    let mut msg = [0u8; 8];
    msg[0..4].copy_from_slice(b"WAYL");
    msg[4..8].copy_from_slice(&surface_id.to_le_bytes());
    msg[8..12].copy_from_slice(&((8u32 << 16) | 6u32).to_le_bytes()); // commit
    unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, msg.as_ptr() as *const core::ffi::c_void, 12, 0); }

    loop {
        let mut buffer = [0u8; 128];
        let mut sender: u32 = 0;
        let len = unsafe { receive(buffer.as_mut_ptr(), buffer.len(), &mut sender) };
        if len > 0 && sender == composer_pid {
            // Check for SideWind input events (Lunas might still send these) or Wayland events
            if &buffer[0..4] == b"SWND" {
                let event = unsafe { core::ptr::read_unaligned(buffer[4..].as_ptr() as *const sidewind::SideWindEvent) };
                if event.event_type == sidewind::SWND_EVENT_TYPE_KEY {
                    if let Some(c) = scancode_to_char(event.data1 as u16) {
                        terminal.process(&[c as u8]);
                        // Signal change (commit)
                        let mut msg = [0u8; 8];
                        msg[0..4].copy_from_slice(b"WAYL");
                        msg[4..8].copy_from_slice(&surface_id.to_le_bytes());
                        msg[8..12].copy_from_slice(&((8u32 << 16) | 6u32).to_le_bytes()); // commit
                        unsafe { let _ = send(composer_pid, MSG_TYPE_WAYLAND, msg.as_ptr() as *const core::ffi::c_void, 12, 0); }
                    }
                }
            }
        }
        unsafe { sleep_ms(16); }
    }
}

#[cfg(not(target_vendor = "eclipse"))]
fn main() { println!("Eclipse OS only."); }
