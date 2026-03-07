//! Compositor Wayland con Smithay — solo se compila para target Linux (host).
//! Mismo binario que en Eclipse; el backend se elige por target.
//!
//! # Por qué crasheaba antes
//!
//! Con `debug = true` + `strip = false` en el perfil de release el binario
//! crecía a ~75 MB.  Un binario tan grande tiene muchos segmentos ELF LOAD
//! que el kernel mapea como VMAs separadas al cargar el proceso.  Antes de
//! que `main()` se ejecute, el runtime de Rust llama a `mprotect()` para
//! instalar el guard-page del alternate signal stack.  Si en ese momento el
//! proceso ya tiene demasiadas VMAs (cerca del límite `vm.max_map_count`),
//! `mprotect()` falla con ENOMEM y el proceso aborta con:
//!
//!   "failed to set up alternative stack guard page: Cannot allocate memory"
//!
//! **La solución** (ya aplicada en `eclipse-apps/Cargo.toml`) es compilar
//! con `debug = false` y `strip = "symbols"`.  Esto reduce el binario a
//! ~4 MB y elimina el exceso de VMAs, resolviendo el crash sin necesidad de
//! eliminar smithay.

use std::os::unix::io::OwnedFd;
use std::sync::Arc;

use smithay::backend::input::{InputEvent, KeyboardKeyEvent};
use smithay::backend::renderer::element::surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::utils::{draw_render_elements, on_commit_buffer_handler};
use smithay::backend::renderer::{Color32F, Frame, Renderer};
use smithay::backend::winit::{self, WinitEvent};
use smithay::input::keyboard::FilterResult;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::protocol::wl_seat;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{Rectangle, Serial, Transform};
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
        &client.get_data::<ClientState>().unwrap().compositor_state
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

struct App {
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    shm_state: ShmState,
    seat_state: SeatState<App>,
    data_device_state: DataDeviceState,
    seat: Seat<App>,
}

/// Count the current number of virtual memory areas (VMAs) for this process
/// by reading /proc/self/maps.  Used for startup diagnostics.
fn count_vmas() -> usize {
    std::fs::read_to_string("/proc/self/maps")
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

/// Número estimado de VMAs adicionales que smithay+libGL/EGL necesita al inicializarse.
/// El driver de la GPU (radv, iris, nvidia, etc.) abre varias bibliotecas via dlopen;
/// cada segmento de esas .so añade VMAs.  400 es un margen conservador.
const GL_VMAS_ESTIMATE: usize = 400;

/// Intenta aumentar vm.max_map_count escribiendo en /proc/sys/vm/max_map_count.
/// Solo funciona si el proceso se ejecuta como root o tiene CAP_SYS_ADMIN.
/// Devuelve true si tuvo éxito.
fn try_raise_max_map_count(target: usize) -> bool {
    std::fs::write(
        "/proc/sys/vm/max_map_count",
        target.to_string().as_bytes(),
    )
    .is_ok()
}

/// Comprueba que haya suficientes VMAs libres para la inicialización de GL.
/// Si no hay suficiente espacio, intenta aumentar vm.max_map_count automáticamente.
/// Si no tiene permisos, devuelve un error con el comando sysctl para que el usuario
/// lo ejecute manualmente.
fn ensure_vma_headroom(current: usize, max: usize) -> Result<(), String> {
    // Margen de seguridad extra sobre la estimación de GL
    const SAFETY_MARGIN: usize = 200;
    // Margen adicional por encima de `needed` al fijar el nuevo max_map_count
    const EXTRA_HEADROOM_MARGIN: usize = 1000;

    let needed = current + GL_VMAS_ESTIMATE + SAFETY_MARGIN;

    if needed <= max {
        // Hay suficiente espacio: no se necesita ninguna acción.
        return Ok(());
    }

    // Espacio insuficiente — intentar auto-corregir
    let new_max = needed + EXTRA_HEADROOM_MARGIN;
    eprintln!(
        "[smithay_app] ADVERTENCIA: VMAs insuficientes ({} actuales + ~{} para GL = {} necesarias, límite = {})",
        current, GL_VMAS_ESTIMATE, needed, max
    );

    if try_raise_max_map_count(new_max) {
        eprintln!(
            "[smithay_app] vm.max_map_count aumentado automáticamente a {} (OK).",
            new_max
        );
        Ok(())
    } else {
        Err(format!(
            "VMAs insuficientes ({} actuales, ~{} usará GL, límite {}, libres tras GL: {}). \
             Ejecuta como root:\n  sudo sysctl -w vm.max_map_count={}",
            current,
            GL_VMAS_ESTIMATE,
            max,
            (max as isize - current as isize - GL_VMAS_ESTIMATE as isize).max(0),
            new_max
        ))
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Diagnóstico de inicio: mostrar cuántas VMAs tiene el proceso al arrancar.
    // Si este número se acerca a `vm.max_map_count` (normalmente 65536), el
    // siguiente mprotect() puede fallar con ENOMEM.
    let vmas_at_start = count_vmas();
    let max_map_count = std::fs::read_to_string("/proc/sys/vm/max_map_count")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(65536);
    eprintln!(
        "[smithay_app] Inicio: VMAs={} / max_map_count={} ({}% ocupado)",
        vmas_at_start,
        max_map_count,
        vmas_at_start * 100 / max_map_count.max(1)
    );

    // Verificar que hay suficientes VMAs libres para GL/Wayland.
    // Si no, intentar aumentar el límite automáticamente o fallar con instrucciones claras.
    ensure_vma_headroom(vmas_at_start, max_map_count)?;

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

    let mut state = App {
        compositor_state,
        xdg_shell_state: XdgShellState::new::<App>(&dh),
        shm_state,
        seat_state,
        data_device_state: DataDeviceState::new::<App>(&dh),
        seat,
    };

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

    // Log VMAs again after GL/Wayland init to show how many were added by GL.
    let vmas_after_gl = count_vmas();
    eprintln!(
        "[smithay_app] Tras init GL/Wayland: VMAs={} (+{} vs inicio)",
        vmas_after_gl,
        vmas_after_gl.saturating_sub(vmas_at_start)
    );

    loop {
        let status = winit.dispatch_new_events(|event| match event {
            WinitEvent::Resized { .. } => {}
            WinitEvent::Input(event) => match event {
                InputEvent::Keyboard { event } => {
                    keyboard.input::<(), _>(
                        &mut state,
                        event.key_code(),
                        event.state(),
                        0.into(),
                        0,
                        |_, _, _| FilterResult::Forward,
                    );
                }
                InputEvent::PointerMotionAbsolute { .. } => {
                    if let Some(surface) = state.xdg_shell_state.toplevel_surfaces().iter().next().cloned() {
                        let surface = surface.wl_surface().clone();
                        keyboard.set_focus(&mut state, Some(surface), 0.into());
                    }
                }
                _ => {}
            },
            _ => (),
        });

        match status {
            PumpStatus::Continue => (),
            PumpStatus::Exit(_) => return Ok(()),
        }

        let size = backend.window_size();
        let damage = Rectangle::from_size(size);
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

            let mut frame = renderer
                // Transform::Flipped180: winit presenta el buffer con Y invertido
                // respecto a la convención OpenGL; el flip compensa esa diferencia.
                .render(&mut framebuffer, size, Transform::Flipped180)?;
            frame.clear(Color32F::new(0.1, 0.0, 0.0, 1.0), &[damage])?;
            draw_render_elements(&mut frame, 1.0, &elements, &[damage])?;
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

        backend.submit(Some(&[damage]))?;
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
