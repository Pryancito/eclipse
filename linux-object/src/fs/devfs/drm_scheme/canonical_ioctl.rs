//! The canonical `_IOC` word of every core DRM ioctl number, what
//! [`DrmDev::drm_ioctl_dispatch`] reconciles a caller's size against. Split
//! out of `drm_scheme.rs` for the size ratchet: a number in, a command word
//! out, and no DRM state touched.

use super::*;

/// The canonical encoding of a core DRM ioctl: the `_IOC` word whose
/// `_IOC_SIZE` is the struct layout [`DrmDev::drm_ioctl_dispatch`] parses.
///
/// Linux never dispatches on the encoded command. `drm_ioctl()` takes
/// `nr = _IOC_NR(cmd)`, looks the handler up in `drm_ioctls[]` by that number
/// alone, and then *reconciles* the caller's `_IOC_SIZE(cmd)` with the size of
/// the struct the kernel parses (`drm_ioctl_kernel`: allocate
/// `max(in_size, out_size, drv_size)`, copy the caller's bytes in, zero the
/// rest, run the handler, copy `out_size` bytes back). That is the entire
/// reason a libdrm built against a 2019 `drm.h` keeps working on a 2026 kernel,
/// and why a struct may grow a trailing field without a flag day.
///
/// This tree matched the full 32-bit command instead, so every struct that ever
/// grew a field became a *different* ioctl that fell through to the driver and
/// out as ENOSYS. It cost three separate hand-patches already — the deadline
/// sizes of `SYNCOBJ_WAIT`/`TIMELINE_WAIT`, `SYNCOBJ_HANDLE_TO_FD` matched by
/// NR in the syscall layer, `PRIME_*` likewise — each found only after a client
/// broke on real hardware. Returning `None` here means "not a core ioctl we
/// know": driver-private numbers (`DRM_COMMAND_BASE..DRM_COMMAND_END`) and
/// anything unrecognised pass through untouched.
pub(super) fn canonical_drm_ioctl(nr: u32) -> Option<u32> {
    Some(match nr {
        0x00 => DRM_IOCTL_VERSION,
        0x01 => DRM_IOCTL_GET_UNIQUE,
        0x02 => DRM_IOCTL_GET_MAGIC,
        0x07 => DRM_IOCTL_SET_VERSION,
        0x09 => DRM_IOCTL_GEM_CLOSE,
        0x0A => DRM_IOCTL_GET_CLIENT,
        0x0C => DRM_IOCTL_GET_CAP,
        0x0D => DRM_IOCTL_SET_CLIENT_CAP,
        0x11 => DRM_IOCTL_AUTH_MAGIC,
        0x1E => DRM_IOCTL_SET_MASTER,
        0x1F => DRM_IOCTL_DROP_MASTER,
        0x3A => DRM_IOCTL_WAIT_VBLANK,
        0xA0 => DRM_IOCTL_MODE_GETRESOURCES,
        0xA1 => DRM_IOCTL_MODE_GETCRTC,
        0xA2 => DRM_IOCTL_MODE_SETCRTC,
        0xA3 => DRM_IOCTL_MODE_CURSOR,
        0xA4 => DRM_IOCTL_MODE_GETGAMMA,
        0xA5 => DRM_IOCTL_MODE_SETGAMMA,
        0xA6 => DRM_IOCTL_MODE_GETENCODER,
        0xA7 => DRM_IOCTL_MODE_GETCONNECTOR,
        0xAA => DRM_IOCTL_MODE_GETPROPERTY,
        0xAB => DRM_IOCTL_MODE_SETPROPERTY,
        0xAC => DRM_IOCTL_MODE_GETPROPBLOB,
        0xAD => DRM_IOCTL_MODE_GETFB,
        0xAE => DRM_IOCTL_MODE_ADDFB,
        0xAF => DRM_IOCTL_MODE_RMFB,
        0xB0 => DRM_IOCTL_MODE_PAGE_FLIP,
        0xB1 => DRM_IOCTL_MODE_DIRTYFB,
        0xB2 => DRM_IOCTL_MODE_CREATE_DUMB,
        0xB3 => DRM_IOCTL_MODE_MAP_DUMB,
        0xB4 => DRM_IOCTL_MODE_DESTROY_DUMB,
        0xB5 => DRM_IOCTL_MODE_GETPLANERESOURCES,
        0xB6 => DRM_IOCTL_MODE_GETPLANE,
        0xB7 => DRM_IOCTL_MODE_SETPLANE,
        0xB8 => DRM_IOCTL_MODE_ADDFB2,
        0xB9 => DRM_IOCTL_MODE_OBJ_GETPROPERTIES,
        0xBA => DRM_IOCTL_MODE_OBJ_SETPROPERTY,
        0xBB => DRM_IOCTL_MODE_CURSOR2,
        0xBC => DRM_IOCTL_MODE_ATOMIC,
        0xBD => DRM_IOCTL_MODE_CREATEPROPBLOB,
        0xBE => DRM_IOCTL_MODE_DESTROYPROPBLOB,
        0xBF => DRM_IOCTL_SYNCOBJ_CREATE,
        0xC0 => DRM_IOCTL_SYNCOBJ_DESTROY,
        0xC3 => DRM_IOCTL_SYNCOBJ_WAIT,
        0xC4 => DRM_IOCTL_SYNCOBJ_RESET,
        0xC5 => DRM_IOCTL_SYNCOBJ_SIGNAL,
        0xC7 => DRM_IOCTL_MODE_LIST_LESSEES,
        0xCA => DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT,
        0xCB => DRM_IOCTL_SYNCOBJ_QUERY,
        0xCC => DRM_IOCTL_SYNCOBJ_TRANSFER,
        0xCD => DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL,
        0xCE => DRM_IOCTL_MODE_GETFB2,
        0xD0 => DRM_IOCTL_MODE_CLOSEFB,
        _ => return None,
    })
}
