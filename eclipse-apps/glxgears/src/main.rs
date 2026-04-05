//! glxgears — Eclipse OS
//!
//! Software-rendered rotating gears demo, inspired by the classic glxgears
//! OpenGL benchmark. Three interlocking gears (red, green, blue) rotate in
//! real time inside a SideWind window managed by the Lunas compositor.


use heapless::String as HString;

use std::time::Instant;
use embedded_graphics::{
    prelude::*,
    pixelcolor::Rgb888,
    text::Text,
    mono_font::{MonoTextStyle, ascii::FONT_10X20},
};
#[cfg(target_os = "eclipse")]
use sidewind::font_terminus_16;
#[cfg(target_os = "eclipse")]
use libc::{c_char, c_int, close, mmap, open, exit};
#[cfg(target_os = "eclipse")]
use wayland_proto::unix_transport::UnixSocketConnection;
#[cfg(target_os = "eclipse")]
use wayland_proto::wl::wire::{RawMessage, ObjectId, NewId, Opcode, Payload, PayloadType};
#[cfg(target_os = "eclipse")]
use wayland_proto::wl::connection::Connection;
#[cfg(target_os = "eclipse")]
use eclipse_syscall::{self, flag};
#[cfg(target_os = "eclipse")]
use eclipse_syscall::call::sched_yield;

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
#[cfg(any(target_os = "eclipse", target_os = "linux"))]
fn draw_fps_overlay(buf: &mut [u32], w: u32, h: u32, fps: u32) {
    let mut target = BufferTarget { words: buf, width: w, height: h };
    let mut fps_str = HString::<32>::new();
    let _ = core::fmt::write(&mut fps_str, format_args!("FPS: {}", fps));

    // For Linux or as fallback, use built-in font. 
    // On eclipse we could still use terminus if we wanted, but let's standardize for now.
    let font = &FONT_10X20;
    
    // Draw background for contrast (glow/outline logic).
    // std::println!("[GLXGEARS-DEBUG] Drawing FPS backdrop...");
    let backdrop_style = MonoTextStyle::new(font, Rgb888::new(0, 0, 0));
    for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
        let _ = Text::new(fps_str.as_str(), Point::new(10 + dx, 25 + dy), backdrop_style).draw(&mut target);
    }

    // std::println!("[GLXGEARS-DEBUG] Drawing FPS text...");
    let text_style = MonoTextStyle::new(font, Rgb888::new(100, 255, 255));
    let _ = Text::new(fps_str.as_str(), Point::new(10, 25), text_style).draw(&mut target);
}

/// PS/2 scancode for the Q key (press).
const SCANCODE_Q: u8 = 0x10;
/// PS/2 scancode for the Escape key (press).
const SCANCODE_ESCAPE: u8 = 0x01;

// Half-tooth phase divisor: gears 2 & 3 each have 16 teeth, so one tooth
// spans 2π/16 radians.  Half of that is π/16 — expressed below as PI/(16×2).
const GEAR_PHASE_DIVISOR: f32 = 16.0 * 2.0;

// ─────────────────────────────────────────────────────────────────────────────
// Eclipse OS helpers (process discovery, shared-memory window)
// ─────────────────────────────────────────────────────────────────────────────

/// Send a Wayland message on the Unix socket connection.
#[cfg(target_os = "eclipse")]
fn send_wayland(conn: &UnixSocketConnection, object: u32, opcode: u16, args: &[Payload]) {
    let _ = conn.send(ObjectId(object), Opcode(opcode), args, &[]);
}

/// Send a Wayland message with an ancillary file descriptor (SCM_RIGHTS).
#[cfg(target_os = "eclipse")]
fn send_wayland_with_fd(conn: &UnixSocketConnection, object: u32, opcode: u16, args: &[Payload], fd: i32) {
    use wayland_proto::wl::wire::Handle;
    let _ = conn.send(ObjectId(object), Opcode(opcode), args, &[Handle(fd)]);
}

/// Compute a per-process unique SHM name derived from `pid`.
#[cfg(target_os = "eclipse")]
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

/// Create a shared-memory framebuffer window via Wayland.
#[cfg(target_os = "eclipse")]
struct GlxGearsApp {
    wayland: UnixSocketConnection,
    surface_id: u32,
    keyboard_id: u32,
    fb_ptr: *mut u32,
    width: u32,
    height: u32,
}

#[cfg(target_os = "eclipse")]
impl GlxGearsApp {
    fn new() -> Option<Self> {
        let self_pid = eclipse_syscall::getpid() as u32;
        let win_w = 520u32;
        let win_h = 380u32;
        let size_bytes = (win_w as usize) * (win_h as usize) * 4;

        // 1. Allocate shared-memory framebuffer
        let sname = shm_name(self_pid);
        let path = format!("/tmp/{}\0", sname.as_str());
        std::println!("[GLXGEARS] Creating SHM file {}...", path);
        let fd = unsafe {
            open(path.as_ptr() as *const c_char, (flag::O_RDWR | flag::O_CREAT) as c_int, 0o644)
        };
        if fd < 0 { 
            std::println!("[GLXGEARS] open() failed: {}", fd);
            return None; 
        }
        if eclipse_syscall::ftruncate(fd as usize, size_bytes).is_err() {
            std::println!("[GLXGEARS] ftruncate() failed!");
            unsafe { close(fd) };
            return None;
        }
        let vaddr = unsafe {
            mmap(core::ptr::null_mut(), size_bytes,
                 (flag::PROT_READ | flag::PROT_WRITE) as c_int,
                 flag::MAP_SHARED as c_int, fd, 0)
        };
        if vaddr.is_null() || vaddr == (-1isize as *mut core::ffi::c_void) {
            std::println!("[GLXGEARS] mmap() failed!");
            unsafe { close(fd) };
            return None;
        }

        // 2. Connect to Wayland
        std::println!("[GLXGEARS] Connecting to Wayland socket...");
        let wayland = UnixSocketConnection::connect("/tmp/wayland-0")?;
        wayland.set_nonblocking();
        std::println!("[GLXGEARS] Connected to Wayland.");

        // 3. Handshake (similar to terminal)
        // wl_display.get_registry(id=2)
        std::println!("[GLXGEARS] Handshaking (Registry)...");
        send_wayland(&wayland, 1, 1, &[Payload::NewId(NewId(2))]);

        let mut compositor_name = 0u32;
        let mut shm_name_id = 0u32;
        let mut xdg_name = 0u32;
        let mut seat_name = 0u32;

        // Lunas solo procesa el socket en `wayland_socket.poll()` una vez por frame
        // (~16 ms de sleep en el main loop). Hace falta más paciencia que un bucle corto
        // de yields; el terminal usa 5000 iteraciones por la misma razón.
        for _ in 0..5000 {
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
                                if iface == "wl_compositor" { compositor_name = name; }
                                else if iface == "wl_shm" { shm_name_id = name; }
                                else if iface == "xdg_wm_base" { xdg_name = name; }
                                else if iface == "wl_seat" { seat_name = name; }
                            }
                        }
                        pos += msg_len.min(data.len() - pos);
                    } else { break; }
                }
            }
            if compositor_name != 0 && shm_name_id != 0 && xdg_name != 0 { break; }
            let _ = sched_yield();
        }

        // Igual que el terminal: wl_seat puede llegar en el mismo lote o un poco después.
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
                let _ = sched_yield();
            }
        }

        if compositor_name == 0 { 
            std::println!("[GLXGEARS] Failed to discover compositor!");
            return None; 
        }
        std::println!("[GLXGEARS] Globals discovered.");

        // Bind globals
        send_wayland(&wayland, 2, 0, &[Payload::UInt(compositor_name), Payload::String(std::string::String::from("wl_compositor")), Payload::UInt(4), Payload::NewId(NewId(3))]);
        send_wayland(&wayland, 2, 0, &[Payload::UInt(shm_name_id), Payload::String(std::string::String::from("wl_shm")), Payload::UInt(1), Payload::NewId(NewId(4))]);
        send_wayland(&wayland, 2, 0, &[Payload::UInt(xdg_name), Payload::String(std::string::String::from("xdg_wm_base")), Payload::UInt(2), Payload::NewId(NewId(5))]);
        if seat_name != 0 {
            send_wayland(&wayland, 2, 0, &[Payload::UInt(seat_name), Payload::String(std::string::String::from("wl_seat")), Payload::UInt(7), Payload::NewId(NewId(6))]);
        }

        // Create surface
        send_wayland(&wayland, 3, 0, &[Payload::NewId(NewId(7))]);

        // Create SHM pool
        std::println!("[GLXGEARS] Creating SHM pool...");
        send_wayland_with_fd(&wayland, 4, 0, &[
            Payload::NewId(NewId(8)),
            Payload::Handle(wayland_proto::wl::wire::Handle(fd)),
            Payload::Int(size_bytes as i32),
        ], fd);

        // Create buffer
        let stride = (win_w * 4) as i32;
        send_wayland(&wayland, 8, 0, &[
            Payload::NewId(NewId(9)),
            Payload::Int(0),
            Payload::Int(win_w as i32), Payload::Int(win_h as i32),
            Payload::Int(stride),
            Payload::UInt(1), // XRGB8888
        ]);

        // XDG setup
        send_wayland(&wayland, 5, 1, &[Payload::NewId(NewId(10)), Payload::ObjectId(ObjectId(7))]);
        send_wayland(&wayland, 10, 1, &[Payload::NewId(NewId(11))]);
        send_wayland(&wayland, 11, 2, &[Payload::String(std::string::String::from("glxgears"))]);

        if seat_name != 0 {
            send_wayland(&wayland, 6, 1, &[Payload::NewId(NewId(12))]);
        }

        // Initial attach & commit
        send_wayland(&wayland, 7, 1, &[Payload::ObjectId(ObjectId(9)), Payload::Int(0), Payload::Int(0)]);
        send_wayland(&wayland, 7, 6, &[]);

        std::println!("[GLXGEARS] Application state built.");

        Some(Self {
            wayland,
            surface_id: 7,
            keyboard_id: if seat_name != 0 { 12 } else { 0 },
            fb_ptr: vaddr as *mut u32,
            width: win_w,
            height: win_h,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

fn main() {
    #[cfg(target_os = "eclipse")]
    {
        std::println!("[GLXGEARS] Starting recruitment of app...");
        let Some(app) = GlxGearsApp::new() else {
            std::println!("[GLXGEARS] Failed to initialize GlxGearsApp!");
            loop { let _ = sched_yield(); }
        };
        std::println!("[GLXGEARS] App initialized successfully!");

        let buf_len = (app.width as usize) * (app.height as usize);
        let buf: &mut [u32] = unsafe { core::slice::from_raw_parts_mut(app.fb_ptr, buf_len) };
        std::println!("[GLXGEARS] Buffer mapped at {:p} (len={})", app.fb_ptr, buf_len);

        let mut angle = 0.0f32;
        const STEP: f32 = PI / 180.0;

        let mut last_second = Instant::now();
        let mut frames = 0;
        let mut current_fps = 0;

        std::println!("[GLXGEARS] Entering main loop...");
        loop {
            frames += 1;
            let now = Instant::now();
            let elapsed = now.duration_since(last_second).as_millis();
            if elapsed >= 1000 {
                // std::println!("[GLXGEARS] Calculating FPS (frames={}, elapsed={})...", frames, elapsed);
                current_fps = (frames as f64 * 1000.0 / elapsed as f64) as u32;
                frames = 0;
                last_second = now;
            }

            // Wayland events
            while let Ok((data, _)) = app.wayland.recv() {
                let mut pos = 0usize;
                while pos + 8 <= data.len() {
                    if let Ok((sender, opcode, msg_len)) = RawMessage::deserialize_header(&data[pos..]) {
                        let chunk = &data[pos..pos + msg_len.min(data.len() - pos)];
                        
                        // xdg_wm_base.ping (sender=5, opcode=0) -> pong
                        if sender == ObjectId(5) && opcode == Opcode(0) {
                            let pts = &[PayloadType::UInt];
                            if let Ok(raw) = RawMessage::deserialize(chunk, pts, &[]) {
                                let serial = match raw.args.get(0) { Some(Payload::UInt(s)) => *s, _ => 0 };
                                send_wayland(&app.wayland, 5, 3, &[Payload::UInt(serial)]);
                            }
                        }
                        // xdg_surface.configure (sender=10, opcode=0) -> ack_configure
                        else if sender == ObjectId(10) && opcode == Opcode(0) {
                            let pts = &[PayloadType::UInt];
                            if let Ok(raw) = RawMessage::deserialize(chunk, pts, &[]) {
                                let serial = match raw.args.get(0) { Some(Payload::UInt(s)) => *s, _ => 0 };
                                send_wayland(&app.wayland, 10, 4, &[Payload::UInt(serial)]);
                            }
                        }
                        // xdg_toplevel.close (sender=11, opcode=1)
                        else if sender == ObjectId(11) && opcode == Opcode(1) {
                            std::println!("[GLXGEARS] Received close event, exiting.");
                            unsafe { exit(0); }
                        }
                        // wl_keyboard.key
                        else if app.keyboard_id != 0 && sender == ObjectId(app.keyboard_id) && opcode == Opcode(3) {
                            let pts = &[PayloadType::UInt, PayloadType::UInt, PayloadType::UInt, PayloadType::UInt];
                            if let Ok(raw) = RawMessage::deserialize(chunk, pts, &[]) {
                                let key = match raw.args.get(2) { Some(Payload::UInt(k)) => *k, _ => 0 };
                                let state = match raw.args.get(3) { Some(Payload::UInt(s)) => *s, _ => 0 };
                                if state == 1 {
                                    let sc = if key >= 8 { (key - 8) as u8 } else { key as u8 };
                                    if sc == SCANCODE_Q || sc == SCANCODE_ESCAPE {
                                        std::println!("[GLXGEARS] Key pressed, exiting.");
                                        unsafe { exit(0); }
                                    }
                                }
                            }
                        }
                        pos += msg_len.min(data.len() - pos);
                    } else { break; }
                }
            }

            render_frame(buf, app.width, app.height, angle);
            draw_fps_overlay(buf, app.width, app.height, current_fps);

            // Commit Wayland frame
            send_wayland(&app.wayland, app.surface_id, 2, &[Payload::Int(0), Payload::Int(0), Payload::Int(i32::MAX), Payload::Int(i32::MAX)]);
            send_wayland(&app.wayland, app.surface_id, 6, &[]);

            angle += STEP;
            if angle >= TAU { angle -= TAU; }
            let _ = sched_yield();
        }
    }

    #[cfg(target_os = "linux")]
    {
        use minifb::{Key, Window, WindowOptions};

        let width = 520usize;
        let height = 380usize;
        let mut buffer: Vec<u32> = vec![0; width * height];

        let mut window = Window::new(
            "glxgears — Linux (minifb)",
            width,
            height,
            WindowOptions::default(),
        )
        .unwrap_or_else(|e| {
            panic!("{}", e);
        });

        window.limit_update_rate(Some(std::time::Duration::from_micros(16600)));

        let mut angle = 0.0f32;
        const STEP: f32 = core::f32::consts::PI / 180.0;

        let mut last_second = std::time::Instant::now();
        let mut frames = 0;
        let mut current_fps = 0;

        while window.is_open() && !window.is_key_down(Key::Escape) && !window.is_key_down(Key::Q) {
            frames += 1;
            let now = std::time::Instant::now();
            let elapsed = now.duration_since(last_second).as_millis();
            if elapsed >= 1000 {
                current_fps = (frames as f64 * 1000.0 / elapsed as f64) as u32;
                frames = 0;
                last_second = now;
            }

            render_frame(&mut buffer, width as u32, height as u32, angle);
            draw_fps_overlay(&mut buffer, width as u32, height as u32, current_fps);
            
            // Draw FPS (rudimentary Linux overlay)
            let fps_msg = format!("FPS: {}", current_fps);
            window.set_title(&format!("glxgears — Linux (minifb) | {}", fps_msg));

            window
                .update_with_buffer(&buffer, width, height)
                .unwrap_or_else(|e| { panic!("{}", e); });

            angle += STEP;
            if angle >= core::f32::consts::TAU { angle -= core::f32::consts::TAU; }
        }
    }
}
