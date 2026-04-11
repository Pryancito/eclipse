use std::vec::Vec;
use std::boxed::Box;
use std::rc::Rc;
use core::cell::RefCell;
use eclipse_syscall::{self, flag};
use heapless::String as HString;

use os_terminal::{ClipboardHandler, DrawTarget, MouseButton, MouseInput, Rgb, Terminal};
use os_terminal::font::BitmapFont;

#[cfg(target_os = "eclipse")]
use libc::{c_int, close, kill, mmap, open};
#[cfg(target_os = "eclipse")]
use eclipse_syscall::call::{
    close as sys_close,
    exit as sys_exit,
    ioctl as sys_ioctl,
    read as sys_read,
    set_child_args as sys_set_child_args,
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
// Focus-event mode tracker
// ============================================================================

/// Watches raw PTY output bytes for `\x1b[?1004h` (enable focus events) and
/// `\x1b[?1004l` (disable).  xterm only forwards focus-in/out events to the
/// running application when focus-event mode is enabled.
#[cfg(target_os = "eclipse")]
struct FocusModeTracker {
    state:   u8,
    enabled: bool,
}

#[cfg(target_os = "eclipse")]
impl FocusModeTracker {
    const fn new() -> Self { Self { state: 0, enabled: false } }

    #[inline]
    fn feed(&mut self, data: &[u8]) {
        for &b in data { self.step(b); }
    }

    fn step(&mut self, b: u8) {
        // Sequence: ESC [ ? 1 0 0 4 h/l
        self.state = match (self.state, b) {
            (_, b'\x1b') => 1,
            (1, b'[')    => 2,
            (2, b'?')    => 3,
            (3, b'1')    => 4,
            (4, b'0')    => 5,
            (5, b'0')    => 6,
            (6, b'4')    => 7,
            (7, b'h')    => { self.enabled = true;  0 }
            (7, b'l')    => { self.enabled = false; 0 }
            _            => if b == b'\x1b' { 1 } else { 0 },
        };
    }
}

// PS/2 Set-1 scancodes that are pure modifier keys and produce no character
// output from pc_keyboard.  Defined at module level to avoid re-creating the
// slice on every keyboard event.
#[cfg(target_os = "eclipse")]
const MODIFIER_SC: &[u8] = &[
    0x1D, 0x61, // L-Ctrl, R-Ctrl
    0x2A, 0x36, // L-Shift, R-Shift
    0x38, 0x64, // L-Alt, R-Alt
    0x5B, 0x5C, 0x5D, // L-Win, R-Win, Menu
    0x3A, 0x45, 0x46, // CapsLk, NumLk, ScrLk
];

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
    pointer_id: u32,
    /// Next available Wayland object ID for dynamic allocation.
    next_obj_id: u32,
    /// Current pointer position in surface coordinates.
    pointer_x: f32,
    pointer_y: f32,
    surface_state: Rc<RefCell<SurfaceBacking>>,
    terminal: Terminal<SharedSurfaceDrawTarget>,
    pty_master_fd: usize,
    pty_pair_id: usize,
    sh_pid: usize,
    sh_bytes: Vec<u8>,
    /// argv[0] (name) to pass when restarting the shell/command.
    sh_name: Vec<u8>,
    /// Full argv (NUL-separated) to set on child restart via set_child_args.
    sh_argv: Vec<u8>,
    /// When true, do not auto-restart the child after it exits.
    no_restart: bool,
    last_title: [u8; 32],
    /// Serial counter for protocol events.
    serial: u32,
    /// Whether Left-Alt or Right-Alt is currently held down.
    /// Used to prefix key sequences with ESC (xterm Meta mode).
    alt_pressed: bool,
    /// Whether any Shift key is currently held down.
    shift_pressed: bool,
    /// Whether any Ctrl key is currently held down.
    ctrl_pressed: bool,
    /// Tracks CSI ?1004 h/l in PTY output to gate focus-event forwarding.
    focus_tracker: FocusModeTracker,
}

impl TerminalApp {
    fn new() -> Option<Self> {
        // ── 0. Parse own argv ─────────────────────────────────────────────
        // Supported forms:
        //   terminal                        → launch default shell (rust-shell or sh)
        //   terminal -e <prog> [args...]    → launch <prog> with optional args
        //   terminal -- <prog> [args...]    → same, alternate separator
        let own_argv = {
            let mut buf = [0u8; 4096];
            let n = eclipse_syscall::call::get_process_args(&mut buf);
            buf[..n].split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| std::string::String::from(core::str::from_utf8(s).unwrap_or("")))
                .collect::<Vec<std::string::String>>()
        };

        // Determine shell/program to run.
        // custom_cmd: (program_path, name_for_kernel, full_argv_nul_list, no_restart)
        let (exec_path, exec_name, exec_argv_bytes, no_restart): (std::string::String, std::string::String, Vec<u8>, bool) = {
            let mut custom: Option<(Vec<std::string::String>, bool)> = None;
            let mut i = 1;
            while i < own_argv.len() {
                match own_argv[i].as_str() {
                    "-e" | "--" => {
                        if i + 1 < own_argv.len() {
                            // Collect args after -e/-- as the command argv
                            custom = Some((own_argv[i + 1..].to_vec(), true));
                        }
                        break;
                    }
                    _ => {}
                }
                i += 1;
            }

            if let Some((cmd_args, one_shot)) = custom {
                // cmd_args[0] is the program, cmd_args[1..] are its arguments.
                let prog = cmd_args.first().map(|s| s.as_str()).unwrap_or("");
                // Resolve the executable path using PATH search
                let prog_path = resolve_exec_path(prog);
                // Extract basename for the kernel process name
                let name = prog.rsplit('/').next().unwrap_or(prog);
                let mut argv_bytes = Vec::<u8>::new();
                for arg in &cmd_args {
                    argv_bytes.extend_from_slice(arg.as_bytes());
                    argv_bytes.push(0);
                }
                (prog_path, name.to_string(), argv_bytes, one_shot)
            } else {
                // Default: prefer rust-shell, fallback to sh
                let default_shell = if std::fs::metadata("/bin/rust-shell").is_ok() {
                    "/bin/rust-shell"
                } else {
                    "/bin/sh"
                };
                let shell_name = default_shell.rsplit('/').next().unwrap_or("sh").to_string();
                let mut argv_bytes = Vec::<u8>::new();
                argv_bytes.extend_from_slice(shell_name.as_bytes());
                argv_bytes.push(0);
                (default_shell.to_string(), shell_name, argv_bytes, false)
            }
        };

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

        // xdg_toplevel.set_app_id("terminal")  — lets WMs apply per-class rules
        send_wayland(&wayland, 11, 3, &[Payload::String(std::string::String::from("terminal"))]);

        // wl_seat.get_keyboard(id=12)
        if seat_name != 0 {
            send_wayland(&wayland, 6, 1, &[Payload::NewId(NewId(12))]);
        }

        // wl_seat.get_pointer(id=13)
        if seat_name != 0 {
            send_wayland(&wayland, 6, 0, &[Payload::NewId(NewId(13))]);
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
        terminal.set_history_size(1000);
        terminal.set_scroll_speed(3);
        // Acknowledge BEL without letting an unhandled character bleed through
        terminal.set_bell_handler(Box::new(|| {}));

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

        let sh_res = std::fs::read(&exec_path);
        if sh_res.is_err() { return None; }
        let sh_bytes = sh_res.unwrap();

        // Build kernel name (≤15 bytes) from exec_name
        let mut name_buf = [0u8; 16];
        let copy_len = exec_name.len().min(15);
        name_buf[..copy_len].copy_from_slice(&exec_name.as_bytes()[..copy_len]);
        let kernel_name = core::str::from_utf8(&name_buf[..copy_len]).unwrap_or("sh");

        let sh_spawn = sys_spawn_with_stdio(&sh_bytes, Some(kernel_name), pty_slave_fd, pty_slave_fd, pty_slave_fd);
        if let Ok(pid) = sh_spawn {
            let _ = sys_set_child_args(pid, &exec_argv_bytes);
        } else {
            let _ = sys_close(pty_slave_fd);
            return None;
        }
        let sh_pid = sh_spawn.unwrap();
        let _ = sys_close(pty_slave_fd);

        // Build sh_name for restart (NUL-terminated kernel name)
        let mut sh_name = Vec::<u8>::new();
        sh_name.extend_from_slice(kernel_name.as_bytes());
        sh_name.push(0);

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
            pointer_id: if seat_name != 0 { 13 } else { 0 },
            next_obj_id: 14,
            pointer_x: 0.0,
            pointer_y: 0.0,
            surface_state: shared_state.clone(),
            terminal,
            pty_master_fd,
            pty_pair_id,
            sh_pid,
            sh_bytes,
            sh_name,
            sh_argv: exec_argv_bytes,
            no_restart,
            last_title: [0; 32],
            serial: 1,
            alt_pressed: false,
            shift_pressed: false,
            ctrl_pressed: false,
            focus_tracker: FocusModeTracker::new(),
        })
    }

    fn run(&mut self) {
        self.terminal.process(b"\x1b[1;36mEclipse OS Terminal v3\x1b[0m\r\n");
        let mut pty_buf = [0u8; 4096];

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
                                        terminal.set_history_size(1000);
                                        terminal.set_scroll_speed(3);
                                        terminal.set_bell_handler(Box::new(|| {}));
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
                                                // Allocate fresh IDs for pool and buffer to avoid
                                                // reusing already-registered IDs (pointer=13, etc.)
                                                let pool_id  = self.next_obj_id;
                                                let buf_id   = self.next_obj_id + 1;
                                                self.next_obj_id += 2;
                                                send_wayland_with_fd(&self.wayland, 4, 0, &[Payload::NewId(NewId(pool_id)), Payload::Handle(wayland_proto::wl::wire::Handle(fd)), Payload::Int(new_size as i32)], fd);
                                                // create_buffer
                                                let stride = (w * 4) as i32;
                                                send_wayland(&self.wayland, pool_id, 0, &[Payload::NewId(NewId(buf_id)), Payload::Int(0), Payload::Int(w), Payload::Int(h), Payload::Int(stride), Payload::UInt(1)]);
                                                
                                                self.buffer_id = buf_id;
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
                                    // Wayland sends XKB keycodes (= evdev + 8).
                                    // For standard non-extended keys, evdev == PS/2 Set-1 make code,
                                    // so sc == PS/2 and handle_keyboard() works correctly.
                                    // Navigation keys (evdev ≥ 102) don't share PS/2 codes and are
                                    // intercepted below with proper ANSI escape sequences.
                                    let sc  = if key >= 8 { (key - 8) as u8 } else { key as u8 };
                                    let ps2 = if state != 0 { sc } else { sc | 0x80 };

                                    // ── Modifier tracking (fallback; authoritative state from opcode=4) ─
                                    if sc == 0x38 || sc == 0x64 { self.alt_pressed  = state != 0; }
                                    if sc == 0x2A || sc == 0x36 { self.shift_pressed = state != 0; }
                                    if sc == 0x1D || sc == 0x61 { self.ctrl_pressed  = state != 0; }

                                    // ── Navigation / extended-key intercept ─────────────────────────
                                    // Navigation keys have evdev codes that don't map to valid PS/2
                                    // Set-1 codes via the XKB-8 formula.  We handle them here and
                                    // emit the correct xterm ANSI escape sequences directly to the PTY.
                                    let mut handled = false;
                                    if state != 0 && !MODIFIER_SC.contains(&sc) {
                                        let (sh, al, ct) = (self.shift_pressed, self.alt_pressed, self.ctrl_pressed);
                                        match sc {
                                            // ── Cursor keys (evdev 103-106 → sc 0x67/0x6C/0x6A/0x69) ──
                                            0x67 => { write_key_seq(self.pty_master_fd, b'A', false, sh, al, ct); handled = true; } // Up
                                            0x6C => { write_key_seq(self.pty_master_fd, b'B', false, sh, al, ct); handled = true; } // Down
                                            0x6A => { write_key_seq(self.pty_master_fd, b'C', false, sh, al, ct); handled = true; } // Right
                                            0x69 => { write_key_seq(self.pty_master_fd, b'D', false, sh, al, ct); handled = true; } // Left

                                            // ── Page scroll / navigation ─────────────────────────────
                                            0x68 => { // PageUp (evdev 104)
                                                if sh {
                                                    for _ in 0..8 { self.terminal.handle_mouse(MouseInput::Scroll(-1)); }
                                                } else {
                                                    write_key_seq(self.pty_master_fd, b'5', true, false, al, ct);
                                                }
                                                handled = true;
                                            }
                                            0x6D => { // PageDown (evdev 109)
                                                if sh {
                                                    for _ in 0..8 { self.terminal.handle_mouse(MouseInput::Scroll(1)); }
                                                } else {
                                                    write_key_seq(self.pty_master_fd, b'6', true, false, al, ct);
                                                }
                                                handled = true;
                                            }
                                            0x66 => { write_key_seq(self.pty_master_fd, b'H', false, sh, al, ct); handled = true; } // Home
                                            0x6B => { write_key_seq(self.pty_master_fd, b'F', false, sh, al, ct); handled = true; } // End
                                            0x6E => { write_key_seq(self.pty_master_fd, b'2', true,  sh, al, ct); handled = true; } // Insert
                                            0x6F => { write_key_seq(self.pty_master_fd, b'3', true,  sh, al, ct); handled = true; } // Delete

                                            // ── Function keys F1–F12 with modifier support ──────────
                                            0x3B => { write_fn_key(self.pty_master_fd,  1, sh, al, ct); handled = true; }
                                            0x3C => { write_fn_key(self.pty_master_fd,  2, sh, al, ct); handled = true; }
                                            0x3D => { write_fn_key(self.pty_master_fd,  3, sh, al, ct); handled = true; }
                                            0x3E => { write_fn_key(self.pty_master_fd,  4, sh, al, ct); handled = true; }
                                            0x3F => { write_fn_key(self.pty_master_fd,  5, sh, al, ct); handled = true; }
                                            0x40 => { write_fn_key(self.pty_master_fd,  6, sh, al, ct); handled = true; }
                                            0x41 => { write_fn_key(self.pty_master_fd,  7, sh, al, ct); handled = true; }
                                            0x42 => { write_fn_key(self.pty_master_fd,  8, sh, al, ct); handled = true; }
                                            0x43 => { write_fn_key(self.pty_master_fd,  9, sh, al, ct); handled = true; }
                                            0x44 => { write_fn_key(self.pty_master_fd, 10, sh, al, ct); handled = true; }
                                            0x57 => { write_fn_key(self.pty_master_fd, 11, sh, al, ct); handled = true; }
                                            0x58 => { write_fn_key(self.pty_master_fd, 12, sh, al, ct); handled = true; }

                                            _ => {}
                                        }
                                    }

                                    if !handled {
                                        // ── ESC prefix for Alt+key (xterm Meta mode) ────────────────
                                        if self.alt_pressed && state != 0 && !MODIFIER_SC.contains(&sc) {
                                            let _ = sys_write(self.pty_master_fd, b"\x1b");
                                        }
                                        // Feed PS/2 scancode to os-terminal; it handles Ctrl+letter,
                                        // Shift+letter, Ctrl+Shift, and clipboard via the pty_writer.
                                        let _ = self.terminal.handle_keyboard(ps2);
                                    }
                                    dirty = true;
                                }
                            }

                            // wl_keyboard.modifiers (opcode=4): serial, mods_depressed, mods_latched,
                            // mods_locked, group — authoritative XKB modifier state.
                            // Standard XKB bits: bit0=Shift, bit2=Control, bit3=Mod1(Alt).
                            if self.keyboard_id != 0 && sender == ObjectId(self.keyboard_id) && opcode == Opcode(4) {
                                let pts = &[PayloadType::UInt, PayloadType::UInt, PayloadType::UInt, PayloadType::UInt, PayloadType::UInt];
                                if let Ok(raw) = RawMessage::deserialize(chunk, pts, &[]) {
                                    let mods = match raw.args.get(1) { Some(Payload::UInt(v)) => *v, _ => 0 };
                                    self.shift_pressed = (mods & 0x01) != 0;
                                    self.ctrl_pressed  = (mods & 0x04) != 0;
                                    self.alt_pressed   = (mods & 0x08) != 0;
                                }
                            }

                            // wl_keyboard.enter (opcode=1) — keyboard focus gained
                            if self.keyboard_id != 0 && sender == ObjectId(self.keyboard_id) && opcode == Opcode(1) {
                                if self.focus_tracker.enabled {
                                    let _ = sys_write(self.pty_master_fd, b"\x1b[I");
                                }
                            }

                            // wl_keyboard.leave (opcode=2) — keyboard focus lost; clear all modifier state
                            if self.keyboard_id != 0 && sender == ObjectId(self.keyboard_id) && opcode == Opcode(2) {
                                self.alt_pressed   = false;
                                self.shift_pressed = false;
                                self.ctrl_pressed  = false;
                                if self.focus_tracker.enabled {
                                    let _ = sys_write(self.pty_master_fd, b"\x1b[O");
                                }
                            }

                            // wl_keyboard.keymap (opcode=0): format, fd, size — just close the fd
                            if self.keyboard_id != 0 && sender == ObjectId(self.keyboard_id) && opcode == Opcode(0) {
                                // handles contains the fd if format != 0; we don't need it
                            }

                            // wl_pointer.enter (opcode=0): serial, surface, surface_x (fixed), surface_y (fixed)
                            if self.pointer_id != 0 && sender == ObjectId(self.pointer_id) && opcode == Opcode(0) {
                                let pts = &[PayloadType::UInt, PayloadType::ObjectId, PayloadType::Fixed, PayloadType::Fixed];
                                if let Ok(raw) = RawMessage::deserialize(chunk, pts, &[]) {
                                    self.pointer_x = match raw.args.get(2) { Some(Payload::Fixed(v)) => *v, _ => 0.0 };
                                    self.pointer_y = match raw.args.get(3) { Some(Payload::Fixed(v)) => *v, _ => 0.0 };
                                }
                            }

                            // wl_pointer.motion (opcode=2): time, surface_x (fixed), surface_y (fixed)
                            if self.pointer_id != 0 && sender == ObjectId(self.pointer_id) && opcode == Opcode(2) {
                                let pts = &[PayloadType::UInt, PayloadType::Fixed, PayloadType::Fixed];
                                if let Ok(raw) = RawMessage::deserialize(chunk, pts, &[]) {
                                    self.pointer_x = match raw.args.get(1) { Some(Payload::Fixed(v)) => *v, _ => self.pointer_x };
                                    self.pointer_y = match raw.args.get(2) { Some(Payload::Fixed(v)) => *v, _ => self.pointer_y };
                                    let col = (self.pointer_x as usize) / (FONT_CHAR_W as usize);
                                    let row = (self.pointer_y as usize) / (FONT_CHAR_H as usize);
                                    self.terminal.handle_mouse(MouseInput::Move(col, row));
                                    dirty = true;
                                }
                            }

                            // wl_pointer.button (opcode=3): serial, time, button, state
                            // Linux button codes: 0x110=BTN_LEFT, 0x111=BTN_RIGHT, 0x112=BTN_MIDDLE
                            if self.pointer_id != 0 && sender == ObjectId(self.pointer_id) && opcode == Opcode(3) {
                                let pts = &[PayloadType::UInt, PayloadType::UInt, PayloadType::UInt, PayloadType::UInt];
                                if let Ok(raw) = RawMessage::deserialize(chunk, pts, &[]) {
                                    let button = match raw.args.get(2) { Some(Payload::UInt(v)) => *v, _ => 0 };
                                    let state  = match raw.args.get(3) { Some(Payload::UInt(v)) => *v, _ => 0 };
                                    let mb = match button {
                                        0x110 => Some(MouseButton::Left),
                                        0x111 => Some(MouseButton::Right),
                                        0x112 => Some(MouseButton::Middle),
                                        _     => None,
                                    };
                                    if let Some(btn) = mb {
                                        let input = if state != 0 {
                                            MouseInput::Pressed(btn)
                                        } else {
                                            MouseInput::Released(btn)
                                        };
                                        self.terminal.handle_mouse(input);
                                        dirty = true;
                                    }
                                }
                            }

                            // wl_pointer.axis (opcode=4): time, axis, value (fixed)
                            // axis=0 → vertical scroll; value > 0 → scroll down, < 0 → scroll up
                            if self.pointer_id != 0 && sender == ObjectId(self.pointer_id) && opcode == Opcode(4) {
                                let pts = &[PayloadType::UInt, PayloadType::UInt, PayloadType::Fixed];
                                if let Ok(raw) = RawMessage::deserialize(chunk, pts, &[]) {
                                    let axis  = match raw.args.get(1) { Some(Payload::UInt(v))  => *v, _ => 0 };
                                    let value = match raw.args.get(2) { Some(Payload::Fixed(v)) => *v, _ => 0.0 };
                                    if axis == 0 && value != 0.0 {
                                        // Normalize to ±1 lines per wheel click
                                        let direction: isize = if value > 0.0 { 1 } else { -1 };
                                        self.terminal.handle_mouse(MouseInput::Scroll(direction));
                                        dirty = true;
                                    }
                                }
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
                // Track focus-event mode (CSI ?1004 h/l) in the raw output stream.
                self.focus_tracker.feed(&pty_buf[..n]);
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
                if self.no_restart {
                    // One-shot command finished — close PTY and exit
                    let _ = sys_close(self.pty_master_fd);
                    sys_exit(0);
                }
                let slave_path = format!("pty:slave/{}\0", self.pty_pair_id);
                let fd = unsafe { open(slave_path.as_ptr() as *const _, flag::O_RDWR as c_int, 0) } as usize;
                if fd != !0 {
                    // Derive kernel name from sh_name (strip NUL)
                    let name_end = self.sh_name.iter().position(|&b| b == 0).unwrap_or(self.sh_name.len());
                    let kname = core::str::from_utf8(&self.sh_name[..name_end]).unwrap_or("sh");
                    if let Ok(pid) = sys_spawn_with_stdio(&self.sh_bytes, Some(kname), fd, fd, fd) {
                        let _ = sys_set_child_args(pid, &self.sh_argv);
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

/// Resolve an executable name to a full path by searching $PATH directories.
/// Searched order:
///   1. Absolute path given directly (starts with '/')
///   2. Relative path starting with "./" or "../"
///   3. Directories in PATH env var (colon-separated)
///   4. Default well-known directories: /bin, /usr/bin, /root/.cargo/bin, /usr/local/bin
#[cfg(target_os = "eclipse")]
fn resolve_exec_path(prog: &str) -> std::string::String {
    if prog.is_empty() {
        return std::string::String::from("/bin/sh");
    }
    // Already absolute or relative
    if prog.starts_with('/') || prog.starts_with("./") || prog.starts_with("../") {
        return std::string::String::from(prog);
    }

    // Build the list of search directories
    let mut dirs: Vec<std::string::String> = Vec::new();

    // Add PATH env dirs first
    if let Ok(path_env) = std::env::var("PATH") {
        for dir in path_env.split(':') {
            if !dir.is_empty() {
                dirs.push(std::string::String::from(dir));
            }
        }
    }

    // Always include these well-known directories
    for d in &["/bin", "/usr/bin", "/root/.cargo/bin", "/usr/local/bin", "/usr/local/sbin"] {
        let ds = std::string::String::from(*d);
        if !dirs.contains(&ds) {
            dirs.push(ds);
        }
    }

    for dir in &dirs {
        let mut full = std::string::String::from(dir.as_str());
        if !full.ends_with('/') { full.push('/'); }
        full.push_str(prog);
        if std::fs::metadata(&full).is_ok() {
            return full;
        }
    }

    // Fallback: return the name as-is so the caller sees a "not found" error
    // with the original program name rather than a confusing /bin/<prog> path.
    std::string::String::from(prog)
}

/// Write an xterm-style escape sequence for a cursor/navigation key with optional modifiers.
///
/// For cursor keys (tilde=false): `\x1b[X`  or  `\x1b[1;<mod>X`  where X ∈ {A,B,C,D,H,F}
/// For tilde keys   (tilde=true):  `\x1b[N~` or  `\x1b[N;<mod>~` where N is the VT sequence number
///
/// The xterm modifier code is:  1 + shift + 2*alt + 4*ctrl
#[cfg(target_os = "eclipse")]
fn write_key_seq(fd: usize, code: u8, tilde: bool, shift: bool, alt: bool, ctrl: bool) {
    let mod_code = (shift as u8) | ((alt as u8) << 1) | ((ctrl as u8) << 2);
    if mod_code == 0 {
        if tilde {
            let _ = sys_write(fd, &[b'\x1b', b'[', code, b'~']);
        } else {
            let _ = sys_write(fd, &[b'\x1b', b'[', code]);
        }
    } else {
        // xterm modifier encoding: 1=plain, 2=Shift, 3=Alt, 4=Shift+Alt,
        // 5=Ctrl, 6=Shift+Ctrl, 7=Alt+Ctrl, 8=Shift+Alt+Ctrl
        let xterm_mod: u8 = mod_code + 1; // 1..=8
        let m = b'0' + xterm_mod;         // ASCII '2'..'8'
        if tilde {
            let _ = sys_write(fd, &[b'\x1b', b'[', code, b';', m, b'~']);
        } else {
            let _ = sys_write(fd, &[b'\x1b', b'[', b'1', b';', m, code]);
        }
    }
}

/// Write an xterm-style function-key escape sequence for F1–F12.
///
/// xterm F-key VT codes (with optional modifier):
///   F1=11, F2=12, F3=13, F4=14, F5=15, F6=17, F7=18, F8=19, F9=20, F10=21, F11=23, F12=24
///   Plain:      \x1b[<N>~
///   With mod:   \x1b[<N>;<mod>~
#[cfg(target_os = "eclipse")]
fn write_fn_key(fd: usize, fnum: u8, shift: bool, alt: bool, ctrl: bool) {
    // VT sequence numbers for F1–F12 (F6 skips 16 to 17, etc.)
    const VT_FN: [u8; 12] = [11, 12, 13, 14, 15, 17, 18, 19, 20, 21, 23, 24];
    let idx = (fnum as usize).saturating_sub(1);
    if idx >= VT_FN.len() { return; }
    let n = VT_FN[idx];
    let tens = b'0' + n / 10;
    let ones = b'0' + n % 10;
    let mod_code = (shift as u8) | ((alt as u8) << 1) | ((ctrl as u8) << 2);
    if mod_code == 0 {
        let _ = sys_write(fd, &[b'\x1b', b'[', tens, ones, b'~']);
    } else {
        let xterm_mod: u8 = mod_code + 1; // 2=Shift, 3=Alt, 5=Ctrl, 6=Shift+Ctrl, …
        let m = b'0' + xterm_mod;
        let _ = sys_write(fd, &[b'\x1b', b'[', tens, ones, b';', m, b'~']);
    }
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