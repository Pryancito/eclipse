//! The Wayland window.
//!
//! The toplevel is an xdg window. The page is a subsurface: EGL when the
//! driver can build a context (zink+NVK on `nvidia.wlr_gles2` /
//! `nvidia.wlr_vulkan`, llvmpipe otherwise), and a software readback into the
//! parent wl_shm buffer when it cannot. The chrome is a second subsurface,
//! also wl_shm, drawn with tiny-skia. Both subsurfaces have an empty input
//! region, so the pointer and the keyboard land on the toplevel and we can
//! decide whether a click is the toolbar or the page.

use std::fs::File;
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::ptr::NonNull;
use std::rc::Rc;

use dpi::PhysicalSize;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle, WindowHandle,
};
use servo::{Code, Key, Modifiers, MouseButton, NamedKey, RenderingContext, WindowRenderingContext};
use wayland_client::protocol::wl_buffer::WlBuffer;
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
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use xkbcommon::xkb::{self, Keymap, State as XkbState};

use crate::chrome::{self, Hit, View};
use crate::engine::{self, Session, Waker};
use crate::files::{self, PickRequest};
use crate::home;
use crate::location::parse_location;
use crate::look::Look;
use crate::wl;

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
}

struct Xkb {
    _keymap: Keymap,
    state: XkbState,
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
    parent_buf: Option<LiveBuf>,
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
    loc_text: String,
    loc_cursor: usize,
    look: Look,
    waker: Option<Waker>,
    session: Option<Session>,
    initial_url: Option<url::Url>,
}

impl App {
    fn new(conn: Connection, qh: QueueHandle<App>, url: url::Url) -> Self {
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
            parent_buf: None,
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
            loc_text: String::new(),
            loc_cursor: 0,
            look: Look::current(),
            waker: None,
            session: None,
            initial_url: Some(url),
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
        let (context, gpu) = self.make_context(pw, ph)?;
        let waker = Waker::new()?;
        let session = engine::start(waker.clone(), context, gpu, url, self.scale as f32)?;
        self.waker = Some(waker);
        self.session = Some(session);
        self.need_paint = true;
        Ok(())
    }

    fn make_context(
        &self,
        width: u32,
        height: u32,
    ) -> Result<(Rc<dyn RenderingContext>, bool), String> {
        if let Some(context) = self.try_gpu(width, height) {
            return Ok((context, true));
        }
        eprintln!("silversurfer: sin contexto EGL, la página se pinta en software");
        Ok((engine::software_context(width, height)?, false))
    }

    fn try_gpu(&self, width: u32, height: u32) -> Option<Rc<dyn RenderingContext>> {
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
        if self.compositor.is_some() && self.parent.is_none() {
            self.create_window()?;
        }
        if self.parent.is_some() && (self.configured || self.session.is_none()) {
            self.ensure_session()?;
        }
        let animating = self
            .session
            .as_ref()
            .is_some_and(|s| s.shell.flags().animating.load(std::sync::atomic::Ordering::Relaxed));
        if let Some(session) = self.session.as_mut() {
            session.servo.spin_event_loop();
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
            self.paint_page()?;
        }
        if chrome || self.need_paint {
            self.paint_chrome()?;
            self.need_paint = false;
        }
        let _ = animating;
        Ok(())
    }

    fn paint_page(&mut self) -> Result<(), String> {
        let Some(session) = self.session.as_ref() else {
            return Ok(());
        };
        let _ = session.context.make_current();
        session.shell.paint();
        if session.gpu {
            session.context.present();
            if let Some(page) = &self.page {
                page.set_buffer_scale(self.scale);
                page.commit();
            }
            return Ok(());
        }
        let (pw, ph) = chrome::content_px(self.logical_w, self.logical_h, self.scale);
        let rect = webrender_api::units::DeviceIntRect::from_size(euclid::size2(
            pw as i32,
            ph as i32,
        ));
        let Some(image) = session.context.read_to_image(rect) else {
            return Ok(());
        };
        let raw = image.into_raw();
        let width = image_width(&raw, pw);
        self.blit_page(&raw, width, ph);
        self.commit_parent()?;
        Ok(())
    }

    fn blit_page(&mut self, raw: &[u8], src_w: u32, src_h: u32) {
        let Some(buf) = self.parent_buf.as_mut() else {
            return;
        };
        if buf.busy {
            return;
        }
        let dst_y = (chrome::HEIGHT * self.scale).max(0) as u32;
        let stride = (buf.width as usize) * 4;
        let len = buf.mem.len;
        let dst = unsafe { std::slice::from_raw_parts_mut(buf.mem.ptr, len) };
        chrome::blit_rgba(raw, src_w, src_h, dst, buf.width as u32, stride, dst_y);
    }

    fn paint_chrome(&mut self) -> Result<(), String> {
        let titles;
        let active;
        let location;
        let can_back;
        let can_forward;
        if let Some(session) = self.session.as_ref() {
            titles = session.shell.tab_titles();
            active = session.shell.active_index();
            can_back = session.shell.can_go_back();
            can_forward = session.shell.can_go_forward();
            location = if self.loc_focused {
                self.loc_text.clone()
            } else {
                session.shell.location_text()
            };
        } else {
            titles = vec!["SilverSurfer".into()];
            active = 0;
            can_back = false;
            can_forward = false;
            location = String::new();
        }
        let pixels = chrome::render(&View {
            width: self.logical_w,
            look: self.look,
            titles: &titles,
            active,
            location: &location,
            location_focused: self.loc_focused,
            can_back,
            can_forward,
            scale: self.scale,
        });
        self.upload_chrome(&pixels.argb, pixels.width, pixels.height)?;
        self.ensure_parent_buffer()?;
        if let Some(page) = &self.page {
            page.set_buffer_scale(self.scale);
        }
        Ok(())
    }

    fn upload_chrome(&mut self, argb: &[u8], width: i32, height: i32) -> Result<(), String> {
        let scale = self.scale;
        let surface = self
            .chrome_surface
            .clone()
            .ok_or("superficie del cromo ausente")?;
        let buf = self.buffer(width, height, true)?;
        copy_into(buf, argb);
        surface.set_buffer_scale(scale);
        surface.attach(Some(&buf.buffer), 0, 0);
        surface.damage_buffer(0, 0, width, height);
        surface.commit();
        buf.busy = true;
        Ok(())
    }

    fn ensure_parent_buffer(&mut self) -> Result<(), String> {
        let width = self.logical_w * self.scale;
        let height = self.logical_h * self.scale;
        let fresh = self
            .parent_buf
            .as_ref()
            .is_none_or(|buf| buf.width != width || buf.height != height || buf.busy);
        if !fresh {
            return Ok(());
        }
        let scale = self.scale;
        let look = self.look;
        let surface = self.parent.clone().ok_or("superficie ausente")?;
        let buf = self.buffer(width, height, false)?;
        fill_panel(buf, look);
        surface.set_buffer_scale(scale);
        surface.attach(Some(&buf.buffer), 0, 0);
        surface.damage_buffer(0, 0, width, height);
        surface.commit();
        buf.busy = true;
        Ok(())
    }

    fn commit_parent(&mut self) -> Result<(), String> {
        let Some(surface) = self.parent.clone() else {
            return Ok(());
        };
        let Some(buf) = self.parent_buf.as_mut() else {
            return Ok(());
        };
        let width = buf.width;
        let height = buf.height;
        surface.attach(Some(&buf.buffer), 0, 0);
        surface.damage_buffer(0, 0, width, height);
        surface.commit();
        buf.busy = true;
        Ok(())
    }

    /// `chrome == true` stores the buffer as the toolbar's. Otherwise it is
    /// the toplevel's.
    fn buffer(&mut self, width: i32, height: i32, chrome: bool) -> Result<&mut LiveBuf, String> {
        let slot = if chrome {
            &mut self.chrome_buf
        } else {
            &mut self.parent_buf
        };
        let reuse = slot
            .as_ref()
            .is_some_and(|buf| buf.width == width && buf.height == height && !buf.busy);
        if !reuse {
            if let Some(old) = slot.take() {
                if old.busy {
                    self.retired.push(old);
                }
            }
            let shm = self.shm.as_ref().ok_or("wl_shm ausente")?.clone();
            let id = self.next_buf;
            self.next_buf += 1;
            *slot = Some(alloc_buffer(&shm, &self.qh, width, height, id)?);
        }
        Ok(slot.as_mut().expect("el búfer acaba de crearse"))
    }

    fn release_buffer(&mut self, id: u64) {
        for buf in [&mut self.parent_buf, &mut self.chrome_buf] {
            if let Some(live) = buf.as_mut() {
                if live.id == id {
                    live.busy = false;
                }
            }
        }
        self.retired.retain(|buf| buf.id != id);
    }

    fn on_pointer_button(&mut self, button: u32, down: bool) {
        if !down {
            return;
        }
        let x = self.px.round() as i32;
        let y = self.py.round() as i32;
        if y < chrome::HEIGHT {
            self.on_chrome_click(x, y);
            return;
        }
        let Some((px, py)) = self.page_px() else {
            return;
        };
        let mouse = match button {
            0x110 => MouseButton::Primary,
            0x111 => MouseButton::Secondary,
            0x112 => MouseButton::Auxiliary,
            0x113 => MouseButton::Back,
            0x114 => MouseButton::Forward,
            other => MouseButton::Other(other as u16),
        };
        if mouse == MouseButton::Back {
            if let Some(session) = &self.session {
                session.shell.go_back();
            }
            return;
        }
        if let Some(session) = &self.session {
            session.shell.pointer(px, py, Some((mouse, true)));
            session.shell.pointer(px, py, Some((mouse, false)));
        }
    }

    fn on_chrome_click(&mut self, x: i32, y: i32) {
        let tabs = self
            .session
            .as_ref()
            .map(|s| s.shell.tab_titles().len())
            .unwrap_or(1)
            .max(1);
        let Some(hit) = chrome::hit(self.logical_w, tabs, x, y) else {
            return;
        };
        self.loc_focused = matches!(hit, Hit::Location);
        match hit {
            Hit::Location => {
                if let Some(session) = &self.session {
                    self.loc_text = session.shell.location_text();
                    self.loc_cursor = self.loc_text.chars().count();
                }
            }
            Hit::Back => {
                if let Some(session) = &self.session {
                    session.shell.go_back();
                }
            }
            Hit::Forward => {
                if let Some(session) = &self.session {
                    session.shell.go_forward();
                }
            }
            Hit::Reload => {
                if let Some(session) = &self.session {
                    session.shell.reload();
                }
            }
            Hit::NewTab => {
                if let Some(session) = &self.session {
                    session.shell.new_tab(home::home_url());
                }
            }
            Hit::Tab(index) => {
                if let Some(session) = &self.session {
                    session.shell.select_tab(index);
                }
            }
            Hit::Close(index) => {
                let keep = self
                    .session
                    .as_ref()
                    .is_none_or(|session| session.shell.close_tab(index));
                if !keep {
                    self.exit = true;
                }
            }
        }
        self.need_paint = true;
    }

    fn on_key(&mut self, evdev: u32, down: bool) {
        match evdev {
            29 | 97 => self.ctrl = down,
            56 | 100 => self.alt = down,
            42 | 54 => self.shift = down,
            _ => {}
        }
        if !down {
            if !self.loc_focused {
                self.send_key(evdev, false, false);
            }
            return;
        }
        if self.handle_shortcut(evdev) {
            return;
        }
        if self.loc_focused {
            self.edit_location(evdev);
            self.need_paint = true;
            return;
        }
        self.send_key(evdev, true, false);
    }

    fn handle_shortcut(&mut self, evdev: u32) -> bool {
        if self.alt && evdev == 105 {
            if let Some(session) = &self.session {
                session.shell.go_back();
            }
            return true;
        }
        if self.alt && evdev == 106 {
            if let Some(session) = &self.session {
                session.shell.go_forward();
            }
            return true;
        }
        if self.ctrl && evdev == 38 {
            self.loc_focused = true;
            if let Some(session) = &self.session {
                self.loc_text = session.shell.location_text();
                self.loc_cursor = self.loc_text.chars().count();
            }
            self.need_paint = true;
            return true;
        }
        if self.ctrl && evdev == 20 {
            if let Some(session) = &self.session {
                session.shell.new_tab(home::home_url());
            }
            return true;
        }
        if self.ctrl && evdev == 17 {
            let keep = self
                .session
                .as_ref()
                .is_none_or(|session| session.shell.close_tab(session.shell.active_index()));
            if !keep {
                self.exit = true;
            }
            self.need_paint = true;
            return true;
        }
        if self.ctrl && evdev == 24 {
            self.open_file();
            return true;
        }
        if evdev == 63 || (self.ctrl && evdev == 19) {
            if let Some(session) = &self.session {
                session.shell.reload();
            }
            return true;
        }
        false
    }

    fn open_file(&mut self) {
        let Ok(paths) = files::pick(&PickRequest {
            multiple: false,
            save_name: None,
        }) else {
            return;
        };
        let Some(path) = paths.into_iter().next() else {
            return;
        };
        if let Ok(url) = url::Url::from_file_path(&path) {
            if let Some(session) = &self.session {
                session.shell.load(url);
            }
            self.loc_focused = false;
            self.need_paint = true;
        }
    }

    fn edit_location(&mut self, evdev: u32) {
        match evdev {
            1 => {
                self.loc_focused = false;
            }
            28 => {
                if let Ok(url) = parse_location(&self.loc_text) {
                    if let Some(session) = &self.session {
                        session.shell.load(url);
                    }
                }
                self.loc_focused = false;
            }
            14 => {
                if self.loc_cursor > 0 {
                    let chars: Vec<char> = self.loc_text.chars().collect();
                    let at = self.loc_cursor - 1;
                    self.loc_text = chars
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| *i != at)
                        .map(|(_, ch)| *ch)
                        .collect();
                    self.loc_cursor = at;
                }
            }
            105 => self.loc_cursor = self.loc_cursor.saturating_sub(1),
            106 => {
                let n = self.loc_text.chars().count();
                if self.loc_cursor < n {
                    self.loc_cursor += 1;
                }
            }
            102 => self.loc_cursor = 0,
            107 => self.loc_cursor = self.loc_text.chars().count(),
            _ => {
                if let Some(text) = self.key_text(evdev) {
                    insert_chars(&mut self.loc_text, &mut self.loc_cursor, &text);
                }
            }
        }
    }

    fn send_key(&self, evdev: u32, down: bool, repeat: bool) {
        let Some(session) = &self.session else {
            return;
        };
        let (key, code) = self.key_and_code(evdev);
        session
            .shell
            .key(down, key, code, self.modifiers(), repeat);
    }

    fn modifiers(&self) -> Modifiers {
        let mut mods = Modifiers::empty();
        if self.shift {
            mods |= Modifiers::SHIFT;
        }
        if self.ctrl {
            mods |= Modifiers::CONTROL;
        }
        if self.alt {
            mods |= Modifiers::ALT;
        }
        mods
    }

    fn key_text(&self, evdev: u32) -> Option<String> {
        let code = evdev + 8;
        if let Some(xkb) = &self.xkb {
            let text = xkb.state.key_get_utf8(xkb::Keycode::new(code));
            if !text.is_empty() && !text.chars().all(char::is_control) {
                return Some(text);
            }
        }
        qwerty(evdev, self.shift)
    }

    fn key_and_code(&self, evdev: u32) -> (Key, Code) {
        let code = evdev_code(evdev);
        let named = named_key(evdev);
        if let Some(named) = named {
            return (Key::Named(named), code);
        }
        if let Some(text) = self.key_text(evdev) {
            return (Key::Character(text.into()), code);
        }
        (Key::Named(NamedKey::Unidentified), code)
    }

    fn page_px(&self) -> Option<(f32, f32)> {
        if self.py < chrome::HEIGHT as f64 {
            return None;
        }
        let scale = self.scale as f64;
        Some((
            (self.px * scale) as f32,
            ((self.py - chrome::HEIGHT as f64) * scale) as f32,
        ))
    }

    fn note_motion(&mut self, x: f64, y: f64) {
        self.px = x;
        self.py = y;
        if let Some((px, py)) = self.page_px() {
            if let Some(session) = &self.session {
                session.shell.pointer(px, py, None);
            }
        }
    }

    fn apply_size(&mut self, width: i32, height: i32) {
        if width > 0 {
            self.logical_w = width.max(320);
        }
        if height > 0 {
            self.logical_h = height.max(chrome::HEIGHT + 64);
        }
        let (pw, ph) = chrome::content_px(self.logical_w, self.logical_h, self.scale);
        if let Some(session) = &self.session {
            session
                .shell
                .resize(pw, ph, self.scale as f32);
        }
        self.need_paint = true;
    }
}

fn insert_chars(text: &mut String, cursor: &mut usize, extra: &str) {
    let mut byte = 0;
    for (i, ch) in text.chars().enumerate() {
        if i == *cursor {
            break;
        }
        byte += ch.len_utf8();
    }
    text.insert_str(byte, extra);
    *cursor += extra.chars().count();
}

fn copy_into(buf: &mut LiveBuf, src: &[u8]) {
    let len = buf.mem.len.min(src.len());
    unsafe {
        std::ptr::copy_nonoverlapping(src.as_ptr(), buf.mem.ptr, len);
    }
}

fn fill_panel(buf: &mut LiveBuf, look: Look) {
    let rgb = look.palette().panel;
    let px = [rgb.b, rgb.g, rgb.r, 255];
    let len = buf.mem.len;
    let dst = unsafe { std::slice::from_raw_parts_mut(buf.mem.ptr, len) };
    for chunk in dst.chunks_exact_mut(4) {
        chunk.copy_from_slice(&px);
    }
}

fn image_width(raw: &[u8], fallback: u32) -> u32 {
    let pixels = (raw.len() / 4) as u32;
    if pixels == 0 {
        return fallback;
    }
    // `read_to_image` is the content size we asked for. Trust the caller when
    // the buffer divides evenly by that width.
    if fallback > 0 && pixels % fallback == 0 {
        fallback
    } else {
        fallback.max(1)
    }
}

fn alloc_buffer(
    shm: &WlShm,
    qh: &QueueHandle<App>,
    width: i32,
    height: i32,
    id: u64,
) -> Result<LiveBuf, String> {
    let width = width.max(1);
    let height = height.max(1);
    let stride = width * 4;
    let len = (stride as usize)
        .checked_mul(height as usize)
        .ok_or("búfer demasiado grande")?;
    let name = std::ffi::CString::new("silversurfer").unwrap();
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err("memfd_create".into());
    }
    if unsafe { libc::ftruncate(fd, len as libc::off_t) } != 0 {
        unsafe { libc::close(fd) };
        return Err("ftruncate".into());
    }
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if map == libc::MAP_FAILED {
        unsafe { libc::close(fd) };
        return Err("mmap".into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let pool = shm.create_pool(file.as_fd(), len as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        width,
        height,
        stride,
        wl_shm::Format::Argb8888,
        qh,
        id,
    );
    drop(file);
    Ok(LiveBuf {
        buffer,
        _pool: pool,
        mem: ShmMem {
            ptr: map.cast(),
            len,
        },
        id,
        busy: false,
        width,
        height,
    })
}

fn proxy_ptr<P: Proxy>(proxy: &P) -> *mut libc::c_void {
    proxy.id().as_ptr().cast()
}

fn named_key(evdev: u32) -> Option<NamedKey> {
    Some(match evdev {
        1 => NamedKey::Escape,
        14 => NamedKey::Backspace,
        15 => NamedKey::Tab,
        28 => NamedKey::Enter,
        102 => NamedKey::Home,
        103 => NamedKey::ArrowUp,
        105 => NamedKey::ArrowLeft,
        106 => NamedKey::ArrowRight,
        107 => NamedKey::End,
        108 => NamedKey::ArrowDown,
        104 => NamedKey::PageUp,
        109 => NamedKey::PageDown,
        111 => NamedKey::Delete,
        59 => NamedKey::F1,
        60 => NamedKey::F2,
        61 => NamedKey::F3,
        62 => NamedKey::F4,
        63 => NamedKey::F5,
        64 => NamedKey::F6,
        65 => NamedKey::F7,
        66 => NamedKey::F8,
        67 => NamedKey::F9,
        68 => NamedKey::F10,
        87 => NamedKey::F11,
        88 => NamedKey::F12,
        _ => return None,
    })
}

fn evdev_code(evdev: u32) -> Code {
    match evdev {
        1 => Code::Escape,
        14 => Code::Backspace,
        15 => Code::Tab,
        28 => Code::Enter,
        57 => Code::Space,
        103 => Code::ArrowUp,
        105 => Code::ArrowLeft,
        106 => Code::ArrowRight,
        108 => Code::ArrowDown,
        63 => Code::F5,
        16 => Code::KeyQ,
        17 => Code::KeyW,
        18 => Code::KeyE,
        19 => Code::KeyR,
        20 => Code::KeyT,
        24 => Code::KeyO,
        30 => Code::KeyA,
        31 => Code::KeyS,
        32 => Code::KeyD,
        33 => Code::KeyF,
        37 => Code::KeyK,
        38 => Code::KeyL,
        _ => Code::Unidentified,
    }
}

fn qwerty(evdev: u32, shift: bool) -> Option<String> {
    let (low, high) = match evdev {
        16 => ('q', 'Q'),
        17 => ('w', 'W'),
        18 => ('e', 'E'),
        19 => ('r', 'R'),
        20 => ('t', 'T'),
        21 => ('y', 'Y'),
        22 => ('u', 'U'),
        23 => ('i', 'I'),
        24 => ('o', 'O'),
        25 => ('p', 'P'),
        30 => ('a', 'A'),
        31 => ('s', 'S'),
        32 => ('d', 'D'),
        33 => ('f', 'F'),
        34 => ('g', 'G'),
        35 => ('h', 'H'),
        36 => ('j', 'J'),
        37 => ('k', 'K'),
        38 => ('l', 'L'),
        44 => ('z', 'Z'),
        45 => ('x', 'X'),
        46 => ('c', 'C'),
        47 => ('v', 'V'),
        48 => ('b', 'B'),
        49 => ('n', 'N'),
        50 => ('m', 'M'),
        2 => ('1', '!'),
        3 => ('2', '@'),
        4 => ('3', '#'),
        5 => ('4', '$'),
        6 => ('5', '%'),
        7 => ('6', '^'),
        8 => ('7', '&'),
        9 => ('8', '*'),
        10 => ('9', '('),
        11 => ('0', ')'),
        12 => ('-', '_'),
        13 => ('=', '+'),
        26 => ('[', '{'),
        27 => (']', '}'),
        39 => (';', ':'),
        40 => ('\'', '"'),
        43 => ('\\', '|'),
        51 => (',', '<'),
        52 => ('.', '>'),
        53 => ('/', '?'),
        57 => (' ', ' '),
        _ => return None,
    };
    Some(if shift { high } else { low }.to_string())
}

fn keymap_from_fd(fd: File, size: u32) -> Option<Xkb> {
    let mut file = fd;
    let mut buf = vec![0u8; size as usize];
    file.read_exact(&mut buf).ok()?;
    let text = String::from_utf8(buf).ok()?;
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = Keymap::new_from_string(
        &context,
        text,
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )?;
    let state = XkbState::new(&keymap);
    Some(Xkb {
        _keymap: keymap,
        state,
    })
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_compositor" => {
                state.compositor = Some(registry.bind(name, version.min(4), qh, ()));
            }
            "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
            "wl_subcompositor" => {
                state.subcompositor = Some(registry.bind(name, 1, qh, ()));
            }
            "xdg_wm_base" => state.wm = Some(registry.bind(name, version.min(4), qh, ())),
            "wl_seat" => state.seat = Some(registry.bind(name, version.min(5), qh, ())),
            "wl_output" => {
                let _: WlOutput = registry.bind(name, version.min(2), qh, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<WlShm, ()> for App {
    fn event(
        _: &mut Self,
        _: &WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlCompositor, ()> for App {
    fn event(
        _: &mut Self,
        _: &WlCompositor,
        _: wayland_client::protocol::wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSubcompositor, ()> for App {
    fn event(
        _: &mut Self,
        _: &WlSubcompositor,
        _: wayland_client::protocol::wl_subcompositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlRegion, ()> for App {
    fn event(
        _: &mut Self,
        _: &WlRegion,
        _: wayland_client::protocol::wl_region::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSurface, ()> for App {
    fn event(
        _: &mut Self,
        _: &WlSurface,
        _: wayland_client::protocol::wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSubsurface, ()> for App {
    fn event(
        _: &mut Self,
        _: &WlSubsurface,
        _: wayland_client::protocol::wl_subsurface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShmPool, ()> for App {
    fn event(
        _: &mut Self,
        _: &WlShmPool,
        _: wayland_client::protocol::wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, u64> for App {
    fn event(
        state: &mut Self,
        _: &WlBuffer,
        event: wayland_client::protocol::wl_buffer::Event,
        id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_buffer::Event::Release = event {
            state.release_buffer(*id);
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        wm: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for App {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            state.configured = true;
            state.need_paint = true;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for App {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                state.apply_size(width, height);
            }
            xdg_toplevel::Event::Close => state.exit = true,
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for App {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities { capabilities } = event else {
            return;
        };
        let WEnum::Value(caps) = capabilities else {
            return;
        };
        if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
            state.keyboard = Some(seat.get_keyboard(qh, ()));
        }
        if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
            state.pointer = Some(seat.get_pointer(qh, ()));
        }
    }
}

impl Dispatch<WlKeyboard, ()> for App {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                if matches!(format, WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)) {
                    state.xkb = keymap_from_fd(File::from(fd), size);
                }
            }
            wl_keyboard::Event::Key { key, state: key_state, .. } => {
                let down = matches!(key_state, WEnum::Value(wl_keyboard::KeyState::Pressed));
                if let Some(xkb) = &mut state.xkb {
                    let dir = if down {
                        xkb::KeyDirection::Down
                    } else {
                        xkb::KeyDirection::Up
                    };
                    xkb.state.update_key(xkb::Keycode::new(key + 8), dir);
                }
                state.on_key(key, down);
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(xkb) = &mut state.xkb {
                    xkb.state.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlPointer, ()> for App {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => state.note_motion(surface_x, surface_y),
            wl_pointer::Event::Button { button, state: st, .. } => {
                let down = matches!(st, WEnum::Value(wl_pointer::ButtonState::Pressed));
                state.on_pointer_button(button, down);
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                let Some((px, py)) = state.page_px() else {
                    return;
                };
                let (dx, dy) = if matches!(axis, WEnum::Value(wl_pointer::Axis::VerticalScroll)) {
                    (0.0, -value)
                } else {
                    (-value, 0.0)
                };
                if let Some(session) = &state.session {
                    session.shell.wheel(px, py, dx, dy);
                }
            }
            wl_pointer::Event::Leave { .. } => {
                if let Some(session) = &state.session {
                    session.shell.pointer_left();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, ()> for App {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Scale { factor } = event {
            let factor = factor.max(1);
            if factor != state.scale {
                state.scale = factor;
                state.apply_size(0, 0);
            }
        }
    }
}

pub fn run(url: url::Url) -> Result<(), String> {
    let conn = wl::connect()?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let mut app = App::new(conn.clone(), qh.clone(), url);
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
        let timeout = if animating { 16 } else { -1 };
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
        if animating {
            if let Some(session) = app.session.as_mut() {
                session.servo.spin_event_loop();
            }
        }
    }
    Ok(())
}
