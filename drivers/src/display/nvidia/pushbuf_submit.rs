//! The part of a real `GEM_PUSHBUF` that touches hardware: mapping the
//! listed buffers, storing the reloc words, and handing each push to the
//! ring.
//!
//! Separate from [`super::pushbuf`] so that module stays free of FFI and runs
//! its tests on the host. Everything decided here is decided there; what is
//! left is plumbing -- the RM call, the stores and the log lines.

use super::super::nouveau_uapi as nv;
use super::pushbuf;
use alloc::vec::Vec;

/// What one listed buffer resolved to: where it lives in the channel's VAS,
/// how big it is, and whether it ended up in VRAM (which is what the `OR`
/// step of a reloc picks `vor` or `tor` by).
#[derive(Clone, Copy)]
pub(super) struct ResolvedBo {
    pub va: u64,
    pub size: u64,
    pub in_vram: bool,
}

impl super::NvidiaGpu {
    /// Give every listed buffer a GPU VA in `ctx_idx`'s VAS.
    ///
    /// A legacy (non-uvmm) client never calls `VM_BIND`, so nothing has
    /// mapped these buffers: this is `nouveau_gem_pushbuf_validate`'s job,
    /// done through the same `vm_bind_map` mechanic `VM_BIND` drives. An
    /// existing whole-object mapping of the same handle in the same context
    /// is reused -- without that, a client that submits sixty times a second
    /// would ask the RM for a fresh virtual range per buffer per frame.
    ///
    /// `requested_va` is 0 so the RM picks, and the VA it picked is recorded
    /// in `nouveau_vm_mappings` like any other mapping, so process-exit
    /// teardown and context retirement already free it. Nothing new has to
    /// remember to.
    pub(super) fn pushbuf_resolve_bos(
        &self,
        device_instance: u32,
        ctx_idx: u32,
        owner_pid: u64,
        bos: &[nv::DrmNouveauGemPushbufBo],
    ) -> Result<Vec<ResolvedBo>, i32> {
        let mut out = Vec::with_capacity(bos.len());
        for (i, bo) in bos.iter().enumerate() {
            // Taken afresh per buffer and dropped before the RM call: the RM
            // gate can block, and `nouveau_gem` is on EXEC's hot path.
            let found = {
                let gem = self.nouveau_gem.lock();
                gem.iter()
                    .find(|o| o.handle == bo.handle && super::gem_usable_by(o, owner_pid))
                    .map(|o| (o.h_memory, o.size, o.vram_offset.is_some()))
            };
            let Some((h_memory, size, in_vram)) = found else {
                crate::klog_warn!(
                    "[nouveau-uapi] GEM_PUSHBUF bo[{i}]: no such object (handle={}) -> ENOENT",
                    bo.handle
                );
                return Err(nv::ENOENT);
            };
            if let Some(va) = self.pushbuf_existing_va(bo.handle, owner_pid, size) {
                out.push(ResolvedBo { va, size, in_vram });
                continue;
            }
            let bind = nvidia_rm_sys::rm_init::vm_bind_map(
                device_instance,
                ctx_idx,
                h_memory,
                size,
                0,
                0,
                nv::PTE_KIND_GENERIC,
                false,
            );
            let Ok(bind) = bind else {
                crate::klog_warn!(
                    "[nouveau-uapi] GEM_PUSHBUF bo[{i}]: the RM refused a mapping for \
                     handle={} ({} bytes) -> EIO",
                    bo.handle,
                    size
                );
                return Err(nv::EIO);
            };
            if bind.actual_va == 0 {
                crate::klog_warn!(
                    "[nouveau-uapi] GEM_PUSHBUF bo[{i}]: the RM mapped handle={} at VA 0 \
                     -> EIO",
                    bo.handle
                );
                return Err(nv::EIO);
            }
            self.nouveau_vm_mappings.lock().push(nv::NouveauVmMapping {
                gem_handle: bo.handle,
                h_virt: bind.h_virt,
                owner_pid,
                va: bind.actual_va,
                size,
                bo_offset: 0,
                pte_kind: nv::PTE_KIND_GENERIC,
            });
            out.push(ResolvedBo {
                va: bind.actual_va,
                size,
                in_vram,
            });
        }
        Ok(out)
    }

    /// An existing whole-object mapping of `handle` owned by `owner_pid`, if
    /// there is one.
    ///
    /// Whole-object only (`bo_offset == 0` and the full size): a partial
    /// mapping left by a client that also used `VM_BIND` would give a VA that
    /// is correct for its own range and wrong for a reloc naming a byte
    /// outside it.
    fn pushbuf_existing_va(&self, handle: u32, owner_pid: u64, size: u64) -> Option<u64> {
        self.nouveau_vm_mappings
            .lock()
            .iter()
            .find(|m| {
                m.gem_handle == handle
                    && m.owner_pid == owner_pid
                    && m.bo_offset == 0
                    && m.size == size
            })
            .map(|m| m.va)
    }

    /// Patch each reloc's word into the buffer it names.
    ///
    /// The store goes through the patched buffer's own CPU mapping, so a
    /// buffer without one (a VRAM object with no BAR1 window) cannot be
    /// patched and the whole request is refused rather than submitted with
    /// one stale address in it.
    pub(super) fn pushbuf_apply_relocs(
        &self,
        relocs: &[nv::DrmNouveauGemPushbufReloc],
        bos: &[nv::DrmNouveauGemPushbufBo],
        resolved: &[ResolvedBo],
        owner_pid: u64,
    ) -> Result<(), i32> {
        for (i, r) in relocs.iter().enumerate() {
            // Both indices are the client's, and the push-side check earlier
            // only covered `bo_index` on the PUSHES.
            let (Some(target), Some(referenced)) = (
                resolved.get(r.reloc_bo_index as usize),
                resolved.get(r.bo_index as usize),
            ) else {
                crate::klog_warn!(
                    "[nouveau-uapi] GEM_PUSHBUF reloc[{i}]: index out of the buffer list \
                     (reloc_bo_index={} bo_index={} nr_buffers={}) -> EINVAL",
                    r.reloc_bo_index,
                    r.bo_index,
                    bos.len()
                );
                return Err(nv::EINVAL);
            };
            let Some(off) = pushbuf::reloc_target_offset(r.reloc_bo_offset, target.size) else {
                crate::klog_warn!(
                    "[nouveau-uapi] GEM_PUSHBUF reloc[{i}]: offset {:#x} is not a word of \
                     its buffer ({} bytes) -> EINVAL",
                    r.reloc_bo_offset,
                    target.size
                );
                return Err(nv::EINVAL);
            };
            let word = pushbuf::reloc_word(
                r.flags,
                r.data,
                r.vor,
                r.tor,
                referenced.va,
                referenced.in_vram,
            );
            let handle = bos
                .get(r.reloc_bo_index as usize)
                .map(|b| b.handle)
                .unwrap_or(0);
            self.pushbuf_store_word(handle, owner_pid, off, word, i)?;
        }
        Ok(())
    }

    /// Store one reloc word into a GEM object through its CPU mapping.
    fn pushbuf_store_word(
        &self,
        handle: u32,
        owner_pid: u64,
        off: u64,
        word: u32,
        i: usize,
    ) -> Result<(), i32> {
        let phys = {
            let gem = self.nouveau_gem.lock();
            gem.iter()
                .find(|o| o.handle == handle && super::gem_usable_by(o, owner_pid))
                .and_then(|o| o.phys_addr)
        };
        let Some(phys) = phys else {
            crate::klog_warn!(
                "[nouveau-uapi] GEM_PUSHBUF reloc[{i}]: buffer handle={handle} has no CPU \
                 mapping, so its word cannot be patched -> EINVAL"
            );
            return Err(nv::EINVAL);
        };
        let Some(at) = phys.checked_add(off) else {
            return Err(nv::EINVAL);
        };
        let vaddr = crate::bus::phys_to_virt(at as usize);
        // Volatile: the GPU reads these words, so the store must not be
        // reordered into or out of the submission that follows it.
        unsafe { core::ptr::write_volatile(vaddr as *mut u32, word) };
        Ok(())
    }

    /// Hand each push to the ring, in the order the client listed them.
    ///
    /// One `exec_submit` per push rather than one for the lot, because that
    /// is the shape of the call the ring submission already offers: it takes
    /// a single `(va, length)`. The pushes of one request land back to back
    /// on the same channel, so the engine sees the same method stream either
    /// way.
    ///
    /// A push the arithmetic refuses stops the request. It cannot be skipped:
    /// the pushes of one `GEM_PUSHBUF` are one method stream, and dropping a
    /// middle one leaves the engine reading state that was set up by a
    /// method that never ran.
    pub(super) fn pushbuf_submit_pushes(
        &self,
        device_instance: u32,
        ctx_idx: u32,
        pushes: &[nv::DrmNouveauGemPushbufPush],
        resolved: &[ResolvedBo],
    ) -> Result<(), i32> {
        for (i, p) in pushes.iter().enumerate() {
            // `bo_index` was checked against `nr_buffers` by the caller, but
            // `resolved` is what actually has an entry per buffer.
            let Some(bo) = resolved.get(p.bo_index as usize) else {
                return Err(nv::EINVAL);
            };
            let Some((va, len)) = pushbuf::push_gpu_range(bo.va, bo.size, p.offset, p.length)
            else {
                crate::klog_warn!(
                    "[nouveau-uapi] GEM_PUSHBUF push[{i}]: offset {:#x} length {:#x} is not \
                     a range of its buffer ({} bytes) -> EINVAL",
                    p.offset,
                    p.length,
                    bo.size
                );
                return Err(nv::EINVAL);
            };
            // `exec_submit` answers `Ok` whenever the call itself went
            // through; whether the push reached the ring is in the stage
            // statuses, as `submit_push_plain` reads them for EXEC. Only
            // the call's own failure was being read here, so a full ring
            // (`NV_ERR_BUSY_RETRY`) or a VA the RM could not look up was
            // reported to the client as submitted.
            match nvidia_rm_sys::rm_init::exec_submit(device_instance, ctx_idx, va, len) {
                Ok(out) if out.submit_status == 0 => log::debug!(
                    "[nouveau-uapi] GEM_PUSHBUF push[{}]: submitted va={:#x} len={:#x} \
                     (gp_put={})",
                    i,
                    va,
                    len,
                    out.gp_put_after
                ),
                Ok(out) => {
                    crate::klog_warn!(
                        "[nouveau-uapi] GEM_PUSHBUF push[{i}]: the ring did not take \
                         va={va:#x} len={len:#x} (lookup={:#x} map={:#x} token={:#x} \
                         submit={:#x}) -> EIO",
                        out.lookup_status,
                        out.map_status,
                        out.token_status,
                        out.submit_status
                    );
                    return Err(nv::EIO);
                }
                Err(status) => {
                    crate::klog_warn!(
                        "[nouveau-uapi] GEM_PUSHBUF push[{i}]: the ring refused va={va:#x} \
                         len={len:#x} (status={status:#x}) -> EIO"
                    );
                    return Err(nv::EIO);
                }
            }
        }
        Ok(())
    }
}

impl super::NvidiaGpu {
    /// A real `GEM_PUSHBUF`, once `nvidia.gem_pushbuf` has let it through and
    /// the ioctl arm has done every check `nouveau_gem_ioctl_pushbuf` does
    /// before it submits anything.
    ///
    /// The order is `nouveau_gem_ioctl_pushbuf`'s: validate (here, give every
    /// buffer a VA), apply the relocs, then push. Relocs before pushes
    /// because a reloc patches the words a push is about to execute.
    ///
    /// `presumed` is written back for every buffer, which is the half of the
    /// uAPI this path exists to serve: Mesa caches the VA it was told and
    /// sends relocs only for the buffers whose `presumed` came back wrong, so
    /// a correct write-back is what stops the next frame carrying a reloc per
    /// buffer.
    pub(super) fn gem_pushbuf_submit(
        &self,
        pb: &nv::DrmNouveauGemPushbuf,
        pushes: &[nv::DrmNouveauGemPushbufPush],
        owner_pid: u64,
    ) -> Result<(), i32> {
        let Some(device_instance) = *self.rm_device_instance.lock() else {
            crate::klog_warn!("[nouveau-uapi] GEM_PUSHBUF: the RM is not attached -> EIO");
            return Err(nv::EIO);
        };
        // The channel was confirmed to be the caller's by the ioctl arm; this
        // is the context its VAS and ring belong to.
        let ctx_idx = {
            let chans = self.nouveau_channels.lock();
            match chans.iter().find(|c| c.id == pb.channel as i32) {
                Some(c) if c.rm_backed => c.ctx_idx,
                Some(_) => {
                    crate::klog_warn!(
                        "[nouveau-uapi] GEM_PUSHBUF: channel {} is a discovery channel with \
                         no ring -> EOPNOTSUPP",
                        pb.channel
                    );
                    return Err(nv::EOPNOTSUPP);
                }
                None => return Err(nv::ENOENT),
            }
        };
        // SAFETY: the arm checked `pb.buffers` is non-null whenever
        // `nr_buffers` is non-zero, and `nr_buffers` is within
        // `NOUVEAU_GEM_MAX_BUFFERS`.
        let bos: &mut [nv::DrmNouveauGemPushbufBo] = if pb.nr_buffers == 0 {
            &mut []
        } else {
            unsafe {
                core::slice::from_raw_parts_mut(
                    pb.buffers as *mut nv::DrmNouveauGemPushbufBo,
                    pb.nr_buffers as usize,
                )
            }
        };
        let resolved = self.pushbuf_resolve_bos(device_instance, ctx_idx, owner_pid, bos)?;
        for (bo, r) in bos.iter_mut().zip(resolved.iter()) {
            // `valid` tells Mesa its guess was right. It is 1 here because
            // the VA this call reports IS where the buffer is mapped, and a
            // 0 would make the next frame send a reloc for a buffer that
            // never moved.
            bo.presumed.valid = 1;
            bo.presumed.offset = r.va;
            bo.presumed.domain = if r.in_vram {
                nv::NOUVEAU_GEM_DOMAIN_VRAM
            } else {
                nv::NOUVEAU_GEM_DOMAIN_GART
            };
        }
        if pb.nr_relocs > 0 {
            if pb.relocs == 0 {
                return Err(nv::EFAULT);
            }
            // SAFETY: as for `buffers`, with `NOUVEAU_GEM_MAX_RELOCS`.
            let relocs = unsafe {
                core::slice::from_raw_parts(
                    pb.relocs as *const nv::DrmNouveauGemPushbufReloc,
                    pb.nr_relocs as usize,
                )
            };
            self.pushbuf_apply_relocs(relocs, bos, &resolved, owner_pid)?;
        }
        self.pushbuf_submit_pushes(device_instance, ctx_idx, pushes, &resolved)
    }
}
