use std::vec::Vec;
use std::boxed::Box;
use std::rc::Rc;
use core::cell::RefCell;
use eclipse_syscall::{self, flag};
use heapless::String as HString;

use os_terminal::{ClipboardHandler, DrawTarget, Rgb, Terminal};
use os_terminal::font::BitmapFont;

#[cfg(target_os = "eclipse")]
use libc::{c_int, close, kill, mmap, open};
#[cfg(target_os = "eclipse")]
use eclipse_syscall::call::{
    close as sys_close,
    exit as sys_exit,
    ioctl as sys_ioctl,
    read as sys_read,
    spawn_with_stdio as sys_spawn_with_stdio,
    wait_pid_nohang as sys_wait_pid_nohang,
    write as sys_write,
};

// Wayland Unix socket client
#[cfg(target_os = "eclipse")]
use wayland_proto::unix_transport::UnixSocketConnection;
#[cfg(target_os = "eclipse")]
use wayland_proto::wl::wire::{RawMessage, ObjectId, NewId, Opcode, Payload, PayloadType};
#[cfg(target_os = "eclipse")]
use wayland_proto::wl::connection::Connection;

// ============================================================================
// Tipos y Estructuras
// ============================================================================

#[cfg(target_os = "eclipse")]
struct EclipseClipboard {
    text: std::string::String,
}

#[cfg(target_os = "eclipse")]
impl EclipseClipboard {
    fn new() -> Self {
        Self { text: std::string::String::new() }
    }
}

#[cfg(target_os = "eclipse")]
impl ClipboardHandler for EclipseClipboard {
    fn get_text(&mut self) -> Option<std::string::String> {
        if self.text.is_empty() { None } else { Some(self.text.clone()) }
    }
    fn set_text(&mut self, text: std::string::String) {
        self.text = text;
    }
}

#[cfg(target_os = "eclipse")]
struct SurfaceBacking {
    ptr: *mut u32,
    width: usize,
    height: usize,
    size_bytes: usize,
    shm_fd: i32,
}

#[cfg(target_os = "eclipse")]
struct SharedSurfaceDrawTarget {
    state: Rc<RefCell<SurfaceBacking>>,
}

#[cfg(target_os = "eclipse")]
unsafe impl Send for SharedSurfaceDrawTarget {}

#[cfg(target_os = "eclipse")]
impl DrawTarget for SharedSurfaceDrawTarget {
    fn size(&self) -> (usize, usize) {
        let b = (*self.state).borrow();
        (b.width, b.height)
    }

    fn draw_pixel(&mut self, x: usize, y: usize, rgb: Rgb) {
        let b = (*self.state).borrow();
        if x >= b.width || y >= b.height {
            return;
        }
        let idx = y * b.width + x;
        let pixel = 0xFF00_0000u32
            | ((rgb.0 as u32) << 16)
            | ((rgb.1 as u32) << 8)
            | (rgb.2 as u32);
        unsafe {
            if !b.ptr.is_null() {
                *b.ptr.add(idx) = pixel;
            }
        }
    }
}

// ============================================================================
// Constantes y Helpers
// ============================================================================

const FONT_CHAR_W: u16 = 8;
const FONT_CHAR_H: u16 = 16;

#[repr(C)]
struct WinSize {
    ws_rows:   u16,
    ws_cols:   u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

fn set_pty_winsize(pty_master_fd: usize, rows: u16, cols: u16, xpix: u16, ypix: u16) {
    let ws = WinSize { ws_rows: rows, ws_cols: cols, ws_xpixel: xpix, ws_ypixel: ypix };
    let _ = sys_ioctl(pty_master_fd, 3, &ws as *const WinSize as usize);
}

fn extract_osc_title(buf: &[u8]) -> Option<[u8; 32]> {
    let mut last: Option<[u8; 32]> = None;
    let mut i = 0;
    while i + 3 < buf.len() {
        if buf[i] == b'\x1b' && buf[i + 1] == b']' {
            if matches!(buf[i + 2], b'0' | b'1' | b'2') && buf.get(i + 3) == Some(&b';') {
                let ts = i + 4;
                let mut j = ts;
                while j < buf.len() {
                    let term_bel = buf[j] == b'\x07';
                    let term_st  = buf[j] == b'\x1b' && buf.get(j + 1) == Some(&b'\\');
                    if term_bel || term_st {
                        let len = (j - ts).min(31);
                        let mut t = [0u8; 32];
                        t[..len].copy_from_slice(&buf[ts..ts + len]);
                        last = Some(t);
                        i = j + if term_bel { 1 } else { 2 };
                        break;
                    }
                    j += 1;
                }
                if j == buf.len() { break; }
            } else { i += 1; }
        } else { i += 1; }
    }
    last
}

fn sidewind_shm_name(pid: u32) -> HString<24> {
    let mut s = HString::new();
    let _ = s.push_str("twb_");
    let mut n = pid;
    let mut tmp = [0u8; 10];
    let mut i = 0usize;
    if n == 0 { tmp[0] = b'0'; i = 1; } else {
        while n > 0 && i < tmp.len() {
            tmp[i] = b'0' + (n % 10) as u8;
            n /= 10; i += 1;
        }
    }
    for j in 0..i / 2 { tmp.swap(j, i - 1 - j); }
    let _ = s.push_str(unsafe { core::str::from_utf8_unchecked(&tmp[..i]) });
    s
}

// ============================================================================
// TerminalApp
// ============================================================================

struct TerminalApp {
    /// Wayland Unix socket connection to Lunas.
    wayland: UnixSocketConnection,
    /// Assigned object IDs from the Wayland handshake.
    surface_id: u32,
    buffer_id: u32,
    toplevel_id: u32,
    keyboard_id: u32,
    surface_state: Rc<RefCell<SurfaceBacking>>,
    terminal: Terminal<SharedSurfaceDrawTarget>,
    pty_master_fd: usize,
    pty_pair_id: usize,
    sh_pid: usize,
    sh_bytes: Vec<u8>,
    last_title: [u8; 32],
    /// Serial counter for protocol events.
    serial: u32,
    ctrl_pressed: bool,
}

impl TerminalApp {
    fn new() -> Option<Self> {
        let self_pid = eclipse_syscall::getpid() as u32;
        let win_w = 640u32;
        let win_h = 400u32;
        let size_bytes = (win_w as usize) * (win_h as usize) * 4;

        // ── 1. Allocate shared-memory framebuffer ─────────────────────────
        let shm_name = sidewind_shm_name(self_pid);
        let shm_path = format!("/tmp/{}\0", shm_name.as_str());
        let shm_fd = unsafe {
            open(shm_path.as_ptr() as *const _, (flag::O_RDWR | flag::O_CREAT) as c_int, 0o644)
        };
        if shm_fd < 0 { return None; }
        let _ = eclipse_syscall::ftruncate(shm_fd as usize, size_bytes);
        let vaddr = unsafe {
            mmap(core::ptr::null_mut(), size_bytes,
                 (flag::PROT_READ | flag::PROT_WRITE) as c_int,
                 flag::MAP_SHARED as c_int, shm_fd, 0)
        };
        if vaddr.is_null() || vaddr == libc::MAP_FAILED {
            unsafe { close(shm_fd) };
            return None;
        }

        // Zero out the framebuffer explicitly to clear old content
        unsafe {
            core::ptr::write_bytes(vaddr as *mut u8, 0, size_bytes);
        }

        // ── 2. Connect to Wayland compositor (/tmp/wayland-0) ────────────────
        let wayland = match UnixSocketConnection::connect("/tmp/wayland-0") {
            Some(c) => c,
            None => {
                let msg = b"[TERMINAL] FAILED to connect to /tmp/wayland-0\n";
                unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len()); }
                return None;
            }
        };
        wayland.set_nonblocking();
        let msg = b"[TERMINAL] Connected to /tmp/wayland-0\n";
        unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len()); }

        // ── 3. Wayland handshake ──────────────────────────────────────────
        // Object ID allocation (client-side, starting at 2):
        //  1 = wl_display (built-in)
        //  2 = wl_registry
        //  3 = wl_compositor
        //  4 = wl_shm
        //  5 = xdg_wm_base
        //  6 = wl_seat
        //  7 = wl_surface
        //  8 = wl_shm_pool
        //  9 = wl_buffer
        // 10 = xdg_surface
        // 11 = xdg_toplevel
        // 12 = wl_keyboard

        // wl_display.get_registry(id=2)
        send_wayland(&wayland, 1, 1, &[Payload::NewId(NewId(2))]);

        // Wait for wl_registry.global events and bind the globals we need.
        let mut compositor_name = 0u32;
        let mut shm_name_id = 0u32;
        let mut xdg_name = 0u32;
        let mut seat_name = 0u32;

        // Blocking read with timeout: up to 5000 iterations
        for _ in 0..5000 {
            if let Ok((data, _)) = wayland.recv() {
                let mut pos = 0usize;
                while pos + 8 <= data.len() {
                    if let Ok((sender, opcode, msg_len)) = RawMessage::deserialize_header(&data[pos..]) {
                        let chunk = &data[pos..pos + msg_len.min(data.len() - pos)];
                        // wl_registry.global: sender=2, opcode=0 → (name:uint, interface:string, version:uint)
                        if sender == ObjectId(2) && opcode == Opcode(0) {
                            let pts: &[PayloadType] = &[PayloadType::UInt, PayloadType::String, PayloadType::UInt];
                            if let Ok(raw) = RawMessage::deserialize(chunk, pts, &[]) {
                                let name = match raw.args.get(0) { Some(Payload::UInt(n)) => *n, _ => 0 };
                                let iface = match raw.args.get(1) { Some(Payload::String(s)) => s.as_str(), _ => "" };
                                if iface == "wl_compositor" { compositor_name = name; }
                                else if iface == "wl_shm"   { shm_name_id = name; }
                                else if iface == "xdg_wm_base" { xdg_name = name; }
                                else if iface == "wl_seat"  { seat_name = name; }
                            }
                        }
                        pos += msg_len.min(data.len() - pos);
                    } else { break; }
                }
            }
            // Break as soon as the 3 core globals are received.
            // wl_seat is optional here; we drain for it below.
            if compositor_name != 0 && shm_name_id != 0 && xdg_name != 0 { break; }
            let _ = eclipse_syscall::call::sched_yield();
        }

        // Drain additional events to pick up wl_seat (and wl_output) that may arrive
        // in the same batch as the 3 core globals or one yield later.
        if seat_name == 0 {
            for _ in 0..500 {
                if let Ok((data, _)) = wayland.recv() {
                    let mut pos = 0usize;
                    while pos + 8 <= data.len() {
                        if let Ok((sender, opcode, msg_len)) = RawMessage::deserialize_header(&data[pos..]) {
                            let chunk = &data[pos..pos + msg_len.min(data.len() - pos)];
                            if sender == ObjectId(2) && opcode == Opcode(0) {
                                let pts: &[PayloadType] = &[PayloadType::UInt, PayloadType::String, PayloadType::UInt];
                                if let Ok(raw) = RawMessage::deserialize(chunk, pts, &[]) {
                                    let name = match raw.args.get(0) { Some(Payload::UInt(n)) => *n, _ => 0 };
                                    let iface = match raw.args.get(1) { Some(Payload::String(s)) => s.as_str(), _ => "" };
                                    if iface == "wl_seat" { seat_name = name; }
                                }
                            }
                            pos += msg_len.min(data.len() - pos);
                        } else { break; }
                    }
                }
                if seat_name != 0 { break; }
                let _ = eclipse_syscall::call::sched_yield();
            }
        }

        if compositor_name == 0 {
            let msg = b"[TERMINAL] TIMEOUT: never received wl_registry globals\n";
            unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len()); }
            return None;
        }
        {
            use core::fmt::Write as _;
            let mut buf = heapless::String::<128>::new();
            let _ = write!(buf, "[TERMINAL] Globals: compositor={} shm={} xdg={} seat={}\n",
                compositor_name, shm_name_id, xdg_name, seat_name);
            unsafe { libc::write(2, buf.as_bytes().as_ptr() as *const _, buf.len()); }
        }

        // wl_registry.bind(compositor → id=3)
        send_wayland(&wayland, 2, 0, &[Payload::UInt(compositor_name), Payload::String(std::string::String::from("wl_compositor")), Payload::UInt(4), Payload::NewId(NewId(3))]);
        // wl_registry.bind(shm → id=4)
        send_wayland(&wayland, 2, 0, &[Payload::UInt(shm_name_id), Payload::String(std::string::String::from("wl_shm")), Payload::UInt(1), Payload::NewId(NewId(4))]);
        // wl_registry.bind(xdg_wm_base → id=5)
        send_wayland(&wayland, 2, 0, &[Payload::UInt(xdg_name), Payload::String(std::string::String::from("xdg_wm_base")), Payload::UInt(2), Payload::NewId(NewId(5))]);
        // wl_registry.bind(wl_seat → id=6)
        if seat_name != 0 {
            send_wayland(&wayland, 2, 0, &[Payload::UInt(seat_name), Payload::String(std::string::String::from("wl_seat")), Payload::UInt(7), Payload::NewId(NewId(6))]);
        }

        // wl_compositor.create_surface(id=7)
        send_wayland(&wayland, 3, 0, &[Payload::NewId(NewId(7))]);

        // wl_shm.create_pool(id=8, fd=shm_fd, size=size_bytes)
        // The Wayland wire protocol signature is (new_id<wl_shm_pool>, fd, int).
        // The fd must sit at arg position 1 as Payload::Handle so the server's
        // RawMessage::deserialize finds it there (PAYLOAD_TYPES = [NewId, Handle, Int]).
        // We also pass it as SCM_RIGHTS ancilla for the actual transfer.
        {
            let msg = b"[TERMINAL] Sending create_pool with fd\n";
            unsafe { libc::write(2, msg.as_ptr() as *const _, msg.len()); }
        }
        send_wayland_with_fd(&wayland, 4, 0, &[
            Payload::NewId(NewId(8)),
            Payload::Handle(wayland_proto::wl::wire::Handle(shm_fd)),
            Payload::Int(size_bytes as i32),
        ], shm_fd);
        // Do NOT close shm_fd here: it is stored in SurfaceBacking and must
        // remain open so that the compositor can call fmap() when it processes
        // the create_pool message.  The fd is closed in the resize path (below)
        // before a new fd replaces it, and implicitly on process exit.

        // wl_shm_pool.create_buffer(id=9, offset=0, width, height, stride, format=1=XRGB8888)
        let stride = (win_w * 4) as i32;
        send_wayland(&wayland, 8, 0, &[
            Payload::NewId(NewId(9)),
            Payload::Int(0),
            Payload::Int(win_w as i32), Payload::Int(win_h as i32),
            Payload::Int(stride),
            Payload::UInt(1), // WL_SHM_FORMAT_XRGB8888
        ]);

        // xdg_wm_base.get_xdg_surface(id=10, surface=7)
        send_wayland(&wayland, 5, 1, &[Payload::NewId(NewId(10)), Payload::ObjectId(ObjectId(7))]);

        // xdg_surface.get_toplevel(id=11)
        send_wayland(&wayland, 10, 1, &[Payload::NewId(NewId(11))]);

        // xdg_toplevel.set_title("Terminal")
        send_wayland(&wayland, 11, 2, &[Payload::String(std::string::String::from("Terminal"))]);

        // wl_seat.get_keyboard(id=12)
        if seat_name != 0 {
            send_wayland(&wayland, 6, 1, &[Payload::NewId(NewId(12))]);
        }

        // wl_surface.attach(buffer=9, x=0, y=0) + commit → triggers initial configure
        send_wayland(&wayland, 7, 1, &[Payload::ObjectId(ObjectId(9)), Payload::Int(0), Payload::Int(0)]);
        send_wayland(&wayland, 7, 6, &[]); // wl_surface.commit

        // ── 4. Surface state + os-terminal setup ──────────────────────────
        // Keep shm_fd open so the compositor can still call fmap() when it
        // processes the create_pool message.  On Eclipse OS, closing the fd
        // removes the OPEN_FILES_SCHEME entry, causing fmap() to fail with
        // EBADF when the compositor later calls mmap(MAP_SHARED, received_fd).
        let shared_state = std::rc::Rc::new(core::cell::RefCell::new(SurfaceBacking {
            ptr: vaddr as *mut u32,
            width: win_w as usize,
            height: win_h as usize,
            size_bytes,
            shm_fd,
        }));
        let draw_target = SharedSurfaceDrawTarget { state: shared_state.clone() };
        let mut terminal = Terminal::new(draw_target, Box::new(BitmapFont));
        terminal.set_crnl_mapping(true);
        terminal.set_clipboard(Box::new(EclipseClipboard::new()));
        terminal.set_auto_flush(false);

        // ── 6. PTY & Shell ────────────────────────────────────────────────
        let pty_master_fd = unsafe {
            open(b"pty:master\0".as_ptr() as *const _, (flag::O_RDWR | flag::O_CREAT) as c_int, 0)
        } as usize;
        if pty_master_fd == !0 { return None; }

        let mut pty_pair_id: usize = 0;
        let _ = sys_ioctl(pty_master_fd, 1, &mut pty_pair_id as *mut _ as usize);

        let slave_path = format!("pty:slave/{}\0", pty_pair_id);
        let pty_slave_fd = unsafe { open(slave_path.as_ptr() as *const _, flag::O_RDWR as c_int, 0) } as usize;
        if pty_slave_fd == !0 { return None; }

        let sh_res = std::fs::read("/bin/sh");
        if sh_res.is_err() { return None; }
        let sh_bytes = sh_res.unwrap();

        let sh_spawn = sys_spawn_with_stdio(&sh_bytes, Some("sh"), pty_slave_fd, pty_slave_fd, pty_slave_fd);
        if sh_spawn.is_err() {
            let _ = sys_close(pty_slave_fd);
            return None;
        }
        let sh_pid = sh_spawn.unwrap();
        let _ = sys_close(pty_slave_fd);

        let init_cols = win_w as u16 / FONT_CHAR_W;
        let init_rows = win_h as u16 / FONT_CHAR_H;
        set_pty_winsize(pty_master_fd, init_rows, init_cols, win_w as u16, win_h as u16);

        let pfd = pty_master_fd;
        terminal.set_pty_writer(Box::new(move |s| { let _ = sys_write(pfd, s.as_bytes()); }));

        Some(Self {
            wayland,
            surface_id: 7,
            buffer_id: 9,
            toplevel_id: 11,
            keyboard_id: if seat_name != 0 { 12 } else { 0 },
            surface_state: shared_state.clone(),
            terminal,
            pty_master_fd,
            pty_pair_id,
            sh_pid,
            sh_bytes,
            last_title: [0; 32],
            serial: 1,
            ctrl_pressed: false,
        })
    }

    fn run(&mut self) {
        self.terminal.process(b"\x1b[1;36mEclipse OS Terminal v3\x1b[0m\r\n");
        let mut pty_buf = [0u8; 1024];

        loop {
            let mut dirty = false;

            // ── 1. Receive Wayland events ──────────────────────────────────
            if let Ok((data, _handles)) = self.wayland.recv() {
                let mut pos = 0usize;
                while pos + 8 <= data.len() {
                    match RawMessage::deserialize_header(&data[pos..]) {
                        Ok((sender, opcode, msg_len)) if pos + msg_len <= data.len() => {
                            let chunk = &data[pos..pos + msg_len];

                            // xdg_wm_base.ping(serial) → pong
                            if sender == ObjectId(5) && opcode == Opcode(0) {
                                let pts = &[PayloadType::UInt];
                                if let Ok(raw) = RawMessage::deserialize(chunk, pts, &[]) {
                                    if let Some(Payload::UInt(s)) = raw.args.get(0) {
                                        send_wayland(&self.wayland, 5, 2, &[Payload::UInt(*s)]);
                                    }
                                }
                            }

                            // xdg_surface.configure(serial) → ack_configure + commit
                            if sender == ObjectId(10) && opcode == Opcode(0) {
                                let pts = &[PayloadType::UInt];
                                if let Ok(raw) = RawMessage::deserialize(chunk, pts, &[]) {
                                    if let Some(Payload::UInt(s)) = raw.args.get(0) {
                                        self.serial = *s;
                                        // xdg_surface.ack_configure(serial)
                                        send_wayland(&self.wayland, 10, 4, &[Payload::UInt(self.serial)]);
                                        // wl_surface.attach + commit to show content
                                        send_wayland(&self.wayland, self.surface_id, 1,
                                            &[Payload::ObjectId(ObjectId(self.buffer_id)), Payload::Int(0), Payload::Int(0)]);
                                        send_wayland(&self.wayland, self.surface_id, 6, &[]);
                                        dirty = true;
                                    }
                                }
                            }

                            // xdg_toplevel.configure(w, h, states) → resize if needed
                            if sender == ObjectId(self.toplevel_id) && opcode == Opcode(0) {
                                let pts = &[PayloadType::Int, PayloadType::Int, PayloadType::Array];
                                if let Ok(raw) = RawMessage::deserialize(chunk, pts, &[]) {
                                    let mut w = match raw.args.get(0) { Some(Payload::Int(v)) => *v, _ => 0 };
                                    let mut h = match raw.args.get(1) { Some(Payload::Int(v)) => *v, _ => 0 };
                                    
                                    // Compositor might send 0,0 to mean we should decide
                                    if w == 0 { w = (*self.surface_state).borrow().width as i32; }
                                    if h == 0 { h = (*self.surface_state).borrow().height as i32; }

                                    if w > 0 && h > 0 && (w != (*self.surface_state).borrow().width as i32 || h != (*self.surface_state).borrow().height as i32) {
                                        let cols = w as u16 / FONT_CHAR_W;
                                        let rows = h as u16 / FONT_CHAR_H;
                                        set_pty_winsize(self.pty_master_fd, rows, cols, w as u16, h as u16);
                                        
                                        // Inform shell of the change
                                        let _ = eclipse_syscall::call::kill(self.sh_pid, 28); // SIGWINCH

                                        // Update internal terminal size (recreate because os-terminal 0.7 has no resize)
                                        let draw_target = SharedSurfaceDrawTarget { state: self.surface_state.clone() };
                                        let mut terminal = Terminal::new(draw_target, Box::new(BitmapFont));
                                        terminal.set_crnl_mapping(true);
                                        terminal.set_clipboard(Box::new(EclipseClipboard::new()));
                                        terminal.set_auto_flush(false);
                                        self.terminal = terminal;
                                        let pfd = self.pty_master_fd;
                                        self.terminal.set_pty_writer(Box::new(move |s| { let _ = sys_write(pfd, s.as_bytes()); }));

                                        // Reallocate SHM buffer if needed
                                        let new_size = (w as usize) * (h as usize) * 4;
                                        let mut state = (*self.surface_state).borrow_mut();
                                        
                                        let shm_name = sidewind_shm_name(eclipse_syscall::getpid() as u32);
                                        let shm_path = format!("/tmp/{}\0", shm_name.as_str());
                                        let fd = unsafe { open(shm_path.as_ptr() as *const _, (flag::O_RDWR | flag::O_CREAT) as c_int, 0o644) };
                                        if fd >= 0 {
                                            let _ = eclipse_syscall::ftruncate(fd as usize, new_size);
                                            let vaddr = unsafe { mmap(core::ptr::null_mut(), new_size, (flag::PROT_READ|flag::PROT_WRITE) as c_int, flag::MAP_SHARED as c_int, fd, 0) };
                                            if !vaddr.is_null() && vaddr != libc::MAP_FAILED {
                                                // Close the previous shm_fd now that we have a
                                                // new mapping.  The compositor will process the
                                                // new create_pool with the new fd below; the old
                                                // pool (pool 8) is no longer the active buffer.
                                                if state.shm_fd >= 0 {
                                                    unsafe { close(state.shm_fd) };
                                                }
                                                state.ptr = vaddr as *mut u32;
                                                state.width = w as usize;
                                                state.height = h as usize;
                                                state.size_bytes = new_size;
                                                // Keep fd alive until compositor processes create_pool.
                                                state.shm_fd = fd;
                                                
                                                // Inform compositor of the new buffer
                                                // create_pool (use id 13 for new pool)
                                                send_wayland_with_fd(&self.wayland, 4, 0, &[Payload::NewId(NewId(13)), Payload::Handle(wayland_proto::wl::wire::Handle(fd)), Payload::Int(new_size as i32)], fd);
                                                // create_buffer (use id 14)
                                                let stride = (w * 4) as i32;
                                                send_wayland(&self.wayland, 13, 0, &[Payload::NewId(NewId(14)), Payload::Int(0), Payload::Int(w), Payload::Int(h), Payload::Int(stride), Payload::UInt(1)]);
                                                
                                                self.buffer_id = 14;
                                            } else {
                                                // mmap failed — close fd immediately, nothing to keep.
                                                unsafe { close(fd) };
                                            }
                                        }
                                        dirty = true;
                                    }
                                }
                            }

                            // xdg_toplevel.close → exit
                            if sender == ObjectId(self.toplevel_id) && opcode == Opcode(1) {
                                unsafe { kill(self.sh_pid as c_int, 9) };
                                let _ = sys_close(self.pty_master_fd);
                                sys_exit(0);
                            }

                            // wl_keyboard.key (opcode=3): serial, time, key, state
                            if self.keyboard_id != 0 && sender == ObjectId(self.keyboard_id) && opcode == Opcode(3) {
                                let pts = &[PayloadType::UInt, PayloadType::UInt, PayloadType::UInt, PayloadType::UInt];
                                if let Ok(raw) = RawMessage::deserialize(chunk, pts, &[]) {
                                    let key   = match raw.args.get(2) { Some(Payload::UInt(v)) => *v, _ => 0 };
                                    let state = match raw.args.get(3) { Some(Payload::UInt(v)) => *v, _ => 0 };
                                    // Convert evdev keycode → PS/2 scancode → pc_keyboard state machine
                                    let sc = if key >= 8 { (key - 8) as u8 } else { key as u8 };
                                    let ps2 = if state != 0 { sc } else { sc | 0x80 };

                                    // Handle Ctrl state for signal propagation
                                    if sc == 0x1D { // Left Ctrl
                                        self.ctrl_pressed = state != 0;
                                    }
                                    if self.ctrl_pressed && sc == 0x2E && state != 0 { // Ctrl+C Make
                                        let _ = sys_write(self.pty_master_fd, b"\x03");
                                    }

                                    let _ = self.terminal.handle_keyboard(ps2);
                                    dirty = true;
                                }
                            }

                            // wl_keyboard.keymap (opcode=0): format, fd, size — just close the fd
                            if self.keyboard_id != 0 && sender == ObjectId(self.keyboard_id) && opcode == Opcode(0) {
                                // handles contains the fd if format != 0; we don't need it
                            }

                            pos += msg_len;
                        }
                        _ => break,
                    }
                }
            }

            // ── 2. PTY Output -> Terminal ──────────────────────────────────
            let mut available: usize = 0;
            let _ = sys_ioctl(self.pty_master_fd, 2, &mut available as *mut _ as usize);
            while available > 0 {
                let limit = pty_buf.len();
                let n = sys_read(self.pty_master_fd, &mut pty_buf[..(available.min(limit))]).unwrap_or(0);
                if n == 0 { break; }
                self.terminal.process(&pty_buf[..n]);
                if let Some(title) = extract_osc_title(&pty_buf[..n]) {
                    if title != self.last_title {
                        // xdg_toplevel.set_title — strip NUL padding before sending
                        let end = title.iter().position(|&b| b == 0).unwrap_or(32);
                        let title_str = core::str::from_utf8(&title[..end]).unwrap_or("Terminal").to_string();
                        send_wayland(&self.wayland, self.toplevel_id, 2, &[Payload::String(title_str)]);
                        self.last_title = title;
                    }
                }
                dirty = true;
                available = 0;
                let _ = sys_ioctl(self.pty_master_fd, 2, &mut available as *mut _ as usize);
            }

            // ── 3. Shell Restart ───────────────────────────────────────────
            let mut status = 0u32;
            if sys_wait_pid_nohang(&mut status, self.sh_pid as usize).map_or(false, |p| p != 0) {
                let slave_path = format!("pty:slave/{}\0", self.pty_pair_id);
                let fd = unsafe { open(slave_path.as_ptr() as *const _, flag::O_RDWR as c_int, 0) } as usize;
                if fd != !0 {
                    if let Ok(pid) = sys_spawn_with_stdio(&self.sh_bytes, Some("sh"), fd, fd, fd) {
                        self.sh_pid = pid;
                        self.terminal.process(b"\r\n\x1b[1;33m[shell restarted]\x1b[0m\r\n");
                        dirty = true;
                    }
                    let _ = sys_close(fd);
                }
            }

            if dirty {
                self.terminal.flush();
                // wl_surface.damage(0,0, max,max) + commit
                send_wayland(&self.wayland, self.surface_id, 2,
                    &[Payload::Int(0), Payload::Int(0), Payload::Int(i32::MAX), Payload::Int(i32::MAX)]);
                send_wayland(&self.wayland, self.surface_id, 6, &[]); // commit
            } else {
                let _ = eclipse_syscall::call::sched_yield();
                std::thread::yield_now();
            }
        }
    }
}

/// Send a Wayland message on the Unix socket connection.
#[cfg(target_os = "eclipse")]
fn send_wayland(conn: &UnixSocketConnection, object: u32, opcode: u16, args: &[Payload]) {
    // let _ = SmallVec::<[Payload; 4]>::new(); // removed unused/missing dependency
    let _ = conn.send(ObjectId(object), Opcode(opcode), args, &[]);
}

/// Send a Wayland message with an ancillary file descriptor (SCM_RIGHTS).
#[cfg(target_os = "eclipse")]
fn send_wayland_with_fd(conn: &UnixSocketConnection, object: u32, opcode: u16, args: &[Payload], fd: i32) {
    use wayland_proto::wl::wire::Handle;
    let _ = conn.send(ObjectId(object), Opcode(opcode), args, &[Handle(fd)]);
}

fn main() {
    #[cfg(target_os = "eclipse")]
    {
        if let Some(mut app) = TerminalApp::new() {
            app.run();
        } else {
            std::println!("Failed to initialize TerminalApp.");
        }
    }
}