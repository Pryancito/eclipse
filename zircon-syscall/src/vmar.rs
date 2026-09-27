use {super::*, bitflags::bitflags, zircon_object::vm::*};

/// Where `ZX_VM_ALIGN_*` lives in `options`: a power of two, not a flag bit.
const ALIGN_FIELD: u32 = 0xFF00_0000;

fn amount_of_alignments(options: u32) -> ZxResult<usize> {
    let mut align_pow2 = (options >> 24) as usize;
    if align_pow2 == 0 {
        align_pow2 = PAGE_SIZE_LOG2;
    }
    if !(PAGE_SIZE_LOG2..=32).contains(&align_pow2) {
        Err(ZxError::INVALID_ARGS)
    } else {
        Ok(1 << align_pow2)
    }
}

/// The flags and the alignment a `zx_vmar_allocate` asked for.
///
/// `ZX_VM_ALIGN_*` is `(align_pow2 << 24)`, so it sits outside the flag bits
/// and the flags have to be read from what is left. `VmOptions::from_bits`
/// refuses any bit it does not know, and it used to be handed the whole word:
/// **every `ZX_VM_ALIGN_*` request answered `INVALID_ARGS`**, and
/// `amount_of_alignments` -- which exists to read exactly those bits -- could
/// never return anything but `PAGE_SIZE`. An unknown bit among the flags is
/// still refused.
fn vmar_options(options: u32) -> ZxResult<(VmOptions, usize)> {
    let flags = VmOptions::from_bits(options & !ALIGN_FIELD).ok_or(ZxError::INVALID_ARGS)?;
    Ok((flags, amount_of_alignments(options)?))
}

impl Syscall<'_> {
    /// Allocate a new subregion.
    ///
    /// Creates a new VMAR within the one specified by `parent_vmar`.
    pub fn sys_vmar_allocate(
        &self,
        parent_vmar: HandleValue,
        options: u32,
        offset: u64,
        size: u64,
        mut out_child_vmar: UserOutPtr<HandleValue>,
        mut out_child_addr: UserOutPtr<usize>,
    ) -> ZxResult {
        let (vm_options, align) = vmar_options(options)?;
        info!(
            "vmar.allocate: parent={:#x?}, options={:#x?}, offset={:#x?}, size={:#x?}",
            parent_vmar, options, offset, size,
        );
        // try to get parent_vmar
        let perm_rights = vm_options.to_rights();
        let proc = self.thread.proc();
        let parent = proc.get_object_with_rights::<VmAddressRegion>(parent_vmar, perm_rights)?;

        if vm_options.intersects(VmOptions::PERM_RXW | VmOptions::MAP_RANGE) {
            return Err(ZxError::INVALID_ARGS);
        }
        // get vmar_flags
        let vmar_flags = vm_options.to_flags();
        if vmar_flags.intersects(
            !(VmarFlags::SPECIFIC
                | VmarFlags::CAN_MAP_SPECIFIC
                | VmarFlags::COMPACT
                | VmarFlags::CAN_MAP_RXW),
        ) {
            return Err(ZxError::INVALID_ARGS);
        }

        // get offest with options
        let offset = if vm_options.contains(VmOptions::SPECIFIC) {
            Some(offset as usize)
        } else if vm_options.contains(VmOptions::SPECIFIC_OVERWRITE) {
            unimplemented!()
        } else {
            if offset != 0 {
                return Err(ZxError::INVALID_ARGS);
            }
            None
        };

        let size = roundup_pages(size as usize);
        // check `size`
        if size == 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        // Both out pointers are looked at before the region exists: a bad
        // one used to leave the child allocated in the parent and its handle
        // in the table, with nothing to tell the caller.
        check_out(proc, &out_child_vmar)?;
        check_out(proc, &out_child_addr)?;
        let child = parent.allocate(offset, size, vmar_flags, align)?;
        let child_addr = child.addr();
        info!("vmar.allocate: at {:#x?}", child_addr);
        install_handle(
            proc,
            Handle::new(child, Rights::DEFAULT_VMAR | perm_rights),
            &mut out_child_vmar,
        )?;
        out_child_addr.write(child_addr)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    /// Add a memory mapping.
    ///
    /// Maps the given VMO into the given virtual memory address region.
    pub fn sys_vmar_map(
        &self,
        vmar_handle: HandleValue,
        options: u32,
        vmar_offset: usize,
        vmo_handle: HandleValue,
        vmo_offset: usize,
        len: usize,
        mut mapped_addr: UserOutPtr<VirtAddr>,
    ) -> ZxResult {
        info!(
            "vmar.map: vmar_handle={:#x?}, options={:#x?}, vmar_offset={:#x?}, vmo_handle={:#x?}, vmo_offset={:#x?}, len={:#x?}",
            vmar_handle, options, vmar_offset, vmo_handle, vmo_offset, len
        );
        let options = VmOptions::from_bits(options).ok_or(ZxError::INVALID_ARGS)?;
        let proc = self.thread.proc();
        let (vmar, vmar_rights) = proc.get_object_and_rights::<VmAddressRegion>(vmar_handle)?;
        let (vmo, vmo_rights) = proc.get_object_and_rights::<VmObject>(vmo_handle)?;
        if !vmo_rights.contains(Rights::MAP) {
            return Err(ZxError::ACCESS_DENIED);
        };
        if options
            .intersects(VmOptions::CAN_MAP_RXW | VmOptions::CAN_MAP_SPECIFIC | VmOptions::COMPACT)
        {
            return Err(ZxError::INVALID_ARGS);
        }
        if options.contains(VmOptions::REQUIRE_NON_RESIZABLE) && vmo.is_resizable() {
            return Err(ZxError::NOT_SUPPORTED);
        }
        // check SPECIFIC options with offset
        let is_specific = options.contains(VmOptions::SPECIFIC)
            || options.contains(VmOptions::SPECIFIC_OVERWRITE);
        if !is_specific && vmar_offset != 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        if !vmar_rights.contains(options.to_required_rights()) {
            return Err(ZxError::ACCESS_DENIED);
        }
        let mut permissions = MMUFlags::empty();
        permissions.set(MMUFlags::READ, vmo_rights.contains(Rights::READ));
        permissions.set(MMUFlags::WRITE, vmo_rights.contains(Rights::WRITE));
        permissions.set(MMUFlags::EXECUTE, vmo_rights.contains(Rights::EXECUTE));
        let mut mapping_flags = MMUFlags::USER;
        mapping_flags.set(
            MMUFlags::READ,
            options.intersects(VmOptions::PERM_READ | VmOptions::PERM_READ_IF_XOM_UNSUPPORTED),
        );
        mapping_flags.set(MMUFlags::WRITE, options.contains(VmOptions::PERM_WRITE));
        mapping_flags.set(MMUFlags::EXECUTE, options.contains(VmOptions::PERM_EXECUTE));
        let overwrite = options.contains(VmOptions::SPECIFIC_OVERWRITE);
        let map_range = if cfg!(any(feature = "deny-page-fault", not(target_os = "none"))) {
            // Hosted mode cannot service guest page faults, so accessible
            // mappings must be populated eagerly.  A permissionless mapping,
            // however, is only an address-space reservation (modern userboot
            // creates a multi-gigabyte one with ZX_VM_ALLOW_FAULTS).  Committing
            // it would incorrectly consume physical memory and exhaust the
            // LibOS backing file.
            mapping_flags.intersects(MMUFlags::RXW)
        } else {
            options.contains(VmOptions::MAP_RANGE)
        };

        info!(
            "mmuflags: {:?}, is_specific {:?}, overwrite {:?}, map_range {:?}",
            mapping_flags, is_specific, overwrite, map_range
        );
        // ZX_VM_MAP_RANGE and ZX_VM_SPECIFIC_OVERWRITE are mutually
        // exclusive. `map_range` may also be enabled internally in hosted
        // mode to populate accessible mappings, which must not make an
        // otherwise valid overwrite request fail validation.
        if options.contains(VmOptions::MAP_RANGE) && overwrite {
            return Err(ZxError::INVALID_ARGS);
        }
        // Note: we should reject non-page-aligned length here,
        // but since zCore use different memory layout from zircon,
        // we should not reject them and round up them instead
        // TODO: reject non-page-aligned length after we have the same memory layout with zircon
        let len = roundup_pages(len);
        if len == 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        let vmar_offset = if is_specific { Some(vmar_offset) } else { None };
        let vaddr = vmar.map_ext(
            vmar_offset,
            vmo,
            vmo_offset,
            len,
            permissions,
            mapping_flags,
            overwrite,
            map_range,
            options.contains(VmOptions::ALLOW_FAULTS),
        )?;
        info!("vmar.map: at {:#x?}", vaddr);
        mapped_addr.write(vaddr)?;
        Ok(())
    }

    pub fn sys_vmar_map_clock(
        &self,
        vmar_handle: HandleValue,
        options: u32,
        vmar_offset: usize,
        clock_handle: HandleValue,
        len: usize,
        mut mapped_addr: UserOutPtr<VirtAddr>,
    ) -> ZxResult {
        const DISALLOWED_OPTIONS: u32 = (1 << 1) | (1 << 2) | (1 << 14) | (1 << 15);
        if options & DISALLOWED_OPTIONS != 0 || len != PAGE_SIZE {
            return Err(ZxError::INVALID_ARGS);
        }
        let options = VmOptions::from_bits(options).ok_or(ZxError::INVALID_ARGS)?;
        let proc = self.thread.proc();
        let (vmar, vmar_rights) = proc.get_object_and_rights::<VmAddressRegion>(vmar_handle)?;
        let (clock, clock_rights) = proc.get_object_and_rights::<Clock>(clock_handle)?;
        if !clock_rights.contains(Rights::READ | Rights::MAP) {
            return Err(ZxError::ACCESS_DENIED);
        }
        let vmo = clock.mapped_vmo().ok_or(ZxError::INVALID_ARGS)?;
        if !vmar_rights.contains(options.to_required_rights()) {
            return Err(ZxError::ACCESS_DENIED);
        }
        let is_specific = options.contains(VmOptions::SPECIFIC)
            || options.contains(VmOptions::SPECIFIC_OVERWRITE);
        if !is_specific && vmar_offset != 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        let mut mapping_flags = MMUFlags::USER;
        mapping_flags.set(MMUFlags::READ, options.contains(VmOptions::PERM_READ));
        let vaddr = vmar.map_ext(
            is_specific.then_some(vmar_offset),
            vmo,
            0,
            PAGE_SIZE,
            MMUFlags::READ,
            mapping_flags,
            options.contains(VmOptions::SPECIFIC_OVERWRITE),
            options.contains(VmOptions::MAP_RANGE),
            options.contains(VmOptions::ALLOW_FAULTS),
        )?;
        mapped_addr.write(vaddr)?;
        Ok(())
    }

    /// Destroy a virtual memory address region.
    ///
    /// Unmaps all mappings within the given region, and destroys all sub-regions of the region.
    /// > This operation is logically recursive.
    pub fn sys_vmar_destroy(&self, handle_value: HandleValue) -> ZxResult {
        info!("vmar.destroy: handle={:#x?}", handle_value);
        let proc = self.thread.proc();
        let vmar = proc.get_object::<VmAddressRegion>(handle_value)?;
        vmar.destroy()?;
        Ok(())
    }

    /// Set protection of virtual memory pages.
    pub fn sys_vmar_protect(
        &self,
        handle_value: HandleValue,
        options: u32,
        addr: u64,
        len: u64,
    ) -> ZxResult {
        let options = VmOptions::from_bits(options).ok_or(ZxError::INVALID_ARGS)?;
        let rights = options.to_required_rights();
        info!(
            "vmar.protect: handle={:#x}, options={:#x}, addr={:#x}, len={:#x}",
            handle_value, options, addr, len
        );
        let proc = self.thread.proc();
        let vmar = proc.get_object_with_rights::<VmAddressRegion>(handle_value, rights)?;
        if options.intersects(!VmOptions::PERM_RXW) {
            return Err(ZxError::INVALID_ARGS);
        }
        let mut mapping_flags = MMUFlags::empty();
        mapping_flags.set(MMUFlags::READ, options.contains(VmOptions::PERM_READ));
        mapping_flags.set(MMUFlags::WRITE, options.contains(VmOptions::PERM_WRITE));
        mapping_flags.set(MMUFlags::EXECUTE, options.contains(VmOptions::PERM_EXECUTE));
        info!("mmuflags: {:?}", mapping_flags);
        let len = roundup_pages(len as usize);
        if len == 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        vmar.protect(addr as usize, len, mapping_flags)?;
        Ok(())
    }

    /// Unmap virtual memory pages.
    pub fn sys_vmar_unmap(&self, handle_value: HandleValue, addr: usize, len: usize) -> ZxResult {
        info!(
            "vmar.unmap: handle_value={:#x}, addr={:#x}, len={:#x}",
            handle_value, addr, len
        );
        let proc = self.thread.proc();
        let vmar = proc.get_object::<VmAddressRegion>(handle_value)?;
        vmar.unmap(addr, pages(len) * PAGE_SIZE)?;
        Ok(())
    }
}

bitflags! {
    struct VmOptions: u32 {
        #[allow(clippy::identity_op)]
        const PERM_READ             = 1 << 0;
        const PERM_WRITE            = 1 << 1;
        const PERM_EXECUTE          = 1 << 2;
        const COMPACT               = 1 << 3;
        const SPECIFIC              = 1 << 4;
        const SPECIFIC_OVERWRITE    = 1 << 5;
        const CAN_MAP_SPECIFIC      = 1 << 6;
        const CAN_MAP_READ          = 1 << 7;
        const CAN_MAP_WRITE         = 1 << 8;
        const CAN_MAP_EXECUTE       = 1 << 9;
        const MAP_RANGE             = 1 << 10;
        const REQUIRE_NON_RESIZABLE = 1 << 11;
        const ALLOW_FAULTS          = 1 << 12;
        const PERM_READ_IF_XOM_UNSUPPORTED = 1 << 14;
        const CAN_MAP_RXW           = Self::CAN_MAP_READ.bits | Self::CAN_MAP_EXECUTE.bits | Self::CAN_MAP_WRITE.bits;
        const PERM_RXW           = Self::PERM_READ.bits | Self::PERM_WRITE.bits | Self::PERM_EXECUTE.bits;
    }
}

impl VmOptions {
    fn to_rights(self) -> Rights {
        let mut rights = Rights::empty();
        if self.contains(VmOptions::CAN_MAP_READ) {
            rights.insert(Rights::READ);
        }
        if self.contains(VmOptions::CAN_MAP_WRITE) {
            rights.insert(Rights::WRITE);
        }
        if self.contains(VmOptions::CAN_MAP_EXECUTE) {
            rights.insert(Rights::EXECUTE);
        }
        rights
    }

    fn to_required_rights(self) -> Rights {
        let mut rights = Rights::empty();
        if self.intersects(VmOptions::PERM_READ | VmOptions::PERM_READ_IF_XOM_UNSUPPORTED) {
            rights.insert(Rights::READ);
        }
        if self.contains(VmOptions::PERM_WRITE) {
            rights.insert(Rights::WRITE);
        }
        if self.contains(VmOptions::PERM_EXECUTE) {
            rights.insert(Rights::EXECUTE);
        }
        rights
    }

    fn to_flags(self) -> VmarFlags {
        let mut flags = VmarFlags::empty();
        if self.contains(VmOptions::COMPACT) {
            flags.insert(VmarFlags::COMPACT);
        }
        if self.contains(VmOptions::SPECIFIC) {
            flags.insert(VmarFlags::SPECIFIC);
        }
        if self.contains(VmOptions::SPECIFIC_OVERWRITE) {
            flags.insert(VmarFlags::SPECIFIC_OVERWRITE);
        }
        if self.contains(VmOptions::CAN_MAP_SPECIFIC) {
            flags.insert(VmarFlags::CAN_MAP_SPECIFIC);
        }
        if self.contains(VmOptions::CAN_MAP_READ) {
            flags.insert(VmarFlags::CAN_MAP_READ);
        }
        if self.contains(VmOptions::CAN_MAP_WRITE) {
            flags.insert(VmarFlags::CAN_MAP_WRITE);
        }
        if self.contains(VmOptions::CAN_MAP_EXECUTE) {
            flags.insert(VmarFlags::CAN_MAP_EXECUTE);
        }
        if self.contains(VmOptions::REQUIRE_NON_RESIZABLE) {
            flags.insert(VmarFlags::REQUIRE_NON_RESIZABLE);
        }
        if self.contains(VmOptions::ALLOW_FAULTS) {
            flags.insert(VmarFlags::ALLOW_FAULTS);
        }
        flags
    }
}

#[cfg(test)]
mod vmar_options_tests {
    //! `ZX_VM_ALIGN_*` rides in bits 24.. of `options`, outside the flag bits.
    //! The whole word used to go to `VmOptions::from_bits`, which refuses any
    //! bit it does not know, so every alignment request was `INVALID_ARGS` and
    //! `amount_of_alignments` could only ever answer `PAGE_SIZE`.

    use super::*;

    /// `ZX_VM_ALIGN_<n>` as Zircon writes it.
    fn align_option(align_pow2: u32) -> u32 {
        align_pow2 << 24
    }

    #[test]
    fn an_alignment_request_is_read_and_not_refused() {
        let (flags, align) =
            vmar_options(align_option(20) | VmOptions::CAN_MAP_READ.bits()).unwrap();
        assert_eq!(align, 1 << 20, "ZX_VM_ALIGN_1MB");
        assert!(
            flags.contains(VmOptions::CAN_MAP_READ),
            "the flags come from the bits that are not the alignment"
        );
        assert_eq!(
            flags.bits() & ALIGN_FIELD,
            0,
            "no alignment bit is left among the flags"
        );
    }

    #[test]
    fn the_largest_alignment_is_accepted() {
        assert_eq!(
            vmar_options(align_option(32)),
            Ok((VmOptions::empty(), 1 << 32))
        );
    }

    #[test]
    fn no_alignment_asked_for_is_a_page() {
        let (flags, align) = vmar_options(VmOptions::SPECIFIC.bits()).unwrap();
        assert_eq!(align, PAGE_SIZE);
        assert_eq!(flags, VmOptions::SPECIFIC);
    }

    #[test]
    fn an_alignment_smaller_than_a_page_or_larger_than_the_largest_is_refused() {
        assert_eq!(vmar_options(align_option(10)), Err(ZxError::INVALID_ARGS));
        assert_eq!(
            vmar_options(align_option((PAGE_SIZE_LOG2 - 1) as u32)),
            Err(ZxError::INVALID_ARGS)
        );
        assert_eq!(vmar_options(align_option(33)), Err(ZxError::INVALID_ARGS));
    }

    #[test]
    fn an_unknown_flag_bit_is_still_refused() {
        assert_eq!(vmar_options(1 << 13), Err(ZxError::INVALID_ARGS));
        assert_eq!(
            vmar_options(align_option(20) | (1 << 13)),
            Err(ZxError::INVALID_ARGS)
        );
    }
}
