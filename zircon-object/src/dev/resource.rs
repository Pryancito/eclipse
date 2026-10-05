use {crate::object::*, alloc::sync::Arc, bitflags::bitflags, numeric_enum_macro::numeric_enum};

numeric_enum! {
    #[repr(u32)]
    /// ResourceKind definition from fuchsia/zircon/system/public/zircon/syscalls/resource.h
    #[allow(missing_docs)]
    #[allow(clippy::upper_case_acronyms)]
    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    pub enum ResourceKind {
        MMIO = 0,
        IRQ = 1,
        IOPORT = 2,
        ROOT = 3,
        SMC = 4,
        SYSTEM = 5,
        COUNT = 6,
    }
}

/// Subranges of the SYSTEM resource, from zircon/syscalls/resource.h.
#[derive(Debug, Clone, Copy)]
#[repr(usize)]
pub enum SystemResource {
    Hypervisor = 0,
    Vmex = 1,
    Debuglog = 12,
}

bitflags! {
    /// Bits for Resource.flags.
    pub struct ResourceFlags: u32 {
        #[allow(clippy::identity_op)]
        /// Exclusive resource.
        const EXCLUSIVE      = 1 << 16;
    }
}

/// Address space rights and accounting.
pub struct Resource {
    base: KObjectBase,
    kind: ResourceKind,
    addr: usize,
    len: usize,
    flags: ResourceFlags,
}

impl_kobject!(Resource);

impl Resource {
    /// Create a new `Resource`.
    pub fn create(
        name: &str,
        kind: ResourceKind,
        addr: usize,
        len: usize,
        flags: ResourceFlags,
    ) -> Arc<Self> {
        Arc::new(Resource {
            base: KObjectBase::with_name(name),
            kind,
            addr,
            len,
            flags,
        })
    }

    /// Validate the resource is the given kind or it is the root resource.
    pub fn validate(&self, kind: ResourceKind) -> ZxResult {
        if self.kind == kind || self.kind == ResourceKind::ROOT {
            Ok(())
        } else {
            Err(ZxError::WRONG_TYPE)
        }
    }

    /// Validate a SYSTEM capability for its specific purpose.
    pub fn validate_system(&self, resource: SystemResource) -> ZxResult {
        self.validate_ranged_resource(ResourceKind::SYSTEM, resource as usize, 1)
    }

    /// Validate the resource is the given kind or it is the root resource,
    /// and [addr, addr+len] is within the range of the resource.
    pub fn validate_ranged_resource(
        &self,
        kind: ResourceKind,
        addr: usize,
        len: usize,
    ) -> ZxResult {
        self.validate(kind)?;
        if addr >= self.addr && len <= self.len && addr - self.addr <= self.len - len {
            Ok(())
        } else {
            Err(ZxError::OUT_OF_RANGE)
        }
    }

    /// Returns `Err(ZxError::INVALID_ARGS)` if the resource is not the root resource, and
    /// either it's flags or parameter `flags` contains `ResourceFlags::EXCLUSIVE`.
    pub fn check_exclusive(&self, flags: ResourceFlags) -> ZxResult {
        if self.kind != ResourceKind::ROOT
            && (self.flags.contains(ResourceFlags::EXCLUSIVE)
                || flags.contains(ResourceFlags::EXCLUSIVE))
        {
            Err(ZxError::INVALID_ARGS)
        } else {
            Ok(())
        }
    }

    /// Get information of the resource.
    pub fn get_info(&self) -> ResourceInfo {
        let name = self.base.name();
        let name = name.as_bytes();
        // `zx_info_resource_t::name` is `ZX_MAX_NAME_LEN` bytes with a NUL,
        // and `zx_resource_create` accepts a name of any length: copying it
        // whole was a kernel panic for a name of 33 bytes or more.
        let mut name_vec = [0u8; 32];
        let len = name.len().min(name_vec.len() - 1);
        name_vec[..len].clone_from_slice(&name[..len]);
        ResourceInfo {
            kind: self.kind as _,
            flags: self.flags.bits,
            base: self.addr as _,
            size: self.len as _,
            name: name_vec,
        }
    }
}

/// Information of a resource.
#[repr(C)]
#[derive(Default)]
pub struct ResourceInfo {
    kind: u32,
    flags: u32,
    base: u64,
    size: u64,
    name: [u8; 32], // should be [char; 32], but I cannot compile it
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::convert::TryFrom;

    /// A name longer than the info field is cut to fit it, NUL and all: it
    /// used to be copied whole, which panicked past 32 bytes.
    #[test]
    fn a_name_longer_than_the_info_field_is_cut_to_fit_it() {
        let long = "a-resource-name-of-forty-characters-long";
        assert_eq!(long.len(), 40);
        let info =
            Resource::create(long, ResourceKind::MMIO, 0, 0, ResourceFlags::empty()).get_info();
        assert_eq!(&info.name[..31], &long.as_bytes()[..31]);
        assert_eq!(info.name[31], 0, "always NUL-terminated");
        let short =
            Resource::create("irq", ResourceKind::IRQ, 4, 2, ResourceFlags::empty()).get_info();
        assert_eq!(&short.name[..4], b"irq\0");
        assert_eq!((short.kind, short.base, short.size), (1, 4, 2));
    }

    #[test]
    fn system_resources_are_scoped_to_their_subrange() {
        let debuglog = Resource::create(
            "debuglog",
            ResourceKind::SYSTEM,
            12,
            1,
            ResourceFlags::empty(),
        );
        assert!(debuglog.validate_system(SystemResource::Debuglog).is_ok());
        assert_eq!(
            debuglog.validate_system(SystemResource::Vmex),
            Err(ZxError::OUT_OF_RANGE)
        );
        assert_eq!(
            debuglog.validate_system(SystemResource::Hypervisor),
            Err(ZxError::OUT_OF_RANGE)
        );
        let root = Resource::create("root", ResourceKind::ROOT, 0, 16, ResourceFlags::empty());
        for purpose in [
            SystemResource::Vmex,
            SystemResource::Debuglog,
            SystemResource::Hypervisor,
        ] {
            assert!(root.validate_system(purpose).is_ok());
        }
        assert_eq!(
            root.validate_ranged_resource(ResourceKind::SYSTEM, usize::MAX, 2),
            Err(ZxError::OUT_OF_RANGE)
        );
    }

    /// A resource validates for its own kind and for nothing else, and the
    /// root resource validates for every kind: that is what makes the root
    /// handle the one `zx_pci_init` and the other privileged syscalls accept.
    #[test]
    fn a_resource_takes_its_own_kind_and_the_root_takes_every_kind() {
        let kinds = [
            ResourceKind::MMIO,
            ResourceKind::IRQ,
            ResourceKind::IOPORT,
            ResourceKind::SMC,
            ResourceKind::SYSTEM,
        ];
        for kind in kinds {
            let res = Resource::create("r", kind, 0, 0, ResourceFlags::empty());
            assert_eq!(res.validate(kind), Ok(()), "{:?} refused itself", kind);
            for other in kinds {
                if other != kind {
                    assert_eq!(
                        res.validate(other),
                        Err(ZxError::WRONG_TYPE),
                        "{:?} answered for {:?}",
                        kind,
                        other
                    );
                }
            }
            // Being the root is the exception on the resource's side, not on
            // the caller's: asking a plain resource for root rights fails.
            assert_eq!(res.validate(ResourceKind::ROOT), Err(ZxError::WRONG_TYPE));
        }
        let root = Resource::create("root", ResourceKind::ROOT, 0, 0, ResourceFlags::empty());
        for kind in kinds {
            assert_eq!(root.validate(kind), Ok(()), "the root refused {:?}", kind);
        }
        assert_eq!(root.validate(ResourceKind::ROOT), Ok(()));
    }

    /// `[addr, addr + len)` has to lie inside the resource, and all three
    /// comparisons are load-bearing: the two subtractions underneath them
    /// wrap, so the guards are what keeps a range that starts before the
    /// resource, or one longer than it, from comparing as inside.
    #[test]
    fn a_ranged_resource_grants_only_the_subranges_that_fit_inside_it() {
        let res = Resource::create(
            "mmio",
            ResourceKind::MMIO,
            0x1000,
            0x100,
            ResourceFlags::empty(),
        );
        let sub = |addr, len| res.validate_ranged_resource(ResourceKind::MMIO, addr, len);
        // The whole of it, its first byte, its last byte, and the empty range
        // that ends where it ends.
        assert_eq!(sub(0x1000, 0x100), Ok(()));
        assert_eq!(sub(0x1000, 1), Ok(()));
        assert_eq!(sub(0x10ff, 1), Ok(()));
        assert_eq!(sub(0x1100, 0), Ok(()));
        // One byte short of it, one byte past it, and one byte longer.
        assert_eq!(sub(0xfff, 1), Err(ZxError::OUT_OF_RANGE));
        assert_eq!(sub(0x1100, 1), Err(ZxError::OUT_OF_RANGE));
        assert_eq!(sub(0x1000, 0x101), Err(ZxError::OUT_OF_RANGE));
        // The two ranges that would pass if either subtraction were reached
        // with the wrong side bigger.
        assert_eq!(sub(0, usize::MAX), Err(ZxError::OUT_OF_RANGE));
        assert_eq!(sub(usize::MAX, 1), Err(ZxError::OUT_OF_RANGE));
        // The kind is checked first, and its answer is the one that comes
        // out: a range inside a resource of the wrong kind is WRONG_TYPE, not
        // OUT_OF_RANGE.
        assert_eq!(
            res.validate_ranged_resource(ResourceKind::IRQ, 0x1000, 1),
            Err(ZxError::WRONG_TYPE)
        );
        assert_eq!(
            res.validate_ranged_resource(ResourceKind::IRQ, 0, 1),
            Err(ZxError::WRONG_TYPE)
        );
        // A SYSTEM resource that carries no bytes grants none of its
        // subranges: each one asks for the single byte at its own index.
        let empty = Resource::create("empty", ResourceKind::SYSTEM, 12, 0, ResourceFlags::empty());
        assert_eq!(
            empty.validate_system(SystemResource::Debuglog),
            Err(ZxError::OUT_OF_RANGE)
        );
    }

    /// Exclusivity is refused from either side -- a resource already taken
    /// exclusively cannot be shared, and a shared one cannot be taken
    /// exclusively -- and the root resource is exempt from both, which is what
    /// lets the kernel's own init hand out the ranges it needs.
    #[test]
    fn exclusive_access_is_refused_from_either_side_and_the_root_is_exempt() {
        let none = ResourceFlags::empty();
        let excl = ResourceFlags::EXCLUSIVE;
        let shared = Resource::create("shared", ResourceKind::MMIO, 0, 0, none);
        let taken = Resource::create("taken", ResourceKind::MMIO, 0, 0, excl);

        assert_eq!(shared.check_exclusive(none), Ok(()));
        assert_eq!(
            shared.check_exclusive(excl),
            Err(ZxError::INVALID_ARGS),
            "asked for exclusive access to a shared resource"
        );
        assert_eq!(
            taken.check_exclusive(none),
            Err(ZxError::INVALID_ARGS),
            "shared a resource that was taken exclusively"
        );
        assert_eq!(taken.check_exclusive(excl), Err(ZxError::INVALID_ARGS));

        for flags in [none, excl] {
            let root = Resource::create("root", ResourceKind::ROOT, 0, 0, flags);
            assert_eq!(root.check_exclusive(none), Ok(()));
            assert_eq!(root.check_exclusive(excl), Ok(()));
        }
    }

    /// The numbers the ABI carries. `ResourceKind` is what
    /// `zx_resource_create` takes and what `zx_info_resource_t::kind` reports,
    /// and `SystemResource` is an index into the SYSTEM resource's range, so
    /// its values are addresses and not an ordering: the debuglog sits at 12
    /// with nothing between it and vmex.
    #[test]
    fn the_resource_numbers_are_the_ones_the_abi_names() {
        for (kind, value) in [
            (ResourceKind::MMIO, 0u32),
            (ResourceKind::IRQ, 1),
            (ResourceKind::IOPORT, 2),
            (ResourceKind::ROOT, 3),
            (ResourceKind::SMC, 4),
            (ResourceKind::SYSTEM, 5),
            (ResourceKind::COUNT, 6),
        ] {
            assert_eq!(kind as u32, value, "{:?}", kind);
            assert_eq!(ResourceKind::try_from(value), Ok(kind));
        }
        assert!(
            ResourceKind::try_from(7u32).is_err(),
            "COUNT is the end of the list, not a kind one past it"
        );
        assert_eq!(SystemResource::Hypervisor as usize, 0);
        assert_eq!(SystemResource::Vmex as usize, 1);
        assert_eq!(SystemResource::Debuglog as usize, 12);
        assert_eq!(ResourceFlags::EXCLUSIVE.bits(), 1 << 16);
    }

    /// `zx_info_resource_t`: every field goes out under its own name and in
    /// the width the header gives it, and the bytes the name does not fill are
    /// zero rather than whatever was in the buffer.
    #[test]
    fn the_info_struct_reports_each_field_under_its_own_name() {
        let res = Resource::create(
            "ioport",
            ResourceKind::IOPORT,
            0x3f8,
            8,
            ResourceFlags::EXCLUSIVE,
        );
        let info = res.get_info();
        assert_eq!(info.kind, ResourceKind::IOPORT as u32);
        assert_eq!(info.flags, ResourceFlags::EXCLUSIVE.bits());
        assert_eq!(info.base, 0x3f8);
        assert_eq!(info.size, 8);
        assert_eq!(&info.name[..7], b"ioport\0");
        assert!(
            info.name[7..].iter().all(|&b| b == 0),
            "the tail of the name field was not cleared"
        );
        // The layout userspace reads it with: two words, two 64-bit values and
        // the name, and no padding anywhere in between.
        assert_eq!(core::mem::size_of::<ResourceInfo>(), 4 + 4 + 8 + 8 + 32);
        assert_eq!(core::mem::offset_of!(ResourceInfo, kind), 0);
        assert_eq!(core::mem::offset_of!(ResourceInfo, flags), 4);
        assert_eq!(core::mem::offset_of!(ResourceInfo, base), 8);
        assert_eq!(core::mem::offset_of!(ResourceInfo, size), 16);
        assert_eq!(core::mem::offset_of!(ResourceInfo, name), 24);
    }
}
