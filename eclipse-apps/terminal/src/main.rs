use std::rc::Rc;
use std::vec::Vec;
use std::boxed::Box;
use core::cell::RefCell;
use wayland_proto::EclipseWaylandConnection;
use eclipse_syscall::{self, flag, ProcessInfo};
use heapless::String as HString;

use os_terminal::{ClipboardHandler, DrawTarget, MouseInput, Rgb, Terminal};
use os_terminal::font::BitmapFont;

#[cfg(target_vendor = "eclipse")]
use libc::{c_int, close, kill, mmap, munmap, open};
#[cfg(target_vendor = "eclipse")]
use eclipse_ipc::prelude::EclipseMessage;
#[cfg(target_vendor = "eclipse")]
use sidewind::{
    IpcChannel, SideWindEvent, SideWindMessage,
    SWND_EVENT_TYPE_KEY, SWND_EVENT_TYPE_CLOSE, SWND_EVENT_TYPE_RESIZE,
    SWND_EVENT_TYPE_MOUSE_BUTTON,
};

#[cfg(target_vendor = "eclipse")]
use eclipse_syscall::call::{
    close as sys_close,
    exit as sys_exit,
    ioctl as sys_ioctl,
    read as sys_read,
    spawn_with_stdio as sys_spawn_with_stdio,
    wait_pid_nohang as sys_wait_pid_nohang,
    write as sys_write,
};

// ============================================================================
// Tipos y Estructuras
// ============================================================================

#[cfg(target_vendor = "eclipse")]
struct EclipseClipboard {
    text: std::string::String,
}

#[cfg(target_vendor = "eclipse")]
impl EclipseClipboard {
    fn new() -> Self {
        Self { text: std::string::String::new() }
    }
}

#[cfg(target_vendor = "eclipse")]
impl ClipboardHandler for EclipseClipboard {
    fn get_text(&mut self) -> Option<std::string::String> {
        if self.text.is_empty() { None } else { Some(self.text.clone()) }
    }
    fn set_text(&mut self, text: std::string::String) {
        self.text = text;
    }
}

#[cfg(target_vendor = "eclipse")]
struct SurfaceBacking {
    ptr: *mut u32,
    width: usize,
    height: usize,
    size_bytes: usize,
}

#[cfg(target_vendor = "eclipse")]
struct SharedSurfaceDrawTarget {
    state: Rc<RefCell<SurfaceBacking>>,
}

#[cfg(target_vendor = "eclipse")]
unsafe impl Send for SharedSurfaceDrawTarget {}

#[cfg(target_vendor = "eclipse")]
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

fn find_pid_by_name(want: &[u8]) -> Option<u32> {
    let mut list = [ProcessInfo::default(); 48];
    let count = eclipse_syscall::get_process_list(&mut list).ok()?;
    for info in list.iter().take(count) {
        if info.pid == 0 { continue; }
        let end = info.name.iter().position(|&b| b == 0).unwrap_or(16);
        if &info.name[..end] == want { return Some(info.pid); }
    }
    None
}

// ============================================================================
// TerminalApp
// ============================================================================

struct TerminalApp {
    lunas_pid: u32,
    connection: Rc<RefCell<EclipseWaylandConnection>>,
    surface_state: Rc<RefCell<SurfaceBacking>>,
    terminal: Terminal<SharedSurfaceDrawTarget>,
    pty_master_fd: usize,
    pty_pair_id: usize,
    sh_pid: usize,
    sh_bytes: Vec<u8>,
    name_str: String,
    last_title: [u8; 32],
}

impl TerminalApp {
    fn new() -> Option<Self> {
        let self_pid = eclipse_syscall::getpid() as u32;
        let lunas_pid = find_pid_by_name(b"lunas").or_else(|| find_pid_by_name(b"gui"));
        if lunas_pid.is_none() {
            std::println!("Terminal Error: Lunas/GUI not found");
            return None;
        }
        let lunas_pid = lunas_pid.unwrap();

        let connection = Rc::new(RefCell::new(EclipseWaylandConnection::new(lunas_pid, self_pid)));

        // 2. SideWind Window
        let name = sidewind_shm_name(self_pid);
        let name_str = String::from(name.as_str());
        let win_w = 640u32;
        let win_h = 400u32;

        let res = Self::open_window(lunas_pid, 100, 100, win_w, win_h, &name_str);
        if res.is_none() {
            std::println!("Terminal Error: Failed to open window");
            return None;
        }
        let (ptr, size_bytes, w, h) = res.unwrap();
        std::println!("Terminal: Window opened at {:?}", ptr);
        let surface_state = Rc::new(RefCell::new(SurfaceBacking {
            ptr, width: w as usize, height: h as usize, size_bytes
        }));

        // 3. os-terminal setup
        let draw_target = SharedSurfaceDrawTarget { state: surface_state.clone() };
        let mut terminal = Terminal::new(draw_target, Box::new(BitmapFont));
        terminal.set_crnl_mapping(true);
        terminal.set_clipboard(Box::new(EclipseClipboard::new()));
        terminal.set_auto_flush(false);

        // 4. PTY & Shell
        let pty_master_fd = unsafe {
            open(b"pty:master\0".as_ptr() as *const _, (flag::O_RDWR | flag::O_CREAT) as c_int, 0)
        } as usize;
        if pty_master_fd == !0 { return None; }

        let mut pty_pair_id: usize = 0;
        let _ = sys_ioctl(pty_master_fd, 1, &mut pty_pair_id as *mut _ as usize);

        let slave_path = format!("pty:slave/{}\0", pty_pair_id);
        let pty_slave_fd = unsafe { open(slave_path.as_ptr() as *const _, flag::O_RDWR as c_int, 0) } as usize;

        let sh_res = std::fs::read("/bin/sh");
        if sh_res.is_err() {
            std::println!("Terminal Error: Failed to read /bin/sh");
            return None;
        }
        let sh_bytes = sh_res.unwrap();

        let sh_spawn = sys_spawn_with_stdio(&sh_bytes, Some("sh"), pty_slave_fd, pty_slave_fd, pty_slave_fd);
        if sh_spawn.is_err() {
            std::println!("Terminal Error: Failed to spawn /bin/sh");
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
            lunas_pid, connection, surface_state, terminal, pty_master_fd, pty_pair_id,
            sh_pid, sh_bytes, name_str, last_title: [0; 32],
        })
    }

    fn open_window(composer_pid: u32, x: i32, y: i32, w: u32, h: u32, name: &str) -> Option<(*mut u32, usize, u32, u32)> {
        let path = format!("/tmp/{}\0", name);
        let size_bytes = (w as usize) * (h as usize) * 4;
        let fd = unsafe { open(path.as_ptr() as *const _, (flag::O_RDWR | flag::O_CREAT) as c_int, 0o644) };
        if fd < 0 { return None; }
        let _ = eclipse_syscall::ftruncate(fd as usize, size_bytes);
        let vaddr = unsafe { mmap(core::ptr::null_mut(), size_bytes, (flag::PROT_READ|flag::PROT_WRITE) as c_int, flag::MAP_SHARED as c_int, fd, 0) };
        unsafe { close(fd) };
        if vaddr.is_null() || vaddr == (!0isize as *mut _) { return None; }

        let msg = SideWindMessage::new_create(x, y, w, h, name);
        if !IpcChannel::send_sidewind(composer_pid, &msg) { return None; }
        Some((vaddr as *mut u32, size_bytes, w, h))
    }

    fn run(&mut self) {
        self.terminal.process(b"\x1b[1;36mEclipse OS Terminal v3\x1b[0m\r\n");
        let mut pty_buf = [0u8; 1024];

        loop {
            let mut dirty = false;

            // 1. Teclado e IPC (Lunas -> Apps)
            while let Some(msg) = (*self.connection).borrow().channel.borrow_mut().recv() {
                if let EclipseMessage::Raw { data, len, .. } = msg {
                    if len == core::mem::size_of::<SideWindEvent>() {
                        let ev = unsafe { core::ptr::read_unaligned(data.as_ptr() as *const SideWindEvent) };
                        match ev.event_type {
                            SWND_EVENT_TYPE_KEY => {
                                let sc = (ev.data1 as u16 & 0xFF) as u8;
                                let ps2 = if ev.data2 != 0 { sc } else { sc | 0x80 };
                                let _ = self.terminal.handle_keyboard(ps2);
                                dirty = true;
                            }
                            SWND_EVENT_TYPE_MOUSE_BUTTON => {
                                if ev.data1 == 4 { self.terminal.handle_mouse(MouseInput::Scroll(3)); dirty = true; }
                                else if ev.data1 == 5 { self.terminal.handle_mouse(MouseInput::Scroll(-3)); dirty = true; }
                            }
                            SWND_EVENT_TYPE_RESIZE => {
                                let (nw, nh) = (ev.data1.max(1) as u32, ev.data2.max(1) as u32);
                                if let Some((ptr, sz, w, h)) = Self::open_window(self.lunas_pid, 0, 0, nw, nh, &self.name_str) {
                                    {
                                        let mut sb = (*self.surface_state).borrow_mut();
                                        unsafe { munmap(sb.ptr as *mut _, sb.size_bytes); }
                                        *sb = SurfaceBacking { ptr, width: w as usize, height: h as usize, size_bytes: sz };
                                    }
                                    set_pty_winsize(self.pty_master_fd, nh as u16 / FONT_CHAR_H, nw as u16 / FONT_CHAR_W, nw as u16, nh as u16);
                                    self.terminal.set_font_manager(Box::new(BitmapFont));
                                    dirty = true;
                                }
                            }
                            SWND_EVENT_TYPE_CLOSE => {
                                unsafe { kill(self.sh_pid as c_int, 9) };
                                let _ = sys_close(self.pty_master_fd);
                                sys_exit(0);
                            }
                            _ => {}
                        }
                    }
                }
            }

            // 2. PTY Output -> Terminal
            let mut available: usize = 0;
            let _ = sys_ioctl(self.pty_master_fd, 2, &mut available as *mut _ as usize);
            while available > 0 {
                let limit = pty_buf.len();
                let n = sys_read(self.pty_master_fd, &mut pty_buf[..(available.min(limit))]).unwrap_or(0);
                if n == 0 { break; }
                self.terminal.process(&pty_buf[..n]);
                if let Some(title) = extract_osc_title(&pty_buf[..n]) {
                    if title != self.last_title {
                        let _ = IpcChannel::send_sidewind(self.lunas_pid, &SideWindMessage::new_set_title(&title));
                        self.last_title = title;
                    }
                }
                dirty = true;
                available = 0;
                let _ = sys_ioctl(self.pty_master_fd, 2, &mut available as *mut _ as usize);
            }

            // 3. Shell Restart
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
                let _ = IpcChannel::send_sidewind(self.lunas_pid, &SideWindMessage::new_commit());
            } else {
                let _ = eclipse_syscall::call::sched_yield();
                std::thread::yield_now();
            }
        }
    }
}

fn main() {
    #[cfg(target_vendor = "eclipse")]
    {
        if let Some(mut app) = TerminalApp::new() {
            app.run();
        } else {
            std::println!("Failed to initialize TerminalApp.");
        }
    }
}