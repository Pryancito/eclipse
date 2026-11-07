//! The Wayland window.
//!
//! The toplevel is an xdg window. The page is a subsurface: EGL when the
//! driver can build a context (zink+NVK on `nvidia.wlr_gles2` /
//! `nvidia.wlr_vulkan`, llvmpipe otherwise), and a software readback into the
//! parent wl_shm buffer when it cannot. The chrome is a second subsurface,
//! also wl_shm, drawn with tiny-skia. Both subsurfaces have an empty input
//! region, so the pointer and the keyboard land on the toplevel and we can
//! decide whether a click is the toolbar or the page.
//!
//! This file holds the window state and its life cycle. The rest is split by
//! concern: `paint` (page readback, chrome, wl_shm buffers), `keys`
//! (shortcuts, repeat, dead keys, evdev → Servo), `location` (the address and
//! find bar), `pointer` (clicks, drags, cursor shape) and `wayland` (the
//! protocol event handlers).

use std::fs::File;
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use dpi::PhysicalSize;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle, WindowHandle,
};
use servo::{
    ClipboardAction, Code, Cursor, Key, Modifiers, MouseButton, NamedKey, RenderingContext,
    WindowRenderingContext,
};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_pointer::{self, WlPointer};
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_subcompositor::WlSubcompositor;
use wayland_client::protocol::wl_subsurface::WlSubsurface;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::{self, Shape, WpCursorShapeDeviceV1},
    wp_cursor_shape_manager_v1::{self, WpCursorShapeManagerV1},
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use xkbcommon::xkb::{self, Keymap, State as XkbState};

use crate::bookmarks;
use crate::chrome::{self, Hit, View};
use crate::clipboard;
use crate::compose::{Composer, Outcome};
use crate::downloads;
use crate::engine::{self, Session, Waker};
use crate::files::{self, PickRequest};
use crate::history;
use crate::home;
use crate::lineedit::{self, LineEdit};
use crate::location::{paste_line, resolve_omnibox, search_template, SchemeHint};
use crate::look::Look;
use crate::menu;
use crate::session::{self, Snapshot};
use crate::spawn;
use crate::wl;

mod keys;
mod location;
mod paint;
mod pointer;
mod wayland;

use keys::*;
use paint::*;

/// How the window chooses its first tab set.
pub enum Start {
    /// One URL from the CLI (no session restore).
    Url(url::Url),
    /// Ignore saved session; open the home page.
    Fresh,
    /// Restore `session.json` or fall back to home.
    Restore,
    /// Ventana privada: sin persistencia; limpia site data al salir.
    Private(url::Url),
}

const DEFAULT_W: i32 = 1100;
const DEFAULT_H: i32 = 720;

struct Handles {
    display: NonNull<libc::c_void>,
    surface: NonNull<libc::c_void>,
}

impl HasDisplayHandle for Handles {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        let raw = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(self.display));
        Ok(unsafe { DisplayHandle::borrow_raw(raw) })
    }
}

impl HasWindowHandle for Handles {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let raw = RawWindowHandle::Wayland(WaylandWindowHandle::new(self.surface));
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

struct ShmMem {
    ptr: *mut u8,
    len: usize,
}

impl Drop for ShmMem {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

struct LiveBuf {
    buffer: WlBuffer,
    // Kept so the pool is not destroyed while this buffer is still attached.
    _pool: WlShmPool,
    mem: ShmMem,
    id: u64,
    busy: bool,
    width: i32,
    height: i32,
    /// Lleva un fotograma de página completo (de uno o dos commits atrás):
    /// basta con copiar las filas que cambiaron desde entonces.
    page: bool,
}

struct Xkb {
    _keymap: Keymap,
    state: XkbState,
    /// Teclas muertas (´ + a → á). `None` si no hay tabla de composición.
    composer: Option<Composer>,
}

pub struct App {
    conn: Connection,
    qh: QueueHandle<App>,
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    subcompositor: Option<WlSubcompositor>,
    wm: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<WlSeat>,
    parent: Option<WlSurface>,
    page: Option<WlSurface>,
    chrome_surface: Option<WlSurface>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    page_sub: Option<WlSubsurface>,
    chrome_sub: Option<WlSubsurface>,
    keyboard: Option<WlKeyboard>,
    pointer: Option<WlPointer>,
    /// `wp_cursor_shape_v1` (labwc ≥ 0.7). Sin él el cursor queda como lo
    /// dejó el compositor.
    cursor_mgr: Option<WpCursorShapeManagerV1>,
    cursor_dev: Option<WpCursorShapeDeviceV1>,
    /// Serial del último `wl_pointer.enter`; `None` con el puntero fuera.
    pointer_serial: Option<u32>,
    /// Último cursor enviado al compositor (evita repetir `set_shape`).
    cursor_applied: Option<Cursor>,
    parent_buf: Option<LiveBuf>,
    /// Second toplevel buffer for the software page. wlroots' pixman
    /// renderer (Eclipse OS without a GPU) keeps a wl_shm buffer until the
    /// surface gets another one, so a single buffer is never released and the
    /// page could not be blitted again after the first commit.
    parent_back: Option<LiveBuf>,
    chrome_buf: Option<LiveBuf>,
    retired: Vec<LiveBuf>,
    next_buf: u64,
    logical_w: i32,
    logical_h: i32,
    scale: i32,
    configured: bool,
    need_paint: bool,
    exit: bool,
    xkb: Option<Xkb>,
    ctrl: bool,
    alt: bool,
    shift: bool,
    px: f64,
    py: f64,
    loc_focused: bool,
    /// Texto, cursor y selección de la barra.
    loc: LineEdit,
    /// Arrastre del ratón seleccionando en la barra.
    loc_drag: bool,
    /// Último clic en la barra (doble clic = palabra).
    loc_clicked: Option<(Instant, i32)>,
    /// Ctrl+F: la barra busca en la página en vez de navegar.
    find_mode: bool,
    /// Linux button code held over the page. Release sends button-up, so a
    /// drag can select text instead of clicking on the press.
    page_button: Option<u32>,
    on_page: bool,
    /// Shortcut that consumed the key-down. Its key-up must not reach the page.
    swallowed: Option<u32>,
    repeat_rate: i32,
    repeat_delay_ms: i32,
    held: Option<u32>,
    held_at: Option<Instant>,
    repeated_at: Option<Instant>,
    look: Look,
    look_mtime: Option<SystemTime>,
    look_checked: Instant,
    waker: Option<Waker>,
    session: Option<Session>,
    /// First URL passed to `engine::start`.
    initial_url: Option<url::Url>,
    /// Extra tabs to open after start (restore).
    pending_tabs: Vec<(url::Url, String)>,
    pending_active: usize,
    persist: bool,
    /// F11 / Fullscreen API: cromo oculto y página a pantalla completa.
    fullscreen: bool,
    /// La Fullscreen API de la página empujó el estado (para no pelear con F11).
    page_drove_fs: bool,
    /// Arrastre de pestaña en curso (índice origen).
    drag_tab: Option<usize>,
    /// Huella del último cromo subido; evita commit Wayland si no cambió.
    chrome_fp: u64,
    /// Reloj de arranque para métricas percibidas.
    started_at: Instant,
    /// `SILVERSURFER_FRAME_STATS=1`: tiempos del camino software en stderr.
    stats: Option<FrameStats>,
    /// Lectura GL del fotograma actual y del último enviado (BGRA, de abajo
    /// arriba). Compararlas da las filas que cambiaron: solo esas se marcan
    /// como daño y, si no cambió nada, no hay commit.
    readback: Vec<u8>,
    shown: Vec<u8>,
    /// `glReadPixels(GL_BGRA)` funcionó; si el driver lo rechaza se vuelve a
    /// `read_to_image` (RGBA, daño completo).
    bgra_read: bool,
    /// The page is read back into the toplevel's wl_shm buffer instead of
    /// being presented on its own EGL subsurface. Always true in software;
    /// with EGL, see [`egl_via_shm`].
    shm_present: bool,
    /// Page frames read back so far; the first few are summarised in the log.
    read_frames: u32,
    /// Hay un `wl_surface.frame` del toplevel sin responder: el compositor aún
    /// no mostró el fotograma anterior, así que el siguiente espera. Limita el
    /// camino software a la frecuencia del monitor (y a cero si no se ve).
    frame_wait: Option<Instant>,
    /// Filas dañadas en el commit anterior de la página. El búfer trasero va
    /// un fotograma por detrás, así que también hay que copiarlas.
    last_rows: Option<(u32, u32)>,
}

impl App {
    fn new(conn: Connection, qh: QueueHandle<App>, start: Start) -> Self {
        let (initial_url, pending_tabs, pending_active, persist) = match start {
            Start::Url(url) => (url, Vec::new(), 0, true),
            Start::Fresh => (home::home_url(), Vec::new(), 0, true),
            Start::Private(url) => (url, Vec::new(), 0, false),
            Start::Restore => match session::load() {
                Some(snap) => snapshot_to_start(snap),
                None => (home::home_url(), Vec::new(), 0, true),
            },
        };
        Self {
            conn,
            qh,
            compositor: None,
            shm: None,
            subcompositor: None,
            wm: None,
            seat: None,
            parent: None,
            page: None,
            chrome_surface: None,
            xdg_surface: None,
            toplevel: None,
            page_sub: None,
            chrome_sub: None,
            keyboard: None,
            pointer: None,
            cursor_mgr: None,
            cursor_dev: None,
            pointer_serial: None,
            cursor_applied: None,
            parent_buf: None,
            parent_back: None,
            chrome_buf: None,
            retired: Vec::new(),
            next_buf: 1,
            logical_w: DEFAULT_W,
            logical_h: DEFAULT_H,
            scale: 1,
            configured: false,
            need_paint: true,
            exit: false,
            xkb: None,
            ctrl: false,
            alt: false,
            shift: false,
            px: 0.0,
            py: 0.0,
            loc_focused: false,
            loc: LineEdit::default(),
            loc_drag: false,
            loc_clicked: None,
            find_mode: false,
            page_button: None,
            on_page: false,
            swallowed: None,
            repeat_rate: 0,
            repeat_delay_ms: 0,
            held: None,
            held_at: None,
            repeated_at: None,
            look: Look::current(),
            look_mtime: std::fs::metadata("/etc/eclipse/look")
                .and_then(|m| m.modified())
                .ok(),
            look_checked: Instant::now(),
            waker: None,
            session: None,
            initial_url: Some(initial_url),
            pending_tabs,
            pending_active,
            persist,
            fullscreen: false,
            page_drove_fs: false,
            drag_tab: None,
            chrome_fp: 0,
            started_at: Instant::now(),
            stats: FrameStats::from_env(),
            readback: Vec::new(),
            shown: Vec::new(),
            bgra_read: true,
            shm_present: false,
            read_frames: 0,
            frame_wait: None,
            last_rows: None,
        }
    }

    fn chrome_h(&self) -> i32 {
        if self.fullscreen {
            0
        } else {
            chrome::HEIGHT
        }
    }

    fn apply_fullscreen_layout(&mut self) {
        let chrome_h = self.chrome_h();
        if let Some(page_sub) = &self.page_sub {
            page_sub.set_position(0, chrome_h);
        }
        if let Some(top) = &self.toplevel {
            if self.fullscreen {
                top.set_fullscreen(None);
            } else {
                top.unset_fullscreen();
            }
        }
        if self.fullscreen {
            if let Some(chrome) = &self.chrome_surface {
                chrome.attach(None, 0, 0);
                chrome.commit();
            }
        }
        let (pw, ph) = self.content_size();
        if let Some(session) = &self.session {
            session.shell.resize(pw, ph, self.scale as f32);
        }
        self.chrome_fp = 0;
        self.need_paint = true;
    }

    /// Page size in pixels: below the chrome, or the whole window in F11.
    fn content_size(&self) -> (u32, u32) {
        if self.fullscreen {
            let scale = self.scale.max(1) as u32;
            (
                (self.logical_w.max(1) as u32) * scale,
                (self.logical_h.max(1) as u32) * scale,
            )
        } else {
            chrome::content_px(self.logical_w, self.logical_h, self.scale)
        }
    }

    fn set_fullscreen(&mut self, on: bool) {
        if self.fullscreen == on {
            return;
        }
        self.fullscreen = on;
        if !on {
            self.page_drove_fs = false;
            if let Some(session) = &self.session {
                session.shell.exit_fullscreen();
            }
        }
        self.apply_fullscreen_layout();
    }

    fn refresh_look(&mut self) {
        if self.look_checked.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.look_checked = Instant::now();
        let mtime = std::fs::metadata("/etc/eclipse/look")
            .and_then(|m| m.modified())
            .ok();
        if mtime == self.look_mtime {
            return;
        }
        self.look_mtime = mtime;
        let next = Look::current();
        if next != self.look {
            self.look = next;
            self.chrome_fp = 0;
            self.need_paint = true;
        }
    }

    fn create_window(&mut self) -> Result<(), String> {
        let compositor = self
            .compositor
            .as_ref()
            .ok_or("el compositor no ofrece wl_compositor")?
            .clone();
        let sub = self
            .subcompositor
            .as_ref()
            .ok_or("el compositor no ofrece wl_subcompositor")?
            .clone();
        let wm = self
            .wm
            .as_ref()
            .ok_or("el compositor no ofrece xdg_wm_base")?
            .clone();
        let qh = self.qh.clone();
        let parent = compositor.create_surface(&qh, ());
        let page = compositor.create_surface(&qh, ());
        let chrome = compositor.create_surface(&qh, ());
        let page_sub = sub.get_subsurface(&page, &parent, &qh, ());
        let chrome_sub = sub.get_subsurface(&chrome, &parent, &qh, ());
        page_sub.set_position(0, chrome::HEIGHT);
        chrome_sub.set_position(0, 0);
        page_sub.set_desync();
        chrome_sub.set_desync();
        chrome_sub.place_above(&page);
        let empty = compositor.create_region(&qh, ());
        page.set_input_region(Some(&empty));
        chrome.set_input_region(Some(&empty));
        let xdg = wm.get_xdg_surface(&parent, &qh, ());
        let top = xdg.get_toplevel(&qh, ());
        top.set_title("SilverSurfer".into());
        top.set_app_id("silversurfer".into());
        // Abre maximizada: el compositor le da toda la pantalla menos la
        // zona exclusiva de lunarbar (layer-shell), sin calcular nada aquí.
        top.set_maximized();
        parent.commit();
        self.parent = Some(parent);
        self.page = Some(page);
        self.chrome_surface = Some(chrome);
        self.page_sub = Some(page_sub);
        self.chrome_sub = Some(chrome_sub);
        self.xdg_surface = Some(xdg);
        self.toplevel = Some(top);
        Ok(())
    }

    fn ensure_session(&mut self) -> Result<(), String> {
        if self.session.is_some() {
            return Ok(());
        }
        let url = self.initial_url.take().unwrap_or_else(home::home_url);
        let (pw, ph) = chrome::content_px(self.logical_w, self.logical_h, self.scale);
        let (context, window_context, gpu) = self.make_context(pw, ph)?;
        let waker = Waker::new()?;
        let t0 = self.started_at;
        let private = !self.persist;
        let session = engine::start(
            waker.clone(),
            context,
            window_context,
            gpu,
            url,
            self.scale as f32,
            private,
        )?;
        self.shm_present = !gpu || egl_via_shm();
        eprintln!(
            "silversurfer: paint={}{} session_ms={}",
            match (gpu, self.shm_present) {
                (true, false) => "EGL",
                (true, true) => "EGL→wl_shm",
                (false, _) => "software→wl_shm",
            },
            if private { " private" } else { "" },
            t0.elapsed().as_millis()
        );
        let extra = std::mem::take(&mut self.pending_tabs);
        let active = self.pending_active;
        if !extra.is_empty() || active > 0 {
            session.shell.restore_tabs(&extra, active);
        }
        self.waker = Some(waker);
        self.session = Some(session);
        self.need_paint = true;
        Ok(())
    }

    fn persist_session(&self) {
        if !self.persist {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let snap = session.shell.snapshot();
        if let Err(err) = session::save(&snap) {
            eprintln!("silversurfer: no se pudo guardar la sesión: {err}");
        }
    }

    fn make_context(
        &self,
        width: u32,
        height: u32,
    ) -> Result<
        (
            Rc<dyn RenderingContext>,
            Option<Rc<WindowRenderingContext>>,
            bool,
        ),
        String,
    > {
        // SILVERSURFER_SOFTWARE=1 skips EGL, so the wl_shm path Eclipse OS
        // takes under pixman can be reproduced on a Linux desktop with a GPU.
        let force_software = std::env::var_os("SILVERSURFER_SOFTWARE").is_some();
        if let Some(context) = (!force_software)
            .then(|| self.try_gpu(width, height))
            .flatten()
        {
            // Read back into wl_shm: render into Servo's offscreen FBO, whose
            // size follows resizes at once. The EGL window's own back buffer
            // only resizes on eglSwapBuffers, which this path never calls, so
            // reading it after a configure gave a shifted, stale page.
            let dyn_context: Rc<dyn RenderingContext> = if egl_via_shm() {
                Rc::new(context.offscreen_context(PhysicalSize::new(width.max(1), height.max(1))))
            } else {
                context.clone()
            };
            return Ok((dyn_context, Some(context), true));
        }
        let require_egl = std::env::var_os("SILVERSURFER_REQUIRE_EGL").is_some();
        if require_egl {
            return Err(
                "SILVERSURFER_REQUIRE_EGL: no hay contexto EGL y el software está prohibido".into(),
            );
        }
        eprintln!("silversurfer: sin contexto EGL, la página se pinta en software");
        Ok((engine::software_context(width, height)?, None, false))
    }

    fn try_gpu(&self, width: u32, height: u32) -> Option<Rc<WindowRenderingContext>> {
        let page = self.page.as_ref()?;
        let surface = NonNull::new(proxy_ptr(page))?;
        let display = NonNull::new(self.conn.backend().display_ptr().cast())?;
        let handles = Handles { display, surface };
        let context = WindowRenderingContext::new(
            handles.display_handle().ok()?,
            handles.window_handle().ok()?,
            PhysicalSize::new(width.max(1), height.max(1)),
        )
        .ok()?;
        let _ = context.make_current();
        Some(Rc::new(context))
    }

    fn after_dispatch(&mut self) -> Result<(), String> {
        self.refresh_look();
        if self.compositor.is_some() && self.parent.is_none() {
            self.create_window()?;
        }
        if self.parent.is_some() && (self.configured || self.session.is_none()) {
            self.ensure_session()?;
        }
        let animating = self.session.as_ref().is_some_and(|s| {
            s.shell
                .flags()
                .animating
                .load(std::sync::atomic::Ordering::Relaxed)
        });
        if let Some(session) = self.session.as_mut() {
            if session
                .shell
                .flags()
                .close_window
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                self.exit = true;
            }
            session.spin_event_loop();
        }
        self.update_cursor();
        let page_fs = self.session.as_ref().is_some_and(|s| {
            s.shell
                .flags()
                .page_fullscreen
                .load(std::sync::atomic::Ordering::Relaxed)
        });
        if page_fs && !self.fullscreen {
            self.page_drove_fs = true;
            self.set_fullscreen(true);
        } else if !page_fs && self.page_drove_fs {
            self.page_drove_fs = false;
            self.set_fullscreen(false);
        }
        let frame = self.session.as_ref().is_some_and(|s| {
            s.shell
                .flags()
                .frame
                .swap(false, std::sync::atomic::Ordering::Relaxed)
        });
        let chrome = self.session.as_ref().is_some_and(|s| {
            s.shell
                .flags()
                .chrome
                .swap(false, std::sync::atomic::Ordering::Relaxed)
        });
        if frame {
            if self.shm_present && self.waiting_for_frame() {
                // Se pinta cuando llegue el `frame` del compositor.
                self.retry_frame();
            } else {
                self.paint_page()?;
            }
        }
        if (chrome || self.need_paint) && !self.fullscreen {
            self.paint_chrome()?;
            self.need_paint = false;
        } else if self.need_paint {
            self.need_paint = false;
            self.ensure_parent_buffer()?;
        }
        let _ = animating;
        Ok(())
    }

    fn apply_size(&mut self, width: i32, height: i32) {
        if width > 0 {
            self.logical_w = width.max(320);
        }
        if height > 0 {
            self.logical_h = height.max(chrome::HEIGHT + 64);
        }
        let (pw, ph) = self.content_size();
        if let Some(session) = &self.session {
            session.shell.resize(pw, ph, self.scale as f32);
        }
        self.chrome_fp = 0;
        self.need_paint = true;
    }
}

/// Whether a GPU page is read back into wl_shm instead of presented.
///
/// On Eclipse OS the GPU sessions run Mesa's GL on zink+NVK. The context
/// builds and Servo renders on the GPU, but the buffers that kopper hands the
/// compositor never show: the page stays empty under a `GL` toolbar. Reading
/// the frame back keeps the GPU rendering (WebGL, WebGPU, layout) and shows it
/// through the same wl_shm path as software. `SILVERSURFER_PRESENT=egl|shm`
/// overrides the guess.
fn egl_via_shm() -> bool {
    match std::env::var("SILVERSURFER_PRESENT").as_deref() {
        Ok("egl") => return false,
        Ok("shm") => return true,
        _ => {}
    }
    ["GALLIUM_DRIVER", "MESA_LOADER_DRIVER_OVERRIDE"]
        .iter()
        .any(|var| std::env::var(var).is_ok_and(|v| v.eq_ignore_ascii_case("zink")))
}

fn proxy_ptr<P: Proxy>(proxy: &P) -> *mut libc::c_void {
    proxy.id().as_ptr().cast()
}

fn snapshot_to_start(snap: Snapshot) -> (url::Url, Vec<(url::Url, String)>, usize, bool) {
    let mut urls = Vec::new();
    for tab in &snap.tabs {
        if let Ok(url) = url::Url::parse(&tab.url) {
            urls.push((url, tab.title.clone()));
        }
    }
    if urls.is_empty() {
        return (home::home_url(), Vec::new(), 0, true);
    }
    let active = snap.active.min(urls.len() - 1);
    let (first, title) = urls.remove(0);
    // restore_tabs opens *extra* tabs after the first; active index is global.
    // If active was 0, pending_active stays 0. If active > 0, first tab is
    // still index 0 and extras follow — select_tab(active) after restore.
    let _ = title;
    (first, urls, active, true)
}

pub fn run(start: Start) -> Result<(), String> {
    let conn = wl::connect()?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let mut app = App::new(conn.clone(), qh.clone(), start);
    let _registry = conn.display().get_registry(&qh, ());
    queue
        .roundtrip(&mut app)
        .map_err(|err| format!("registro Wayland: {err}"))?;
    app.create_window()?;
    queue
        .roundtrip(&mut app)
        .map_err(|err| format!("configure: {err}"))?;
    if !app.configured {
        app.logical_w = DEFAULT_W;
        app.logical_h = DEFAULT_H;
    }
    app.ensure_session()?;
    app.need_paint = true;
    app.after_dispatch()?;

    while !app.exit {
        queue
            .dispatch_pending(&mut app)
            .map_err(|err| format!("wayland: {err}"))?;
        app.after_dispatch()?;
        app.pump_key_repeat();
        if app.exit {
            break;
        }
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let _ = app.conn.flush();
        let wl_fd = app.conn.backend().poll_fd().as_raw_fd();
        let wake_fd = app.waker.as_ref().map(Waker::fd).unwrap_or(-1);
        let animating = app.session.as_ref().is_some_and(|s| {
            s.shell
                .flags()
                .animating
                .load(std::sync::atomic::Ordering::Relaxed)
        });
        let mut fds = [
            libc::pollfd {
                fd: wl_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let repeating = app.held.is_some() && app.repeat_rate > 0;
        // Un fotograma retenido por `frame_wait` necesita volver a mirar aunque
        // no llegue el callback (ver `waiting_for_frame`).
        let held_frame = app.frame_wait.is_some()
            && app.session.as_ref().is_some_and(|s| {
                s.shell
                    .flags()
                    .frame
                    .load(std::sync::atomic::Ordering::Relaxed)
            });
        let timeout = if animating || repeating || held_frame {
            16
        } else {
            -1
        };
        unsafe {
            libc::poll(fds.as_mut_ptr(), 2, timeout);
        }
        if fds[1].revents & libc::POLLIN != 0 {
            if let Some(waker) = &app.waker {
                waker.drain();
            }
        }
        if fds[0].revents & libc::POLLIN != 0 {
            if let Err(err) = guard.read() {
                return Err(format!("lectura Wayland: {err}"));
            }
        } else {
            // Dropping the guard is the cancel: nothing was read from the socket.
            drop(guard);
        }
    }
    app.persist_session();
    // Ventana privada: purgar cookies/storage/caché de esta instancia Servo
    // antes de tumbar el proceso.
    if let Some(session) = app.session.as_ref() {
        if session.private {
            let _ = session.shell.clear_browsing_data();
        }
    }
    // Tear Servo / WebGL / EGL down while Wayland is still alive: Session::drop
    // drains CloseWebView, runs Servo Exit (joins WebGL destroy_context), then
    // take_window so FBO renderbuffers are not left for surfman Drop panics.
    drop(app.session.take());
    Ok(())
}
