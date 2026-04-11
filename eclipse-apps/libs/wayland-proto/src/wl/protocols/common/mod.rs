pub mod wl_display;
pub mod wl_registry;
pub mod wl_compositor;
pub mod wl_region;
pub mod wl_surface;
pub mod wl_shm;
pub mod wl_buffer;
pub mod wl_seat;
pub mod wl_keyboard;
pub mod wl_pointer;
pub mod wl_output;
pub mod wl_callback;
pub mod wl_shell;
pub mod xdg_wm_base;
pub mod xdg_surface;
pub mod xdg_toplevel;
pub mod xdg_popup;
pub mod xdg_decoration;
pub mod xdg_output;
pub mod xwayland_shell;
pub mod zwlr_layer_shell;


pub enum EventSet {
    WlDisplay(wl_display::WlDisplay, wl_display::Event),
    WlRegistry(wl_registry::WlRegistry, wl_registry::Event),
    WlCompositor(wl_compositor::WlCompositor, wl_compositor::Event),
    WlSurface(wl_surface::WlSurface, wl_surface::Event),
    WlShm(wl_shm::WlShm, wl_shm::Event),
    WlShmPool(wl_shm::WlShmPool, wl_shm::PoolEvent),
    WlBuffer(wl_buffer::WlBuffer, wl_buffer::Event),
    WlSeat(wl_seat::WlSeat, wl_seat::Event),
    WlKeyboard(wl_keyboard::WlKeyboard, wl_keyboard::Event),
    WlPointer(wl_pointer::WlPointer, wl_pointer::Event),
    WlOutput(wl_output::WlOutput, wl_output::Event),
    WlCallback(wl_callback::WlCallback, wl_callback::Event),
    WlShell(wl_shell::WlShell, wl_shell::ShellEvent),
    WlShellSurface(wl_shell::WlShellSurface, wl_shell::SurfaceEvent),
    XdgWmBase(xdg_wm_base::XdgWmBase, xdg_wm_base::Event),
    XdgSurface(xdg_surface::XdgSurface, xdg_surface::Event),
    XdgToplevel(xdg_toplevel::XdgToplevel, xdg_toplevel::Event),
    XdgPositioner(xdg_popup::XdgPositioner, xdg_popup::PositionerEvent),
    XdgPopup(xdg_popup::XdgPopup, xdg_popup::PopupEvent),
    XdgDecorationManager(xdg_decoration::ZxdgDecorationManagerV1, xdg_decoration::ManagerEvent),
    XdgToplevelDecoration(xdg_decoration::ZxdgToplevelDecorationV1, xdg_decoration::DecorationEvent),
    XdgOutputManager(xdg_output::ZxdgOutputManagerV1, xdg_output::ManagerEvent),
    XdgOutput(xdg_output::ZxdgOutputV1, xdg_output::OutputEvent),
    XwaylandShell(xwayland_shell::XwaylandShellV1, xwayland_shell::Event),
    ZwlrLayerShell(zwlr_layer_shell::ZwlrLayerShellV1, zwlr_layer_shell::ShellEvent),
    ZwlrLayerSurface(zwlr_layer_shell::ZwlrLayerSurfaceV1, zwlr_layer_shell::SurfaceEvent),
}

pub enum RequestSet {
    WlDisplay(wl_display::Request),
    WlRegistry(wl_registry::Request),
    WlCompositor(wl_compositor::Request),
    WlSurface(wl_surface::Request),
    WlShm(wl_shm::Request),
    WlShmPool(wl_shm::PoolRequest),
    WlBuffer(wl_buffer::Request),
    WlSeat(wl_seat::Request),
    WlKeyboard(wl_keyboard::Request),
    WlPointer(wl_pointer::Request),
    WlOutput(wl_output::Request),
    WlCallback(wl_callback::Request),
    WlShell(wl_shell::ShellRequest),
    WlShellSurface(wl_shell::SurfaceRequest),
    XdgWmBase(xdg_wm_base::Request),
    XdgSurface(xdg_surface::Request),
    XdgToplevel(xdg_toplevel::Request),
    XdgPositioner(xdg_popup::PositionerRequest),
    XdgPopup(xdg_popup::PopupRequest),
    XdgDecorationManager(xdg_decoration::ManagerRequest),
    XdgToplevelDecoration(xdg_decoration::DecorationRequest),
    XdgOutputManager(xdg_output::ManagerRequest),
    XdgOutput(xdg_output::OutputRequest),
    XwaylandShell(xwayland_shell::Request),
    ZwlrLayerShell(zwlr_layer_shell::ShellRequest),
    ZwlrLayerSurface(zwlr_layer_shell::SurfaceRequest),
}
