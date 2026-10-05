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
use zircon_object::vm::{page_aligned, roundup_pages};

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

/// `DMA_BUF_IOCTL_SYNC`: `_IOW('b', 0, struct dma_buf_sync)`, one `__u64` of
/// flags.
const DMA_BUF_IOCTL_SYNC: usize = 0x4008_6200;
/// `DMA_BUF_SET_NAME` as the header first spelt it (`_IOW('b', 1, __u32)`)
/// and as it spells it now (`_IOW('b', 1, const char *)`); the kernel takes
/// both.
const DMA_BUF_SET_NAME_A: usize = 0x4004_6201;
const DMA_BUF_SET_NAME_B: usize = 0x4008_6201;
/// `DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE`.
const DMA_BUF_SYNC_RW: u64 = 1 | 2;
/// `DMA_BUF_SYNC_END`.
const DMA_BUF_SYNC_END: u64 = 4;
/// `DMA_BUF_SYNC_VALID_FLAGS_MASK`.
const DMA_BUF_SYNC_VALID_FLAGS_MASK: u64 = DMA_BUF_SYNC_RW | DMA_BUF_SYNC_END;
/// `DMA_BUF_NAME_LEN`.
const DMA_BUF_NAME_LEN: usize = 32;

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

    // `dma_buf_fops` has no read or write: `vfs_read`/`vfs_write` answer
    // EINVAL for a file without them, not ENOSYS.
    async fn read(&self, _buf: &mut [u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    fn write(&self, _buf: &[u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    async fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    /// Mesa's software dma-buf import (`kms_sw_displaytarget_from_handle`) sizes
    /// the buffer with `lseek(fd, 0, SEEK_END)` before mmap()ing it. Report the
    /// backing size so the import succeeds; without this lseek failed (EBADF on
    /// a non-`File` fd) and eglCreateImageKHR returned EGL_BAD_ALLOC.
    fn seek(&self, pos: SeekFrom) -> LxResult<u64> {
        // `dma_buf_llseek`: "only support discovering the end of the buffer,
        // but also allow SEEK_SET to maintain the idiomatic SEEK_END(0),
        // SEEK_CUR(0) pattern" -- SEEK_END and SEEK_SET, offset 0, and
        // EINVAL for the rest. This took any whence and any offset and
        // answered with arithmetic on a file that has no position, so a
        // client probing the size the Linux way got the right answer and
        // one probing it any other way got a number that meant nothing.
        let (base, offset) = match pos {
            SeekFrom::End(off) => (self.size as u64, off),
            SeekFrom::Start(off) => (0, off as i64),
            SeekFrom::Current(_) => return Err(LxError::EINVAL),
        };
        if offset != 0 {
            return Err(LxError::EINVAL);
        }
        Ok(base)
    }

    /// `dma_buf_ioctl`: `DMA_BUF_IOCTL_SYNC` with its flags validated,
    /// `DMA_BUF_SET_NAME` with its string validated, ENOTTY for the rest.
    /// None of it existed: every ioctl on a dma-buf fd was ENOSYS, and a
    /// client bracketing its CPU access with `SYNC` (Firefox's and
    /// Chromium's dma-buf surfaces do) took that as the fd not being a
    /// dma-buf at all.
    fn ioctl(&self, request: usize, arg1: usize, _arg2: usize, _arg3: usize) -> LxResult<usize> {
        match request {
            DMA_BUF_IOCTL_SYNC => {
                if !kernel_hal::user::user_range_ok(arg1, core::mem::size_of::<u64>()) {
                    return Err(LxError::EFAULT);
                }
                let flags = unsafe { *(arg1 as *const u64) };
                if flags & !DMA_BUF_SYNC_VALID_FLAGS_MASK != 0 {
                    return Err(LxError::EINVAL);
                }
                // The direction has to be READ, WRITE or both; "neither" is
                // the `default:` arm of the kernel's switch.
                if flags & DMA_BUF_SYNC_RW == 0 {
                    return Err(LxError::EINVAL);
                }
                // `begin_cpu_access`/`end_cpu_access`: the backing here is
                // coherent with the CPU (system memory or a BAR mapping),
                // so there is no cache to flush or invalidate.
                Ok(0)
            }
            DMA_BUF_SET_NAME_A | DMA_BUF_SET_NAME_B => {
                // `strndup_user(buf, DMA_BUF_NAME_LEN)`: the name has to end
                // within 32 bytes (EINVAL otherwise). Nothing here shows it
                // (Linux puts it in fdinfo), so it is checked and dropped.
                if !kernel_hal::user::user_range_ok(arg1, DMA_BUF_NAME_LEN) {
                    return Err(LxError::EFAULT);
                }
                let name =
                    unsafe { core::slice::from_raw_parts(arg1 as *const u8, DMA_BUF_NAME_LEN) };
                if !name.contains(&0) {
                    return Err(LxError::EINVAL);
                }
                Ok(0)
            }
            _ => Err(LxError::ENOTTY),
        }
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
    /// software renderer / scanout), from `offset` on and only inside the
    /// buffer; see [`Self::mmap_window`].
    ///
    /// The offset used to be ignored: `mmap(fd, len, offset = 1 page)`
    /// handed back page 0, so a client mapping the second half of an
    /// imported buffer read and wrote its first half.
    fn get_vmo(&self, offset: usize, len: usize) -> LxResult<Arc<VmObject>> {
        self.mmap_window(offset, len)?;
        if offset == 0 {
            return Ok(self.vmo.clone());
        }
        // A slice keeps the frames shared and bakes the offset in, which is
        // what the MAP_PRIVATE path expects (it maps its VMO from 0).
        self.vmo
            .create_slice(offset, roundup_pages(len))
            .map_err(|_| LxError::EINVAL)
    }

    /// `MAP_SHARED`: every mapper gets the one backing VMO, at `offset`.
    /// The default of this method dropped the offset on the floor.
    fn get_vmo_shared(&self, offset: usize, len: usize) -> LxResult<(Arc<VmObject>, usize)> {
        self.mmap_window(offset, len)?;
        Ok((self.vmo.clone(), offset))
    }
}

impl DmaBuf {
    /// `dma_buf_mmap_internal`: the window `[offset, offset + len)` has to lie
    /// inside the buffer, counted in pages (`vm_pgoff + vma_pages(vma) >
    /// dmabuf->size >> PAGE_SHIFT` is `EINVAL`), and `offset` is a page
    /// offset. The buffer's size is rounded UP to pages here, where Linux's
    /// is already page-aligned by the GEM exporter: a dumb buffer's `size` is
    /// `pitch * height`, and its last partial page is backed and mappable.
    ///
    /// Without this a window past the end was silently accepted: `mmap` then
    /// backed the tail with anonymous zero pages, so a client that mapped too
    /// much (or at the wrong offset) wrote into memory no other importer
    /// could see, and never learned it.
    fn mmap_window(&self, offset: usize, len: usize) -> LxResult<()> {
        if !page_aligned(offset) {
            return Err(LxError::EINVAL);
        }
        let end = offset
            .checked_add(roundup_pages(len))
            .ok_or(LxError::EINVAL)?;
        if end > roundup_pages(self.size) {
            return Err(LxError::EINVAL);
        }
        Ok(())
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

#[cfg(test)]
mod fops_tests {
    //! The dma-buf fd's own file operations, against `dma_buf_fops`.

    use super::super::devfs::drm_scheme::DRM_RDWR;
    use super::*;
    use zircon_object::vm::VmObject;

    fn dmabuf() -> Arc<DmaBuf> {
        DmaBuf::from_prime(1, 0, 4096, VmObject::new_paged(1), DRM_RDWR)
    }

    /// `dma_buf_llseek`: SEEK_END(0) is how userspace learns the size, and
    /// SEEK_SET(0) is allowed for the idiom; any other whence or offset is
    /// EINVAL. Any whence and any offset were accepted and answered with
    /// arithmetic on a file that has no position.
    #[test]
    fn lseek_discovers_the_size_and_refuses_everything_else() {
        let d = dmabuf();
        assert_eq!(d.seek(SeekFrom::End(0)), Ok(4096));
        assert_eq!(d.seek(SeekFrom::Start(0)), Ok(0));
        assert_eq!(d.seek(SeekFrom::Current(0)), Err(LxError::EINVAL));
        assert_eq!(d.seek(SeekFrom::End(-8)), Err(LxError::EINVAL));
        assert_eq!(d.seek(SeekFrom::End(8)), Err(LxError::EINVAL));
        assert_eq!(d.seek(SeekFrom::Start(8)), Err(LxError::EINVAL));
        assert_eq!(d.seek(SeekFrom::Current(8)), Err(LxError::EINVAL));
    }

    /// `dma_buf_mmap_internal`: the window has to lie inside the buffer, in
    /// pages, from a page-aligned offset; the exact fit is allowed. A
    /// dumb buffer's `size` is `pitch * height`, so its last partial page
    /// counts as mappable. Every window was accepted.
    #[test]
    fn a_mapping_window_must_lie_inside_the_buffer() {
        const PAGE: usize = 4096;
        let d = DmaBuf::from_prime(1, 0, 3 * PAGE, VmObject::new_paged(3), DRM_RDWR);
        let shared = |offset, len| d.get_vmo_shared(offset, len).map(|(_, off)| off);
        assert_eq!(shared(0, 3 * PAGE), Ok(0), "the whole buffer");
        assert_eq!(
            shared(2 * PAGE, PAGE),
            Ok(2 * PAGE),
            "the last page, exactly"
        );
        assert_eq!(shared(PAGE, 1), Ok(PAGE), "a byte of the second page");
        assert_eq!(
            shared(0, 3 * PAGE + 1),
            Err(LxError::EINVAL),
            "one byte past"
        );
        assert_eq!(
            shared(2 * PAGE, PAGE + 1),
            Err(LxError::EINVAL),
            "past from the end"
        );
        assert_eq!(
            shared(3 * PAGE, PAGE),
            Err(LxError::EINVAL),
            "starting at the end"
        );
        assert_eq!(
            shared(PAGE + 1, PAGE),
            Err(LxError::EINVAL),
            "unaligned offset"
        );
        assert_eq!(
            shared(usize::MAX - PAGE + 1, PAGE),
            Err(LxError::EINVAL),
            "an offset whose window wraps around"
        );
        for (offset, len) in [(0, 3 * PAGE), (2 * PAGE, PAGE), (PAGE, 1)] {
            assert!(
                d.get_vmo(offset, len).is_ok(),
                "private {:#x}+{:#x}",
                offset,
                len
            );
        }
        for (offset, len) in [
            (0, 3 * PAGE + 1),
            (2 * PAGE, PAGE + 1),
            (3 * PAGE, PAGE),
            (PAGE + 1, PAGE),
            (usize::MAX - PAGE + 1, PAGE),
        ] {
            assert_eq!(
                d.get_vmo(offset, len).map(|_| ()),
                Err(LxError::EINVAL),
                "private {:#x}+{:#x}",
                offset,
                len
            );
        }

        // pitch * height that is not a page multiple: the partial page maps.
        let odd = DmaBuf::from_prime(1, 0, PAGE + 1, VmObject::new_paged(2), DRM_RDWR);
        assert_eq!(odd.get_vmo_shared(PAGE, PAGE).map(|(_, off)| off), Ok(PAGE));
        assert_eq!(
            odd.get_vmo_shared(2 * PAGE, PAGE).map(|(_, off)| off),
            Err(LxError::EINVAL)
        );
    }

    /// The offset is honoured: a `MAP_SHARED` window is the one backing VMO
    /// at that offset, and a `MAP_PRIVATE` one is a slice that starts there,
    /// so both see the byte the exporter wrote at `offset`. Both used to hand
    /// back page 0 whatever the offset.
    #[test]
    fn the_mapping_starts_at_the_offset_the_client_asked_for() {
        const PAGE: usize = 4096;
        let backing = VmObject::new_paged(3);
        backing.write(PAGE + 8, b"page one").unwrap();
        let d = DmaBuf::from_prime(1, 0, 3 * PAGE, backing.clone(), DRM_RDWR);

        let (vmo, off) = d.get_vmo_shared(PAGE, PAGE).unwrap();
        assert!(
            Arc::ptr_eq(&vmo, &backing),
            "shared mappers get the one VMO"
        );
        let mut got = [0u8; 8];
        vmo.read(off + 8, &mut got).unwrap();
        assert_eq!(&got, b"page one");

        // The middle page: a slice of the window, not of the rest of the buffer.
        let slice = d.get_vmo(PAGE, PAGE).unwrap();
        assert_eq!(slice.len(), PAGE, "the slice is the window, not the buffer");
        slice.read(8, &mut got).unwrap();
        assert_eq!(&got, b"page one");
        // And it is a view, not a copy: a store through it lands in the buffer.
        slice.write(0, b"via slice").unwrap();
        let mut back = [0u8; 9];
        backing.read(PAGE, &mut back).unwrap();
        assert_eq!(&back, b"via slice");

        assert!(
            Arc::ptr_eq(&d.get_vmo(0, PAGE).unwrap(), &backing),
            "offset 0 is still the buffer itself"
        );
    }

    /// `dma_buf_fops` has no write (nor read): EINVAL, as `vfs_write` answers
    /// for such a file, not ENOSYS.
    #[test]
    fn a_dma_buf_cannot_be_written_through_the_fd() {
        assert_eq!(dmabuf().write(&[0u8; 4]), Err(LxError::EINVAL));
    }

    /// `dma_buf_ioctl`: SYNC takes READ, WRITE or both, with or without END,
    /// and nothing else (EINVAL); SET_NAME wants a string that ends within
    /// 32 bytes (EINVAL); anything else is ENOTTY. Every ioctl was ENOSYS.
    #[test]
    fn sync_and_set_name_are_validated_and_the_rest_is_enotty() {
        let d = dmabuf();
        let sync = |flags: u64| d.ioctl(DMA_BUF_IOCTL_SYNC, &flags as *const u64 as usize, 0, 0);
        for ok in [1u64, 2, 3, 1 | 4, 2 | 4, 3 | 4] {
            assert_eq!(sync(ok), Ok(0), "flags {:#x}", ok);
        }
        assert_eq!(sync(0), Err(LxError::EINVAL), "no direction");
        assert_eq!(sync(4), Err(LxError::EINVAL), "END with no direction");
        assert_eq!(
            sync(8),
            Err(LxError::EINVAL),
            "a flag the kernel does not define"
        );
        assert_eq!(sync(1 << 32), Err(LxError::EINVAL), "in the high word too");
        assert_eq!(
            sync(8 | 1),
            Err(LxError::EINVAL),
            "an undefined flag is refused even next to a direction"
        );
        assert_eq!(
            sync((1 << 32) | 1),
            Err(LxError::EINVAL),
            "and so is one in the high word"
        );

        let name = *b"scanout\0";
        for request in [DMA_BUF_SET_NAME_A, DMA_BUF_SET_NAME_B] {
            assert_eq!(d.ioctl(request, name.as_ptr() as usize, 0, 0), Ok(0));
        }
        let unterminated = [b'x'; DMA_BUF_NAME_LEN];
        assert_eq!(
            d.ioctl(DMA_BUF_SET_NAME_B, unterminated.as_ptr() as usize, 0, 0),
            Err(LxError::EINVAL),
            "a name that does not end within DMA_BUF_NAME_LEN"
        );
        let mut terminated = [b'x'; DMA_BUF_NAME_LEN];
        terminated[DMA_BUF_NAME_LEN - 1] = 0;
        assert_eq!(
            d.ioctl(DMA_BUF_SET_NAME_B, terminated.as_ptr() as usize, 0, 0),
            Ok(0),
            "one that ends on the last byte"
        );

        let flags = 1u64;
        assert_eq!(
            d.ioctl(0x4008_6209, &flags as *const u64 as usize, 0, 0),
            Err(LxError::ENOTTY),
            "an ioctl the dma-buf does not have"
        );
    }
}
