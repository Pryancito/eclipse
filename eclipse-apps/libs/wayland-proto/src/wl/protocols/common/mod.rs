pub mod wl_display;
pub mod wl_registry;
pub mod wl_compositor;
pub mod wl_surface;
pub mod wl_shm;
pub mod wl_buffer;

pub enum EventSet {
    WlDisplay(wl_display::WlDisplay, wl_display::Event),
    WlRegistry(wl_registry::WlRegistry, wl_registry::Event),
    WlCompositor(wl_compositor::WlCompositor, wl_compositor::Event),
    WlSurface(wl_surface::WlSurface, wl_surface::Event),
    WlShm(wl_shm::WlShm, wl_shm::Event),
    WlShmPool(wl_shm::WlShmPool, wl_shm::PoolEvent),
    WlBuffer(wl_buffer::WlBuffer, wl_buffer::Event),
}

pub enum RequestSet {
    WlDisplay(wl_display::Request),
    WlRegistry(wl_registry::Request),
    WlCompositor(wl_compositor::Request),
    WlSurface(wl_surface::Request),
    WlShm(wl_shm::Request),
    WlShmPool(wl_shm::PoolRequest),
    WlBuffer(wl_buffer::Request),
}
