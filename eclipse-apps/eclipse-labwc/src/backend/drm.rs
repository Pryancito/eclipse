//! DRM/KMS backend — esqueleto adaptado a Smithay 0.7.
//!
//! En Smithay 0.7 el manejo de DRM se hace mediante:
//!   - `DrmDevice::new(fd, _)` (sigue igual)
//!   - `DrmDevice::resource_handles()` reemplazado por iterar via la trait
//!     `drm::control::Device` (smithay re-exporta `drm`).
//!   - Conector → `device.get_connector(handle, _)` (de `drm::control::Device`).
//!
//! Este archivo deja preparada la inicialización, la enumeración de cards/inputs
//! sin libudev (`backend::session::enumerate_*`), y el bucle calloop. La
//! integración completa con `GbmAllocator` + `GlesRenderer` + page-flip se
//! deja como TODO siguiendo `anvil/src/udev.rs` upstream.

use std::os::fd::OwnedFd;

use smithay::{
    backend::drm::{DrmDevice, DrmDeviceFd},
    reexports::{
        calloop::EventLoop,
        wayland_server::Display,
    },
    utils::DeviceFd,
};

use crate::state::LabwcState;

pub fn run(mut state: LabwcState) -> anyhow::Result<()> {
    let mut event_loop: EventLoop<'static, LabwcState> = EventLoop::try_new()?;
    let display = Display::<LabwcState>::new()?;

    // 1) Enumerar `/dev/dri/card*`.
    let cards = super::session::enumerate_drm_devices();
    let card_path = cards.first().cloned()
        .unwrap_or_else(|| "/dev/dri/card0".into());

    // 2) Abrir card0 directamente (sin libudev/seatd).
    let fd = open_card(&card_path)?;
    let device_fd = DrmDeviceFd::new(DeviceFd::from(fd));
    let (_drm, _drm_notifier) = DrmDevice::new(device_fd, true)?;

    // 3) TODO: inicializar GbmAllocator + GlesRenderer + crear smithay Output
    //    a partir del primer connector conectado y mapearlo al Space.
    //    Ver: anvil/src/udev.rs::run_udev() upstream.

    // 4) Listener Wayland en `wayland-0`.
    let socket_name = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    let listener = smithay::wayland::socket::ListeningSocketSource::with_name(&socket_name)?;
    let _ = event_loop.handle().insert_source(listener, |client_stream, _, st: &mut LabwcState| {
        let _ = st.display_handle.insert_client(
            client_stream,
            std::sync::Arc::new(crate::state::ClientState::default()),
        );
    });

    // 5) libinput sobre `/dev/input/event*` (sin libudev).
    super::winit::insert_libinput(&event_loop.handle())?;

    // 6) XWayland (best-effort).
    let _ = crate::handlers::xwayland::start_xwayland(&mut state);

    // 7) Run.
    state.run_autostart();
    let _ = display;
    let _ = event_loop.run(None, &mut state, |st: &mut LabwcState| {
        if st.should_exit { st.loop_signal.stop(); }
    });

    Ok(())
}

fn open_card(path: &std::path::Path) -> anyhow::Result<OwnedFd> {
    use std::os::fd::FromRawFd;
    let mut cstr = path.as_os_str().as_encoded_bytes().to_vec();
    cstr.push(0);
    let fd = unsafe {
        libc::open(cstr.as_ptr() as *const _, libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC)
    };
    if fd < 0 { return Err(anyhow::anyhow!("open {:?} failed", path)); }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
