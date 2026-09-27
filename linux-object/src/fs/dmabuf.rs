//! dma-buf: a shareable handle to a buffer's physical memory.
//!
//! Used by DRM PRIME (`PRIME_HANDLE_TO_FD` / `PRIME_FD_TO_HANDLE`) to pass GPU
//! buffers between DRM nodes — e.g. a buffer rendered on `/dev/dri/renderD128`
//! (Mesa llvmpipe) exported as a dma-buf fd and imported into `/dev/dri/card0`
//! for scanout. The dma-buf just carries the backing frames (a contiguous
//! `VmObject`) plus its physical address and size; the pixel layout
//! (width/height/format/pitch) travels separately via `ADDFB2`.
//!
//! For nouveau-uAPI GEM objects the dma-buf also holds a `gem_mmap` reference
//! ([`super::devfs::drm::DMABUF_HOLDER`]) for its whole lifetime. Without that,
//! the exporter's `GEM_CLOSE` after `PRIME_HANDLE_TO_FD` (the normal DRI3
//! dance under Xwayland) frees the object while the fd is still in flight to
//! the importer — see the module docs on `DMABUF_HOLDER`.

use super::*;
use alloc::sync::Arc;
use lock::Mutex;
use zircon_object::object::*;

/// A dma-buf file object.
pub struct DmaBuf {
    base: KObjectBase,
    /// Physical base address of the backing buffer.
    pub phys_addr: u64,
    /// Buffer size in bytes.
    pub size: usize,
    /// Backing frames — kept alive while the dma-buf (or any GEM handle
    /// imported from it) is referenced.
    vmo: Arc<VmObject>,
    /// Nouveau-uAPI GEM handle this dma-buf was exported from, if any. The
    /// corresponding `DMABUF_HOLDER` reference is taken in [`Self::from_prime`]
    /// / [`Self::dup`] and released in [`Drop`]. `None` for dumb/generic
    /// exports, which stay alive via `vmo` alone.
    nouveau_handle: Option<u32>,
    /// What the fd is open as: the access mode and `O_CLOEXEC` the exporter
    /// asked for in `drm_prime_handle.flags` (see [`prime_open_flags`]),
    /// plus whatever `fcntl(F_SETFL)` set since.
    flags: Mutex<OpenFlags>,
}

impl_kobject!(DmaBuf);

/// What a dma-buf fd exported with `flags` (`drm_prime_handle.flags`, the
/// `DRM_CLOEXEC | DRM_RDWR` word) is open as.
///
/// Linux hands the word to `dma_buf_export` and `dma_buf_fd` as if it were an
/// `open(2)` flags word: `dma_buf_getfile` keeps `flags & (O_ACCMODE |
/// O_NONBLOCK)` as the file's mode, so without `DRM_RDWR` (`O_RDWR`) the fd
/// is `O_RDONLY` and a `MAP_SHARED | PROT_WRITE` mapping of it is `EACCES`;
/// and `get_unused_fd_flags(flags)` sets `FD_CLOEXEC` only for
/// `DRM_CLOEXEC` (`O_CLOEXEC`). Here every export used to come back
/// `O_RDWR | O_CLOEXEC` whatever the caller wrote: an exporter that left
/// `DRM_CLOEXEC` out because it meant to hand the fd across an `exec` lost
/// it, and a read-only export could be mapped and written.
pub fn prime_open_flags(flags: u32) -> OpenFlags {
    use super::devfs::drm_scheme::{DRM_CLOEXEC, DRM_RDWR};
    let mut open = if flags & DRM_RDWR != 0 {
        OpenFlags::RDWR
    } else {
        OpenFlags::RDONLY
    };
    if flags & DRM_CLOEXEC != 0 {
        open |= OpenFlags::CLOEXEC;
    }
    open
}

impl DmaBuf {
    /// Wrap a buffer's physical memory in a shareable dma-buf object.
    ///
    /// Prefer [`Self::from_prime`] when the export came from a known GEM
    /// handle: that path takes the nouveau reference a DRI3 importer needs.
    /// The fd is open as a `DRM_CLOEXEC | DRM_RDWR` export would be.
    pub fn new(phys_addr: u64, size: usize, vmo: Arc<VmObject>) -> Arc<Self> {
        Arc::new(Self {
            base: KObjectBase::new(),
            phys_addr,
            size,
            vmo,
            nouveau_handle: None,
            flags: Mutex::new(OpenFlags::RDWR | OpenFlags::CLOEXEC),
        })
    }

    /// Like [`Self::new`], but if `gem_handle` is a nouveau-uAPI object, take a
    /// `DMABUF_HOLDER` reference so the exporter can `GEM_CLOSE` its local
    /// handle without freeing the buffer out from under this fd; and open the
    /// fd as `flags` (the `drm_prime_handle.flags` word) says, see
    /// [`prime_open_flags`].
    pub fn from_prime(
        gem_handle: u32,
        phys_addr: u64,
        size: usize,
        vmo: Arc<VmObject>,
        flags: u32,
    ) -> Arc<Self> {
        let nouveau_handle = if gem_handle >= zcore_drivers::scheme::gem_mmap::DRIVER_HANDLE_BASE {
            super::devfs::drm::dmabuf_take_gem_ref(gem_handle);
            Some(gem_handle)
        } else {
            None
        };
        Arc::new(Self {
            base: KObjectBase::new(),
            phys_addr,
            size,
            vmo,
            nouveau_handle,
            flags: Mutex::new(prime_open_flags(flags)),
        })
    }

    /// The backing frames, for importing into another DRM node's GEM table.
    pub fn vmo(&self) -> Arc<VmObject> {
        self.vmo.clone()
    }
}

impl Drop for DmaBuf {
    fn drop(&mut self) {
        if let Some(handle) = self.nouveau_handle.take() {
            super::devfs::drm::dmabuf_drop_gem_ref(handle);
        }
    }
}

#[async_trait]
impl FileLike for DmaBuf {
    fn flags(&self) -> OpenFlags {
        *self.flags.lock()
    }

    fn set_flags(&self, f: OpenFlags) -> LxResult {
        // Same trap as syncobj/epoll/perf before `take_settable`: a hardcoded
        // `flags()` plus a no-op `set_flags` made `fcntl(F_SETFL, O_NONBLOCK)`
        // "succeed" while `F_GETFL` never changed.
        self.flags.lock().take_settable(f);
        Ok(())
    }

    // Linux `dma_buf_fops` can have read/write for CPU access; without that
    // path here, vfs-style "unsupported on this fd" is `-EINVAL`, not `-ENOSYS`.
    async fn read(&self, _buf: &mut [u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    fn write(&self, _buf: &[u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    async fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    /// Mesa's software dma-buf import (`kms_sw_displaytarget_from_handle`) sizes
    /// the buffer with `lseek(fd, 0, SEEK_END)` before mmap()ing it. Report the
    /// backing size so the import succeeds; without this lseek failed (EBADF on
    /// a non-`File` fd) and eglCreateImageKHR returned EGL_BAD_ALLOC.
    fn seek(&self, pos: SeekFrom) -> LxResult<u64> {
        let offset = match pos {
            SeekFrom::Start(off) => off as i64,
            SeekFrom::End(off) => self.size as i64 + off,
            // dma-bufs are stateless here (no kept cursor); treat relative seeks
            // from a zero base, which is all Mesa needs.
            SeekFrom::Current(off) => off,
        };
        if offset < 0 {
            return Err(LxError::EINVAL);
        }
        Ok(offset as u64)
    }

    fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: true,
            error: false,
            hangup: false,
        })
    }

    async fn async_poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: true,
            error: false,
            hangup: false,
        })
    }

    /// mmap of the dma-buf maps the same backing frames (CPU access for the
    /// software renderer / scanout).
    fn get_vmo(&self, _offset: usize, _len: usize) -> LxResult<Arc<VmObject>> {
        Ok(self.vmo.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `read`/`write` on a dma-buf fd must be `-EINVAL`, not `-ENOSYS`, when
    /// CPU access is not implemented here.
    #[test]
    fn read_write_on_dmabuf_are_einval_not_enosys() {
        use async_std::task::block_on;
        let vmo = VmObject::new_paged(1);
        let fd = DmaBuf::new(0, 4096, vmo);
        let mut buf = [0u8; 8];
        assert_eq!(block_on(fd.read(&mut buf)), Err(LxError::EINVAL));
        assert_eq!(fd.write(&[0u8; 8]), Err(LxError::EINVAL));
        assert_eq!(block_on(fd.read_at(0, &mut buf)), Err(LxError::EINVAL));
    }

    /// `fcntl(F_SETFL, O_NONBLOCK)` on a dma-buf fd used to return success
    /// while `F_GETFL` stayed forever at the hardcoded RDWR|CLOEXEC.
    #[test]
    fn set_flags_turns_a_blocking_dmabuf_non_blocking() {
        let vmo = VmObject::new_paged(1);
        let fd = DmaBuf::new(0, 4096, vmo);
        assert!(!fd.flags().non_block());
        assert!(fd.flags().close_on_exec());
        let mut f = fd.flags();
        f.set(OpenFlags::NON_BLOCK, true);
        fd.set_flags(f).unwrap();
        assert!(fd.flags().non_block() && fd.flags().close_on_exec());
    }
}

#[cfg(test)]
mod prime_flags_tests {
    //! The exported fd is open as `drm_prime_handle.flags` says, which
    //! `PRIME_HANDLE_TO_FD` never looked at.

    use super::super::devfs::drm_scheme::{DRM_CLOEXEC, DRM_RDWR};
    use super::*;
    use crate::process::LinuxProcess;
    use rcore_fs_ramfs::RamFS;
    use zircon_object::vm::VmObject;

    /// A dumb-buffer export: a handle below the nouveau range touches no
    /// GEM table, so the fd can be built without a DRM node.
    fn exported_with(flags: u32) -> Arc<DmaBuf> {
        DmaBuf::from_prime(1, 0, 4096, VmObject::new_paged(1), flags)
    }

    /// Only `DRM_RDWR` makes the fd writable, only `DRM_CLOEXEC` makes it
    /// close-on-exec, and the two do not bleed into each other.
    #[test]
    fn the_word_maps_to_the_access_mode_and_the_cloexec_bit() {
        assert_eq!(prime_open_flags(0), OpenFlags::RDONLY);
        assert_eq!(prime_open_flags(DRM_RDWR), OpenFlags::RDWR);
        assert_eq!(
            prime_open_flags(DRM_CLOEXEC),
            OpenFlags::RDONLY | OpenFlags::CLOEXEC
        );
        assert_eq!(
            prime_open_flags(DRM_CLOEXEC | DRM_RDWR),
            OpenFlags::RDWR | OpenFlags::CLOEXEC
        );
        for flags in [0, DRM_RDWR, DRM_CLOEXEC, DRM_CLOEXEC | DRM_RDWR] {
            assert_eq!(exported_with(flags).flags(), prime_open_flags(flags));
        }
    }

    /// The mode `mmap` checks: an export without `DRM_RDWR` is `O_RDONLY`
    /// (Linux refuses a shared writable mapping of it with `EACCES`), and
    /// one with it is `O_RDWR`. Every export used to be `O_RDWR`.
    #[test]
    fn without_drm_rdwr_the_fd_is_read_only() {
        let ro = exported_with(DRM_CLOEXEC);
        assert!(ro.flags().readable());
        assert!(!ro.flags().writable(), "no DRM_RDWR, no write access");
        let rw = exported_with(DRM_CLOEXEC | DRM_RDWR);
        assert!(rw.flags().readable());
        assert!(rw.flags().writable());
    }

    /// The descriptor `PRIME_HANDLE_TO_FD` installs is close-on-exec exactly
    /// when the word carries `DRM_CLOEXEC`: `add_file` reads the flag off the
    /// object, as it does for `open(O_CLOEXEC)`, so an exporter that meant
    /// to hand the fd across an `exec` keeps it. Every export used to be
    /// close-on-exec.
    #[test]
    fn the_descriptor_is_close_on_exec_only_with_drm_cloexec() {
        let proc = LinuxProcess::new(RamFS::new(), 0);
        let inherited = proc.add_file(exported_with(DRM_RDWR)).unwrap();
        assert!(!proc.fd_cloexec(inherited).unwrap(), "no DRM_CLOEXEC asked");
        let private = proc
            .add_file(exported_with(DRM_CLOEXEC | DRM_RDWR))
            .unwrap();
        assert!(proc.fd_cloexec(private).unwrap());
    }

    /// The generic constructor keeps the `O_RDWR | O_CLOEXEC` every export
    /// used to get.
    #[test]
    fn a_generic_dma_buf_is_read_write_and_close_on_exec() {
        let generic = DmaBuf::new(0, 4096, VmObject::new_paged(1));
        assert_eq!(generic.flags(), OpenFlags::RDWR | OpenFlags::CLOEXEC);
    }
}
