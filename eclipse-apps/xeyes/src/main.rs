//! xeyes — X11 eye-tracking demo using x11rb
//!
//! Connects to the X11 server (XWayland on Eclipse OS, or any X server on Linux)
//! and draws a pair of animated eyes whose pupils follow the mouse cursor.
//!
//! On Eclipse OS, connect to the XWayland socket at `:0` set via `$DISPLAY`.

use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::errors::{ConnectionError, ReplyOrIdError};
use x11rb::protocol::shape::{self, ConnectionExt as ShapeExt};
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::COPY_DEPTH_FROM_PARENT;

// ─────────────────────────────────────────────────────────────────────────────
// Eye geometry constants
// ─────────────────────────────────────────────────────────────────────────────

const PUPIL_SIZE: i16 = 50;
const EYE_SIZE: i16 = 50;

// ─────────────────────────────────────────────────────────────────────────────
// Draw the white and black sclera/outline of both eyes
// ─────────────────────────────────────────────────────────────────────────────

fn draw_eyes<C: Connection>(
    conn: &C,
    win_id: Drawable,
    black_gc: Gcontext,
    white_gc: Gcontext,
    window_size: (u16, u16),
) -> Result<(), ConnectionError> {
    // Black outlines
    let mut arc1 = Arc {
        x: 0,
        y: 0,
        width: window_size.0 / 2,
        height: window_size.1,
        angle1: 0,
        angle2: 360 * 64,
    };
    let mut arc2 = arc1;
    arc2.x = arc2.width as i16;
    conn.poly_fill_arc(win_id, black_gc, &[arc1, arc2])?;

    // White inner part
    for arc in [&mut arc1, &mut arc2].iter_mut() {
        arc.x += EYE_SIZE;
        arc.y += EYE_SIZE;
        arc.width -= 2 * EYE_SIZE as u16;
        arc.height -= 2 * EYE_SIZE as u16;
    }
    conn.poly_fill_arc(win_id, white_gc, &[arc1, arc2])?;

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Draw both pupils at the given positions
// ─────────────────────────────────────────────────────────────────────────────

fn draw_pupils<C: Connection>(
    conn: &C,
    win_id: Drawable,
    gc: Gcontext,
    ((x1, y1), (x2, y2)): ((i16, i16), (i16, i16)),
) -> Result<(), ConnectionError> {
    let (x1, y1) = (x1 - PUPIL_SIZE / 2, y1 - PUPIL_SIZE / 2);
    let (x2, y2) = (x2 - PUPIL_SIZE / 2, y2 - PUPIL_SIZE / 2);

    let arc1 = Arc {
        x: x1,
        y: y1,
        width: PUPIL_SIZE as u16,
        height: PUPIL_SIZE as u16,
        angle1: 0,
        angle2: 360 * 64,
    };
    let mut arc2 = arc1;
    arc2.x = x2;
    arc2.y = y2;

    conn.poly_fill_arc(win_id, gc, &[arc1, arc2])?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Geometry helpers
// ─────────────────────────────────────────────────────────────────────────────

fn distance_squared(p1: (f64, f64), p2: (f64, f64)) -> f64 {
    let dx = p1.0 - p2.0;
    let dy = p1.1 - p2.1;
    dx * dx + dy * dy
}

/// Compute the position of one pupil inside the given eye area.
fn compute_pupil(area: (i16, i16, i16, i16), mouse: (i16, i16)) -> (i16, i16) {
    let center_x = area.0 + area.2 / 2;
    let center_y = area.1 + area.3 / 2;
    let (w, h) = (f64::from(area.2) / 2.0, f64::from(area.3) / 2.0);

    if (center_x, center_y) == mouse {
        return mouse;
    }

    let center = (f64::from(center_x), f64::from(center_y));
    let mouse_f = (f64::from(mouse.0), f64::from(mouse.1));
    let diff = (mouse_f.0 - center.0, mouse_f.1 - center.1);
    let angle = (diff.1 * w).atan2(diff.0 * h);
    let (cx, cy) = (w * angle.cos(), h * angle.sin());
    let (x, y) = ((center.0 + cx) as i16, (center.1 + cy) as i16);

    if distance_squared(center, mouse_f)
        < distance_squared(center, (f64::from(x), f64::from(y)))
    {
        mouse
    } else {
        (x, y)
    }
}

/// Compute positions of both pupils.
fn compute_pupils(
    window_size: (u16, u16),
    mouse_position: (i16, i16),
) -> ((i16, i16), (i16, i16)) {
    let border = PUPIL_SIZE + EYE_SIZE;
    let half_width = window_size.0 as i16 / 2;
    let width = half_width - 2 * border;
    let height = window_size.1 as i16 - 2 * border;

    (
        compute_pupil((border, border, width, height), mouse_position),
        compute_pupil(
            (border + half_width, border, width, height),
            mouse_position,
        ),
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Window setup
// ─────────────────────────────────────────────────────────────────────────────

fn setup_window<C: Connection>(
    conn: &C,
    screen: &Screen,
    window_size: (u16, u16),
    wm_protocols: Atom,
    wm_delete_window: Atom,
) -> Result<Window, ReplyOrIdError> {
    let win_id = conn.generate_id()?;
    let win_aux = CreateWindowAux::new()
        .event_mask(
            EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY | EventMask::POINTER_MOTION,
        )
        .background_pixel(screen.white_pixel);

    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        win_id,
        screen.root,
        0,
        0,
        window_size.0,
        window_size.1,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &win_aux,
    )?;

    conn.change_property8(
        PropMode::REPLACE,
        win_id,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        b"xeyes",
    )?;
    conn.change_property32(
        PropMode::REPLACE,
        win_id,
        wm_protocols,
        AtomEnum::ATOM,
        &[wm_delete_window],
    )?;

    conn.map_window(win_id)?;
    Ok(win_id)
}

// ─────────────────────────────────────────────────────────────────────────────
// Shape the window into two ellipses (optional; requires Shape extension)
// ─────────────────────────────────────────────────────────────────────────────

fn shape_window<C: Connection>(
    conn: &C,
    win_id: Window,
    window_size: (u16, u16),
) -> Result<(), ReplyOrIdError> {
    let pixmap =
        PixmapWrapper::create_pixmap(conn, 1, win_id, window_size.0, window_size.1)?;

    let gc = GcontextWrapper::create_gc(
        conn,
        pixmap.pixmap(),
        &CreateGCAux::new().graphics_exposures(0).foreground(0),
    )?;

    let rect = Rectangle {
        x: 0,
        y: 0,
        width: window_size.0,
        height: window_size.1,
    };
    conn.poly_fill_rectangle(pixmap.pixmap(), gc.gcontext(), &[rect])?;

    let values = ChangeGCAux::new().foreground(1);
    conn.change_gc(gc.gcontext(), &values)?;
    draw_eyes(
        conn,
        pixmap.pixmap(),
        gc.gcontext(),
        gc.gcontext(),
        window_size,
    )?;

    conn.shape_mask(
        shape::SO::SET,
        shape::SK::BOUNDING,
        win_id,
        0,
        0,
        &pixmap,
    )?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Connect to the X11 server.  On Eclipse OS this will be XWayland at :0.
    // The display string can be overridden with the DISPLAY environment variable.
    let (conn, screen_num) = x11rb::connect(None)?;
    let conn = std::sync::Arc::new(conn);

    let screen = &conn.setup().roots[screen_num];

    // Intern atoms needed for WM_DELETE_WINDOW protocol.
    let wm_protocols = conn.intern_atom(false, b"WM_PROTOCOLS")?;
    let wm_delete_window = conn.intern_atom(false, b"WM_DELETE_WINDOW")?;
    let (wm_protocols, wm_delete_window) =
        (wm_protocols.reply()?.atom, wm_delete_window.reply()?.atom);

    let mut window_size = (700u16, 500u16);

    // Check whether the Shape extension is available.
    let has_shape = conn
        .extension_information(shape::X11_EXTENSION_NAME)?
        .is_some();

    let win_id = setup_window(&conn, screen, window_size, wm_protocols, wm_delete_window)?;

    // Off-screen pixmap for double-buffering.
    let mut pixmap = PixmapWrapper::create_pixmap(
        conn.clone(),
        screen.root_depth,
        win_id,
        window_size.0,
        window_size.1,
    )?;

    // Graphics contexts for black and white fills.
    let black_gc = GcontextWrapper::create_gc(
        &conn,
        win_id,
        &CreateGCAux::new()
            .graphics_exposures(0)
            .foreground(screen.black_pixel),
    )?;
    let white_gc = GcontextWrapper::create_gc(
        &conn,
        win_id,
        &CreateGCAux::new()
            .graphics_exposures(0)
            .foreground(screen.white_pixel),
    )?;

    conn.flush()?;

    let mut need_repaint = false;
    let mut need_reshape = false;
    let mut mouse_position: (i16, i16) = (0, 0);

    loop {
        let event = conn.wait_for_event()?;
        let mut event_option = Some(event);

        while let Some(event) = event_option {
            match event {
                Event::Expose(ev) => {
                    if ev.count == 0 {
                        need_repaint = true;
                    }
                }
                Event::ConfigureNotify(ev) => {
                    window_size = (ev.width, ev.height);
                    pixmap = PixmapWrapper::create_pixmap(
                        conn.clone(),
                        screen.root_depth,
                        win_id,
                        window_size.0,
                        window_size.1,
                    )?;
                    need_reshape = true;
                    need_repaint = true;
                }
                Event::MotionNotify(ev) => {
                    mouse_position = (ev.event_x, ev.event_y);
                    need_repaint = true;
                }
                Event::MapNotify(_) => {
                    need_reshape = true;
                    need_repaint = true;
                }
                Event::ClientMessage(ev) => {
                    let data = ev.data.as_data32();
                    if ev.format == 32
                        && ev.window == win_id
                        && data[0] == wm_delete_window
                    {
                        return Ok(());
                    }
                }
                Event::Error(err) => {
                    eprintln!("X11 error: {err:?}");
                }
                _ => {}
            }

            event_option = conn.poll_for_event()?;
        }

        if need_reshape && has_shape {
            let _ = shape_window(&conn, win_id, window_size);
            need_reshape = false;
        }

        if need_repaint {
            let pos = compute_pupils(window_size, mouse_position);

            // Draw into the off-screen pixmap.
            draw_eyes(
                &conn,
                pixmap.pixmap(),
                black_gc.gcontext(),
                white_gc.gcontext(),
                window_size,
            )?;
            draw_pupils(&conn, pixmap.pixmap(), black_gc.gcontext(), pos)?;

            // Blit pixmap to the window.
            conn.copy_area(
                pixmap.pixmap(),
                win_id,
                white_gc.gcontext(),
                0,
                0,
                0,
                0,
                window_size.0,
                window_size.1,
            )?;

            conn.flush()?;
            need_repaint = false;
        }
    }
}
