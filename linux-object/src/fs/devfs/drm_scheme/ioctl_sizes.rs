//! `drm_ioctl()`'s size arithmetic, and the command-number range it applies
//! to. Split out of `drm_scheme.rs` because it is self-contained: a command
//! number and the direction bits in, three byte counts out, and no DRM state
//! touched at all.

pub(super) const DRM_COMMAND_BASE: u32 = 0x40;
pub(super) const DRM_COMMAND_END: u32 = 0xA0;

pub(super) const IOC_WRITE_DIR: u32 = 1 << 30;
pub(super) const IOC_READ_DIR: u32 = 2 << 30;

pub(super) const fn ioc_size(cmd: u32) -> usize {
    ((cmd >> 16) & 0x3fff) as usize
}

/// The byte counts `drm_ioctl()` derives for one call: what to copy in from the
/// caller, what to copy back, and how big the kernel-side struct must be.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct IoctlSizes {
    pub(super) in_size: usize,
    pub(super) out_size: usize,
    pub(super) ksize: usize,
}

/// `drm_ioctl()`'s size arithmetic, verbatim:
///
/// ```text
/// in_size = out_size = _IOC_SIZE(cmd);
/// if ((cmd & ioctl->cmd & IOC_IN)  == 0) in_size  = 0;
/// if ((cmd & ioctl->cmd & IOC_OUT) == 0) out_size = 0;
/// ksize = max(max(in_size, out_size), drv_size);
/// ```
///
/// Direction is INTERSECTED with the handler's own, which is what stops a
/// caller from encoding `_IOC_READ` on a write-only ioctl to have the kernel
/// copy a struct back that it was never going to fill.
pub(super) fn reconcile_sizes(cmd: u32, canon: u32) -> IoctlSizes {
    let user_size = ioc_size(cmd);
    let in_size = if cmd & canon & IOC_WRITE_DIR != 0 {
        user_size
    } else {
        0
    };
    let out_size = if cmd & canon & IOC_READ_DIR != 0 {
        user_size
    } else {
        0
    };
    IoctlSizes {
        in_size,
        out_size,
        ksize: core::cmp::max(core::cmp::max(in_size, out_size), ioc_size(canon)),
    }
}
