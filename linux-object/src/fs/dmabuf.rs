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
    flags: Mutex<OpenFlags>,
}

impl_kobject!(DmaBuf);

impl DmaBuf {
    /// Wrap a buffer's physical memory in a shareable dma-buf object.
    ///
    /// Prefer [`Self::from_prime`] when the export came from a known GEM
    /// handle: that path takes the nouveau reference a DRI3 importer needs.
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
    /// handle without freeing the buffer out from under this fd.
    pub fn from_prime(
        gem_handle: u32,
        phys_addr: u64,
        size: usize,
        vmo: Arc<VmObject>,
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
            flags: Mutex::new(OpenFlags::RDWR | OpenFlags::CLOEXEC),
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
