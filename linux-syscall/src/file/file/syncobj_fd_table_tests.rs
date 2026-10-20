//! The two syncobj fd ioctls on a real fd table: what an fd that names
//! nothing, a file of the wrong kind, and a handle that is not the
//! caller's get, and in which order, against `drm_syncobj.c`.

use super::{drm_syncobj_fd_op, DrmSyncobjHandle, LxError};
use alloc::sync::Arc;
use kernel_hal::drivers::scheme::syncobj;
use linux_object::fs::{FileDesc, SyncobjHandle};
use linux_object::process::{LinuxProcess, ProcessExt};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// Bit 0 on both ioctls: `EXPORT_SYNC_FILE` / `IMPORT_SYNC_FILE`.
const SYNC_FILE: u32 = 1;

fn a_process(pid: u64) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "p", LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

fn args(handle: u32, flags: u32, fd: i32) -> DrmSyncobjHandle {
    DrmSyncobjHandle {
        handle,
        flags,
        fd,
        pad: 0,
    }
}

fn is_sync_file(proc: &LinuxProcess, fd: i32) -> bool {
    proc.get_file_like(FileDesc::from(fd as usize))
        .expect("the fd the export installed")
        .downcast_ref::<SyncobjHandle>()
        .expect("a syncobj file")
        .sync_file_point
        .is_some()
}

fn gone(handle: u32) {
    while syncobj::exists(handle) {
        syncobj::destroy(handle);
    }
}

/// `fdget` / `sync_file_get_fence` on an fd that names no file: EINVAL,
/// for the whole-syncobj import and the fence import alike, and before
/// the destination handle is looked at (an unknown one is ENOENT, but
/// only once the fd is good).
#[test]
fn an_fd_that_names_no_file_is_einval_on_both_imports_before_the_handle() {
    let pid = 0x4a10_0001;
    let proc = a_process(pid);
    let linux = proc.linux();
    let dst = syncobj::create(false);
    let mut h = args(dst, 0, 999);
    assert_eq!(
        drm_syncobj_fd_op(linux, pid, false, &mut h),
        Err(LxError::EINVAL)
    );
    let mut h = args(dst, SYNC_FILE, 999);
    assert_eq!(
        drm_syncobj_fd_op(linux, pid, false, &mut h),
        Err(LxError::EINVAL)
    );
    let mut h = args(0xdead_0001, SYNC_FILE, 999);
    assert_eq!(
        drm_syncobj_fd_op(linux, pid, false, &mut h),
        Err(LxError::EINVAL),
        "the fd decides before the handle"
    );
    assert_eq!(syncobj::query(dst), Some(0), "nothing landed");
    gone(dst);
}

/// The fence import wants a sync_file: a syncobj fd is EINVAL and puts
/// nothing on the destination; a sync_file fd lands the source's fence
/// there, an unknown destination is ENOENT (after the fd), and the
/// whole-syncobj import hands the handle back for a syncobj fd and
/// refuses a sync_file.
#[test]
fn a_syncobj_fd_is_not_a_fence_to_import_and_a_sync_file_is() {
    let pid = 0x4a10_0002;
    let proc = a_process(pid);
    let linux = proc.linux();
    let src = syncobj::create(false);
    let dst = syncobj::create(false);

    let mut h = args(src, 0, -1);
    assert_eq!(drm_syncobj_fd_op(linux, pid, true, &mut h), Ok(()));
    let opaque_fd = h.fd;
    assert!(opaque_fd >= 0 && !is_sync_file(linux, opaque_fd));

    let mut h = args(dst, SYNC_FILE, opaque_fd);
    assert_eq!(
        drm_syncobj_fd_op(linux, pid, false, &mut h),
        Err(LxError::EINVAL),
        "a syncobj fd carries no fence"
    );
    assert!(syncobj::signal(src));
    assert_eq!(
        syncobj::query(dst),
        Some(0),
        "and the destination did not take the source's fence"
    );
    assert!(syncobj::reset(src));

    let mut h = args(src, SYNC_FILE, -1);
    assert_eq!(drm_syncobj_fd_op(linux, pid, true, &mut h), Ok(()));
    let fence_fd = h.fd;
    assert!(fence_fd >= 0 && is_sync_file(linux, fence_fd));

    let mut h = args(0xdead_0002, SYNC_FILE, fence_fd);
    assert_eq!(
        drm_syncobj_fd_op(linux, pid, false, &mut h),
        Err(LxError::ENOENT),
        "an unknown destination, once the fd is a sync_file"
    );
    let mut h = args(dst, SYNC_FILE, fence_fd);
    assert_eq!(drm_syncobj_fd_op(linux, pid, false, &mut h), Ok(()));
    assert_eq!(syncobj::query(dst), Some(0), "the fence is still in flight");
    assert!(syncobj::signal(src));
    assert_eq!(
        syncobj::query(dst),
        Some(1),
        "the imported fence is the source's"
    );

    let mut h = args(0, 0, opaque_fd);
    assert_eq!(drm_syncobj_fd_op(linux, pid, false, &mut h), Ok(()));
    assert_eq!(h.handle, src, "the whole-syncobj import names the object");
    let mut h = args(0, 0, fence_fd);
    assert_eq!(
        drm_syncobj_fd_op(linux, pid, false, &mut h),
        Err(LxError::EINVAL),
        "a sync_file is not a syncobj to import"
    );
    // A negative fd is no fd (`fdget` finds nothing), not the positive
    // one with its sign dropped.
    let mut h = args(dst, SYNC_FILE, -fence_fd);
    assert_eq!(
        drm_syncobj_fd_op(linux, pid, false, &mut h),
        Err(LxError::EINVAL)
    );
    let mut h = args(0, 0, -opaque_fd);
    assert_eq!(
        drm_syncobj_fd_op(linux, pid, false, &mut h),
        Err(LxError::EINVAL)
    );
    assert_eq!(h.handle, 0, "and no handle came back");
    drop(proc);
    gone(src);
    gone(dst);
}

/// A handle that is another process's: EINVAL on the whole-syncobj
/// export (`drm_syncobj_handle_to_fd`), ENOENT on the fence export
/// (`drm_syncobj_find_fence`) and as an import's destination; no fd is
/// installed for a refused export.
#[test]
fn another_processs_handle_is_einval_opaque_and_enoent_as_a_fence() {
    let pid = 0x4a10_0003;
    let proc = a_process(pid);
    let linux = proc.linux();
    let theirs = syncobj::create_for(0x4a10_0004, true);
    let mine = syncobj::create(true);

    let mut h = args(theirs, 0, -1);
    assert_eq!(
        drm_syncobj_fd_op(linux, pid, true, &mut h),
        Err(LxError::EINVAL)
    );
    assert_eq!(h.fd, -1, "no fd installed");
    let mut h = args(theirs, SYNC_FILE, -1);
    assert_eq!(
        drm_syncobj_fd_op(linux, pid, true, &mut h),
        Err(LxError::ENOENT)
    );
    assert_eq!(h.fd, -1);

    let mut h = args(mine, SYNC_FILE, -1);
    assert_eq!(drm_syncobj_fd_op(linux, pid, true, &mut h), Ok(()));
    let fence_fd = h.fd;
    assert_eq!(
        fence_fd, 3,
        "the first fd after the standard streams: the refusals installed none"
    );
    let mut h = args(theirs, SYNC_FILE, fence_fd);
    assert_eq!(
        drm_syncobj_fd_op(linux, pid, false, &mut h),
        Err(LxError::ENOENT),
        "another process's syncobj is not a destination"
    );
    drop(proc);
    gone(theirs);
    gone(mine);
}
