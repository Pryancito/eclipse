//! Servo, one window, several tabs.
//!
//! The rendering context is either the EGL surface of the page (NVIDIA via
//! zink+NVK, or llvmpipe) or a software context whose pixels are copied into
//! the wl_shm buffer. WebGL, WebGPU and GStreamer all live in this process
//! tree: `multiprocess` gives each content process its own DRM file, which
//! is the GPU context Eclipse already hands out per process.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};

use dpi::PhysicalSize;
use euclid::Scale;
use servo::{
    DevicePoint, EmbedderControl, EventLoopWaker, InputEvent, Key, KeyState, KeyboardEvent,
    LoadStatus, Location, Modifiers, MouseButton, MouseButtonAction, MouseButtonEvent,
    MouseLeftViewportEvent, MouseMoveEvent, Opts, Preferences, Servo, ServoBuilder, Theme,
    UserContentManager, UserScript, WebView, WebViewBuilder, WebViewDelegate, WebViewPoint,
    WheelDelta, WheelEvent, WheelMode,
};
use crate::about;
use crate::files::{self, PickRequest};
use crate::privacy;
use crate::session::{SavedTab, Snapshot};
use crate::spa_compat::{self, GITHUB_REVEAL_USERSCRIPT};

/// Default HTTP / `navigator.userAgent` string.
///
/// A custom `SilverSurfer/… Servo/…` UA without `Firefox`/`Gecko` tokens makes
/// several major sites (notably Google) serve a degraded shell. Servo itself
/// defaults to a Firefox-shaped UA; we keep that shape and add the `Gecko/`
/// token some CDNs require when they see `Firefox` (see servo/servo#36951).
/// Override with `SILVERSURFER_UA` for debugging.
fn user_agent() -> String {
    match std::env::var("SILVERSURFER_UA") {
        Ok(ua) if !ua.is_empty() => ua,
        _ => {
            "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0 Servo/0.7"
                .into()
        }
    }
}

fn install_compat_preferences(preferences: &mut Preferences) {
    preferences.user_agent = user_agent();
    preferences.dom_webgpu_enabled = true;
    preferences.dom_webgl2_enabled = true;
    preferences.dom_webrtc_enabled = true;

    // Full embedder React/SPA pref surface for Servo 0.7. Incomplete engine
    // impls still fail softer than a hard ReferenceError. Service Workers
    // stay off — half-broken SW registration breaks sites worse than missing SW.
    // layout_css_* container/backdrop prefs are not on Preferences 0.7.
    preferences.dom_intersection_observer_enabled = true;
    preferences.dom_web_animations_enabled = true;
    preferences.dom_fontface_enabled = true;
    preferences.dom_adoptedstylesheet_enabled = true;
    preferences.dom_indexeddb_enabled = true;
    preferences.dom_async_clipboard_enabled = true;
    preferences.dom_visual_viewport_enabled = true;
    preferences.dom_entries_api_enabled = true;
    preferences.dom_offscreen_canvas_enabled = true;
    preferences.dom_storage_manager_api_enabled = true;
    preferences.dom_composition_event_enabled = true;
    preferences.dom_cookiestore_enabled = true;
    preferences.dom_credential_management_enabled = true;
    preferences.dom_exec_command_enabled = true;
    preferences.dom_geolocation_enabled = true;
    preferences.dom_notification_enabled = true;
    preferences.dom_permissions_enabled = true;
    preferences.dom_allow_preloading_module_descendants = true;
    preferences.dom_canvas_capture_enabled = true;
    preferences.dom_wakelock_enabled = true;
    preferences.dom_webrtc_transceiver_enabled = true;

    // Eclipse's panel is dark. A white flash on the first frame reads as a
    // broken window more than as an unloaded page.
    preferences.shell_background_color_rgba = [0.043, 0.071, 0.125, 1.0];
}

fn build_user_content(servo: &Servo) -> Rc<UserContentManager> {
    let manager = Rc::new(UserContentManager::new(servo));
    for script in spa_compat::userscripts() {
        manager.add_script(Rc::new(UserScript::from(script)));
    }
    manager
}

fn reveal_github_if_needed(webview: &WebView) {
    let Some(url) = webview.url() else {
        return;
    };
    let Some(host) = url.host_str() else {
        return;
    };
    if !spa_compat::is_github_host(host) {
        return;
    }
    webview.evaluate_javascript(GITHUB_REVEAL_USERSCRIPT, |_| {});
}

#[derive(Clone)]
pub struct Waker {
    fd: std::os::fd::RawFd,
}

impl Waker {
    pub fn new() -> Result<Self, String> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err("eventfd".into());
        }
        Ok(Self { fd })
    }

    pub fn fd(&self) -> std::os::fd::RawFd {
        self.fd
    }

    pub fn drain(&self) {
        let mut buf = [0u8; 8];
        unsafe {
            libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len());
        }
    }
}

impl EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(self.clone())
    }

    fn wake(&self) {
        let one: u64 = 1;
        unsafe {
            libc::write(self.fd, (&one as *const u64).cast(), std::mem::size_of::<u64>());
        }
    }
}

/// Flags the delegate sets and the Wayland loop reads. Atomics so a wake from
/// another thread never needs the tab list.
#[derive(Default)]
pub struct Flags {
    pub frame: AtomicBool,
    pub chrome: AtomicBool,
    pub animating: AtomicBool,
    /// The script closed the last tab. The window loop exits.
    pub close_window: AtomicBool,
}

pub struct Tab {
    pub view: WebView,
    pub title: String,
    pub url: String,
    pub loading: bool,
    pub animating: bool,
}

pub struct Model {
    pub tabs: Vec<Tab>,
    pub active: usize,
}

pub struct Delegate {
    this: Weak<Delegate>,
    /// Filled in once `ServoBuilder` returns. The first tab is created with
    /// the handle `start` still holds; later tabs clone this one.
    servo: RefCell<Option<Servo>>,
    model: RefCell<Model>,
    context: Rc<dyn servo::RenderingContext>,
    flags: Rc<Flags>,
    scale: Cell<f32>,
    /// Shared userscripts (SPA polyfills + GitHub opacity reveal).
    user_content: RefCell<Option<Rc<UserContentManager>>>,
}

pub struct Session {
    pub servo: Servo,
    pub shell: Rc<Delegate>,
    pub context: Rc<dyn servo::RenderingContext>,
    pub gpu: bool,
    /// Ventana `--private`: no persiste sesión; al salir limpia site data.
    pub private: bool,
}

/// Tear down tabs before `Servo` so content processes get `CloseWebView` and
/// then a clean `Exit`. Without this, the `Delegate` ↔ `WebView` cycle keeps
/// the constellation alive until the process dies, and Servo's BHM logs
/// `BHM control disconnected before shutting down!`.
impl Drop for Session {
    fn drop(&mut self) {
        let tabs = {
            let mut model = self.shell.model.borrow_mut();
            std::mem::take(&mut model.tabs)
        };
        // Dropping each WebView sends CloseWebView and releases its Servo +
        // Delegate refs, breaking the cycle that would otherwise leak the
        // session past the window loop.
        drop(tabs);
        for _ in 0..32 {
            self.servo.spin_event_loop();
        }
        *self.shell.servo.borrow_mut() = None;
        // Field drop order then drops `servo` (ServoInner::Exit + spin) while
        // `shell` is still alive to receive constellation callbacks, then the
        // shell itself.
    }
}

impl Delegate {
    pub fn flags(&self) -> &Flags {
        &self.flags
    }

    pub fn set_scale(&self, scale: f32) {
        self.scale.set(scale.max(1.0));
    }

    fn this(&self) -> Rc<dyn WebViewDelegate> {
        self.this
            .upgrade()
            .expect("el delegado sigue vivo mientras hay pestañas")
    }

    pub fn active_view(&self) -> Option<WebView> {
        let model = self.model.borrow();
        model.tabs.get(model.active).map(|tab| tab.view.clone())
    }

    pub fn tab_titles(&self) -> Vec<String> {
        self.model
            .borrow()
            .tabs
            .iter()
            .map(|tab| tab.title.clone())
            .collect()
    }

    pub fn tab_loading(&self) -> Vec<bool> {
        self.model
            .borrow()
            .tabs
            .iter()
            .map(|tab| tab.loading)
            .collect()
    }

    pub fn active_index(&self) -> usize {
        self.model.borrow().active
    }

    pub fn location_text(&self) -> String {
        let model = self.model.borrow();
        model
            .tabs
            .get(model.active)
            .map(|tab| tab.url.clone())
            .unwrap_or_default()
    }

    /// Título de la ventana xdg: pestaña activa, o marca de carga.
    pub fn window_title(&self) -> String {
        let model = self.model.borrow();
        let Some(tab) = model.tabs.get(model.active) else {
            return "SilverSurfer".into();
        };
        if tab.loading && !tab.title.is_empty() {
            format!("…{} — SilverSurfer", tab.title)
        } else if tab.loading {
            "Cargando — SilverSurfer".into()
        } else if tab.title.is_empty() {
            "SilverSurfer".into()
        } else {
            format!("{} — SilverSurfer", tab.title)
        }
    }

    pub fn can_go_back(&self) -> bool {
        self.active_view().is_some_and(|view| view.can_go_back())
    }

    pub fn can_go_forward(&self) -> bool {
        self.active_view().is_some_and(|view| view.can_go_forward())
    }

    pub fn load(&self, url: url::Url) {
        let index = self.active_index();
        let Some(view) = self.active_view() else {
            return;
        };
        let url = self.expand_url(url);
        let text = url.to_string();
        view.load(url);
        if let Some(tab) = self.model.borrow_mut().tabs.get_mut(index) {
            tab.url = text;
            tab.loading = true;
        }
        self.flags.chrome.store(true, Ordering::Relaxed);
    }

    pub fn new_tab(&self, url: url::Url) {
        let url = self.expand_url(url);
        let text = url.to_string();
        let view = self.make_view(Some(url));
        self.add_tab(view, text);
    }

    fn expand_url(&self, url: url::Url) -> url::Url {
        if url.scheme() != "about" {
            return about::expand(url);
        }
        match url.path().to_ascii_lowercase().as_str() {
            "clear" => {
                let summary = self.clear_browsing_data();
                privacy::cleared_page_url(&summary)
            }
            "cookies" | "site-data" => {
                let sites = self.list_site_data();
                privacy::cookies_page_url(&sites)
            }
            _ => about::expand(url),
        }
    }

    /// Borra cookies, session/local storage y caché HTTP de esta instancia.
    pub fn clear_browsing_data(&self) -> String {
        let Some(servo) = self.servo.borrow().clone() else {
            return "Servo no está listo".into();
        };
        let sdm = servo.site_data_manager();
        let sites = sdm.site_data(servo::StorageType::all());
        let n = sites.len();
        let names: Vec<String> = sites.iter().map(|s| s.name()).collect();
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        if !refs.is_empty() {
            sdm.clear_site_data(&refs, servo::StorageType::all());
        }
        sdm.clear_cookies(None);
        sdm.clear_session_cookies(None);
        servo.network_manager().clear_cache();
        eprintln!("silversurfer: borrados datos de {n} sitios + cookies + caché");
        format!("Borrados datos de {n} sitios, cookies y caché HTTP.")
    }

    pub fn list_site_data(&self) -> Vec<(String, String)> {
        let Some(servo) = self.servo.borrow().clone() else {
            return Vec::new();
        };
        let sdm = servo.site_data_manager();
        sdm.site_data(servo::StorageType::all())
            .into_iter()
            .map(|site| {
                let mut kinds = Vec::new();
                let t = site.storage_types();
                if t.contains(servo::StorageType::Cookies) {
                    kinds.push("cookies");
                }
                if t.contains(servo::StorageType::Local) {
                    kinds.push("local");
                }
                if t.contains(servo::StorageType::Session) {
                    kinds.push("session");
                }
                (site.name(), kinds.join("+"))
            })
            .collect()
    }

    pub fn go_back(&self) {
        if let Some(view) = self.active_view() {
            self.mark_active_loading();
            view.go_back(1);
        }
    }

    pub fn go_forward(&self) {
        if let Some(view) = self.active_view() {
            self.mark_active_loading();
            view.go_forward(1);
        }
    }

    pub fn reload(&self) {
        if let Some(view) = self.active_view() {
            self.mark_active_loading();
            view.reload();
        }
    }

    fn mark_active_loading(&self) {
        let active = self.active_index();
        if let Some(tab) = self.model.borrow_mut().tabs.get_mut(active) {
            tab.loading = true;
        }
        self.flags.chrome.store(true, Ordering::Relaxed);
    }

    /// Close `index`. `false` means it was the last tab and the window should
    /// close.
    pub fn close_tab(&self, index: usize) -> bool {
        let view = {
            let mut model = self.model.borrow_mut();
            if model.tabs.len() <= 1 {
                return false;
            }
            if index >= model.tabs.len() {
                return true;
            }
            let tab = model.tabs.remove(index);
            if model.active >= model.tabs.len() {
                model.active = model.tabs.len() - 1;
            } else if index < model.active {
                model.active -= 1;
            }
            tab.view
        };
        view.hide();
        if let Some(next) = self.active_view() {
            next.show();
            next.set_focused(true);
        }
        self.sync_animating();
        self.flags.chrome.store(true, Ordering::Relaxed);
        self.flags.frame.store(true, Ordering::Relaxed);
        true
    }

    pub fn select_tab(&self, index: usize) {
        let (prev, next) = {
            let mut model = self.model.borrow_mut();
            if index >= model.tabs.len() || index == model.active {
                return;
            }
            let prev = model.tabs[model.active].view.clone();
            model.active = index;
            let next = model.tabs[index].view.clone();
            (prev, next)
        };
        prev.set_focused(false);
        prev.hide();
        next.show();
        next.set_focused(true);
        self.sync_animating();
        self.flags.chrome.store(true, Ordering::Relaxed);
        self.flags.frame.store(true, Ordering::Relaxed);
    }

    pub fn next_tab(&self) {
        let n = self.model.borrow().tabs.len();
        if n == 0 {
            return;
        }
        let next = (self.active_index() + 1) % n;
        self.select_tab(next);
    }

    pub fn prev_tab(&self) {
        let n = self.model.borrow().tabs.len();
        if n == 0 {
            return;
        }
        let active = self.active_index();
        let prev = if active == 0 { n - 1 } else { active - 1 };
        self.select_tab(prev);
    }

    pub fn tab_count(&self) -> usize {
        self.model.borrow().tabs.len()
    }

    /// Snapshot for session.json (URLs + titles + active index).
    pub fn snapshot(&self) -> Snapshot {
        let model = self.model.borrow();
        Snapshot {
            active: model.active,
            tabs: model
                .tabs
                .iter()
                .map(|tab| SavedTab {
                    url: tab.url.clone(),
                    title: tab.title.clone(),
                })
                .collect(),
        }
    }

    /// Open extra tabs after `start` and select `active`.
    pub fn restore_tabs(&self, tabs: &[(url::Url, String)], active: usize) {
        for (url, title) in tabs {
            let text = url.to_string();
            let view = self.make_view(Some(url.clone()));
            self.add_tab(view, text);
            if let Some(tab) = self.model.borrow_mut().tabs.last_mut() {
                if !title.is_empty() {
                    tab.title = title.clone();
                }
            }
        }
        if active > 0 {
            self.select_tab(active.min(self.tab_count().saturating_sub(1)));
        }
    }

    pub fn set_page_focused(&self, focused: bool) {
        if let Some(view) = self.active_view() {
            view.set_focused(focused);
        }
    }

    fn sync_animating(&self) {
        let animating = {
            let model = self.model.borrow();
            model
                .tabs
                .get(model.active)
                .is_some_and(|tab| tab.animating)
        };
        self.flags.animating.store(animating, Ordering::Relaxed);
    }

    pub fn resize(&self, width: u32, height: u32, scale: f32) {
        self.set_scale(scale);
        let views: Vec<WebView> = self
            .model
            .borrow()
            .tabs
            .iter()
            .map(|tab| tab.view.clone())
            .collect();
        let factor = Scale::new(self.scale.get());
        let size = PhysicalSize::new(width.max(1), height.max(1));
        for view in views {
            view.set_hidpi_scale_factor(factor);
            view.resize(size);
        }
    }

    pub fn paint(&self) {
        if let Some(view) = self.active_view() {
            view.paint();
        }
    }

    pub fn pointer(&self, x: f32, y: f32, button: Option<(MouseButton, bool)>) {
        let Some(view) = self.active_view() else {
            return;
        };
        let point = WebViewPoint::Device(DevicePoint::new(x, y));
        if let Some((button, down)) = button {
            let action = if down {
                MouseButtonAction::Down
            } else {
                MouseButtonAction::Up
            };
            view.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                action, button, point,
            )));
        } else {
            view.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(point)));
        }
    }

    pub fn pointer_left(&self) {
        if let Some(view) = self.active_view() {
            view.notify_input_event(InputEvent::MouseLeftViewport(MouseLeftViewportEvent::default()));
        }
    }

    pub fn wheel(&self, x: f32, y: f32, dx: f64, dy: f64) {
        let Some(view) = self.active_view() else {
            return;
        };
        view.notify_input_event(InputEvent::Wheel(WheelEvent::new(
            WheelDelta {
                x: dx,
                y: dy,
                z: 0.0,
                mode: WheelMode::DeltaPixel,
            },
            WebViewPoint::Device(DevicePoint::new(x, y)),
        )));
    }

    pub fn key(&self, down: bool, key: Key, code: servo::Code, mods: Modifiers, repeat: bool) {
        let Some(view) = self.active_view() else {
            return;
        };
        let state = if down { KeyState::Down } else { KeyState::Up };
        view.notify_input_event(InputEvent::Keyboard(KeyboardEvent::new_without_event(
            state,
            key,
            code,
            Location::Standard,
            mods,
            repeat,
            false,
        )));
    }

    fn make_view(&self, url: Option<url::Url>) -> WebView {
        let mut builder = WebViewBuilder::new(&self.servo_handle(), self.context.clone())
            .delegate(self.this())
            .hidpi_scale_factor(Scale::new(self.scale.get()));
        if let Some(manager) = self.user_content.borrow().clone() {
            builder = builder.user_content_manager(manager);
        }
        if let Some(url) = url {
            builder = builder.url(self.expand_url(url));
        }
        let view = builder.build();
        view.notify_theme_change(Theme::Dark);
        view
    }

    fn servo_handle(&self) -> Servo {
        self.servo
            .borrow()
            .clone()
            .expect("Servo ya está construido cuando se abre una pestaña")
    }

    fn add_tab(&self, view: WebView, url: String) {
        if let Some(prev) = self.active_view() {
            prev.hide();
        }
        view.show();
        view.set_focused(true);
        let mut model = self.model.borrow_mut();
        model.tabs.push(Tab {
            view,
            title: "Nueva pestaña".into(),
            url,
            loading: true,
            animating: false,
        });
        model.active = model.tabs.len() - 1;
        self.flags.chrome.store(true, Ordering::Relaxed);
        self.flags.frame.store(true, Ordering::Relaxed);
    }

    fn tab_mut_by_id(&self, view: &WebView) -> Option<std::cell::RefMut<'_, Tab>> {
        let id = view.id();
        let model = self.model.borrow_mut();
        let index = model.tabs.iter().position(|tab| tab.view.id() == id)?;
        Some(std::cell::RefMut::map(model, |model| &mut model.tabs[index]))
    }
}

impl WebViewDelegate for Delegate {
    fn notify_new_frame_ready(&self, _webview: WebView) {
        self.flags.frame.store(true, Ordering::Relaxed);
    }

    fn notify_animating_changed(&self, webview: WebView, animating: bool) {
        let active = {
            let mut model = self.model.borrow_mut();
            let Some(tab) = model.tabs.iter_mut().find(|tab| tab.view.id() == webview.id()) else {
                return;
            };
            tab.animating = animating;
            model.tabs.get(model.active).map(|tab| tab.view.id())
        };
        if active == Some(webview.id()) {
            self.flags.animating.store(animating, Ordering::Relaxed);
        }
    }

    fn notify_url_changed(&self, webview: WebView, url: url::Url) {
        if let Some(mut tab) = self.tab_mut_by_id(&webview) {
            tab.url = url.to_string();
        }
        self.flags.chrome.store(true, Ordering::Relaxed);
    }

    fn notify_page_title_changed(&self, webview: WebView, title: Option<String>) {
        if let Some(mut tab) = self.tab_mut_by_id(&webview) {
            tab.title = title.unwrap_or_else(|| "SilverSurfer".into());
        }
        self.flags.chrome.store(true, Ordering::Relaxed);
    }

    fn notify_load_status_changed(&self, webview: WebView, status: LoadStatus) {
        if let Some(mut tab) = self.tab_mut_by_id(&webview) {
            tab.loading = status != LoadStatus::Complete;
        }
        if status == LoadStatus::Complete {
            // Belt-and-suspenders: userscripts run at document start, but if
            // React re-hydrates the hero back to opacity:0 after a failed GSAP
            // path, re-run the reveal once the load settles.
            reveal_github_if_needed(&webview);
        }
        self.flags.chrome.store(true, Ordering::Relaxed);
    }

    fn request_create_new(
        &self,
        _parent: WebView,
        request: servo::CreateNewWebViewRequest,
    ) {
        let mut builder = request
            .builder(self.context.clone())
            .delegate(self.this())
            .hidpi_scale_factor(Scale::new(self.scale.get()));
        if let Some(manager) = self.user_content.borrow().clone() {
            builder = builder.user_content_manager(manager);
        }
        let view = builder.build();
        view.notify_theme_change(Theme::Dark);
        self.add_tab(view, "about:blank".into());
    }

    fn notify_closed(&self, webview: WebView) {
        let index = {
            let model = self.model.borrow();
            model.tabs.iter().position(|tab| tab.view.id() == webview.id())
        };
        let Some(index) = index else {
            return;
        };
        if !self.close_tab(index) {
            self.flags.close_window.store(true, Ordering::Relaxed);
        }
    }

    fn show_embedder_control(&self, _webview: WebView, control: EmbedderControl) {
        let EmbedderControl::FilePicker(mut picker) = control else {
            return;
        };
        let request = PickRequest {
            multiple: picker.allow_select_multiple(),
            save_name: None,
        };
        match files::pick(&request) {
            Ok(paths) if !paths.is_empty() => {
                picker.select(&paths);
                picker.submit();
            }
            _ => picker.dismiss(),
        }
    }
}

pub fn start(
    waker: Waker,
    context: Rc<dyn servo::RenderingContext>,
    gpu: bool,
    url: url::Url,
    scale: f32,
    private: bool,
) -> Result<Session, String> {
    install_crypto();
    let mut opts = Opts::default();
    opts.multiprocess = true;
    let mut preferences = Preferences::default();
    install_compat_preferences(&mut preferences);

    let flags = Rc::new(Flags::default());
    let context_for_shell = context.clone();
    let shell = Rc::new_cyclic(|weak| Delegate {
        this: weak.clone(),
        servo: RefCell::new(None),
        model: RefCell::new(Model {
            tabs: Vec::new(),
            active: 0,
        }),
        context: context_for_shell,
        flags: flags.clone(),
        scale: Cell::new(scale.max(1.0)),
        user_content: RefCell::new(None),
    });

    let servo = ServoBuilder::default()
        .event_loop_waker(Box::new(waker))
        .opts(opts)
        .preferences(preferences)
        .build();
    servo.setup_logging();
    *shell.servo.borrow_mut() = Some(servo.clone());
    let user_content = build_user_content(&servo);
    *shell.user_content.borrow_mut() = Some(user_content.clone());

    // The first tab is built with the handle we still hold. Later tabs clone
    // the copy stored on the delegate.
    let url = shell.expand_url(url);
    let view = WebViewBuilder::new(&servo, context.clone())
        .delegate(shell.clone())
        .user_content_manager(user_content)
        .url(url.clone())
        .hidpi_scale_factor(Scale::new(scale.max(1.0)))
        .build();
    view.notify_theme_change(Theme::Dark);
    view.show();
    view.set_focused(true);
    shell.model.borrow_mut().tabs.push(Tab {
        view,
        title: if private {
            "Privado".into()
        } else {
            "Nueva pestaña".into()
        },
        url: url.to_string(),
        loading: true,
        animating: false,
    });
    shell.flags.chrome.store(true, Ordering::Relaxed);

    Ok(Session {
        servo,
        shell,
        context,
        gpu,
        private,
    })
}

fn install_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

pub fn software_context(
    width: u32,
    height: u32,
) -> Result<Rc<dyn servo::RenderingContext>, String> {
    let context = servo::SoftwareRenderingContext::new(PhysicalSize::new(width.max(1), height.max(1)))
        .map_err(|err| format!("contexto de software: {err:?}"))?;
    Ok(Rc::new(context))
}

