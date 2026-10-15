//! `SYNC_IOC_MERGE` on a real fd table: what `fd2` may be and what it
//! gets when it is not that, against `sync_file_ioctl_merge`.

use super::{sync_file_merge_op, LxError, SyncMergeData};
use alloc::sync::Arc;
use kernel_hal::drivers::scheme::syncobj;
use linux_object::fs::{FileDesc, SyncobjHandle};
use linux_object::process::{LinuxProcess, ProcessExt};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

fn a_process(pid: u64) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "p", LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

fn data(fd2: i32) -> SyncMergeData {
    SyncMergeData {
        name: [b'n'; 32],
        fd2,
        fence: -1,
        flags: 0,
        pad: 0,
    }
}

fn gone(handle: u32) {
    while syncobj::exists(handle) {
        syncobj::destroy(handle);
    }
}

/// `sync_file_fdget` is empty for an fd that names no file (it used to
/// be EBADF), for a file of another kind and for a syncobj fd: ENOENT,
/// after the flags and pad (EINVAL) and before anything is installed.
#[test]
fn fd2_must_name_a_sync_file_or_it_is_enoent() {
    let pid = 0x4a20_0001;
    let proc = a_process(pid);
    let linux = proc.linux();
    let a = syncobj::create(true);
    let b = syncobj::create(true);
    assert!(syncobj::add_ref(a));
    let first = SyncobjHandle::new_sync_file(a, 1);
    let opaque_fd = i32::from(linux.add_file(SyncobjHandle::new(b)).unwrap());

    let mut d = data(999);
    assert_eq!(
        sync_file_merge_op(linux, pid, &first, &mut d),
        Err(LxError::ENOENT),
        "no such fd"
    );
    let mut d = data(0);
    assert_eq!(
        sync_file_merge_op(linux, pid, &first, &mut d),
        Err(LxError::ENOENT),
        "stdin is not a sync_file"
    );
    let mut d = data(opaque_fd);
    assert_eq!(
        sync_file_merge_op(linux, pid, &first, &mut d),
        Err(LxError::ENOENT),
        "a syncobj fd is not a sync_file"
    );
    let mut d = SyncMergeData {
        flags: 1,
        ..data(999)
    };
    assert_eq!(
        sync_file_merge_op(linux, pid, &first, &mut d),
        Err(LxError::EINVAL),
        "the flags come before the fd"
    );
    let mut d = SyncMergeData {
        pad: 1,
        ..data(999)
    };
    assert_eq!(
        sync_file_merge_op(linux, pid, &first, &mut d),
        Err(LxError::EINVAL)
    );
    assert_eq!(d.fence, -1, "nothing came back");
    assert!(
        linux.get_file_like(FileDesc::from(4)).is_err(),
        "nothing was installed after the one fd this test added"
    );
    drop(first);
    drop(proc);
    gone(a);
    gone(b);
}

/// Two sync_files merge into a third, close-on-exec, on the next free
/// fd, ready once both are; the name goes with it, cut to 31 bytes and
/// NUL-terminated in what is written back; and a syncobj fd as the
/// first operand has no ioctl (ENOTTY).
#[test]
fn two_sync_files_merge_into_a_named_third_on_the_next_fd() {
    let pid = 0x4a20_0002;
    let proc = a_process(pid);
    let linux = proc.linux();
    let a = syncobj::create(false);
    let b = syncobj::create(true);
    let c = syncobj::create(true);
    assert!(syncobj::add_ref(a));
    assert!(syncobj::add_ref(b));
    assert!(syncobj::add_ref(c));
    let first = SyncobjHandle::new_sync_file(a, 1);
    let second_fd = i32::from(linux.add_file(SyncobjHandle::new_sync_file(b, 1)).unwrap());
    assert_eq!(second_fd, 3);

    let mut d = data(second_fd);
    assert_eq!(sync_file_merge_op(linux, pid, &first, &mut d), Ok(()));
    assert_eq!(d.fence, 4, "the next free fd");
    assert_eq!(&d.name[..31], &[b'n'; 31], "the name, cut to 31 bytes");
    assert_eq!(d.name[31], 0, "and NUL-terminated as written back");
    let file = linux.get_file_like(FileDesc::from(4)).unwrap();
    assert!(file.flags().close_on_exec());
    let merged = file.downcast_ref::<SyncobjHandle>().unwrap();
    assert!(merged.sync_file_point.is_some(), "a sync_file");
    assert_eq!(
        syncobj::query(merged.handle),
        Some(0),
        "waiting for the first"
    );
    assert!(syncobj::signal(a));
    assert_eq!(syncobj::query(merged.handle), Some(1));
    // The name travelled with the fence: `SYNC_IOC_FILE_INFO` reports
    // it (`struct sync_file_info`, summary only).
    #[repr(C)]
    struct SyncFileInfo {
        name: [u8; 32],
        status: i32,
        flags: u32,
        num_fences: u32,
        pad: u32,
        sync_fence_info: u64,
    }
    let mut info = SyncFileInfo {
        name: [0; 32],
        status: 0,
        flags: 0,
        num_fences: 0,
        pad: 0,
        sync_fence_info: 0,
    };
    assert_eq!(
        file.ioctl(0xc038_3e04, &mut info as *mut SyncFileInfo as usize, 0, 0),
        Ok(0)
    );
    assert_eq!(&info.name[..31], &[b'n'; 31]);
    assert_eq!(info.name[31], 0);
    assert_eq!(info.status, 1);

    let opaque = SyncobjHandle::new(c);
    let mut d = data(second_fd);
    assert_eq!(
        sync_file_merge_op(linux, pid, &opaque, &mut d),
        Err(LxError::ENOTTY),
        "a syncobj fd has no ioctl"
    );
    drop(opaque);
    drop(first);
    drop(proc);
    gone(a);
    gone(b);
    gone(c);
}
