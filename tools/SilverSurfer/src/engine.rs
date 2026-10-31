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
    Location, Modifiers, MouseButton, MouseButtonAction, MouseButtonEvent, MouseLeftViewportEvent,
    MouseMoveEvent, Opts, Preferences, Servo, ServoBuilder, Theme, WebView, WebViewBuilder,
    WebViewDelegate, WebViewPoint, WheelDelta, WheelEvent, WheelMode,
};
use crate::files::{self, PickRequest};

const USER_AGENT: &str = "SilverSurfer/0.1 (Eclipse OS; Wayland) Servo/0.7";

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
}

pub struct Tab {
    pub view: WebView,
    pub title: String,
    pub url: String,
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
}

pub struct Session {
    pub servo: Servo,
    pub shell: Rc<Delegate>,
    pub context: Rc<dyn servo::RenderingContext>,
    pub gpu: bool,
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

    pub fn can_go_back(&self) -> bool {
        self.active_view().is_some_and(|view| view.can_go_back())
    }

    pub fn can_go_forward(&self) -> bool {
        self.active_view().is_some_and(|view| view.can_go_forward())
    }

    pub fn load(&self, url: url::Url) {
        let Some(view) = self.active_view() else {
            return;
        };
        let text = url.to_string();
        view.load(url);
        if let Some(tab) = self.model.borrow_mut().tabs.get_mut(self.active_index()) {
            tab.url = text;
        }
        self.flags.chrome.store(true, Ordering::Relaxed);
    }

    pub fn go_back(&self) {
        if let Some(view) = self.active_view() {
            view.go_back(1);
        }
    }

    pub fn go_forward(&self) {
        if let Some(view) = self.active_view() {
            view.go_forward(1);
        }
    }

    pub fn reload(&self) {
        if let Some(view) = self.active_view() {
            view.reload();
        }
    }

    pub fn new_tab(&self, url: url::Url) {
        let text = url.to_string();
        let view = self.make_view(Some(url));
        self.add_tab(view, text);
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
        prev.hide();
        next.show();
        next.set_focused(true);
        self.flags.chrome.store(true, Ordering::Relaxed);
        self.flags.frame.store(true, Ordering::Relaxed);
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
        if let Some(url) = url {
            builder = builder.url(url);
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

    fn notify_animating_changed(&self, _webview: WebView, animating: bool) {
        self.flags.animating.store(animating, Ordering::Relaxed);
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

    fn request_create_new(
        &self,
        _parent: WebView,
        request: servo::CreateNewWebViewRequest,
    ) {
        let view = request
            .builder(self.context.clone())
            .delegate(self.this())
            .hidpi_scale_factor(Scale::new(self.scale.get()))
            .build();
        view.notify_theme_change(Theme::Dark);
        self.add_tab(view, "about:blank".into());
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
) -> Result<Session, String> {
    install_crypto();
    let mut opts = Opts::default();
    opts.multiprocess = true;
    let mut preferences = Preferences::default();
    preferences.user_agent = USER_AGENT.into();
    preferences.dom_webgpu_enabled = true;
    preferences.dom_webgl2_enabled = true;
    preferences.dom_webrtc_enabled = true;
    // Eclipse's panel is dark. A white flash on the first frame reads as a
    // broken window more than as an unloaded page.
    preferences.shell_background_color_rgba = [0.043, 0.071, 0.125, 1.0];

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
    });

    let servo = ServoBuilder::default()
        .event_loop_waker(Box::new(waker))
        .opts(opts)
        .preferences(preferences)
        .build();
    servo.setup_logging();
    *shell.servo.borrow_mut() = Some(servo.clone());

    // The first tab is built with the handle we still hold. Later tabs clone
    // the copy stored on the delegate.
    let view = WebViewBuilder::new(&servo, context.clone())
        .delegate(shell.clone())
        .url(url.clone())
        .hidpi_scale_factor(Scale::new(scale.max(1.0)))
        .build();
    view.notify_theme_change(Theme::Dark);
    view.show();
    view.set_focused(true);
    shell.model.borrow_mut().tabs.push(Tab {
        view,
        title: "Nueva pestaña".into(),
        url: url.to_string(),
    });
    shell.flags.chrome.store(true, Ordering::Relaxed);

    Ok(Session {
        servo,
        shell,
        context,
        gpu,
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

