//! Compositor de pantalla para Linux (host).
//!
//! Usa winit + softbuffer (buffer de píxeles en software) en lugar de
//! smithay + EGL/OpenGL.  Ventajas:
//!   - No carga libGL / libEGL ni drivers de GPU en el arranque.
//!   - Crea muchos menos VMAs, evitando el fallo de sigaltstack guard page
//!     ("failed to set up alternative stack guard page: Cannot allocate memory")
//!     que se producía con la versión smithay+GlesRenderer.
//!   - Muestra el framebuffer Eclipse real dibujado con la CPU.

use std::num::NonZeroU32;
use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

const DEFAULT_W: u32 = 1280;
const DEFAULT_H: u32 = 720;

// --- Software framebuffer render ----------------------------------------

/// Render a simple "Eclipse OS compositor (Linux test mode)" screen into
/// the 32-bit XRGB pixel buffer.
fn render_frame(buf: &mut [u32], width: u32, height: u32, tick: u64) {
    let w = width as usize;
    let h = height as usize;

    // Background: dark cosmic blue gradient
    for y in 0..h {
        for x in 0..w {
            let r = (2u32).saturating_add((x as u32 * 10) / width);
            let g = (2u32).saturating_add((y as u32 * 8) / height);
            let b = (16u32).saturating_add((y as u32 * 20) / height);
            buf[y * w + x] = 0xFF000000 | (r << 16) | (g << 8) | b;
        }
    }

    // Grid lines (perspective-style)
    let grid_spacing: usize = 64;
    let scroll = (tick % 128) as usize;
    for y in (0..h).step_by(grid_spacing) {
        let ys = (y + scroll) % h;
        for x in 0..w {
            let idx = ys * w + x;
            buf[idx] = blend_pixel(buf[idx], 0xFF1A2855, 80);
        }
    }
    for x in (0..w).step_by(grid_spacing) {
        for y in 0..h {
            let idx = y * w + x;
            buf[idx] = blend_pixel(buf[idx], 0xFF1A2855, 80);
        }
    }

    // Sidebar (left panel) — clamp sb_w so it's always strictly < w.
    let sb_w = ((width / 8).clamp(60, 200) as usize).min(w.saturating_sub(1));
    for y in 0..h {
        for x in 0..sb_w {
            buf[y * w + x] = 0xFF080F20;
        }
    }
    // Sidebar border (only if there's room to the right of sb_w)
    if sb_w < w {
        for y in 0..h {
            buf[y * w + sb_w] = 0xFF0050A0;
        }
    }

    // Pulsing circle (logo placeholder): compute pulse once, outside the pixel loops.
    let cx = (w / 2) as i32;
    let cy = (h / 2) as i32;
    let r_max = (width.min(height) / 3) as i32;
    let pulse = ((tick % 120) as f64 / 120.0 * std::f64::consts::TAU).sin();
    let r = (r_max as f64 * (0.9 + pulse * 0.05)) as i32;

    for dy in -r..=r {
        for dx in -r..=r {
            let d2 = dx * dx + dy * dy;
            if d2 > r * r { continue; }
            let dist = (d2 as f64).sqrt();
            let rim_start = (r - 4) as f64;
            if dist < rim_start { continue; }

            let px = (cx + dx) as usize;
            let py = (cy + dy) as usize;
            if px >= w || py >= h { continue; }

            // Cyan ring color
            let alpha = ((r as f64 - dist) * 60.0) as u32;
            let alpha = alpha.min(255);
            let rim_color = 0xFF00D4FF;
            buf[py * w + px] = blend_pixel(buf[py * w + px], rim_color, alpha);
        }
    }

    // HUD text representation (simple colored bars since we have no font)
    draw_text_bar(buf, w, h, sb_w + 20, 20, "ECLIPSE OS COMPOSITOR", 0xFF00D4FF, tick);
    draw_text_bar(buf, w, h, sb_w + 20, 50, "Linux Test Mode (softbuffer)", 0xFFFFFFFF, 0);
    draw_text_bar(buf, w, h, sb_w + 20, 80, "Presiona ESC para salir", 0xFF888888, 0);
}

/// Blend src over dst with `alpha` (0=transparent, 255=opaque).
#[inline]
fn blend_pixel(dst: u32, src: u32, alpha: u32) -> u32 {
    let ia = 255 - alpha;
    let r = ((src >> 16 & 0xFF) * alpha + (dst >> 16 & 0xFF) * ia) / 255;
    let g = ((src >> 8 & 0xFF) * alpha + (dst >> 8 & 0xFF) * ia) / 255;
    let b = ((src & 0xFF) * alpha + (dst & 0xFF) * ia) / 255;
    0xFF000000 | (r << 16) | (g << 8) | b
}

/// Draw a coloured label as a simple bar (pixel-level, no font needed).
fn draw_text_bar(buf: &mut [u32], w: usize, h: usize, x: usize, y: usize, _label: &str, color: u32, tick: u64) {
    let bar_h = 10usize;
    let bar_w = 200usize;
    let blink = (tick / 30) % 2 == 0;
    // When not blinking: halve each RGB component (divide by 2) while keeping alpha=FF.
    // `(color >> 1) & 0x7F7F7F` shifts right by 1 (halves) and masks out the sign bit of each byte.
    let draw_color = if blink { color } else { ((color >> 1) & 0x7F7F7F) | 0xFF000000 };
    for dy in 0..bar_h {
        for dx in 0..bar_w {
            let px = x + dx;
            let py = y + dy;
            if px >= w || py >= h { continue; }
            buf[py * w + px] = blend_pixel(buf[py * w + px], draw_color, 200);
        }
    }
}

// --- winit ApplicationHandler -------------------------------------------

struct App {
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    context: Option<softbuffer::Context<Arc<Window>>>,
    tick: u64,
}

impl App {
    fn new() -> Self {
        Self { window: None, surface: None, context: None, tick: 0 }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() { return; }

        let attrs = Window::default_attributes()
            .with_title("Eclipse OS — Compositor (Linux test mode)")
            .with_inner_size(LogicalSize::new(DEFAULT_W, DEFAULT_H));

        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("[smithay_app] create_window failed: {e}");
                event_loop.exit();
                return;
            }
        };

        let context = match softbuffer::Context::new(window.clone()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[smithay_app] softbuffer::Context::new failed: {e}");
                event_loop.exit();
                return;
            }
        };

        let surface = match softbuffer::Surface::new(&context, window.clone()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[smithay_app] softbuffer::Surface::new failed: {e}");
                event_loop.exit();
                return;
            }
        };

        self.context = Some(context);
        self.surface = Some(surface);
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            WindowEvent::KeyboardInput {
                event: KeyEvent { physical_key: PhysicalKey::Code(KeyCode::Escape), state: ElementState::Pressed, .. },
                ..
            } => {
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
                let Some(window) = &self.window else { return };
                let Some(surface) = &mut self.surface else { return };

                let size = window.inner_size();
                let (width, height) = (size.width, size.height);
                // Both checked for zero before creating NonZeroU32 — safe to unwrap.
                let (Some(w_nz), Some(h_nz)) = (NonZeroU32::new(width), NonZeroU32::new(height)) else {
                    return;
                };
                if surface.resize(w_nz, h_nz).is_err() { return; }

                if let Ok(mut buf) = surface.buffer_mut() {
                    render_frame(&mut buf, width, height, self.tick);
                    self.tick += 1;
                    let _ = buf.present();
                }
                window.request_redraw();
            }
            WindowEvent::Resized(_) => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            _ => {}
        }
    }
}

// --- Public entry point --------------------------------------------------

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    println!("[smithay_app] Iniciando en modo Linux (softbuffer, sin EGL/GL).");
    println!("[smithay_app] Presiona ESC o cierra la ventana para salir.");

    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = App::new();
    event_loop.run_app(&mut app)?;

    Ok(())
}
