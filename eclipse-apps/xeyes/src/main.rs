//! xeyes — Eclipse OS
//!
//! Classic two-eye demo: a pair of eyes whose pupils track the mouse cursor.
//! When no mouse event has been received yet the pupils follow a smooth
//! Lissajous animation path so the window is always alive.
//!
//! Rendering is pixel-by-pixel into a SideWind shared-memory framebuffer,
//! using sin / cos / atan2 / sqrt / hypot from eclipse-relibc.

use heapless::String as HString;

#[cfg(target_vendor = "eclipse")]
use libc::{atan2, c_int, close, cos, exit, hypot, mmap, munmap, open, sin};
#[cfg(target_vendor = "eclipse")]
use eclipse_ipc::prelude::EclipseMessage;
#[cfg(target_vendor = "eclipse")]
use sidewind::{
    IpcChannel, SideWindEvent, SideWindMessage,
    SWND_EVENT_TYPE_CLOSE, SWND_EVENT_TYPE_KEY, SWND_EVENT_TYPE_MOUSE_MOVE,
};
#[cfg(target_vendor = "eclipse")]
use eclipse_syscall::{self, flag, ProcessInfo};
#[cfg(target_vendor = "eclipse")]
use eclipse_syscall::call::sched_yield;

// ─────────────────────────────────────────────────────────────────────────────
// Window geometry
// ─────────────────────────────────────────────────────────────────────────────

const WIN_W: u32 = 300;
const WIN_H: u32 = 180;

// Sclera (outer white), iris, pupil radii as fractions of half the window height.
const SCLERA_R: f64 = 0.38;
const IRIS_R: f64 = 0.26;
const PUPIL_R: f64 = 0.12;

// Eye centres as fractions of window width / height.
const EYE_LEFT_X: f64 = 0.30;
const EYE_RIGHT_X: f64 = 0.70;
const EYE_Y: f64 = 0.50;

// PS/2 scancodes for Q and Escape.
const SCANCODE_Q: u8 = 0x10;
const SCANCODE_ESCAPE: u8 = 0x01;

// Lissajous animation parameters (used when no mouse cursor is available).
const LISS_A: f64 = 1.3;
const LISS_B: f64 = 2.7;
const LISS_STEP: f64 = 0.012;

// Maximum length of a SHM name (excluding the '/tmp/' prefix).
const MAX_SHM_NAME_LEN: usize = 32;
const TMP_PREFIX_LEN: usize = 5;

// ─────────────────────────────────────────────────────────────────────────────
// Colour palette
// ─────────────────────────────────────────────────────────────────────────────

const BG_COLOR: u32 = 0xFF_50_50_50; // dark-grey window background
const OUTLINE_COLOR: u32 = 0xFF_18_18_18; // thin ring around sclera
const SCLERA_COLOR: u32 = 0xFF_F5_F0_E8; // warm white sclera
const IRIS_COLOR: u32 = 0xFF_22_66_CC; // blue iris
const IRIS_SHADOW: u32 = 0xFF_14_44_88; // darker ring inside iris
const PUPIL_COLOR: u32 = 0xFF_08_08_08; // nearly-black pupil
const PUPIL_SHINE: u32 = 0xFF_D8_E8_FF; // tiny specular highlight

// ─────────────────────────────────────────────────────────────────────────────
// Helper: write a square pixel if in bounds
// ─────────────────────────────────────────────────────────────────────────────

#[inline(always)]
fn put_pixel(buf: &mut [u32], w: u32, x: i32, y: i32, color: u32) {
    let w = w as i32;
    if x >= 0 && x < w && y >= 0 && y < WIN_H as i32 {
        buf[(y * w + x) as usize] = color;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Draw one eye into the framebuffer
// ─────────────────────────────────────────────────────────────────────────────
//
//  cx, cy     — eye centre in pixels
//  sr, ir, pr — sclera / iris / pupil radii in pixels
//  gx, gy     — gaze target in pixels
//
#[cfg(target_vendor = "eclipse")]
fn draw_eye(buf: &mut [u32], cx: f64, cy: f64, sr: f64, ir: f64, pr: f64, gx: f64, gy: f64) {
    // Direction from eye centre to gaze target.
    let dx = gx - cx;
    let dy = gy - cy;
    let dist = unsafe { hypot(dx, dy) };
    let angle = unsafe { atan2(dy, dx) };

    // Pupil centre: constrained so that the pupil stays inside the iris.
    let max_offset = ir - pr;
    let offset = if dist < max_offset { dist } else { max_offset };
    let px_c = cx + unsafe { cos(angle) } * offset;
    let py_c = cy + unsafe { sin(angle) } * offset;

    // Specular highlight: a small dot at a fixed offset within the pupil.
    let shine_x = px_c - pr * 0.35;
    let shine_y = py_c - pr * 0.35;
    let shine_r = pr * 0.25;

    // Bounding box for the sclera (add 1-pixel margin for outline).
    let x0 = (cx - sr - 2.0) as i32;
    let x1 = (cx + sr + 2.0) as i32;
    let y0 = (cy - sr - 2.0) as i32;
    let y1 = (cy + sr + 2.0) as i32;

    let w = WIN_W;
    for py in y0..=y1 {
        for px in x0..=x1 {
            let fx = px as f64 + 0.5;
            let fy = py as f64 + 0.5;

            let de = unsafe { hypot(fx - cx, fy - cy) }; // dist from eye centre
            let dp = unsafe { hypot(fx - px_c, fy - py_c) }; // dist from pupil centre
            let ds = unsafe { hypot(fx - shine_x, fy - shine_y) }; // dist from shine

            let color = if ds < shine_r {
                PUPIL_SHINE
            } else if dp < pr {
                PUPIL_COLOR
            } else if de < ir * 0.72 {
                // Inner iris shadow ring
                IRIS_SHADOW
            } else if de < ir {
                IRIS_COLOR
            } else if de < sr {
                SCLERA_COLOR
            } else if de < sr + 1.5 {
                OUTLINE_COLOR
            } else {
                continue;
            };
            put_pixel(buf, w, px, py, color);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Render one complete frame
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(target_vendor = "eclipse")]
fn render_frame(buf: &mut [u32], gaze_x: f64, gaze_y: f64) {
    // Fill background.
    for px in buf.iter_mut() {
        *px = BG_COLOR;
    }

    let half_h = WIN_H as f64 * 0.5;
    let sr = half_h * SCLERA_R;
    let ir = half_h * IRIS_R;
    let pr = half_h * PUPIL_R;

    let left_cx = WIN_W as f64 * EYE_LEFT_X;
    let right_cx = WIN_W as f64 * EYE_RIGHT_X;
    let eye_cy = WIN_H as f64 * EYE_Y;

    draw_eye(buf, left_cx, eye_cy, sr, ir, pr, gaze_x, gaze_y);
    draw_eye(buf, right_cx, eye_cy, sr, ir, pr, gaze_x, gaze_y);
}

// ─────────────────────────────────────────────────────────────────────────────
// Eclipse OS helpers (shared with glxgears pattern)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(target_vendor = "eclipse")]
fn process_name_bytes(name: &[u8; 16]) -> &[u8] {
    let end = name.iter().position(|&b| b == 0).unwrap_or(16);
    &name[..end]
}

#[cfg(target_vendor = "eclipse")]
fn find_pid_by_name(want: &[u8]) -> Option<u32> {
    let mut list = [ProcessInfo::default(); 48];
    let count = eclipse_syscall::get_process_list(&mut list).ok()?;
    for info in list.iter().take(count) {
        if info.pid == 0 {
            continue;
        }
        if process_name_bytes(&info.name) == want {
            return Some(info.pid);
        }
    }
    None
}

/// Derive a per-process SHM name from `pid`.
#[cfg(target_vendor = "eclipse")]
fn shm_name(pid: u32) -> HString<24> {
    let mut s = HString::new();
    let _ = s.push_str("xeyes_");
    let mut n = pid;
    let mut tmp = [0u8; 10];
    let mut i = 0usize;
    if n == 0 {
        tmp[0] = b'0';
        i = 1;
    } else {
        while n > 0 && i < tmp.len() {
            tmp[i] = b'0' + (n % 10) as u8;
            n /= 10;
            i += 1;
        }
    }
    for j in 0..i / 2 {
        tmp.swap(j, i - 1 - j);
    }
    let _ = s.push_str(unsafe { core::str::from_utf8_unchecked(&tmp[..i]) });
    s
}

/// Create an SHM-backed window via the SideWind / Lunas IPC protocol.
#[cfg(target_vendor = "eclipse")]
fn open_sidewind_window(
    composer_pid: u32,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    name: &str,
) -> Option<(*mut u32, usize)> {
    let mut path = [0u8; 64];
    path[0..5].copy_from_slice(b"/tmp/");
    let nb = name.as_bytes();
    let nlen = nb.len().min(MAX_SHM_NAME_LEN).min(path.len() - TMP_PREFIX_LEN - 1);
    path[5..5 + nlen].copy_from_slice(&nb[..nlen]);

    let size_bytes = (w as usize).saturating_mul(h as usize).saturating_mul(4);

    let fd = unsafe {
        open(
            path.as_ptr() as *const core::ffi::c_char,
            (flag::O_RDWR | flag::O_CREAT) as c_int,
            0o644,
        )
    };
    if fd < 0 {
        return None;
    }
    if eclipse_syscall::ftruncate(fd as usize, size_bytes).is_err() {
        unsafe { close(fd) };
        return None;
    }
    let vaddr = unsafe {
        mmap(
            core::ptr::null_mut(),
            size_bytes,
            (flag::PROT_READ | flag::PROT_WRITE) as c_int,
            flag::MAP_SHARED as c_int,
            fd,
            0,
        )
    };
    unsafe { close(fd) };
    if vaddr.is_null() || vaddr == (-1isize as *mut core::ffi::c_void) {
        return None;
    }

    let msg = SideWindMessage::new_create(x, y, w, h, name);
    if !IpcChannel::send_sidewind(composer_pid, &msg) {
        unsafe { munmap(vaddr, size_bytes) };
        return None;
    }

    Some((vaddr as *mut u32, size_bytes))
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

fn main() {
    #[cfg(target_vendor = "eclipse")]
    {
        let self_pid = eclipse_syscall::getpid() as u32;
        let lunas_pid = find_pid_by_name(b"lunas").or_else(|| find_pid_by_name(b"gui"));
        let Some(lunas_pid) = lunas_pid else {
            loop {
                let _ = sched_yield();
            }
        };

        let name = shm_name(self_pid);
        let name_str = name.as_str();

        let Some((fb_ptr, fb_size)) =
            open_sidewind_window(lunas_pid, 200, 150, WIN_W, WIN_H, name_str)
        else {
            loop {
                let _ = sched_yield();
            }
        };

        let buf_len = (WIN_W as usize).saturating_mul(WIN_H as usize);
        let buf: &mut [u32] =
            unsafe { core::slice::from_raw_parts_mut(fb_ptr, buf_len) };

        let mut ipc_ch = IpcChannel::new();
        let sw_ev_sz = core::mem::size_of::<SideWindEvent>();

        // Last-known mouse position (starts at centre of window).
        let mut cursor_x: f64 = WIN_W as f64 * 0.5;
        let mut cursor_y: f64 = WIN_H as f64 * 0.5;
        // Whether the cursor has been set by an actual mouse event.
        let mut cursor_set = false;
        // Lissajous animation phase (used before any mouse event arrives).
        let mut phase: f64 = 0.0;

        loop {
            // Process all pending IPC events.
            while let Some(msg) = ipc_ch.recv() {
                if let EclipseMessage::Raw { data, len, .. } = msg {
                    if len == sw_ev_sz {
                        let ev = unsafe {
                            core::ptr::read_unaligned(data.as_ptr() as *const SideWindEvent)
                        };
                        match ev.event_type {
                            SWND_EVENT_TYPE_MOUSE_MOVE => {
                                cursor_x = ev.data1 as f64;
                                cursor_y = ev.data2 as f64;
                                cursor_set = true;
                            }
                            SWND_EVENT_TYPE_KEY => {
                                let sc = ev.data1 as u8;
                                if sc == SCANCODE_Q || sc == SCANCODE_ESCAPE {
                                    let _ = IpcChannel::send_sidewind(
                                        lunas_pid,
                                        &SideWindMessage::new_destroy(),
                                    );
                                    unsafe { munmap(fb_ptr as *mut core::ffi::c_void, fb_size) };
                                    unsafe { exit(0) };
                                }
                            }
                            SWND_EVENT_TYPE_CLOSE => {
                                let _ = IpcChannel::send_sidewind(
                                    lunas_pid,
                                    &SideWindMessage::new_destroy(),
                                );
                                unsafe { munmap(fb_ptr as *mut core::ffi::c_void, fb_size) };
                                unsafe { exit(0) };
                            }
                            _ => {}
                        }
                    }
                }
            }

            // When no mouse events have been received yet, animate with a
            // Lissajous curve so the eyes are always moving.
            let (gaze_x, gaze_y) = if cursor_set {
                (cursor_x, cursor_y)
            } else {
                let amp_x = WIN_W as f64 * 0.35;
                let amp_y = WIN_H as f64 * 0.35;
                let gx = WIN_W as f64 * 0.5
                    + amp_x * unsafe { sin(LISS_A * phase) };
                let gy = WIN_H as f64 * 0.5
                    + amp_y * unsafe { sin(LISS_B * phase + 1.0) };
                phase += LISS_STEP;
                if phase > 1000.0 {
                    phase -= 1000.0;
                }
                (gx, gy)
            };

            render_frame(buf, gaze_x, gaze_y);

            let _ = IpcChannel::send_sidewind(lunas_pid, &SideWindMessage::new_commit());

            let _ = sched_yield();
        }
    }

    #[cfg(not(target_vendor = "eclipse"))]
    {
        std::println!("xeyes: host-testing stub — no rendering outside Eclipse OS.");
    }
}
