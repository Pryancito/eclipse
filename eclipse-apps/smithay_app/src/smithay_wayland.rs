//! Compositor Wayland con Smithay — solo se compila para target Linux (host).
//! Mismo binario que en Eclipse; el backend se elige por target.
//!
//! # Por qué crasheaba antes ("failed to set up alternative stack guard page")
//!
//! Con `debug = true` + `strip = false` en el perfil de release el binario
//! crecía a ~75 MB.  Un binario tan grande tiene cientos de segmentos ELF
//! PT_LOAD que el kernel mapea como VMAs separadas al cargar el proceso.
//! Antes de que `main()` se ejecute, el runtime de Rust llama a `mprotect()`
//! para instalar el guard-page del alternate signal stack.  Si en ese momento
//! el proceso ya tiene demasiadas VMAs (cerca del límite `vm.max_map_count`),
//! `mprotect()` falla con ENOMEM y el proceso aborta con:
//!
//!   "failed to set up alternative stack guard page: Cannot allocate memory"
//!
//! **La solución** está en `eclipse-apps/Cargo.toml` (raíz del workspace):
//! compilar con `debug = false` y `strip = "symbols"`.  Esto reduce el binario
//! a ~3.5 MB con solo 4 segmentos PT_LOAD y ~4 bibliotecas dinámicas NEEDED,
//! lo que resulta en ~20–25 VMAs al arrancar — muy por debajo del límite.
//!
//! **IMPORTANTE:** Las opciones de perfil deben estar en `eclipse-apps/Cargo.toml`,
//! NO en `smithay_app/Cargo.toml`.  Cargo ignora silenciosamente las secciones
//! `[profile.*]` definidas en paquetes miembro de un workspace.

use std::os::unix::io::OwnedFd;
use std::sync::Arc;

use smithay::backend::input::{InputEvent, KeyboardKeyEvent, AbsolutePositionEvent};
use smithay::backend::renderer::element::surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement};
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::{Kind, Id};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::utils::{draw_render_elements, on_commit_buffer_handler};
use smithay::backend::renderer::{Color32F, Frame, Renderer, ImportMem};
use smithay::backend::allocator::Fourcc;
use smithay::backend::winit::{self, WinitEvent};
use smithay::input::keyboard::FilterResult;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::protocol::wl_seat;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{Rectangle, Serial, Transform, Point, Physical, Scale};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    with_surface_tree_downward, CompositorClientState, CompositorHandler, CompositorState,
    SurfaceAttributes, TraversalAction,
};
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shell::xdg::{PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_buffer;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, ListeningSocket};
use smithay::reexports::winit::platform::pump_events::PumpStatus;

use embedded_graphics::geometry::Point as EgPoint;
use crate::render::FramebufferState;
// use crate::compositor::{ShellWindow, WindowContent};
// use crate::input::KeyAction;

smithay::delegate_compositor!(App);
smithay::delegate_data_device!(App);
smithay::delegate_seat!(App);
smithay::delegate_shm!(App);
smithay::delegate_xdg_shell!(App);

impl BufferHandler for App {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl XdgShellHandler for App {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();
    }

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {}

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}

    fn reposition_request(&mut self, _surface: PopupSurface, _positioner: PositionerState, _token: u32) {}
}

impl SelectionHandler for App {
    type SelectionUserData = ();
}

impl DataDeviceHandler for App {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for App {}
impl ServerDndGrabHandler for App {
    fn send(&mut self, _mime_type: String, _fd: OwnedFd, _seat: Seat<App>) {}
}

impl CompositorHandler for App {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("ClientState missing: client not inserted via our accept()")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
    }
}

impl ShmHandler for App {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl SeatHandler for App {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<App> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<App>, _focused: Option<&WlSurface>) {}
    fn cursor_image(&mut self, _seat: &Seat<App>, _image: smithay::input::pointer::CursorImageStatus) {}
}

impl App {
    pub fn draw_desktop(&mut self) {
        self.counter = self.counter.wrapping_add(1);
        
        crate::render::draw_desktop_shell(
            &mut self.desktop_fb,
            &[],
            0,
            self.counter,
            self.cursor_x,
            self.cursor_y,
            &mut self.log_buf,
            &mut self.log_len,
        );

        if self.dashboard_active {
            crate::render::draw_dashboard(
                &mut self.desktop_fb, 
                self.counter, 
                self.cpu_usage, 
                self.mem_usage, 
                self.net_usage, 
                0, // cpu_temp
                0, // gpu_load
                0, // gpu_temp
                0, // anomalies
                0, // heap fragmentation
                0, // uptime_ticks
                0, // cpu_count
                0, // mem_total_kb
                0, // gpu_vram_total_kb
            );
        }

        if self.launcher_active {
            crate::render::draw_launcher(&mut self.desktop_fb, 710.0); // Fixed position for now
        }

    }
}

struct App {
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    shm_state: ShmState,
    seat_state: SeatState<App>,
    data_device_state: DataDeviceState,
    seat: Seat<App>,

    // Desktop Desktop components
    desktop_fb: FramebufferState,
    dashboard_active: bool,
    launcher_active: bool,
    tiling_active: bool,
    
    // Performance metrics
    cpu_usage: f32,
    mem_usage: f32,
    net_usage: f32,
    
    counter: u64,

    /// Textura cacheada del desktop para evitar re-importar ~8MB cada frame.
    desktop_texture_cache: Option<GlesTexture>,

    pub log_buf: [u8; 512],
    pub log_len: usize,

    pub cursor_x: i32,
    pub cursor_y: i32,
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }

    let mut display: Display<App> = Display::new()?;
    let dh = display.handle();

    let compositor_state = CompositorState::new::<App>(&dh);
    let shm_state = ShmState::new::<App>(&dh, vec![]);
    let mut seat_state = SeatState::new();
    let seat = seat_state.new_wl_seat(&dh, "winit");

    let desktop_fb = FramebufferState::init_software(1920, 1080).expect("Failed to init software FB");

    let mut state = Box::new(App {
        compositor_state,
        xdg_shell_state: XdgShellState::new::<App>(&dh),
        shm_state,
        seat_state,
        data_device_state: DataDeviceState::new::<App>(&dh),
        seat,
        desktop_fb,
        dashboard_active: false,
        launcher_active: false,
        tiling_active: false,
        cpu_usage: 0.0,
        mem_usage: 0.0,
        net_usage: 0.0,
        counter: 0,
        desktop_texture_cache: None,
        log_buf: [0; 512],
        log_len: 0,
        cursor_x: 0,
        cursor_y: 0,
    });

    let listener = ListeningSocket::bind("wayland-5")
        .map_err(|e| format!("No se pudo crear el socket Wayland 'wayland-5': {e}"))?;
    let mut _clients = Vec::new();

    let (mut backend, mut winit) = winit::init::<GlesRenderer>()?;

    let start_time = std::time::Instant::now();

    let keyboard = state
        .seat
        .add_keyboard(Default::default(), 200, 200)
        .map_err(|e| format!("Error al inicializar teclado Wayland: {e}"))?;

    // set_var es seguro aquí: smithay usa un solo hilo en este punto del arranque.
    std::env::set_var("WAYLAND_DISPLAY", "wayland-5");

    // Intentar lanzar weston-terminal como cliente de prueba; el fallo es ignorable
    // porque el compositor funciona sin él (el usuario puede conectar otros clientes).
    if let Err(e) = std::process::Command::new("weston-terminal").spawn() {
        eprintln!("[smithay_app] weston-terminal no disponible (ignorado): {e}");
    }

    loop {
        let status = winit.dispatch_new_events(|event| match event {
            WinitEvent::Resized { .. } => {}
            WinitEvent::Input(event) => match event {
                InputEvent::Keyboard { event } => {
                    let scancode = event.key_code();
                    let pressed = event.state() == smithay::backend::input::KeyState::Pressed;
                    
                    let scancode_u32 = u32::from(scancode);
                    if pressed {
                        match scancode_u32 {
                            0x5B => state.dashboard_active = !state.dashboard_active, // Super
                            0x1E => if state.dashboard_active { state.launcher_active = !state.launcher_active }, // A
                            _ => {}
                        }
                    }

                    keyboard.input::<(), _>(
                        &mut state,
                        scancode,
                        event.state(),
                        0.into(),
                        0,
                        |_, _, _| FilterResult::Forward,
                    );
                }
                InputEvent::PointerMotionAbsolute { event, .. } => {
                    let size = backend.window_size();
                    let logical_size = smithay::utils::Size::from((size.w, size.h));
                    let pos = event.position_transformed(logical_size);
                    state.cursor_x = pos.x as i32;
                    state.cursor_y = pos.y as i32;

                    if let Some(surface) = state.xdg_shell_state.toplevel_surfaces().iter().next().cloned() {
                        let surface = surface.wl_surface().clone();
                        keyboard.set_focus(&mut state, Some(surface), 0.into());
                    }
                }
                _ => {}
            },
            _ => (),
        });

        let size = backend.window_size();

        // 1. Draw UI elements into the desktop software framebuffer
        state.draw_desktop();

        {
            let (mut renderer, mut framebuffer) = backend.bind()?;
            let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = state
                .xdg_shell_state
                .toplevel_surfaces()
                .iter()
                .flat_map(|surface| {
                    render_elements_from_surface_tree::<GlesRenderer, _>(
                        &mut renderer,
                        surface.wl_surface(),
                        (0, 0),
                        1.0,
                        1.0,
                        Kind::Unspecified,
                    )
                })
                .collect();

            // 2. Import o actualizar textura del desktop (cache para evitar 8MB upload cada frame)
            let width = state.desktop_fb.info.width as i32;
            let height = state.desktop_fb.info.height as i32;
            let fb_size = (state.desktop_fb.info.pitch * state.desktop_fb.info.height) as usize;
            let data = unsafe {
                core::slice::from_raw_parts(
                    state.desktop_fb.back_addr as *const u8,
                    fb_size
                )
            };
            let desktop_texture = match &mut state.desktop_texture_cache {
                Some(cached) => {
                    let region = Rectangle::new(Point::from((0, 0)), smithay::utils::Size::from((width, height)));
                    let _ = renderer.update_memory(cached, data, region);
                    cached.clone()
                }
                None => {
                    let tex = renderer.import_memory(
                        data,
                        Fourcc::Argb8888,
                        (width, height).into(),
                        false,
                    ).unwrap();
                    state.desktop_texture_cache = Some(tex.clone());
                    tex
                }
            };
            let desktop_element = TextureRenderElement::from_static_texture(
                Id::new(),
                renderer.context_id(),
                Point::<f64, Physical>::from((0.0, 0.0)),
                desktop_texture,
                1,
                Transform::Normal,
                Some(1.0f32),
                None,
                None,
                None,
                Default::default(),
            );

            let mut frame = renderer
                // Transform::Flipped180: winit presenta el buffer con Y invertido
                // respecto a la convención OpenGL; el flip compensa esa diferencia.
                .render(&mut framebuffer, size, Transform::Flipped180)?;
            frame.clear(Color32F::new(0.1, 0.0, 0.0, 1.0), &[])?;

            // Draw background first, then Wayland windows on top
            draw_render_elements::<GlesRenderer, Scale<f64>, TextureRenderElement<GlesTexture>>(&mut frame, Scale::from(1.0), &[desktop_element], &[])?;
            draw_render_elements::<GlesRenderer, Scale<f64>, WaylandSurfaceRenderElement<GlesRenderer>>(&mut frame, Scale::from(1.0), &elements, &[])?;

            let _ = frame.finish()?;

            for surface in state.xdg_shell_state.toplevel_surfaces() {
                send_frames_surface_tree(surface.wl_surface(), start_time.elapsed().as_millis() as u32);
            }

            if let Some(stream) = listener.accept()? {
                let client = display
                    .handle()
                    .insert_client(stream, Arc::new(ClientState::default()))
                    .unwrap();
                _clients.push(client);
            }

            display.dispatch_clients(&mut state)?;
            display.flush_clients()?;
        }

        backend.submit(None)?;
    }
}

fn send_frames_surface_tree(surface: &WlSurface, time: u32) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |_surf, states, &()| {
            for callback in states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
            {
                callback.done(time);
            }
        },
        |_, _, &()| true,
    );
}

#[derive(Default)]
struct ClientState {
    compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {
        println!("initialized");
    }

    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {
        println!("disconnected");
    }
}
