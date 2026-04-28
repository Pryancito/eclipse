//! Estado global del compositor — `LabwcState`. Glue entre Smithay y nuestra capa labwc.
//!
//! Smithay impone una organización donde TODOS los protocolos requieren un
//! `*Handler` impl en el state struct. Definimos aquí el struct y delegamos
//! cada handler a su archivo en `handlers/`.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use smithay::{
    desktop::{Space, Window, PopupManager},
    input::{Seat, SeatState, pointer::CursorImageStatus},
    output::Output,
    reexports::{
        calloop::{LoopHandle, LoopSignal},
        wayland_server::{Display, DisplayHandle, backend::ClientData},
    },
    wayland::{
        compositor::CompositorState,
        output::OutputManagerState,
        selection::data_device::DataDeviceState,
        selection::primary_selection::PrimarySelectionState,
        shell::xdg::XdgShellState,
        shell::xdg::decoration::XdgDecorationState,
        shell::wlr_layer::WlrLayerShellState,
        shm::ShmState,
        viewporter::ViewporterState,
    },
};

use crate::config::LabwcConfig;
use crate::menu::{MenuOverlay, MenuRegistry};
use crate::theme::Theme;
use crate::view::Stack;

pub struct LabwcState {
    // ── Smithay protocol states ────────────────────────────────────────────
    pub display_handle: DisplayHandle,
    pub compositor:     CompositorState,
    pub xdg_shell:      XdgShellState,
    pub xdg_decoration: XdgDecorationState,
    pub layer_shell:    WlrLayerShellState,
    pub shm:            ShmState,
    pub output_mgr:     OutputManagerState,
    pub seat_state:     SeatState<Self>,
    pub data_device:    DataDeviceState,
    pub primary_sel:    PrimarySelectionState,
    pub viewporter:     ViewporterState,
    pub popups:         PopupManager,

    pub seat: Seat<Self>,
    pub space: Space<Window>,

    // ── Loop control ────────────────────────────────────────────────────────
    pub loop_handle:    LoopHandle<'static, Self>,
    pub loop_signal:    LoopSignal,

    // ── labwc-specific state ────────────────────────────────────────────────
    pub config:  LabwcConfig,
    pub theme:   Theme,
    pub menus:   MenuRegistry,
    pub stack:   Stack,
    pub menu_overlay: Option<MenuOverlay>,
    pub cursor_status: Arc<Mutex<CursorImageStatus>>,
    pub start_time: Instant,
    pub current_workspace: u32,
    pub should_exit: bool,

    // ── Output activo (DRM card0 o ventana winit) ────────────────────────
    pub output: Option<Output>,
}

impl LabwcState {
    pub fn new(
        display: &mut Display<Self>,
        loop_handle: LoopHandle<'static, Self>,
        loop_signal: LoopSignal,
    ) -> anyhow::Result<Self> {
        let dh = display.handle();

        // Crea todos los globales Wayland que labwc expone:
        let compositor     = CompositorState::new::<Self>(&dh);
        let xdg_shell      = XdgShellState::new::<Self>(&dh);
        let xdg_decoration = XdgDecorationState::new::<Self>(&dh);
        let layer_shell    = WlrLayerShellState::new::<Self>(&dh);
        let shm            = ShmState::new::<Self>(&dh, vec![]);
        let output_mgr     = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let data_device    = DataDeviceState::new::<Self>(&dh);
        let primary_sel    = PrimarySelectionState::new::<Self>(&dh);
        let viewporter     = ViewporterState::new::<Self>(&dh);

        let mut seat = seat_state.new_wl_seat(&dh, "seat0");
        let _ = seat.add_keyboard(Default::default(), 200, 25)?;
        let _ = seat.add_pointer();

        let space = Space::default();
        let popups = PopupManager::default();

        // Carga rc.xml + themerc + menu.xml (mismo formato que labwc upstream).
        let config = LabwcConfig::load();
        let theme  = Theme::load(&config.theme.name);
        let menus  = MenuRegistry::load();

        Ok(Self {
            display_handle: dh,
            compositor, xdg_shell, xdg_decoration, layer_shell, shm,
            output_mgr, seat_state, data_device, primary_sel, viewporter,
            popups, seat, space,
            loop_handle, loop_signal,
            config, theme, menus,
            stack: Stack::default(),
            menu_overlay: None,
            cursor_status: Arc::new(Mutex::new(CursorImageStatus::default_named())),
            start_time: Instant::now(),
            current_workspace: 1,
            should_exit: false,
            output: None,
        })
    }

    /// Lanza los `<autostart>` del rc.xml.
    pub fn run_autostart(&self) {
        for cmd in &self.config.autostart {
            spawn(cmd);
        }
    }
}

#[derive(Default)]
pub struct ClientState;
impl ClientData for ClientState {
    fn initialized(&self, _client_id: smithay::reexports::wayland_server::backend::ClientId) {}
    fn disconnected(
        &self,
        _client_id: smithay::reexports::wayland_server::backend::ClientId,
        _reason: smithay::reexports::wayland_server::backend::DisconnectReason,
    ) {}
}

/// Lanza un comando como subproceso. En Eclipse usa la syscall nativa;
/// en host Linux, std::process::Command.
pub fn spawn(cmd: &str) {
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("/bin/sh").arg("-c").arg(cmd).spawn();
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut buf = cmd.as_bytes().to_vec();
        buf.push(0);
        unsafe { let _ = eclipse_syscall::call::spawn_command(buf.as_ptr()); }
    }
}
