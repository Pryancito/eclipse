use {
    super::*,
    bitflags::bitflags,
    kernel_hal::{CachePolicy, MMUFlags},
    numeric_enum_macro::numeric_enum,
    zircon_object::{dev::*, task::PolicyCondition, vm::*},
};

/// The kernel-side buffer one `zx_vmo_read` copies through at a time.
///
/// A read is served whole however long it is; only the staging buffer is
/// bounded, so no caller can pick the size of a kernel allocation.
const VMO_READ_CHUNK: usize = 64 * 1024;

impl Syscall<'_> {
    /// Create a new virtual memory object(VMO).
    pub fn sys_vmo_create(
        &self,
        size: u64,
        options: u32,
        mut out: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        info!(
            "vmo.create: size={:#x?}, options={:#x?}, out={:#x?}",
            size, options, out
        );
        const RESIZABLE: u32 = 1 << 1;
        const UNBOUNDED: u32 = 1 << 4;
        if options & !(RESIZABLE | UNBOUNDED) != 0 || options == (RESIZABLE | UNBOUNDED) {
            return Err(ZxError::INVALID_ARGS);
        }
        let resizable = options & RESIZABLE != 0;
        let unbounded = options & UNBOUNDED != 0;
        let proc = self.thread.proc();
        let vmo = VmObject::new_paged_with_options(resizable, unbounded, size as usize)?;
        install_handle(proc, Handle::new(vmo, Rights::DEFAULT_VMO), &mut out)
    }

    /// Read bytes from a VMO.
    pub fn sys_vmo_read(
        &self,
        handle_value: HandleValue,
        buf: UserOutPtr<u8>,
        offset: u64,
        buf_size: usize,
    ) -> ZxResult {
        info!(
            "vmo.read: handle={:#x?}, offset={:#x?}, buf=({:#x?}; {:#x?})",
            handle_value, offset, buf, buf_size,
        );
        let proc = self.thread.proc();
        let vmo = proc.get_object_with_rights::<VmObject>(handle_value, Rights::READ)?;
        // in case integer addition overflows
        if offset as usize > vmo.len() || buf_size > vmo.len() - (offset as usize) {
            return Err(ZxError::OUT_OF_RANGE);
        }
        proc.vmar()
            .check_user_range(buf.as_addr(), buf_size, MMUFlags::WRITE)?;
        // Through a bounded kernel buffer, a chunk at a time. `vec![0u8;
        // buf_size]` was an allocation of whatever the caller asked for: a VMO
        // may be gigabytes long, and a request the heap cannot serve is not an
        // error here but a kernel panic. The same reason `process_read_memory`
        // reads in chunks.
        // `base + done` stays inside the object: the check above establishes
        // `base + buf_size <= vmo.len()`, and `done` never reaches `buf_size`.
        let base = offset as usize;
        let mut chunk = vec![0u8; buf_size.min(VMO_READ_CHUNK)];
        for (done, want) in read_chunks(buf_size, VMO_READ_CHUNK) {
            vmo.read(base + done, &mut chunk[..want])?;
            buf.add(done).write_array(&chunk[..want])?;
        }
        Ok(())
    }

    /// Write bytes to a VMO.
    pub fn sys_vmo_write(
        &self,
        handle_value: HandleValue,
        buf: UserInPtr<u8>,
        offset: u64,
        buf_size: usize,
    ) -> ZxResult {
        info!(
            "vmo.write: handle={:#x?}, offset={:#x?}, buf=({:#x?}; {:#x?})",
            handle_value, offset, buf, buf_size,
        );
        let proc = self.thread.proc();
        let vmo = proc.get_object_with_rights::<VmObject>(handle_value, Rights::WRITE)?;
        let end = (offset as usize)
            .checked_add(buf_size)
            .ok_or(ZxError::OUT_OF_RANGE)?;
        // The caller's buffer is checked BEFORE the object is grown. An
        // unbounded or resizable VMO grows to fit a write, and this used to
        // grow it first: a `zx_vmo_write` naming a buffer the caller does not
        // have answered the error with the object already bigger, which
        // `zx_vmo_get_size` reports and a `zx_vmo_read` can then read as
        // zeroes. No test: there is no harness in this crate that can call a
        // syscall with a live process, and the order is the whole of it.
        proc.vmar()
            .check_user_range(buf.as_addr(), buf_size, MMUFlags::READ)?;
        vmo.grow_for_write(end)?;
        if offset as usize > vmo.len() || buf_size > vmo.len() - (offset as usize) {
            return Err(ZxError::OUT_OF_RANGE);
        }
        vmo.write(offset as usize, buf.as_slice(buf_size)?)
    }

    /// Add execute rights to a VMO.
    pub fn sys_vmo_replace_as_executable(
        &self,
        handle: HandleValue,
        vmex: HandleValue,
        mut out: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        info!(
            "vmo.replace_as_executable: handle={:#x?}, vmex={:#x?}",
            handle, vmex
        );
        let proc = self.thread.proc();
        if vmex != INVALID_HANDLE {
            proc.get_object::<Resource>(vmex)?
                .validate_system(SystemResource::Vmex)?;
        } else {
            proc.check_policy(PolicyCondition::AmbientMarkVMOExec)?;
        }
        let _ = proc.get_object_and_rights::<VmObject>(handle)?;
        // The old handle goes away below, so a bad `out` used to leave the
        // caller with no handle to its VMO at all.
        check_out(proc, &out)?;
        let new_handle = proc.dup_handle_operating_rights(handle, |handle_rights| {
            Ok(handle_rights | Rights::EXECUTE)
        })?;
        proc.remove_handle(handle)?;
        install_handle_value(proc, new_handle, &mut out)
    }

    /// Obtain the current size of a VMO object.
    pub fn sys_vmo_get_size(&self, handle: HandleValue, mut size: UserOutPtr<usize>) -> ZxResult {
        info!("vmo.get_size: handle={:?}", handle);
        let proc = self.thread.proc();
        let vmo = proc.get_object::<VmObject>(handle)?;
        size.write(vmo.len())?;
        Ok(())
    }

    /// Obtain the logical stream size associated with a VMO.
    pub fn sys_vmo_get_stream_size(
        &self,
        handle: HandleValue,
        mut size: UserOutPtr<usize>,
    ) -> ZxResult {
        let proc = self.thread.proc();
        let vmo = proc.get_object::<VmObject>(handle)?;
        size.write(vmo.content_size())?;
        Ok(())
    }

    /// Set the logical stream size associated with a VMO.
    pub fn sys_vmo_set_stream_size(&self, handle: HandleValue, size: usize) -> ZxResult {
        let proc = self.thread.proc();
        let vmo = proc.get_object_with_rights::<VmObject>(handle, Rights::WRITE)?;
        vmo.set_stream_size(size)
    }

    /// Create a child of an existing VMO (new virtual memory object).
    pub fn sys_vmo_create_child(
        &self,
        handle_value: HandleValue,
        options: u32,
        offset: usize,
        size: usize,
        mut out: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        let mut options = VmoCloneFlags::from_bits(options).ok_or(ZxError::INVALID_ARGS)?;
        info!(
            "vmo_create_child: handle={:#x}, options={:?}, offset={:#x}, size={:#x}",
            handle_value, options, offset, size
        );
        // check options given
        let no_write = options.contains(VmoCloneFlags::NO_WRITE);
        if no_write {
            options.remove(VmoCloneFlags::NO_WRITE);
        }

        let resizable = options.contains(VmoCloneFlags::RESIZABLE);
        let child_size = roundup_pages(size);
        if child_size < size {
            return Err(ZxError::OUT_OF_RANGE);
        }
        info!("size of child vmo: {:#x}", child_size);

        let proc = self.thread.proc();
        let (vmo, parent_rights) = proc.get_object_and_rights::<VmObject>(handle_value)?;
        if !parent_rights.contains(Rights::DUPLICATE | Rights::READ) {
            return Err(ZxError::ACCESS_DENIED);
        }
        let child_vmo = if options.contains(VmoCloneFlags::SLICE) {
            if options != VmoCloneFlags::SLICE {
                Err(ZxError::INVALID_ARGS)
            } else {
                vmo.create_slice(offset, child_size)
            }
        } else {
            if !options
                .intersects(VmoCloneFlags::SNAPSHOT | VmoCloneFlags::SNAPSHOT_AT_LEAST_ON_WRITE)
            {
                return Err(ZxError::NOT_SUPPORTED);
            }
            vmo.create_child(resizable, offset, child_size)
        }?;
        // generate rights
        let mut child_rights = parent_rights;
        child_rights.insert(Rights::GET_PROPERTY | Rights::SET_PROPERTY);
        if no_write {
            child_rights.remove(Rights::WRITE);
        } else if options.contains(VmoCloneFlags::SNAPSHOT)
            || options.contains(VmoCloneFlags::SNAPSHOT_AT_LEAST_ON_WRITE)
        {
            child_rights.remove(Rights::EXECUTE);
            child_rights.insert(Rights::WRITE);
        };
        info!(
            "parent_rights: {:?} child_rights: {:?}",
            parent_rights, child_rights
        );
        install_handle(proc, Handle::new(child_vmo, child_rights), &mut out)
    }

    /// Create a VM object referring to a specific contiguous range of physical memory.
    pub fn sys_vmo_create_physical(
        &self,
        resource: HandleValue,
        paddr: PhysAddr,
        size: usize,
        mut out: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        info!(
            "vmo.create_physical: handle={:#x?}, paddr={:#x?}, size={:#x}, out={:#x?}",
            resource, paddr, size, out
        );
        let proc = self.thread.proc();
        proc.check_policy(PolicyCondition::NewVMO)?;
        proc.get_object::<Resource>(resource)?
            .validate_ranged_resource(ResourceKind::MMIO, paddr, size)?;
        let size = roundup_pages(size);
        if size == 0 || !page_aligned(paddr) {
            return Err(ZxError::INVALID_ARGS);
        }
        if paddr.overflowing_add(size).1 {
            return Err(ZxError::INVALID_ARGS);
        }
        let vmo = VmObject::new_physical(paddr, size / PAGE_SIZE);
        install_handle(
            proc,
            Handle::new(vmo, Rights::DEFAULT_VMO | Rights::EXECUTE),
            &mut out,
        )
    }

    /// Create a VM object referring to a specific contiguous range of physical frame.
    pub fn sys_vmo_create_contiguous(
        &self,
        bti: HandleValue,
        size: usize,
        align_log2: u32,
        mut out: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        info!(
            "vmo.create_contiguous: handle={:#x?}, size={:#x?}, align={}, out={:#x?}",
            bti, size, align_log2, out
        );
        if size == 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        let align_log2 = if align_log2 == 0 {
            PAGE_SIZE_LOG2
        } else {
            align_log2 as usize
        };
        if align_log2 < PAGE_SIZE_LOG2 || align_log2 >= 8 * core::mem::size_of::<usize>() {
            return Err(ZxError::INVALID_ARGS);
        }
        let proc = self.thread.proc();
        proc.check_policy(PolicyCondition::NewVMO)?;
        let _bti = proc.get_object_with_rights::<BusTransactionInitiator>(bti, Rights::MAP)?;
        let vmo = VmObject::new_contiguous(pages(size), align_log2)?;
        install_handle(proc, Handle::new(vmo, Rights::DEFAULT_VMO), &mut out)
    }

    /// Resize a VMO object.
    pub fn sys_vmo_set_size(&self, handle_value: HandleValue, size: usize) -> ZxResult {
        let proc = self.thread.proc();
        let vmo = proc.get_object_with_rights::<VmObject>(handle_value, Rights::WRITE)?;
        info!(
            "vmo.set_size: handle={:#x}, size={:#x}, current_size={:#x}",
            handle_value,
            size,
            vmo.len(),
        );
        vmo.set_len(size)
    }

    /// Perform an operation on a range of a VMO.
    ///
    /// Performs cache and memory operations against pages held by the VMO.
    pub fn sys_vmo_op_range(
        &self,
        handle_value: HandleValue,
        op: u32,
        offset: usize,
        len: usize,
        _buffer: UserOutPtr<u8>,
        _buffer_size: usize,
    ) -> ZxResult {
        info!(
            "vmo.op_range: handle={:#x}, op={:#X}, offset={:#x}, len={:#x}, buffer_size={:#x}",
            handle_value, op, offset, len, _buffer_size,
        );
        let op = VmoOpType::try_from(op).or(Err(ZxError::INVALID_ARGS))?;
        let proc = self.thread.proc();
        let (vmo, rights) = proc.get_object_and_rights::<VmObject>(handle_value)?;
        match op {
            VmoOpType::Commit => {
                if !rights.contains(Rights::WRITE) {
                    return Err(ZxError::ACCESS_DENIED);
                }
                if !page_aligned(offset) || !page_aligned(len) {
                    return Err(ZxError::INVALID_ARGS);
                }
                vmo.commit(offset, len)?;
                Ok(())
            }
            VmoOpType::Decommit => {
                if !rights.contains(Rights::WRITE) {
                    return Err(ZxError::ACCESS_DENIED);
                }
                if !page_aligned(offset) || !page_aligned(len) {
                    return Err(ZxError::INVALID_ARGS);
                }
                vmo.decommit(offset, len)
            }
            VmoOpType::Zero => {
                if !rights.contains(Rights::WRITE) {
                    return Err(ZxError::ACCESS_DENIED);
                }
                vmo.zero(offset, len)
            }
            // The caches these keep coherent are coherent on their own here,
            // so the range is checked and nothing is done. Any of them used
            // to be `unimplemented!()`: a kernel panic a driver could ask for
            // with `ZX_VMO_OP_CACHE_SYNC` after a DMA transfer.
            VmoOpType::CacheSync | VmoOpType::CacheClean | VmoOpType::CacheCleanInvalidate => {
                if !rights.contains(Rights::READ) {
                    return Err(ZxError::ACCESS_DENIED);
                }
                vmo_range_check(&vmo, offset, len)
            }
            VmoOpType::CacheInvalidate => {
                if !rights.contains(Rights::WRITE) {
                    return Err(ZxError::ACCESS_DENIED);
                }
                vmo_range_check(&vmo, offset, len)
            }
            // Locking is for discardable objects, which do not exist here.
            VmoOpType::Lock | VmoOpType::Unlock => Err(ZxError::NOT_SUPPORTED),
        }
    }

    /// Set the caching policy for pages held by a VMO.
    pub fn sys_vmo_cache_policy(&self, handle_value: HandleValue, policy: u32) -> ZxResult {
        let proc = self.thread.proc();
        let vmo = proc.get_object_with_rights::<VmObject>(handle_value, Rights::MAP)?;
        let policy = CachePolicy::try_from(policy).or(Err(ZxError::INVALID_ARGS))?;
        (*vmo).set_cache_policy(policy)
    }
}

/// `[offset, offset + len)` lies inside the object, or `OUT_OF_RANGE`.
fn vmo_range_check(vmo: &VmObject, offset: usize, len: usize) -> ZxResult {
    match offset.checked_add(len) {
        Some(end) if end <= vmo.len() => Ok(()),
        _ => Err(ZxError::OUT_OF_RANGE),
    }
}

bitflags! {
    struct VmoCloneFlags: u32 {
        #[allow(clippy::identity_op)]
        const SNAPSHOT                   = 1 << 0;
        const RESIZABLE                  = 1 << 2;
        const SLICE                      = 1 << 3;
        const SNAPSHOT_AT_LEAST_ON_WRITE = 1 << 4;
        const NO_WRITE                   = 1 << 5;
    }
}

numeric_enum! {
    #[repr(u32)]
    /// VMO Opcodes (for vmo_op_range)
    pub enum VmoOpType {
        Commit = 1,
        Decommit = 2,
        Lock = 3,
        Unlock = 4,
        CacheSync = 6,
        CacheInvalidate = 7,
        CacheClean = 8,
        CacheCleanInvalidate = 9,
        Zero = 10,
    }
}

/// The pieces one staged read is copied through: an offset into the caller's
/// buffer and a length, together covering `buf_size` once, in order, with no
/// piece longer than `chunk`.
///
/// The whole read is still served; only the kernel buffer behind it is bounded.
fn read_chunks(buf_size: usize, chunk: usize) -> impl Iterator<Item = (usize, usize)> {
    let chunk = chunk.max(1);
    (0..buf_size)
        .step_by(chunk)
        .map(move |done| (done, (buf_size - done).min(chunk)))
}

#[cfg(test)]
mod read_chunk_tests {
    use super::*;
    use alloc::vec::Vec;

    /// Every byte of the read is copied exactly once, in order: the caller asked
    /// for the whole of it and gets the whole of it, however small the kernel
    /// buffer in between.
    #[test]
    fn the_chunks_cover_the_read_once_and_in_order() {
        for &buf_size in &[0usize, 1, 7, 8, 9, 64, 4096, 4097, 1 << 20] {
            for &chunk in &[1usize, 8, 4096, 1 << 16] {
                let pieces: Vec<_> = read_chunks(buf_size, chunk).collect();
                let mut at = 0;
                for &(done, want) in &pieces {
                    assert_eq!(done, at, "a gap or an overlap at {}", at);
                    assert!(want > 0, "an empty piece at {}", at);
                    assert!(want <= chunk, "a piece of {} over the {} cap", want, chunk);
                    at += want;
                }
                assert_eq!(at, buf_size, "{} bytes were covered of {}", at, buf_size);
            }
        }
    }

    /// A read of nothing copies nothing rather than one empty piece.
    #[test]
    fn a_read_of_no_bytes_has_no_chunks() {
        assert_eq!(read_chunks(0, 4096).count(), 0);
        assert_eq!(read_chunks(1, 4096).count(), 1);
    }
}
