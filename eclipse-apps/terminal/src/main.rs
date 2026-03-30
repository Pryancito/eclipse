#![cfg_attr(target_vendor = "eclipse", no_std)]

#[cfg(target_vendor = "eclipse")]
extern crate alloc;
#[cfg(target_vendor = "eclipse")]
extern crate eclipse_std as std;

use os_terminal::{DrawTarget, Rgb, Terminal};
use os_terminal::font::BitmapFont;

#[cfg(target_vendor = "eclipse")]
use alloc::{boxed::Box, vec::Vec};

#[cfg(not(target_vendor = "eclipse"))]
use std::{boxed::Box, vec::Vec};

// --- Universal Yield ---
pub fn platform_yield() {
    #[cfg(target_vendor = "eclipse")]
    eclipse_syscall::call::sched_yield();
}

// --- Host Mocks ---
#[cfg(not(target_vendor = "eclipse"))]
pub enum SideWindEvent { }
#[cfg(not(target_vendor = "eclipse"))]
pub const SWND_EVENT_TYPE_KEY: u32 = 1;
#[cfg(not(target_vendor = "eclipse"))]
pub fn discover_composer() -> Option<u32> { None }
#[cfg(not(target_vendor = "eclipse"))]
pub struct SideWindSurface { }
#[cfg(not(target_vendor = "eclipse"))]
impl SideWindSurface {
    pub fn new(_: u32, _: i32, _: i32, _: u32, _: u32, _: &str) -> Option<Self> { None }
    pub fn buffer(&mut self) -> &mut [u32] { &mut [] }
    pub fn commit(&mut self) { }
    pub fn poll_event(&self) -> Option<SideWindEvent> { None }
}
#[cfg(not(target_vendor = "eclipse"))]
pub fn open(_: &str, _: usize) -> Result<usize, ()> { Err(()) }
#[cfg(not(target_vendor = "eclipse"))]
pub fn read(_: usize, _: &mut [u8]) -> Result<usize, ()> { Err(()) }
#[cfg(not(target_vendor = "eclipse"))]
pub fn write(_: usize, _: &[u8]) -> Result<usize, ()> { Err(()) }
#[cfg(not(target_vendor = "eclipse"))]
pub fn ioctl(_: usize, _: usize, _: *mut usize) -> Result<(), ()> { Err(()) }
#[cfg(not(target_vendor = "eclipse"))]
pub const O_RDWR: usize = 0;
#[cfg(not(target_vendor = "eclipse"))]
pub const O_CREAT: usize = 0;

// --- Eclipse Native ---
#[cfg(target_vendor = "eclipse")]
use sidewind::{
    discover_composer, SideWindSurface, SWND_EVENT_TYPE_KEY,
};
#[cfg(target_vendor = "eclipse")]
use eclipse_syscall::call::{close, ioctl, open, read, write, spawn_with_stdio};
#[cfg(target_vendor = "eclipse")]
use eclipse_syscall::flag::{O_CREAT, O_RDWR};

const WIDTH: u32 = 800;
const HEIGHT: u32 = 500;

/// Draw target that writes pixels directly into the SNP shared memory buffer.
struct SurfaceDrawTarget {
    ptr: *mut u32,
    width: usize,
    height: usize,
}

unsafe impl Send for SurfaceDrawTarget {}

impl DrawTarget for SurfaceDrawTarget {
    fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    #[inline(always)]
    fn draw_pixel(&mut self, x: usize, y: usize, color: Rgb) {
        if x < self.width && y < self.height {
            let idx = y * self.width + x;
            let pixel = 0xFF00_0000u32
                | ((color.0 as u32) << 16)
                | ((color.1 as u32) << 8)
                | (color.2 as u32);
            unsafe { 
                if !self.ptr.is_null() {
                    *self.ptr.add(idx) = pixel;
                }
            };
        }
    }
}

pub struct PtyManager {
    fd: usize,
}

impl PtyManager {
    pub fn new() -> Self {
        #[cfg(target_vendor = "eclipse")]
        {
            let fd = open("pty:master", O_RDWR | O_CREAT).unwrap_or(usize::MAX);
            Self { fd }
        }
        #[cfg(not(target_vendor = "eclipse"))]
        Self { fd: usize::MAX }
    }

    pub fn is_valid(&self) -> bool {
        self.fd != usize::MAX
    }

    pub fn write(&self, data: &[u8]) {
        #[cfg(target_vendor = "eclipse")]
        if self.is_valid() {
            let _ = write(self.fd, data);
        }
        let _ = data;
    }

    pub fn read(&self, buf: &mut [u8]) -> Option<usize> {
        if !self.is_valid() { return None; }
        
        #[cfg(target_vendor = "eclipse")]
        {
            let mut available: usize = 0;
            if ioctl(self.fd, 2, &mut available as *mut usize as usize).is_ok() && available > 0 {
                let n = available.min(buf.len());
                read(self.fd, &mut buf[..n]).ok()
            } else {
                None
            }
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            let _ = buf;
            None
        }
    }

    pub fn spawn_shell(&self) {
        if !self.is_valid() { return; }
        
        #[cfg(target_vendor = "eclipse")]
        if let Ok(sh_fd) = open("/bin/sh", O_RDWR) {
            let mut sh_data = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                match read(sh_fd, &mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sh_data.extend_from_slice(&tmp[..n]),
                }
            }
            let _ = close(sh_fd);
            
            if !sh_data.is_empty() {
                let _ = spawn_with_stdio(
                    &sh_data,
                    Some("sh"),
                    self.fd,
                    self.fd,
                    self.fd,
                );
            }
        }
    }
}

pub struct TerminalApp {
    surface: SideWindSurface,
    terminal: Terminal<SurfaceDrawTarget>,
    pty: PtyManager,
}
impl TerminalApp {
    pub fn new(composer_pid: u32) -> Option<Self> {
        #[cfg(target_vendor = "eclipse")]
        {
            let mut surface = SideWindSurface::new(composer_pid, 100, 100, WIDTH, HEIGHT, "terminal")?;
            
            let draw_target = SurfaceDrawTarget {
                ptr: surface.buffer().as_mut_ptr(),
                width: WIDTH as usize,
                height: HEIGHT as usize,
            };

            let font = Box::new(BitmapFont);
            let mut terminal = Terminal::new(draw_target, font);
            terminal.set_crnl_mapping(true);
            
            let pty = PtyManager::new();
            
            Some(Self {
                surface,
                terminal,
                pty,
            })
        }
        #[cfg(not(target_vendor = "eclipse"))]
        {
            let _ = composer_pid;
            None
        }
    }

    pub fn run(mut self) {
        self.pty.spawn_shell();
        
        let pfd = self.pty.fd;
        self.terminal.set_pty_writer(Box::new(move |s: &str| {
            #[cfg(target_vendor = "eclipse")]
            if pfd != usize::MAX {
                let _ = write(pfd, s.as_bytes());
            }
            let _ = s;
        }));

        self.terminal.process(b"\x1b[1;36mEclipse OS SNP v2 Terminal\x1b[0m\r\n");
        self.terminal.flush();
        self.surface.commit();

        #[cfg(target_vendor = "eclipse")]
        loop {
            while let Some(event) = self.surface.poll_event() {
                if event.event_type == SWND_EVENT_TYPE_KEY {
                    let scancode = event.data1 as u8;
                    let pressed = event.data2 != 0;
                    let ps2 = if pressed { scancode } else { scancode | 0x80 };
                    let _ = self.terminal.handle_keyboard(ps2);
                    self.terminal.flush();
                    self.surface.commit();
                }
            }

            let mut buf = [0u8; 1024];
            if let Some(n) = self.pty.read(&mut buf) {
                if n > 0 {
                    self.terminal.process(&buf[..n]);
                    self.terminal.flush();
                    self.surface.commit();
                }
            }

            platform_yield();
        }
    }
}

fn main() {
    #[cfg(target_vendor = "eclipse")]
    {
        let composer_pid = loop {
            if let Some(pid) = discover_composer() {
                break pid;
            }
            platform_yield();
        };

        if let Some(app) = TerminalApp::new(composer_pid) {
            app.run();
        }
    }
    #[cfg(not(target_vendor = "eclipse"))]
    {
        println!("Not running on Eclipse OS");
    }
}
