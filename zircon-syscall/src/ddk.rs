use {
    super::*,
    bitflags::bitflags,
    kernel_hal::DevVAddr,
    zircon_object::{dev::*, signal::*, task::*, vm::*},
};

/// The options `zx_bti_pin` were given, or `INVALID_ARGS`.
///
/// This used to be `from_bits_truncate`, which drops every bit it does not
/// know instead of refusing it. A caller that asked for something this kernel
/// has no notion of -- a permission bit from a newer ABI, a typo in a
/// `#define` -- got a pin with whatever was left of its request and was told
/// it succeeded.
fn bti_options(options: u32) -> ZxResult<BtiOptions> {
    BtiOptions::from_bits(options).ok_or(ZxError::INVALID_ARGS)
}

/// The options `zx_interrupt_create` were given, or `INVALID_ARGS`.
///
/// Same as [`bti_options`]: an unknown bit was dropped rather than refused.
/// The `options != VIRTUAL` check below could not see one either, because
/// truncation had already removed it.
fn interrupt_options(options: u32) -> ZxResult<InterruptOptions> {
    InterruptOptions::from_bits(options).ok_or(ZxError::INVALID_ARGS)
}

impl Syscall<'_> {
    /// Create a new object in the kernel representing an IOMMU device.
    pub fn sys_iommu_create(
        &self,
        resource: HandleValue,
        type_: u32,
        desc: UserInPtr<u8>,
        desc_size: usize,
        mut out: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        info!(
            "iommu.create: resource={:#x}, type={:#x}, desc={:#x?} desc_size={:#x} out={:#x?}",
            resource, type_, desc, desc_size, out
        );
        let proc = self.thread.proc();
        proc.get_object::<Resource>(resource)?
            .validate(ResourceKind::ROOT)?;
        if desc_size > IOMMU_MAX_DESC_LEN {
            return Err(ZxError::INVALID_ARGS);
        }
        if desc_size != IOMMU_DESC_SIZE {
            return Err(ZxError::INVALID_ARGS);
        }
        // Only the dummy IOMMU exists; asking for another kind is the
        // caller's error, and used to be `unimplemented!()`, a kernel panic
        // for whoever holds the root resource.
        if type_ != IOMMU_TYPE_DUMMY {
            return Err(ZxError::NOT_SUPPORTED);
        }
        let _copied_desc = desc.read_array(desc_size)?;
        let iommu = Iommu::create();
        install_handle(proc, Handle::new(iommu, Rights::DEFAULT_CHANNEL), &mut out)
    }
    /// Creates a new bus transaction initiator.
    ///
    /// `iommu: HandleValue`, a handle to an IOMMU.
    /// `options: u32`, must be 0 (reserved for future definition of creation flags).
    /// `bti_id: u64`, a hardware transaction identifier for a device downstream of that IOMMU.
    pub fn sys_bti_create(
        &self,
        iommu: HandleValue,
        options: u32,
        bti_id: u64,
        mut out: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        info!(
            "bti.create: iommu={:#x}, options={:?}, bti_id={:#x?}",
            iommu, options, bti_id
        );
        let proc = self.thread.proc();
        if options != 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        let iommu = proc.get_object::<Iommu>(iommu)?;
        if !iommu.is_valid_bus_txn_id() {
            return Err(ZxError::INVALID_ARGS);
        }
        let bti = BusTransactionInitiator::create(iommu, bti_id);
        install_handle(proc, Handle::new(bti, Rights::DEFAULT_BTI), &mut out)
    }

    #[allow(clippy::too_many_arguments)]
    /// Pin pages and grant devices access to them.
    pub fn sys_bti_pin(
        &self,
        bti: HandleValue,
        options: u32,
        vmo: HandleValue,
        offset: usize,
        size: usize,
        mut addrs: UserOutPtr<DevVAddr>,
        addrs_count: usize,
        mut out: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        let options = bti_options(options)?;
        info!(
            "bti.pin: bti={:#x}, options={:?}, vmo={:#x}, offset={:#x}, size={:#x}, addrs={:#x?}, addrs_count={:#x}",
            bti, options, vmo, offset, size, addrs, addrs_count
        );
        let proc = self.thread.proc();
        let bti = proc.get_object_with_rights::<BusTransactionInitiator>(bti, Rights::MAP)?;
        if !page_aligned(offset) || !page_aligned(size) {
            return Err(ZxError::INVALID_ARGS);
        }
        let vmo = proc.get_object_with_rights::<VmObject>(vmo, options.to_vmo_rights())?;
        let compress_results = options.contains(BtiOptions::COMPRESS);
        let contiguous = options.contains(BtiOptions::CONTIGUOUS);
        if contiguous && (compress_results || !vmo.is_contiguous()) {
            return Err(ZxError::INVALID_ARGS);
        }
        let (pmt, encoded_addrs) = bti.pin_and_encode(
            vmo,
            offset,
            size,
            options.to_iommu_perms(),
            compress_results,
            contiguous,
            addrs_count,
        )?;
        // Everything past the pin undoes it when it fails, for the same reason
        // `pin_and_encode` does: an error here would otherwise answer the
        // caller with the pages pinned and no handle to let them go.
        let mut hand_over = || -> ZxResult {
            addrs.write_array(&encoded_addrs)?;
            install_handle(proc, Handle::new(pmt.clone(), Rights::INSPECT), &mut out)
        };
        let handed_over = hand_over();
        if handed_over.is_err() {
            pmt.unpin();
        }
        handed_over
    }

    /// Unpins pages that were previously pinned by `zx_bti_pin()`.
    pub fn sys_pmt_unpin(&self, pmt: HandleValue) -> ZxResult {
        info!("pmt.unpin: pmt={:#x}", pmt);
        let proc = self.thread.proc();
        let pmt = proc.remove_object::<PinnedMemoryToken>(pmt)?;
        pmt.unpin();
        Ok(())
    }

    /// Releases all quarantined PMTs for the given BTI.
    pub fn sys_bti_release_quarantine(&self, bti: HandleValue) -> ZxResult {
        info!("bti.release_quarantine: bti = {:#x}", bti);
        let proc = self.thread.proc();
        let bti = proc.get_object_with_rights::<BusTransactionInitiator>(bti, Rights::WRITE)?;
        bti.release_quarantine();
        Ok(())
    }

    #[allow(unused_variables, unused_mut)]
    pub fn sys_pc_firmware_tables(
        &self,
        resource: HandleValue,
        mut acpi_rsdp_ptr: UserOutPtr<u64>,
        mut smbios_ptr: UserOutPtr<u64>,
    ) -> ZxResult {
        info!("pc_firmware_tables: handle={:?}", resource);
        let proc = self.thread.proc();
        proc.get_object::<Resource>(resource)?
            .validate(ResourceKind::ROOT)?;
        cfg_if::cfg_if! {
            if #[cfg(all(target_arch = "x86_64", target_os = "none"))] {
                let (acpi_rsdp, smbios) = kernel_hal::x86_64::pc_firmware_tables();
                acpi_rsdp_ptr.write(acpi_rsdp)?;
                smbios_ptr.write(smbios)?;
                Ok(())
            } else {
                Err(ZxError::NOT_SUPPORTED)
            }
        }
    }

    /// Creates an interrupt object which represents a physical or virtual interrupt.
    pub fn sys_interrupt_create(
        &self,
        resource: HandleValue,
        src_num: usize,
        options: u32,
        mut out: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        info!(
            "interrupt.create: handle={:?} src_num={:?} options={:?}",
            resource, src_num, options
        );
        let proc = self.thread.proc();
        let options = interrupt_options(options)?;
        let interrupt = if options.contains(InterruptOptions::VIRTUAL) {
            if options != InterruptOptions::VIRTUAL {
                return Err(ZxError::INVALID_ARGS);
            }
            Interrupt::new_virtual()
        } else {
            let resource = proc.get_object::<Resource>(resource)?;
            resource.validate_ranged_resource(ResourceKind::IRQ, src_num, 1)?;
            Interrupt::new_physical(src_num, options)?
        };
        install_handle(
            proc,
            Handle::new(interrupt, Rights::DEFAULT_INTERRUPT),
            &mut out,
        )
    }

    /// Binds or unbinds an interrupt object to a port.
    ///
    /// The key used when binding the interrupt will be present in the key field of the `zx_port_packet_t`.
    pub fn sys_interrupt_bind(
        &self,
        interrupt: HandleValue,
        port: HandleValue,
        key: u64,
        options: u32,
    ) -> ZxResult {
        info!(
            "interrupt.bind: interrupt={:?} port={:?} key={:?} options={:?}",
            interrupt, port, key, options
        );
        let proc = self.thread.proc();
        let interrupt = proc.get_object_with_rights::<Interrupt>(interrupt, Rights::READ)?;
        let port = proc.get_object_with_rights::<Port>(port, Rights::WRITE)?;
        if !port.can_bind_to_interrupt() {
            return Err(ZxError::WRONG_TYPE);
        }
        if options == InterruptOp::Bind as _ {
            interrupt.bind(&port, key)
        } else if options == InterruptOp::Unbind as _ {
            interrupt.unbind(&port)
        } else {
            Err(ZxError::INVALID_ARGS)
        }
    }

    /// Triggers a virtual interrupt object.
    pub fn sys_interrupt_trigger(
        &self,
        interrupt: HandleValue,
        options: u32,
        timestamp: i64,
    ) -> ZxResult {
        info!(
            "interrupt.trigger: interrupt={:?} options={:?} timestamp={:?}",
            interrupt, options, timestamp
        );
        let interrupt = self
            .thread
            .proc()
            .get_object_with_rights::<Interrupt>(interrupt, Rights::SIGNAL)?;
        interrupt.trigger(timestamp)
    }

    /// Acknowledge an interrupt and re-arm it.
    ///
    /// This system call acknowledges an interrupt object, causing it to be eligible to trigger again (and delivering a packet to the port it is bound to).
    pub fn sys_interrupt_ack(&self, interrupt: HandleValue) -> ZxResult {
        info!("interupt.ack: interrupt={:?}", interrupt);
        let interrupt = self
            .thread
            .proc()
            .get_object_with_rights::<Interrupt>(interrupt, Rights::WRITE)?;
        interrupt.ack()
    }

    /// Destroys an interrupt object.
    pub fn sys_interrupt_destroy(&self, interrupt: HandleValue) -> ZxResult {
        info!("interupt.destory: interrupt={:?}", interrupt);
        let interrupt = self.thread.proc().get_object::<Interrupt>(interrupt)?;
        interrupt.destroy()
    }

    /// A blocking syscall which causes the caller to wait until an interrupt is triggered.
    pub async fn sys_interrupt_wait(
        &self,
        interrupt: HandleValue,
        mut out: UserOutPtr<i64>,
    ) -> ZxResult {
        info!("interrupt.wait: handle={:?}", interrupt);
        let proc = self.thread.proc();
        let interrupt = proc.get_object_with_rights::<Interrupt>(interrupt, Rights::WAIT)?;
        let future = interrupt.wait();
        pin_mut!(future);
        let timestamp = self
            .thread
            .blocking_run(
                future,
                ThreadState::BlockedInterrupt,
                Deadline::forever().into(),
                None,
            )
            .await?;
        out.write_if_not_null(timestamp)?;
        Ok(())
    }
}

const IOMMU_TYPE_DUMMY: u32 = 0;
const IOMMU_MAX_DESC_LEN: usize = 4096;
const IOMMU_DESC_SIZE: usize = 1;

bitflags! {
    struct BtiOptions: u32 {
        #[allow(clippy::identity_op)]
        const PERM_READ             = 1 << 0;
        const PERM_WRITE            = 1 << 1;
        const PERM_EXECUTE          = 1 << 2;
        const COMPRESS              = 1 << 3;
        const CONTIGUOUS            = 1 << 4;
    }
}

enum InterruptOp {
    Bind = 0,
    Unbind = 1,
}

impl BtiOptions {
    /// Get desired rights of VMO handle.
    fn to_vmo_rights(self) -> Rights {
        let mut rights = Rights::MAP;
        if self.contains(BtiOptions::PERM_READ) {
            rights.insert(Rights::READ);
        }
        if self.contains(BtiOptions::PERM_WRITE) {
            rights.insert(Rights::WRITE);
        }
        if self.contains(BtiOptions::PERM_EXECUTE) {
            // NOTE: Check Rights::READ instead of Rights::EXECUTE,
            // because Rights::EXECUTE applies to the execution permission of the host CPU,
            // but ZX_BTI_PERM_EXECUTE applies to transactions initiated by the bus device.
            rights.insert(Rights::READ);
        }
        rights
    }

    fn to_iommu_perms(self) -> IommuPerms {
        let mut perms = IommuPerms::empty();
        if self.contains(BtiOptions::PERM_READ) {
            perms.insert(IommuPerms::PERM_READ)
        }
        if self.contains(BtiOptions::PERM_WRITE) {
            perms.insert(IommuPerms::PERM_WRITE);
        }
        if self.contains(BtiOptions::PERM_EXECUTE) {
            perms.insert(IommuPerms::PERM_EXECUTE);
        }
        perms
    }
}

#[cfg(test)]
mod tests {
    //! The options words of `zx_bti_pin` and `zx_interrupt_create`: a bit this
    //! kernel does not know is the caller's mistake to hear about, not
    //! something to drop on the floor and carry on as if it had been asked for
    //! what was left.
    use super::*;

    #[test]
    fn bti_pin_takes_the_bits_it_knows_and_no_others() {
        assert_eq!(bti_options(0).unwrap(), BtiOptions::empty());
        assert_eq!(
            bti_options(0b1_1111).unwrap(),
            BtiOptions::PERM_READ
                | BtiOptions::PERM_WRITE
                | BtiOptions::PERM_EXECUTE
                | BtiOptions::COMPRESS
                | BtiOptions::CONTIGUOUS,
        );
        // The first bit above the ones defined, and the top of the word.
        assert_eq!(bti_options(0b10_0000).err(), Some(ZxError::INVALID_ARGS));
        assert_eq!(bti_options(u32::MAX).err(), Some(ZxError::INVALID_ARGS));
        // It used to keep the part it understood and answer `Ok`.
        assert_eq!(
            bti_options(BtiOptions::PERM_READ.bits() | 0b10_0000).err(),
            Some(ZxError::INVALID_ARGS),
        );
    }

    #[test]
    fn interrupt_create_takes_every_mode_and_no_unknown_bit() {
        // The mode is a field written as flags, so every value it can hold has
        // to go through: refusing one of these would be a regression of its
        // own.
        for mode in [
            InterruptOptions::MODE_DEFAULT,
            InterruptOptions::MODE_EDGE_LOW,
            InterruptOptions::MODE_EDGE_HIGH,
            InterruptOptions::MODE_LEVEL_LOW,
            InterruptOptions::MODE_LEVEL_HIGH,
            InterruptOptions::MODE_EDGE_BOTH,
        ] {
            let word = mode.bits() | InterruptOptions::REMAP_IRQ.bits();
            assert_eq!(interrupt_options(word).unwrap().bits(), word);
        }
        assert_eq!(
            interrupt_options(InterruptOptions::VIRTUAL.bits()).unwrap(),
            InterruptOptions::VIRTUAL,
        );

        assert_eq!(
            interrupt_options(InterruptOptions::all().bits() + 1).err(),
            Some(ZxError::INVALID_ARGS),
        );
        assert_eq!(
            interrupt_options(u32::MAX).err(),
            Some(ZxError::INVALID_ARGS)
        );
    }
}
