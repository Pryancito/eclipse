//! The two syncobj-fd ioctls and the `sync_file` merge, with the UABI structs
//! they decode.
//!
//! `DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD` and `..._FD_TO_HANDLE` are one function in
//! Linux (`drm_syncobj_handle_fd_ioctl`) because the two directions share the
//! flag, the errno table and the `sync_file` special case; keeping them
//! together with `SYNC_IOC_MERGE` is how the frozen prefix of each struct stays
//! next to the only code that reads it.

use super::*;

/// `struct drm_syncobj_handle` (`drm.h`), the argument of both syncobj fd
/// ioctls: `__u32 handle; __u32 flags; __s32 fd; __u32 pad;`.
///
/// These 16 bytes are the frozen prefix. The UABI struct is append-only and
/// newer kernels grew a tail, which is why `Syscall::sys_drm_syncobj_fd`
/// matches on the ioctl nr and type and never on the size it encodes -- and
/// why what has to be written down is that the prefix is these four words in
/// this order. Here rather than inside that method for the reason
/// [`SyncMergeData`] gives.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct DrmSyncobjHandle {
    pub handle: u32,
    pub flags: u32,
    pub fd: i32,
    pub pad: u32,
}

/// `struct drm_syncobj_eventfd` (`drm.h`), the argument of
/// `DRM_IOCTL_SYNCOBJ_EVENTFD`: `__u32 handle; __u32 flags; __u64 point;
/// __s32 fd; __u32 pad;`.
///
/// Here rather than inside `Syscall::sys_drm_syncobj_eventfd` for the reason
/// [`SyncMergeData`] gives.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct DrmSyncobjEventfd {
    pub(super) handle: u32,
    pub(super) flags: u32,
    pub(super) point: u64,
    pub(super) fd: i32,
    pub(super) pad: u32,
}

/// Whether `cmd` is `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` (`Some(true)`) or
/// `DMA_BUF_IOCTL_IMPORT_SYNC_FILE` (`Some(false)`), the two dma-buf ioctls
/// that need the fd table. Exact numbers: both structs are two `u32`s and
/// frozen, so the encoded size cannot move as `drm_syncobj_handle`'s did.
pub(crate) fn dma_buf_sync_file_ioctl(cmd: u32) -> Option<bool> {
    use linux_object::fs::{DMA_BUF_IOCTL_EXPORT_SYNC_FILE, DMA_BUF_IOCTL_IMPORT_SYNC_FILE};
    match cmd {
        DMA_BUF_IOCTL_EXPORT_SYNC_FILE => Some(true),
        DMA_BUF_IOCTL_IMPORT_SYNC_FILE => Some(false),
        _ => None,
    }
}

/// The one flag `SYNCOBJ_HANDLE_TO_FD` and `SYNCOBJ_FD_TO_HANDLE` know:
/// `DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE` and
/// `DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE`, bit 0 on both.
pub(super) const SYNCOBJ_FD_SYNC_FILE: u32 = 1 << 0;

/// What both syncobj fd ioctls refuse before looking at the handle or the
/// fd, as `drm_syncobj_handle_to_fd_ioctl` / `drm_syncobj_fd_to_handle_ioctl`
/// do: a non-zero `pad`, and `flags` other than 0 or the one bit. Every
/// other flag is EINVAL -- which includes the `_TIMELINE` variants newer
/// kernels grew (bit 1, with a `point` past the 16-byte prefix): this tree
/// has no timeline sync_file, and a client that asked for one used to get
/// its request honoured as the plain, whole-syncobj kind, so the point it
/// wanted to export or import was silently not the one that travelled.
pub(crate) fn syncobj_fd_args_check(flags: u32, pad: u32) -> Result<(), LxError> {
    if pad != 0 {
        return Err(LxError::EINVAL);
    }
    if flags != 0 && flags != SYNCOBJ_FD_SYNC_FILE {
        return Err(LxError::EINVAL);
    }
    Ok(())
}

/// The errno for a `handle` that is not the caller's, which is how Linux's
/// three lookups answer: EINVAL from `drm_syncobj_handle_to_fd` (the
/// whole-syncobj export), ENOENT from `drm_syncobj_find_fence` (the
/// sync_file export, which used to say EINVAL here too) and from
/// `drm_syncobj_import_sync_file_fence` (the import's destination).
pub(crate) fn syncobj_fd_missing_handle_errno(handle_to_fd: bool, sync_file: bool) -> LxError {
    if handle_to_fd && !sync_file {
        LxError::EINVAL
    } else {
        LxError::ENOENT
    }
}

/// `sync_file_ioctl_merge` after the copy from userspace, on the caller's
/// fd table `proc` as process `pid`: `first` is the sync_file the ioctl was
/// issued on, `data.fd2` the other one, and `data.fence` comes back as the
/// new fd, close-on-exec, of a sync_file that is ready once both are.
///
/// Its errors, in its order: `flags` or `pad` set is EINVAL; then
/// `sync_file_fdget(fd2)`, which is empty for an fd that names no file as
/// much as for a file that is not a sync_file (a syncobj fd included), is
/// ENOENT. The first of those used to come back as the fd table's EBADF.
/// The name is cut at 31 bytes and NUL-terminated before it is used and
/// written back, as `data.name[sizeof(data.name) - 1] = '\0'` does.
pub(crate) fn sync_file_merge_op(
    proc: &LinuxProcess,
    pid: KoID,
    first: &linux_object::fs::SyncobjHandle,
    data: &mut SyncMergeData,
) -> LxResult {
    use linux_object::fs::SyncobjHandle;

    if data.flags != 0 || data.pad != 0 {
        return Err(LxError::EINVAL);
    }
    let second = proc
        .get_file_like(FileDesc::from(data.fd2 as usize))
        .map_err(|_| LxError::ENOENT)?;
    let Some(second) = second.downcast_ref::<SyncobjHandle>() else {
        return Err(LxError::ENOENT);
    };
    data.name[31] = 0;
    // ENOTTY on a syncobj fd (it has no ioctl), ENOENT when `fd2` is not
    // a sync_file; the name goes on the new fence for `FILE_INFO`.
    let merged = first.merge(second, &data.name)?;
    let fd = proc.add_file(merged)?;
    // Bounded, LOG=error-visible trace: the first merges of a boot are the
    // GLX client's second round of acquires, the step that used to fail.
    {
        static MERGE_TRACE_BUDGET: core::sync::atomic::AtomicU32 =
            core::sync::atomic::AtomicU32::new(0);
        let n = MERGE_TRACE_BUDGET.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if n < 8 {
            kernel_hal::klog_info!(
                "[drm] SYNC_IOC_MERGE #{} pid={} fd2={} -> fd={}",
                n + 1,
                pid,
                data.fd2,
                i32::from(fd),
            );
        }
    }
    data.fence = i32::from(fd);
    Ok(())
}

/// `SYNCOBJ_HANDLE_TO_FD` / `SYNCOBJ_FD_TO_HANDLE` after the argument check,
/// on the caller's fd table `proc` as process `pid`; `h.fd` (an export) or
/// `h.handle` (an import) comes back filled in.
///
/// The `_SYNC_FILE` variants (same bit on both ioctls) move a single FENCE
/// rather than the syncobj itself: export takes the fence that is current on
/// `handle` and wraps it in an fd; import gives that fence to a DIFFERENT,
/// already-existing syncobj. That is how Mesa hands completed work across a
/// process boundary (a GLX/DRI3 client and the X server exchanging frames),
/// so a GL client under Xwayland exercises this pair on paths a
/// Wayland-native client never touches. A fence here is "syncobj S reached
/// point N", so a sync_file fd is exactly that pair, and
/// `zcore_drivers::scheme::syncobj` resolves it on import (immediately when
/// already satisfied, as a recorded dependency otherwise).
///
/// The errors are the ones `drm_syncobj.c` gives, in its order:
/// - an export of a handle that is not the caller's is EINVAL for the
///   whole syncobj (`drm_syncobj_handle_to_fd`) and ENOENT for its fence
///   (`drm_syncobj_find_fence`);
/// - an import looks at the fd FIRST: one that names no file is EINVAL
///   (`fdget` / `sync_file_get_fence` come back empty; it used to be the
///   fd table's EBADF), and so is a file of the wrong kind, a syncobj fd
///   where a fence is wanted included (`sync_file_get_fence` knows only
///   sync_files; a syncobj fd used to be taken as "whatever its object
///   carries now"), or a sync_file where the object is wanted;
/// - only then the destination handle of an `IMPORT_SYNC_FILE`, ENOENT
///   when it is not the caller's (it used to be looked up first).
pub(crate) fn drm_syncobj_fd_op(
    proc: &LinuxProcess,
    pid: KoID,
    is_handle_to_fd: bool,
    h: &mut DrmSyncobjHandle,
) -> LxResult {
    use kernel_hal::drivers::scheme::syncobj;
    use linux_object::fs::SyncobjHandle;

    let sync_file = h.flags & SYNCOBJ_FD_SYNC_FILE != 0;
    // Bounded, LOG=error-visible trace of the export/import ops NVK issues
    // during physical-device / semaphore init. This is the LAST syncobj path
    // NOT covered by `drm_scheme.rs`'s `trace_syncobj`: HANDLE_TO_FD /
    // FD_TO_HANDLE are intercepted HERE (they need the process fd table),
    // before the inode `io_control` those traces live in. An export->import
    // round-trip is exactly how Mesa auto-detects a syncobj's EXTERNAL-handle
    // features, the one detection step we could never see. `opaque` =
    // OPAQUE_FD whole-syncobj share; `sync_file` = single-fence transfer.
    {
        static SYNCOBJ_FD_TRACE_BUDGET: core::sync::atomic::AtomicU32 =
            core::sync::atomic::AtomicU32::new(0);
        let n = SYNCOBJ_FD_TRACE_BUDGET.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if n < 32 {
            kernel_hal::klog_info!(
                "[drm] SYNCOBJ_{} #{} pid={} handle={} fd={} flags={:#x} ({})",
                if is_handle_to_fd {
                    "HANDLE_TO_FD"
                } else {
                    "FD_TO_HANDLE"
                },
                n + 1,
                pid,
                h.handle,
                h.fd,
                h.flags,
                if sync_file { "sync_file" } else { "opaque" },
            );
        }
    }
    if is_handle_to_fd {
        // `h.handle` is one of the CALLER'S syncobjs (`drm_syncobj_find`):
        // another process's is not there.
        if !syncobj::usable_by(pid, h.handle) {
            let errno = syncobj_fd_missing_handle_errno(true, sync_file);
            warn!(
                "[drm] SYNCOBJ_HANDLE_TO_FD{} {:?}: handle={} is not a live syncobj of pid {}",
                if sync_file { "(EXPORT_SYNC_FILE)" } else { "" },
                errno,
                h.handle,
                pid
            );
            return Err(errno);
        }
        if sync_file {
            // The file gets the FENCE the syncobj carries now, as a syncobj
            // of its own (`export_fence`), never a live reading of the
            // source: a RESET of the source, or its re-arm by the next
            // submit, must not take the fence away from a sync_file already
            // exported (Mesa resets a binary semaphore in the same call that
            // exports its SYNC_FD). The file owns the carrier's one reference
            // and `destroy`s it on close (Drop on SyncobjHandle).
            let Some(carrier) = syncobj::export_fence(h.handle) else {
                warn!(
                    "[drm] SYNCOBJ_HANDLE_TO_FD(EXPORT_SYNC_FILE) ENOENT: handle={} went away",
                    h.handle
                );
                return Err(LxError::ENOENT);
            };
            let new_fd = match proc.add_file(SyncobjHandle::new_sync_file(carrier, 1)) {
                Ok(fd) => fd,
                Err(e) => {
                    let _ = syncobj::destroy(carrier);
                    return Err(e);
                }
            };
            h.fd = i32::from(new_fd);
            return Ok(());
        }
        // Fd holds a syncobj ref so SYNCOBJ_DESTROY cannot free it while
        // the exported fd is still live (Drop on SyncobjHandle dec_refs).
        if !syncobj::add_ref(h.handle) {
            return Err(LxError::EINVAL);
        }
        let new_fd = match proc.add_file(SyncobjHandle::new(h.handle)) {
            Ok(fd) => fd,
            Err(e) => {
                warn!("[drm] SYNCOBJ_HANDLE_TO_FD add_file {:?}", e);
                let _ = syncobj::destroy(h.handle);
                return Err(e);
            }
        };
        h.fd = i32::from(new_fd);
        return Ok(());
    }
    // FD_TO_HANDLE: the fd first. `fdget` (opaque) and `sync_file_get_fence`
    // (import) answer an fd that names nothing with EINVAL, as they do a
    // file of another kind.
    let target = proc
        .get_file_like(FileDesc::from(h.fd as usize))
        .map_err(|_| {
            warn!(
                "[drm] SYNCOBJ_FD_TO_HANDLE EINVAL: fd={} is not in the fd table",
                h.fd
            );
            LxError::EINVAL
        })?;
    let file = target
        .downcast_ref::<SyncobjHandle>()
        .ok_or(LxError::EINVAL)?;
    if sync_file {
        // Only a sync_file carries a fence to import; a syncobj fd is the
        // object, and `sync_file_get_fence` has nothing to give for it.
        let Some(point) = file.sync_file_point else {
            warn!(
                "[drm] SYNCOBJ_FD_TO_HANDLE(IMPORT_SYNC_FILE) EINVAL: fd={} is a syncobj fd, not a sync_file",
                h.fd
            );
            return Err(LxError::EINVAL);
        };
        // The DESTINATION must already exist and be the caller's (Linux does
        // not create one for this variant): `drm_syncobj_find`, ENOENT.
        if !syncobj::usable_by(pid, h.handle) {
            let errno = syncobj_fd_missing_handle_errno(false, true);
            warn!(
                "[drm] SYNCOBJ_FD_TO_HANDLE(IMPORT_SYNC_FILE) {:?}: handle={} is not a live syncobj of pid {}",
                errno, h.handle, pid
            );
            return Err(errno);
        }
        if !syncobj::import_snapshot(h.handle, file.handle, point) {
            warn!(
                "[drm] SYNCOBJ_FD_TO_HANDLE(IMPORT_SYNC_FILE) EINVAL: dst handle={} or src handle={} not live",
                h.handle, file.handle
            );
            return Err(LxError::EINVAL);
        }
        return Ok(());
    }
    if file.sync_file_point.is_some() {
        // A sync_file fd imported WITHOUT the flag would alias the two
        // objects (the importer would signal the exporter's syncobj)
        // instead of copying one fence. Linux rejects the mismatch
        // (`f_op != &drm_syncobj_file_fops`); so do we, loudly, because
        // silently aliasing them produces fences that appear to signal
        // early, the worst failure to debug from a frame that merely
        // looks wrong.
        warn!(
            "[drm] SYNCOBJ_FD_TO_HANDLE EINVAL: fd={} is a sync_file, not a syncobj \
             (missing IMPORT_SYNC_FILE flag)",
            h.fd
        );
        return Err(LxError::EINVAL);
    }
    // The importer gets a reference of its own (Linux: a new handle in its
    // table), so its later DESTROY frees only that.
    let Some(handle) = file.import_opaque(pid) else {
        warn!(
            "[drm] SYNCOBJ_FD_TO_HANDLE EINVAL: fd={} names syncobj handle={} that no longer exists",
            h.fd, file.handle
        );
        return Err(LxError::EINVAL);
    };
    h.handle = handle;
    Ok(())
}
