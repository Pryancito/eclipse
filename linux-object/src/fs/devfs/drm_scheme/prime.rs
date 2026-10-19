//! What a PRIME ioctl asks for, decided by the ioctl number the way
//! `drm_ioctl` decides it. Out of `drm_scheme.rs` for its size ceiling.

use super::*;

/// What a PRIME ioctl asks for, decided the way `drm_ioctl` decides it: by
/// the ioctl NUMBER. `PRIME_HANDLE_TO_FD` is an export of `handle`, whatever
/// the caller left in `fd`; `PRIME_FD_TO_HANDLE` is an import of `fd`,
/// whatever it left in `handle`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrimeRequest {
    /// `PRIME_HANDLE_TO_FD`: wrap GEM `handle` in a new dma-buf fd.
    Export { handle: u32, flags: u32 },
    /// `PRIME_FD_TO_HANDLE`: the dma-buf behind `fd`, as a GEM handle.
    Import { fd: i32 },
}

/// Reads a PRIME request the way Linux does. `cmd` picks the operation (the
/// caller has already matched it against [`nr::PRIME_HANDLE_TO_FD`] and
/// [`nr::PRIME_FD_TO_HANDLE`], so anything else is `ENOTTY`), then each
/// operation checks only the fields it reads: an export refuses a flag
/// other than `DRM_CLOEXEC | DRM_RDWR` with `EINVAL`
/// (`drm_prime_handle_to_fd_ioctl`), an import of a negative fd is `EBADF`
/// (`dma_buf_get`).
///
/// The operation used to be read off the STRUCT instead: `fd < 0` meant an
/// export, because libdrm's `drmPrimeHandleToFD` presets the output field to
/// -1. That was a workaround for the two NRs being filed the wrong way
/// round in [`nr`], and it broke every exporter that does not preset the
/// field -- a `drm_prime_handle` zeroed by the caller (`fd = 0`) was taken
/// for an import of fd 0, so the export answered `EINVAL` (stdin is not a
/// dma-buf), or imported whatever dma-buf the client happened to hold at
/// fd 0. Linux never looks at `fd` on an export: it is an output.
pub fn prime_request(
    cmd: u32,
    args: DrmPrimeHandle,
) -> core::result::Result<PrimeRequest, LxError> {
    let (export, export_min) = nr::PRIME_HANDLE_TO_FD;
    let (import, import_min) = nr::PRIME_FD_TO_HANDLE;
    if is_drm_ioctl_nr(cmd, export, export_min) {
        if args.flags & !(DRM_CLOEXEC | DRM_RDWR) != 0 {
            return Err(LxError::EINVAL);
        }
        Ok(PrimeRequest::Export {
            handle: args.handle,
            flags: args.flags,
        })
    } else if is_drm_ioctl_nr(cmd, import, import_min) {
        if args.fd < 0 {
            return Err(LxError::EBADF);
        }
        Ok(PrimeRequest::Import { fd: args.fd })
    } else {
        Err(LxError::ENOTTY)
    }
}
