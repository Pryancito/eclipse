//! Sesión sin seatd para Eclipse OS.
//!
//! En Linux estándar, smithay usa `libseat` para abrir `/dev/dri/card*` y
//! `/dev/input/event*` con privilegios. En Eclipse OS no existe seatd;
//! abrimos los dispositivos directamente con `eclipse-relibc::open`, que
//! emite syscalls del kernel Eclipse. El kernel concede acceso completo al
//! proceso registrado como "display server" mediante
//! `eclipse_syscall::call::become_display_server()`.
//!
//! Implementamos el trait `smithay::backend::session::Session` para que el
//! resto del stack (DRM device manager, libinput backend) lo use de forma
//! transparente.

use std::os::fd::OwnedFd;
use std::path::Path;

use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::reexports::calloop::EventSource;
use smithay::reexports::rustix::fs::OFlags;

#[derive(Clone, Debug)]
pub struct EclipseSession {
    /// Nombre del seat (siempre `seat0` en Eclipse OS — single-seat).
    seat: String,
    active: bool,
}

impl EclipseSession {
    pub fn new() -> anyhow::Result<Self> {
        // En Eclipse OS, registramos el proceso como display server.
        // En target Linux dev, este syscall no existe → no-op.
        #[cfg(not(target_os = "linux"))]
        unsafe { let _ = eclipse_syscall::call::become_display_server(); }

        Ok(Self { seat: "seat0".into(), active: true })
    }
}

/// Error type que satisface `smithay::backend::session::AsErrno`.
#[derive(Debug)]
pub struct SessionError(pub i32);
impl core::fmt::Display for SessionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "EclipseSession errno={}", self.0)
    }
}
impl std::error::Error for SessionError {}
impl smithay::backend::session::AsErrno for SessionError {
    fn as_errno(&self) -> Option<i32> { Some(self.0) }
}

impl Session for EclipseSession {
    type Error = SessionError;

    fn open(&mut self, path: &Path, _flags: OFlags) -> Result<OwnedFd, Self::Error> {
        use std::os::fd::FromRawFd;
        let mut cstr = path.as_os_str().as_encoded_bytes().to_vec();
        cstr.push(0);
        let fd = unsafe {
            libc::open(
                cstr.as_ptr() as *const _,
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(SessionError(unsafe { *libc::__errno_location() }));
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn close(&mut self, _fd: OwnedFd) -> Result<(), Self::Error> { Ok(()) }

    fn change_vt(&mut self, _vt: i32) -> Result<(), Self::Error> {
        // Eclipse OS no usa VTs (no hay multi-tty), no-op.
        Ok(())
    }

    fn is_active(&self) -> bool { self.active }
    fn seat(&self) -> String { self.seat.clone() }
}

/// Notifier de cambios de sesión — en Eclipse no hay (single-seat permanente),
/// pero hay que devolver algo `EventSource`-compatible para el calloop loop.
pub struct EclipseSessionNotifier;

impl EventSource for EclipseSessionNotifier {
    type Event = SessionEvent;
    type Metadata = ();
    type Ret = ();
    type Error = std::io::Error;

    fn process_events<F>(
        &mut self,
        _readiness: smithay::reexports::calloop::Readiness,
        _token: smithay::reexports::calloop::Token,
        _callback: F,
    ) -> Result<smithay::reexports::calloop::PostAction, Self::Error>
    where F: FnMut(SessionEvent, &mut ()) {
        Ok(smithay::reexports::calloop::PostAction::Continue)
    }

    fn register(
        &mut self,
        _poll: &mut smithay::reexports::calloop::Poll,
        _factory: &mut smithay::reexports::calloop::TokenFactory,
    ) -> smithay::reexports::calloop::Result<()> { Ok(()) }

    fn reregister(
        &mut self,
        _poll: &mut smithay::reexports::calloop::Poll,
        _factory: &mut smithay::reexports::calloop::TokenFactory,
    ) -> smithay::reexports::calloop::Result<()> { Ok(()) }

    fn unregister(&mut self, _poll: &mut smithay::reexports::calloop::Poll)
        -> smithay::reexports::calloop::Result<()> { Ok(()) }
}

/// Helper: enumera dispositivos DRM en `/dev/dri/` sin libudev.
pub fn enumerate_drm_devices() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/dev/dri") {
        for entry in rd.flatten() {
            let p = entry.path();
            if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                if name.starts_with("card") { out.push(p); }
            }
        }
    }
    out.sort();
    out
}

/// Helper: enumera dispositivos input en `/dev/input/event*` sin libudev.
pub fn enumerate_input_devices() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/dev/input") {
        for entry in rd.flatten() {
            let p = entry.path();
            if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                if name.starts_with("event") { out.push(p); }
            }
        }
    }
    out.sort();
    out
}
