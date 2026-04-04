//! glxgears — Eclipse OS
//!
//! Software-rendered rotating gears demo, inspired by the classic glxgears
//! OpenGL benchmark. Three interlocking gears (red, green, blue) rotate in
//! real time inside a SideWind window managed by the Lunas compositor.


use heapless::String as HString;

use core::time::Duration;
use std::time::Instant;
use embedded_graphics::{
    prelude::*,
    pixelcolor::Rgb888,
    text::Text,
    mono_font::MonoTextStyle,
};
#[cfg(target_os = "eclipse")]
use sidewind::font_terminus_16;

#[cfg(target_os = "eclipse")]
use libc::{c_int, close, mmap, munmap, open, exit};
#[cfg(target_os = "eclipse")]
use eclipse_ipc::prelude::EclipseMessage;
#[cfg(target_os = "eclipse")]
use sidewind::{IpcChannel, SideWindEvent, SideWindMessage, SWND_EVENT_TYPE_KEY, SWND_EVENT_TYPE_CLOSE};
#[cfg(target_os = "eclipse")]
use eclipse_syscall::{self, flag, ProcessInfo, SystemStats};
#[cfg(target_os = "eclipse")]
use eclipse_syscall::call::{sched_yield, exit as syscall_exit, get_system_stats};

// ─────────────────────────────────────────────────────────────────────────────
// Math helpers (no libm, no_std)
// ─────────────────────────────────────────────────────────────────────────────

const PI: f32 = core::f32::consts::PI;
const TAU: f32 = 2.0 * PI;
const FRAC_PI_2: f32 = PI / 2.0;

/// Round to nearest integer (no libm).
#[inline]
fn roundf(x: f32) -> f32 {
    if x >= 0.0 {
        (x + 0.5) as i32 as f32
    } else {
        (x - 0.5) as i32 as f32
    }
}

/// Taylor-series sin — accurate for any angle (range-reduced to [-π, π]).
#[inline]
fn fast_sin(x: f32) -> f32 {
    let a = x - TAU * roundf(x / TAU);
    let a2 = a * a;
    a * (1.0 - a2 * (1.0 / 6.0 - a2 * (1.0 / 120.0 - a2 * (1.0 / 5040.0))))
}

/// Taylor-series cos.
#[inline]
fn fast_cos(x: f32) -> f32 {
    fast_sin(x + FRAC_PI_2)
}

/// Babylonian square root — ~6 ULP accuracy for positive f32.
#[inline]
fn fast_sqrt(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    let i = x.to_bits();
    let j = (i >> 1).wrapping_add(0x1FBD_1DF5);
    let mut v = f32::from_bits(j);
    v = 0.5 * (v + x / v);
    v = 0.5 * (v + x / v);
    v
}

/// Euclidean remainder for f32 (replaces the std-only `f32::rem_euclid`).
#[inline]
fn rem_euclid_f32(a: f32, b: f32) -> f32 {
    let r = a - b * roundf(a / b);
    // Ensure result is in [0, b).
    if r < 0.0 { r + b } else { r }
}

/// Approximation of atan2 using the Atan2 octant method.
/// Returns the angle in [-π, π].
#[inline]
fn fast_atan2(y: f32, x: f32) -> f32 {
    let abs_y = if y < 0.0 { -y } else { y } + 1e-10;
    let (r, base_angle) = if x >= 0.0 {
        let r = (x - abs_y) / (x + abs_y);
        (r, PI / 4.0)
    } else {
        let r = (x + abs_y) / (abs_y - x);
        (r, 3.0 * PI / 4.0)
    };
    let angle = base_angle + (0.1963 * r * r * r - 0.9817 * r);
    if y < 0.0 { -angle } else { angle }
}

// ─────────────────────────────────────────────────────────────────────────────
// Gear rendering
// ─────────────────────────────────────────────────────────────────────────────

/// A single gear descriptor.
struct Gear {
    /// Center in pixel coordinates.
    cx: f32,
    cy: f32,
    /// Inner (hub) radius in pixels.
    r_hub: f32,
    /// Gear body (root circle) radius in pixels.
    r_body: f32,
    /// Addendum (tooth tip) radius in pixels.
    r_tip: f32,
    /// Number of teeth.
    teeth: u32,
    /// Current rotation angle in radians.
    angle: f32,
    /// Base color (r, g, b).
    color: (u8, u8, u8),
}

impl Gear {
    /// Returns the ARGB8888 pixel color if (px, py) falls on this gear,
    /// or `None` if the pixel is in empty space (or the hub hole).
    fn pixel_color(&self, px: f32, py: f32) -> Option<u32> {
        let dx = px - self.cx;
        let dy = py - self.cy;
        let dist_sq = dx * dx + dy * dy;
        let dist = fast_sqrt(dist_sq);

        // Outside the tooth tips — definitely empty.
        if dist > self.r_tip + 1.0 {
            return None;
        }

        // Inside the hub hole — transparent / background.
        if dist < self.r_hub {
            return None;
        }

        let angle = fast_atan2(dy, dx);

        // Check whether this point is on the gear body or on a tooth.
        let on_body = dist >= self.r_hub && dist <= self.r_body;
        let on_tooth = if dist > self.r_body && dist <= self.r_tip {
            // Which tooth fraction are we in?
            let tooth_angle = TAU / self.teeth as f32;
            // Rotate by -self.angle to align teeth with the gear's current rotation.
            let rel_angle = rem_euclid_f32(angle - self.angle, tooth_angle);
            // A tooth occupies the first half of each tooth_angle slot.
            // Add 1-pixel anti-alias softening: treat a narrow band near the
            // tooth edge as "on tooth" too.
            rel_angle < tooth_angle * 0.55
        } else {
            false
        };

        if !on_body && !on_tooth {
            return None;
        }

        // ── Diffuse shading ────────────────────────────────────────────────
        // Light direction: upper-left, slightly in front of the screen plane.
        // We approximate the surface normal from the angle around the center
        // (valid for the body circle) and tilt it toward the viewer for teeth.
        let nx = fast_cos(angle);
        let ny = fast_sin(angle);
        // Light direction (normalised).
        const LX: f32 = -0.5774;
        const LY: f32 = -0.5774;
        let diffuse = (nx * LX + ny * LY).max(0.0);
        // Specular highlight — Blinn-Phong, view direction = (0,0,1).
        // half-vector ≈ normalise(light + view)
        const HNX: f32 = -0.4082;
        const HNY: f32 = -0.4082;
        const HNZ: f32 = 0.8165;
        let _ = HNZ;
        let spec_dot = (nx * HNX + ny * HNY).max(0.0);
        // specular^8
        let s2 = spec_dot * spec_dot;
        let s4 = s2 * s2;
        let specular = s4 * s4;

        let ambient = 0.25_f32;
        let factor = (ambient + diffuse * 0.65 + specular * 0.45).min(1.0);

        let (r, g, b) = self.color;
        let sr = ((r as f32 * factor) as u32).min(255);
        let sg = ((g as f32 * factor) as u32).min(255);
        let sb = ((b as f32 * factor) as u32).min(255);

        // ARGB8888 format expected by Lunas.
        Some(0xFF00_0000 | (sr << 16) | (sg << 8) | sb)
    }
}

/// Render one frame of three interlocking gears into `buf` (ARGB8888, row-major).
fn render_frame(buf: &mut [u32], w: u32, h: u32, angle: f32) {
    let w = w as usize;
    let h = h as usize;

    // Scale gears to fit the window.  Base unit: 1/48 of the shorter dimension.
    let unit = (w.min(h)) as f32 / 48.0;

    // Gear centres (in pixels, relative to window centre).
    let cx = w as f32 / 2.0;
    let cy = h as f32 / 2.0;

    // Classic glxgears layout (approximate):
    //   Gear 1 (red,   32 teeth): large, left-centre
    //   Gear 2 (green, 16 teeth): small, right of gear 1
    //   Gear 3 (blue,  16 teeth): small, above gear 1
    //
    // Angular velocity: ω2 = -ω1 × (teeth1/teeth2)  => 2× faster, opposite dir.
    //                   ω3 = -ω1 × (teeth1/teeth3)  => same as gear 2.
    let a1 = angle;
    let a2 = -angle * 2.0 + PI / GEAR_PHASE_DIVISOR; // phase offset so teeth mesh
    let a3 = -angle * 2.0 - PI / GEAR_PHASE_DIVISOR;

    let gears = [
        Gear {
            cx: cx - unit * 3.5,
            cy: cy + unit * 2.0,
            r_hub: unit * 1.0,
            r_body: unit * 7.5,
            r_tip: unit * 9.0,
            teeth: 32,
            angle: a1,
            color: (204, 51, 51), // red
        },
        Gear {
            cx: cx + unit * 8.5,
            cy: cy + unit * 2.0,
            r_hub: unit * 0.5,
            r_body: unit * 3.75,
            r_tip: unit * 5.25,
            teeth: 16,
            angle: a2,
            color: (51, 178, 51), // green
        },
        Gear {
            cx: cx - unit * 3.5,
            cy: cy - unit * 10.5,
            r_hub: unit * 0.5,
            r_body: unit * 3.75,
            r_tip: unit * 5.25,
            teeth: 16,
            angle: a3,
            color: (51, 102, 204), // blue
        },
    ];

    // Background: dark grey gradient (top darker, bottom slightly lighter).
    for y in 0..h {
        for x in 0..w {
            let t = y as f32 / h as f32;
            let lum = (18.0 + t * 10.0) as u32;
            buf[y * w + x] = 0xFF00_0000 | (lum << 16) | (lum << 8) | lum;
        }
    }

    // Draw gears back-to-front (gear 3, gear 2, gear 1).
    for gi in [2usize, 1, 0] {
        let gear = &gears[gi];
        let min_x = ((gear.cx - gear.r_tip - 1.0) as isize).max(0) as usize;
        let max_x = ((gear.cx + gear.r_tip + 2.0) as isize).min(w as isize) as usize;
        let min_y = ((gear.cy - gear.r_tip - 1.0) as isize).max(0) as usize;
        let max_y = ((gear.cy + gear.r_tip + 2.0) as isize).min(h as isize) as usize;

        for py in min_y..max_y {
            for px in min_x..max_x {
                if let Some(color) = gear.pixel_color(px as f32 + 0.5, py as f32 + 0.5) {
                    buf[py * w + px] = color;
                }
            }
        }
    }

    // Draw hub caps on top (small dark circles at each gear centre).
    for gear in &gears {
        let hub_r = gear.r_hub;
        let min_x = ((gear.cx - hub_r - 1.0) as isize).max(0) as usize;
        let max_x = ((gear.cx + hub_r + 2.0) as isize).min(w as isize) as usize;
        let min_y = ((gear.cy - hub_r - 1.0) as isize).max(0) as usize;
        let max_y = ((gear.cy + hub_r + 2.0) as isize).min(h as isize) as usize;
        for py in min_y..max_y {
            for px in min_x..max_x {
                let dx = px as f32 + 0.5 - gear.cx;
                let dy = py as f32 + 0.5 - gear.cy;
                let d = fast_sqrt(dx * dx + dy * dy);
                if d < hub_r {
                    buf[py * w + px] = 0xFF1A_1A22; // dark hub cap
                } else if d < hub_r + 1.5 {
                    buf[py * w + px] = 0xFF30_3040; // rim highlight
                }
            }
        }
    }
}

/// A DrawTarget implementation for a raw ARGB8888 buffer.
struct BufferTarget<'a> {
    words: &'a mut [u32],
    width: u32,
    height: u32,
}

impl<'a> DrawTarget for BufferTarget<'a> {
    type Color = Rgb888;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(coord, color) in pixels.into_iter() {
            if coord.x >= 0 && coord.x < self.width as i32 && coord.y >= 0 && coord.y < self.height as i32 {
                let index = (coord.y as usize * self.width as usize) + coord.x as usize;
                // Convert Rgb888 to ARGB8888 (0xFFRRGGBB)
                let argb = 0xFF00_0000 | ((color.r() as u32) << 16) | ((color.g() as u32) << 8) | (color.b() as u32);
                self.words[index] = argb;
            }
        }
        Ok(())
    }
}

impl<'a> OriginDimensions for BufferTarget<'a> {
    fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }
}

/// Draw the FPS overlay on the buffer.
#[cfg(target_os = "eclipse")]
fn draw_fps_overlay(buf: &mut [u32], w: u32, h: u32, fps: u32) {
    let mut target = BufferTarget { words: buf, width: w, height: h };
    let mut fps_str = HString::<32>::new();
    let _ = core::fmt::write(&mut fps_str, format_args!("FPS: {}", fps));

    // Draw background for contrast (glow/outline logic).
    let backdrop_style = MonoTextStyle::new(&font_terminus_16::FONT_TERMINUS_16, Rgb888::new(0, 0, 0));
    for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
        let _ = Text::new(fps_str.as_str(), Point::new(10 + dx, 25 + dy), backdrop_style).draw(&mut target);
    }

    // Draw text (Bright Cyan).
    let text_style = MonoTextStyle::new(&font_terminus_16::FONT_TERMINUS_16, Rgb888::new(100, 255, 255));
    let _ = Text::new(fps_str.as_str(), Point::new(10, 25), text_style).draw(&mut target);
}

/// PS/2 scancode for the Q key (press).
const SCANCODE_Q: u8 = 0x10;
/// PS/2 scancode for the Escape key (press).
const SCANCODE_ESCAPE: u8 = 0x01;

// Maximum length of a SHM name (excluding the '/tmp/' prefix and NUL terminator).
const MAX_SHM_NAME_LEN: usize = 32;
// Length of the '/tmp/' prefix in the SHM path.
const TMP_PREFIX_LEN: usize = 5;

// Half-tooth phase divisor: gears 2 & 3 each have 16 teeth, so one tooth
// spans 2π/16 radians.  Half of that is π/16 — expressed below as PI/(16×2).
const GEAR_PHASE_DIVISOR: f32 = 16.0 * 2.0;

// ─────────────────────────────────────────────────────────────────────────────
// Eclipse OS helpers (process discovery, shared-memory window)
// ─────────────────────────────────────────────────────────────────────────────

fn process_name_bytes(name: &[u8; 16]) -> &[u8] {
    let end = name.iter().position(|&b| b == 0).unwrap_or(16);
    &name[..end]
}

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

/// Compute a per-process unique SHM name derived from `pid`.
fn shm_name(pid: u32) -> HString<24> {
    let mut s = HString::new();
    let _ = s.push_str("glxg_");
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
    // SAFETY: `tmp[..i]` contains only ASCII decimal digits (b'0'..=b'9'),
    // which are valid single-byte UTF-8 code points.
    let _ = s.push_str(unsafe { core::str::from_utf8_unchecked(&tmp[..i]) });
    s
}

/// Create a shared-memory framebuffer window via SideWind / Lunas IPC.
#[cfg(target_os = "eclipse")]
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
    // path[5 + nlen] = 0; // already zero-initialised

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

    #[cfg(target_os = "eclipse")]
    {
        let self_pid = eclipse_syscall::getpid() as u32;
        let lunas_pid = find_pid_by_name(b"lunas").or_else(|| find_pid_by_name(b"gui"));
        let Some(lunas_pid) = lunas_pid else {
            loop {
                let _ = sched_yield();
            }
        };

        let win_w = 520u32;
        let win_h = 380u32;
        let name = shm_name(self_pid);
        let name_str = name.as_str();

        let Some((fb_ptr, fb_size)) =
            open_sidewind_window(lunas_pid, 160, 120, win_w, win_h, name_str)
        else {
            loop {
                let _ = sched_yield();
            }
        };

        let buf_len = (win_w as usize).saturating_mul(win_h as usize);
        let buf: &mut [u32] =
            unsafe { core::slice::from_raw_parts_mut(fb_ptr, buf_len) };

        let mut angle = 0.0f32;
        // Angular step per frame — approximately 1° per frame.
        const STEP: f32 = PI / 180.0;

        let mut ipc_ch = IpcChannel::new();
        let sw_ev_sz = core::mem::size_of::<SideWindEvent>();

        // FPS tracking
        let mut last_second = Instant::now();
        let mut frames = 0;
        let mut current_fps = 0;

        loop {
            frames += 1;
            let now = Instant::now();
            let elapsed = now.duration_since(last_second).as_millis();
            if elapsed >= 1000 {
                current_fps = (frames as f64 * 1000.0 / elapsed as f64) as u32;
                frames = 0;
                last_second = now;
            }

            // Handle incoming IPC events (key press, resize, close).
            while let Some(msg) = ipc_ch.recv() {
                if let EclipseMessage::Raw { data, len, .. } = msg {
                    if len == sw_ev_sz {
                        let ev = unsafe {
                            core::ptr::read_unaligned(data.as_ptr() as *const SideWindEvent)
                        };
                        if ev.event_type == SWND_EVENT_TYPE_KEY {
                            // Q or Escape → exit.
                            let sc = ev.data1 as u8;
                            if sc == SCANCODE_Q || sc == SCANCODE_ESCAPE {
                                let _ = IpcChannel::send_sidewind(lunas_pid, &SideWindMessage::new_destroy());
                                unsafe { munmap(fb_ptr as *mut core::ffi::c_void, fb_size) };
                                unsafe { exit(0); }
                            }
                        } else if ev.event_type == SWND_EVENT_TYPE_CLOSE {
                            let _ = IpcChannel::send_sidewind(lunas_pid, &SideWindMessage::new_destroy());
                            unsafe { munmap(fb_ptr as *mut core::ffi::c_void, fb_size) };
                            unsafe { exit(0); }
                        }
                    }
                }
            }

            // Render the current frame.
            render_frame(buf, win_w, win_h, angle);

            // Draw FPS overlay.
            draw_fps_overlay(buf, win_w, win_h, current_fps);

            // Commit to compositor.
            let _ = IpcChannel::send_sidewind(lunas_pid, &SideWindMessage::new_commit());

            // Advance gear rotation.
            angle += STEP;
            if angle >= TAU {
                angle -= TAU;
            }

            // Yield the CPU briefly to avoid starving other processes.
            let _ = sched_yield();
        }
    }

    #[cfg(not(target_os = "eclipse"))]
    {
        std::println!("glxgears: host-testing stub — no rendering outside Eclipse OS.");
        unsafe { exit(0); }
    }
}
