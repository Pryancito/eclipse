//! The ioctls of `/dev/nvidiactl` and `/dev/nvidia<N>`.
//!
//! They are not dispatched through the inode's `io_control`, which sees
//! neither the fd table nor the caller, and runs under the `File`'s lock:
//! REGISTER_FD names a fd, the RM asks whether the caller has a GPU's node
//! open before it lets a client reach that GPU (`osIsGpuAccessible`), and an
//! effective uid of 0 makes the caller `RS_PRIV_LEVEL_USER_ROOT`. Everything
//! else is `linux_object::fs::devfs::nvidia`, but for the one thing mmap
//! needs to know here: whether the mapping is read-only.

use super::*;
use linux_object::fs::devfs::nvidia::{gpu_mask, NvCaller};
use linux_object::fs::devfs::NvidiaOpen;

impl Syscall<'_> {
    /// `Some` when `file_like` is an open of an NVIDIA node, with the ioctl's
    /// result; `None` sends the ioctl down the ordinary path.
    pub(super) fn sys_nvidia_ioctl(
        &self,
        file_like: &Arc<dyn FileLike>,
        request: usize,
        arg: usize,
    ) -> Option<SysResult> {
        let inode = file_like.downcast_ref::<File>()?.inode();
        let open = inode.downcast_ref::<NvidiaOpen>()?;
        let proc = self.linux_process();
        let mask = || {
            gpu_mask(
                proc.get_files()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|(_, f)| Some(f.downcast_arc::<File>().ok()?.inode())),
            )
        };
        let open_of_fd = |fd: i32| Some(proc.get_file(fd.into()).ok()?.inode());
        let caller = NvCaller {
            admin: proc.euid() == 0,
            gpu_mask: &mask,
            open_of_fd: &open_of_fd,
        };
        Some(open.ioctl(request as u32, arg, &caller))
    }

    /// Whether `file_like` is an open of an NVIDIA node holding a mapping the
    /// RM granted read-only, which mmap installs read-only whatever it was
    /// asked for (see `NvidiaOpen::mmap_read_only`).
    pub(crate) fn nvidia_mmap_read_only(file_like: &Arc<dyn FileLike>) -> bool {
        let Some(file) = file_like.downcast_ref::<File>() else {
            return false;
        };
        let inode = file.inode();
        inode
            .downcast_ref::<NvidiaOpen>()
            .is_some_and(NvidiaOpen::mmap_read_only)
    }
}
