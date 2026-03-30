#![cfg_attr(target_vendor = "eclipse", no_std)]

#[cfg(target_vendor = "eclipse")]
extern crate alloc;
#[cfg(target_vendor = "eclipse")]
extern crate eclipse_std as std;

use os_terminal::{DrawTarget, Rgb, Terminal};
use os_terminal::font::BitmapFont;

#[cfg(target_vendor = "eclipse")]
use alloc::{boxed::Box, vec::Vec};

#[cfg(target_vendor = "eclipse")]
use sidewind::{
    discover_composer, SideWindSurface, SWND_EVENT_TYPE_KEY,
};

const WIDTH: u32 = 800;
const HEIGHT: u32 = 500;

/// Draw target that writes ARGB pixels directly into the SideWind surface buffer.
struct SurfaceDrawTarget {
    ptr: *mut u32,
    width: usize,
    height: usize,
}

// Safety: the pointer is valid for the lifetime of the surface, which outlives the terminal.
unsafe impl Send for SurfaceDrawTarget {}

impl DrawTarget for SurfaceDrawTarget {
    fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    #[inline(always)]
    fn draw_pixel(&mut self, x: usize, y: usize, color: Rgb) {
        if x < self.width && y < self.height {
            let idx = y * self.width + x;
            // ARGB8888: alpha=0xFF, R, G, B
            let pixel = 0xFF00_0000u32
                | ((color.0 as u32) << 16)
                | ((color.1 as u32) << 8)
                | (color.2 as u32);
            // Safety: idx is bounds-checked above; ptr is valid and aligned.
            unsafe { *self.ptr.add(idx) = pixel };
        }
    }
}

#[cfg(target_vendor = "eclipse")]
fn main() {
    use std::prelude::v1::*;
    use eclipse_syscall::call::{close, ioctl, open, read, sched_yield, write};
    use eclipse_syscall::flag::{O_CREAT, O_RDWR};

    // 1. Locate the compositor; retry until it is ready.
    let composer_pid = loop {
        if let Some(pid) = discover_composer() {
            break pid;
        }
        let _ = sched_yield();
    };

    // 2. Create the window surface.
    let mut surface =
        match SideWindSurface::new(composer_pid, 100, 100, WIDTH, HEIGHT, "terminal") {
            Some(s) => s,
            None => return,
        };

    // 3. Build the draw target using the surface's shared-memory buffer.
    let buf_ptr: *mut u32 = surface.buffer().as_mut_ptr();
    let draw_target = SurfaceDrawTarget {
        ptr: buf_ptr,
        width: WIDTH as usize,
        height: HEIGHT as usize,
    };

    // 4. Create the terminal emulator.
    let mut terminal = Terminal::new(draw_target, Box::new(BitmapFont));
    terminal.set_crnl_mapping(true);

    // 5. Open the PTY master for bidirectional communication with the shell.
    let pty_fd = open("pty:master", O_RDWR | O_CREAT).unwrap_or(usize::MAX);

    // 6. Wire PTY writes: keyboard input flows from the terminal to the shell.
    let pfd = pty_fd;
    terminal.set_pty_writer(Box::new(move |s: &str| {
        if pfd != usize::MAX {
            let _ = write(pfd, s.as_bytes());
        }
    }));

    // 7. Spawn the shell connected to the PTY.
    if pty_fd != usize::MAX {
        if let Ok(sh_fd) = open("/bin/sh", O_RDWR) {
            let mut sh_data: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                match read(sh_fd, &mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sh_data.extend_from_slice(&tmp[..n]),
                }
            }
            let _ = close(sh_fd);
            if !sh_data.is_empty() {
                let _ = eclipse_syscall::call::spawn_with_stdio(
                    &sh_data,
                    Some("sh"),
                    pty_fd,
                    pty_fd,
                    pty_fd,
                );
            }
        }
    }

    // 8. Show a welcome banner.
    terminal.process(b"\x1b[1;32mEclipse OS Terminal\x1b[0m\r\n");
    terminal.flush();
    surface.commit();

    // 9. Main event loop.
    loop {
        // Process compositor events (keyboard, resize, …).
        while let Some(event) = surface.poll_event() {
            if event.event_type == SWND_EVENT_TYPE_KEY {
                let scancode = event.data1 as u8;
                let pressed = event.data2 != 0;
                // PS/2 Scancode Set 1: release events have bit 7 set.
                let ps2 = if pressed { scancode } else { scancode | 0x80 };
                let _ = terminal.handle_keyboard(ps2);
                terminal.flush();
                surface.commit();
            }
        }

        // Drain any output produced by the shell via the PTY.
        if pty_fd != usize::MAX {
            let mut available: usize = 0;
            if ioctl(pty_fd, 2, &mut available as *mut usize as usize).is_ok()
                && available > 0
            {
                let n = available.min(512);
                let mut buf = [0u8; 512];
                if let Ok(n) = read(pty_fd, &mut buf[..n]) {
                    if n > 0 {
                        terminal.process(&buf[..n]);
                        terminal.flush();
                        surface.commit();
                    }
                }
            }
        }

        let _ = sched_yield();
    }
}

#[cfg(not(target_vendor = "eclipse"))]
fn main() {
    println!("Eclipse OS only.");
}
