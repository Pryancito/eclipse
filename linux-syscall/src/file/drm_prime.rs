//! The DRM ioctls that have to reach the process fd table: PRIME dma-buf
//! export and import, and `MODE_CREATE_LEASE`.
//!
//! They are not dispatched through the inode's `io_control`, which sees
//! neither the fd table nor the caller: an export has to INSTALL a new fd
//! for the dma-buf it makes, an import has to LOOK UP the fd it was handed,
//! and a lease needs a second descriptor of the same open file description.
//! Everything the DRM core itself decides is
//! `linux_object::fs::devfs::drm`; this is the fd half.
//!
//! Out of `file.rs` for its size ceiling, by the same route the fourteen
//! `#[cfg(test)] mod`s took on 20-oct-2026 (see `xtask::tamano`): this
//! function is a whole subject of its own and the only one in that file
//! that is about DRM.

use super::file::DrmModeCreateLease;
use super::*;

impl Syscall<'_> {
    /// DRM ioctls that must install / look up a process fd (PRIME dma-buf
    /// export-import, and CREATE_LEASE) — the inode-level DRM `io_control`
    /// cannot touch the fd table. Returns `Ok(Some(0))` if handled, `Ok(None)`
    /// if `fd` is not a DRM device (fall through to the normal ioctl path).
    pub(super) fn sys_drm_prime(
        &self,
        file_like: &alloc::sync::Arc<dyn linux_object::fs::FileLike>,
        request: usize,
        arg1: usize,
    ) -> Result<Option<usize>, LxError> {
        use linux_object::fs::devfs::drm;
        use linux_object::fs::devfs::drm_scheme::{
            nr, prime_request, DrmPrimeHandle, PrimeRequest,
        };
        use linux_object::fs::DmaBuf;

        const PRIME_HANDLE_TO_FD: u32 = nr::PRIME_HANDLE_TO_FD.0; // DRM_IOWR(0x2d, drm_prime_handle)
        const PRIME_FD_TO_HANDLE: u32 = nr::PRIME_FD_TO_HANDLE.0; // DRM_IOWR(0x2e, drm_prime_handle)
        const MODE_CREATE_LEASE: u32 = nr::MODE_CREATE_LEASE.0; // DRM_IOWR(0xc6, drm_mode_create_lease)

        // `get_unused_fd_flags(cl->flags & (O_CLOEXEC | O_NONBLOCK))`.
        const O_CLOEXEC: u32 = 0o2000000;

        // The fd really has to be a DRM node. Dispatching on the request number
        // alone meant `PRIME_HANDLE_TO_FD` on ANY fd reached the export path,
        // so a process that had never opened `/dev/dri/*` could still ask for a
        // dma-buf over a GEM handle. Linux only ever reaches
        // `drm_prime_handle_to_fd_ioctl` through a DRM file. The export path
        // now also checks who owns the handle (see `drm::export_handle`), but
        // both gates belong here: this is the one that keeps the ioctl on the
        // device it is defined for. Same downcast `WAIT_VBLANK` already uses.
        let drm = file_like.downcast_ref::<File>().and_then(|f| {
            let inode = f.inode();
            let state = inode
                .as_any_ref()
                .downcast_ref::<linux_object::fs::devfs::DrmDev>()
                .map(|dev| dev.file_state().clone())?;
            Some((inode, state))
        });
        let Some((drm_inode, drm_file)) = drm else {
            return Ok(None);
        };
        let proc = self.linux_process();
        // Match on the DRM ioctl NR only. The struct size the client encoded is
        // deliberately NOT part of the comparison -- see `is_drm_ioctl_nr`.
        match request as u32 & 0xff {
            // EXPORT (HANDLE_TO_FD, 0x2d) and IMPORT (FD_TO_HANDLE, 0x2e)
            // share one arm; `prime_request` tells them apart by the ioctl
            // NUMBER, as `drm_ioctl` does. This arm used to read the
            // operation off the struct instead (`fd < 0` = export, because
            // libdrm presets that output field to -1): the two numbers were
            // filed the wrong way round in `nr`, which is what "0xc00c642d
            // mis-routed into the import path on real hardware" was, and the
            // struct heuristic papered over it -- while breaking any exporter
            // that zeroes the struct (fd = 0 read as "import stdin").
            PRIME_HANDLE_TO_FD | PRIME_FD_TO_HANDLE => {
                let mut ptr = UserInOutPtr::<DrmPrimeHandle>::from(arg1);
                let mut h = match ptr.read() {
                    Ok(h) => h,
                    Err(e) => {
                        warn!("[drm] PRIME read(args @ {:#x}) EFAULT: {:?}", arg1, e);
                        return Err(e.into());
                    }
                };
                let op = match prime_request(request as u32, h) {
                    Ok(op) => op,
                    Err(e) => {
                        warn!(
                            "[drm] PRIME request={:#x} handle={} flags={:#x} fd={} refused: {:?}",
                            request, h.handle, h.flags, h.fd, e
                        );
                        return Err(e);
                    }
                };
                match op {
                    PrimeRequest::Export { handle, flags } => {
                        // handle -> new dma-buf fd. `h.fd` is an OUTPUT here:
                        // whatever the caller left in it is not read. `flags`
                        // says what the fd is open as (`DRM_RDWR`) and whether
                        // it is close-on-exec (`DRM_CLOEXEC`); `add_file` reads
                        // the second off the object, as it does for `open`.
                        let (phys, size, vmo) = match drm::export_handle(handle, drm_file.minor()) {
                            Ok(v) => v,
                            Err(refusal) => {
                                warn!(
                                    "[drm] PRIME export handle={} refused: {:?}",
                                    handle, refusal
                                );
                                return Err(refusal.into());
                            }
                        };
                        let dmabuf = DmaBuf::from_prime(handle, phys, size, vmo, flags);
                        let new_fd = match proc.add_file(dmabuf) {
                            Ok(fd) => fd,
                            Err(e) => {
                                warn!("[drm] PRIME export add_file {:?}", e);
                                return Err(e);
                            }
                        };
                        h.fd = i32::from(new_fd);
                        // debug-only: export is a HOT path. wlroots re-exports both
                        // swapchain buffers on every recreate/present cycle, so at
                        // error! this floods the graphical console (~8/s) and buries
                        // the real failure in labwc.log -- exactly the console-flood
                        // pattern to avoid. The diagnostic job (compare exported phys
                        // vs the import-side reverse lookup) is done; keep it at debug.
                        log::debug!(
                            "[drm] PRIME export handle={:#x} -> phys={:#x} size={} fd={}",
                            h.handle,
                            phys,
                            size,
                            h.fd
                        );
                        if let Err(e) = ptr.write(h) {
                            warn!("[drm] PRIME export write-back EFAULT: {:?}", e);
                            return Err(e.into());
                        }
                        Ok(Some(0))
                    }
                    PrimeRequest::Import { fd } => {
                        // fd -> new GEM handle. `h.handle` is the OUTPUT here.
                        let target = match proc.get_file_like(FileDesc::from(fd as usize)) {
                            Ok(t) => t,
                            Err(e) => {
                                warn!("[drm] PRIME import EBADF: fd={} not in fd table", fd);
                                return Err(e);
                            }
                        };
                        let dmabuf = target.downcast_ref::<DmaBuf>().ok_or(LxError::EINVAL)?;
                        // Self-import first: if this dma-buf was exported from a
                        // nouveau-uAPI GEM object, hand back the ORIGINAL nouveau
                        // handle -- real Linux PRIME semantics (importing your own
                        // export resolves to the existing GEM object). This is
                        // what the compositor does for every swapchain buffer:
                        // gbm/NVK allocates (GEM_NEW), exports the fd, and EGL
                        // imports it again on the same device. A fresh GENERIC
                        // handle over the same memory (the old behaviour) passes
                        // the generic ioctls and then fails every driver-private
                        // one -- nouveau GEM_INFO gave ENOENT, NVK's dma-buf
                        // import died, and the desktop fell over at
                        // "createImageFromDmaBufs failed" / zink "couldn't
                        // allocate memory".
                        // ...and the card behind THIS node decides, because an
                        // import is per device: `drm_gem_prime_fd_to_handle`
                        // resolves against the importing device's prime table
                        // and sends anything else to that device's own importer
                        // (`nouveau_gem_prime_import_sg_table`). The reverse
                        // lookup here is by physical address, with no card in
                        // the key, so importing a card0 buffer through `card1`
                        // used to answer card0's HANDLE: an id out of the other
                        // card's slice, handed out through this node, whose
                        // GEM_CLOSE drops a reference on the other card's
                        // object and whose GEM_INFO/VM_BIND name an object of
                        // the other card's RM. See `drm::prime_import`.
                        let (handle_id, kind) = match drm::prime_import_for(
                            dmabuf.phys_addr,
                            drm_file.minor(),
                        ) {
                            drm::PrimeImport::SameCard(nouveau_handle) => {
                                // Take a PRIME reference: the importer will GEM_CLOSE
                                // this handle when done, but the exporter (wlroots)
                                // still owns the same nouveau handle. Without this
                                // bump the importer's close frees the buffer out from
                                // under the exporter, the next self-import misses, and
                                // NVK falls back to a generic handle that GEM_INFO
                                // ENOENTs -> zink "couldn't allocate memory heap=0".
                                // One reference per importing FILE, not per import:
                                // see `nouveau_gem_import_ref`.
                                let n = drm::nouveau_gem_import_ref(nouveau_handle, &drm_file);
                                // debug-only: this is the SUCCESS path and it is hot
                                // (every swapchain buffer import). Stay quiet so a
                                // working stack does not flood the console; the failure
                                // path below is the one that logs loudly.
                                log::debug!(
                                    "[drm] PRIME self-import ref++ handle={:#x} -> refcount={:?}",
                                    nouveau_handle,
                                    n
                                );
                                // ...except the first few per boot, at error! (the
                                // rig runs LOG=error): whether the COMPOSITOR ever
                                // imports a CLIENT's dma-buf is the missing half of
                                // the vkcube/eglgears present-hang picture. The pid
                                // tells the two apart -- labwc importing a phys that
                                // a client pid exported means the Wayland dmabuf
                                // dance reached the compositor at all.
                                {
                                    use core::sync::atomic::{AtomicU32, Ordering};
                                    static FIRST_IMPORTS: AtomicU32 = AtomicU32::new(0);
                                    let k = FIRST_IMPORTS.fetch_add(1, Ordering::Relaxed);
                                    if k < 8 {
                                        log::error!(
                                        "[drm] PRIME import pid={} fd={} phys={:#x} size={} -> nouveau handle={:#x} (import {}/8 this boot; informational, not an error)",
                                        self.zircon_process().id(), h.fd,
                                        dmabuf.phys_addr, dmabuf.size, nouveau_handle, k + 1
                                    );
                                    }
                                }
                                (nouveau_handle, "self-import(nouveau)")
                            }
                            // The object is another card's. A GEM handle names
                            // an object of ONE device's driver, so this node
                            // imports the pages as an object of its own --
                            // which is what wlroots' secondary card does with
                            // the composed frame under
                            // `WLR_DRM_DEVICES=/dev/dri/card0:/dev/dri/card1`.
                            // A core handle, not a driver-private one: the
                            // buffer is foreign memory with a host physical
                            // address, which is exactly what the CPU present
                            // path reads, and the importing card runs no
                            // driver-private ioctl on a frame it only scans
                            // out. Not a miss, so not the loud line below.
                            drm::PrimeImport::OtherCard(theirs) => {
                                let mine =
                                    drm::import_dmabuf(dmabuf.phys_addr, dmabuf.size, dmabuf.vmo());
                                log::info!(
                                    "[drm] PRIME cross-card import fd={} phys={:#x} size={}: \
                                     card{:?} object handle={:#x} imported on card{:?} as \
                                     handle={:#x} (a handle is one card's; the other card's id \
                                     is not handed out here)",
                                    h.fd,
                                    dmabuf.phys_addr,
                                    dmabuf.size,
                                    drm::card_index_holding_gem(theirs),
                                    theirs,
                                    drm::card_index_of_minor(drm_file.minor()),
                                    mine,
                                );
                                (mine, "cross-card")
                            }
                            drm::PrimeImport::Untracked => (
                                drm::import_dmabuf(dmabuf.phys_addr, dmabuf.size, dmabuf.vmo()),
                                "generic",
                            ),
                        };
                        // Loud ONLY on the failure path: "generic" means the
                        // self-import reverse lookup MISSED (the dma-buf's phys is not
                        // registered as a nouveau GEM object), so NVK's driver-private
                        // GEM_INFO will ENOENT the fresh handle -> zink "couldn't
                        // allocate memory heap=0" / createImageFromDmaBufs failed. The
                        // success path ("self-import(nouveau)") is hot and stays at
                        // debug so a working swapchain loop doesn't flood the console.
                        if kind == "generic" {
                            log::info!(
                            "[drm] PRIME import fd={} phys={:#x} size={} -> generic handle={:#x} (SELF-IMPORT MISS -> GEM_INFO will ENOENT)",
                            h.fd, dmabuf.phys_addr, dmabuf.size, handle_id
                        );
                        } else {
                            log::debug!(
                                "[drm] PRIME import fd={} phys={:#x} size={} -> {} handle={:#x}",
                                h.fd,
                                dmabuf.phys_addr,
                                dmabuf.size,
                                kind,
                                handle_id
                            );
                        }
                        h.handle = handle_id;
                        ptr.write(h)?;
                        Ok(Some(0))
                    }
                }
            }
            MODE_CREATE_LEASE => {
                // `drm_mode_create_lease_ioctl` in two halves. The DRM file
                // does everything up to the fd (`DRM_IOCTL_MODE_CREATE_LEASE`
                // in `drm_scheme.rs`: the master check, the flags, the object
                // list and the lessee id); what only this layer can do is
                // install the fd. Linux clones the lessor's file into a new
                // one that holds the lease; here the lease fd is a second
                // descriptor of this same open file description, the way
                // `dup` makes one -- so closing the lease does not run the
                // DRM file's teardown on state the lessor is still using, and
                // so the lessee keeps the whole device (see README-drm). Our
                // GEM table is global, so the per-fd handle ref-counting that
                // makes wlroots' dumb allocator lease is not needed either.
                drm_inode.io_control(request as u32, arg1)?;
                let mut ptr = UserInOutPtr::<DrmModeCreateLease>::from(arg1);
                let mut l = ptr.read()?;
                let lease = file_like.clone();
                let new_fd = match proc.add_file_cloexec(lease, l.flags & O_CLOEXEC != 0) {
                    Ok(fd) => fd,
                    Err(e) => {
                        // `get_unused_fd_flags` failing: no lease is made.
                        drm::lease_drop(drm_file.minor(), l.lessee_id);
                        return Err(e);
                    }
                };
                l.fd = i32::from(new_fd);
                ptr.write(l)?;
                Ok(Some(0))
            }
            _ => Ok(None),
        }
    }
}
