use {super::*, alloc::sync::Arc};

/// The value refers to a Handle in user space.
pub type HandleValue = u32;

/// Invalid handle value.
pub const INVALID_HANDLE: HandleValue = 0;

/// A Handle is how a specific process refers to a specific kernel object.
#[derive(Debug, Clone)]
pub struct Handle {
    /// The object referred to by the handle.
    pub object: Arc<dyn KernelObject>,
    /// The handle's associated rights.
    pub rights: Rights,
}

impl Handle {
    /// Create a new handle referring to the given object with given rights.
    pub fn new(object: Arc<dyn KernelObject>, rights: Rights) -> Self {
        Handle { object, rights }
    }

    /// Get information about the provided handle and the object the handle refers to.
    pub fn get_info(&self) -> HandleBasicInfo {
        HandleBasicInfo {
            koid: self.object.id(),
            rights: self.rights.bits(),
            obj_type: obj_type(&self.object),
            related_koid: self.object.related_koid(),
            props: if self.rights.contains(Rights::WAIT) {
                1
            } else {
                0
            },
            padding: 0,
        }
    }

    /// Get information about the handle itself.
    ///
    /// The returned `HandleInfo`'s `handle` field should set manually.
    pub fn get_handle_info(&self) -> HandleInfo {
        HandleInfo {
            obj_type: obj_type(&self.object),
            rights: self.rights.bits(),
            ..Default::default()
        }
    }
}

/// Information about a handle and the object it refers to.
#[repr(C)]
#[derive(Default, Debug)]
pub struct HandleBasicInfo {
    koid: u64,
    rights: u32,
    obj_type: u32,
    related_koid: u64,
    props: u32,
    padding: u32,
}

/// Get an object's type.
pub fn obj_type(object: &Arc<dyn KernelObject>) -> u32 {
    obj_type_of_name(object.type_name())
}

/// `ZX_OBJ_TYPE_NONE`: what an object this table does not name reports.
pub const OBJ_TYPE_NONE: u32 = 0;

/// The `zx_obj_type_t` for a `KernelObject::type_name()`.
///
/// The names on the left are Rust type names, not Zircon's spelling of the
/// numbers: `impl_kobject!` makes `type_name()` return `stringify!` of the
/// type it is written on, so an arm spelled any other way simply never
/// matches. Three of them were spelled Zircon's way -- `"Bti"`, `"Pmt"`,
/// `"VCpu"` -- against types actually called `BusTransactionInitiator`,
/// `PinnedMemoryToken` and `Vcpu`, so those three fell through to the
/// default. Split out from [`obj_type`] so a test can go from the Rust type
/// to the number without having to build one of every object.
fn obj_type_of_name(type_name: &str) -> u32 {
    match type_name {
        "Process" => 1,
        "Thread" => 2,
        "VmObject" => 3,
        "Channel" => 4,
        "Event" => 5,
        "Port" => 6,
        "Interrupt" => 9,
        // `ZX_OBJ_TYPE_PCI_DEVICE`. This is the object `zx_pci_get_nth_device`
        // hands back, and it used to report 32 while a dead `"PciDevice"` arm
        // sat here holding the right number under a name no type answers to.
        "PcieDeviceKObject" => 11,
        "DebugLog" => 12,
        "Socket" => 14,
        "Resource" => 15,
        "EventPair" => 16,
        "Job" => 17,
        "VmAddressRegion" => 18,
        "Fifo" => 19,
        "Guest" => 20,
        "Vcpu" => 21,
        "Timer" => 22,
        "Iommu" => 23,
        "BusTransactionInitiator" => 24,
        // 25 (`Profile`) and 28 (`Pager`) have no type in this tree yet.
        "Profile" => 25,
        "PinnedMemoryToken" => 26,
        "SuspendToken" => 27,
        "Pager" => 28,
        "ExceptionObject" => 29,
        "Clock" => 30,
        "Stream" => 31,
        "Counter" => 34,
        // Everything else: every Linux-side object in `linux-object`, and
        // whatever `impl_kobject!` is written on next. Reporting a number for
        // a handle is not worth a kernel panic, and `unimplemented!()` here
        // made every new object type a landmine under
        // `zx_object_get_info(ZX_INFO_HANDLE_BASIC)`.
        _ => OBJ_TYPE_NONE,
    }
}

/// Information about a handle itself, including its `HandleValue`.
#[repr(C)]
#[derive(Default, Debug)]
pub struct HandleInfo {
    /// The handle's value in user space.
    pub handle: HandleValue,
    obj_type: u32,
    rights: u32,
    unused: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An object this table does not name reports `ZX_OBJ_TYPE_NONE`.
    /// It used to panic the kernel, which made every new `impl_kobject!` --
    /// and every Linux-side object in `linux-object`, which are the large
    /// majority of them -- a landmine under
    /// `zx_object_get_info(ZX_INFO_HANDLE_BASIC)`.
    #[test]
    fn an_object_this_table_does_not_name_is_type_none() {
        let obj: Arc<dyn KernelObject> = DummyObject::new();
        assert_eq!(obj_type(&obj), OBJ_TYPE_NONE);
        assert_eq!(Handle::new(obj, Rights::DEFAULT_JOB).get_info().obj_type, 0);
        assert_eq!(obj_type_of_name(""), OBJ_TYPE_NONE);
        assert_eq!(obj_type_of_name("NoSuchObject"), OBJ_TYPE_NONE);
    }

    /// The last segment of a Rust type's path, which is what `stringify!`
    /// -- and therefore `impl_kobject!`'s `type_name()` -- produces.
    fn rust_name<T: ?Sized>() -> &'static str {
        core::any::type_name::<T>().rsplit("::").next().unwrap()
    }

    /// Every arm stands for a Rust type, so go from the TYPE to the number
    /// and never from a string literal. An arm spelled some other way --
    /// Zircon's name for the number, say -- answers `NONE` here, which is
    /// exactly how `"Bti"`, `"Pmt"` and `"VCpu"` sat in this table unnoticed
    /// while a BTI, a PMT or a VCPU handle panicked the kernel on being
    /// asked what it was.
    ///
    /// The types that are awkward to build are covered here and nowhere
    /// else; the rest are built for real below.
    #[test]
    fn each_arm_is_named_after_the_rust_type_it_stands_for() {
        use crate::dev::pci::PcieDeviceKObject;
        use crate::dev::{BusTransactionInitiator, Iommu, PinnedMemoryToken, Resource};
        use crate::ipc::{Channel, Fifo, Socket};
        use crate::signal::Port;
        use crate::signal::{Event, EventPair, Timer};
        use crate::task::{ExceptionObject, Job, Process, SuspendToken, Thread};
        use crate::vm::{Stream, VmAddressRegion, VmObject};

        assert_eq!(obj_type_of_name(rust_name::<Process>()), 1);
        assert_eq!(obj_type_of_name(rust_name::<Thread>()), 2);
        assert_eq!(obj_type_of_name(rust_name::<VmObject>()), 3);
        assert_eq!(obj_type_of_name(rust_name::<Channel>()), 4);
        assert_eq!(obj_type_of_name(rust_name::<Event>()), 5);
        assert_eq!(obj_type_of_name(rust_name::<Port>()), 6);
        assert_eq!(obj_type_of_name(rust_name::<crate::dev::Interrupt>()), 9);
        assert_eq!(obj_type_of_name(rust_name::<PcieDeviceKObject>()), 11);
        assert_eq!(
            obj_type_of_name(rust_name::<crate::debuglog::DebugLog>()),
            12
        );
        assert_eq!(obj_type_of_name(rust_name::<Socket>()), 14);
        assert_eq!(obj_type_of_name(rust_name::<Resource>()), 15);
        assert_eq!(obj_type_of_name(rust_name::<EventPair>()), 16);
        assert_eq!(obj_type_of_name(rust_name::<Job>()), 17);
        assert_eq!(obj_type_of_name(rust_name::<VmAddressRegion>()), 18);
        assert_eq!(obj_type_of_name(rust_name::<Fifo>()), 19);
        assert_eq!(obj_type_of_name(rust_name::<Timer>()), 22);
        assert_eq!(obj_type_of_name(rust_name::<Iommu>()), 23);
        assert_eq!(obj_type_of_name(rust_name::<BusTransactionInitiator>()), 24);
        assert_eq!(obj_type_of_name(rust_name::<PinnedMemoryToken>()), 26);
        assert_eq!(obj_type_of_name(rust_name::<SuspendToken>()), 27);
        assert_eq!(obj_type_of_name(rust_name::<ExceptionObject>()), 29);
        assert_eq!(obj_type_of_name(rust_name::<Clock>()), 30);
        assert_eq!(obj_type_of_name(rust_name::<Stream>()), 31);
        assert_eq!(obj_type_of_name(rust_name::<Counter>()), 34);
        // `Guest` and `Vcpu` live behind the `hypervisor` feature, which
        // does not build in this tree at all (its `rvm` dependency is
        // commented out in Cargo.toml), which is why the misspelling here
        // outlived the other two. Fixed by inspection, measured by nothing.
        #[cfg(feature = "hypervisor")]
        {
            assert_eq!(
                obj_type_of_name(rust_name::<crate::hypervisor::Guest>()),
                20
            );
            assert_eq!(obj_type_of_name(rust_name::<crate::hypervisor::Vcpu>()), 21);
        }
    }

    /// The same numbers again, but from real objects: `obj_type` reads
    /// `type_name()` off the object it is handed, so this measures the whole
    /// chain -- `impl_kobject!` on the type, the string it produces, the arm
    /// that matches it -- rather than one table against another.
    ///
    /// The two that used to take the kernel down are here: `zx_bti_create`
    /// and `zx_bti_pin` put a BTI and a PMT in the handle table of any
    /// process holding the root resource, and
    /// `zx_object_get_info(ZX_INFO_HANDLE_BASIC)` on either was all it took.
    #[test]
    fn every_object_answers_with_its_own_number() {
        use crate::debuglog::DebugLog;
        use crate::dev::{BusTransactionInitiator, Interrupt, Iommu, IommuPerms};
        use crate::dev::{Resource, ResourceFlags, ResourceKind};
        use crate::ipc::{Channel, Fifo, Socket};
        use crate::signal::{Event, EventPair, Port, Timer};
        use crate::task::{Job, Process, SuspendToken, Task, Thread};
        use crate::vm::{Stream, VmObject, PAGE_SIZE, PAGE_SIZE_LOG2};

        fn check(obj: Arc<dyn KernelObject>, expect: u32) {
            assert_eq!(
                obj_type(&obj),
                expect,
                "{} reports the wrong zx_obj_type_t",
                obj.type_name()
            );
        }

        let job = Job::root();
        let proc = Process::create(&job, "obj-type").unwrap();
        let thread = Thread::create(&proc, "obj-type").unwrap();
        let vmo = VmObject::new_paged(1);
        let iommu = Iommu::create();
        let bti = BusTransactionInitiator::create(iommu.clone(), 0);
        let pinned = VmObject::new_contiguous(1, PAGE_SIZE_LOG2).unwrap();
        let pmt = bti
            .pin(pinned, 0, PAGE_SIZE, IommuPerms::PERM_READ)
            .unwrap();

        check(proc.clone(), 1);
        check(thread.clone(), 2);
        check(vmo.clone(), 3);
        check(Channel::create().0, 4);
        check(Event::new(), 5);
        check(Port::new(0).unwrap(), 6);
        check(Interrupt::new_virtual(), 9);
        check(DebugLog::create(0), 12);
        check(Socket::create(0).unwrap().0, 14);
        check(
            Resource::create("r", ResourceKind::ROOT, 0, 0, ResourceFlags::empty()),
            15,
        );
        check(EventPair::create().0, 16);
        check(job, 17);
        check(proc.vmar(), 18);
        check(Fifo::create(2, 8).0, 19);
        check(Timer::new(), 22);
        check(iommu, 23);
        check(bti, 24);
        check(pmt, 26);
        check(SuspendToken::create(&(thread as Arc<dyn Task>)), 27);
        check(Clock::new(0, false), 30);
        check(Stream::create(vmo, 0, 0), 31);
        check(Counter::new(), 34);
    }

    #[test]
    fn test_get_info() {
        let obj = crate::task::Job::root();
        let handle1 = Handle::new(obj.clone(), Rights::DEFAULT_JOB);
        let info1 = handle1.get_info();
        assert_eq!(info1.obj_type, 17);
        assert_eq!(info1.props, 1);

        let handle_info = handle1.get_handle_info();
        assert_eq!(handle_info.obj_type, 17);

        let handle2 = Handle::new(obj, Rights::READ);
        let info2 = handle2.get_info();
        assert_eq!(info2.props, 0);

        // Let struct lines counted covered.
        // See https://github.com/mozilla/grcov/issues/450
        let _ = HandleBasicInfo::default();
    }

    /// The layout `zx_object_get_info(ZX_INFO_HANDLE_BASIC)` copies straight
    /// out to userspace, so the offsets below are the ABI and not an
    /// implementation detail: they are where a C caller of
    /// `zx_info_handle_basic_t` reads each field.
    ///
    /// `size_of` and `align_of` alone do not hold them. Rust promises nothing
    /// about where it puts the fields of a struct that is not `repr(C)`, and
    /// that is the point of the test: dropping the attribute leaves this
    /// rustc with the same size 32 and alignment 8 and still moves fields,
    /// which a reader of the copied bytes cannot see -- it gets one field's
    /// value where it expected another's. Only an offset per field says where
    /// they are.
    #[test]
    fn handle_basic_info_is_the_zx_info_handle_basic_layout() {
        use core::mem::{align_of, offset_of, size_of};
        assert_eq!(size_of::<HandleBasicInfo>(), 32);
        assert_eq!(align_of::<HandleBasicInfo>(), 8);
        assert_eq!(offset_of!(HandleBasicInfo, koid), 0);
        assert_eq!(offset_of!(HandleBasicInfo, rights), 8);
        assert_eq!(offset_of!(HandleBasicInfo, obj_type), 12);
        assert_eq!(offset_of!(HandleBasicInfo, related_koid), 16);
        assert_eq!(offset_of!(HandleBasicInfo, props), 24);
        assert_eq!(offset_of!(HandleBasicInfo, padding), 28);
    }

    /// `zx_handle_info_t`: four words, in the order
    /// `zx_channel_read_etc` writes them.
    #[test]
    fn handle_info_is_the_zx_handle_info_layout() {
        use core::mem::{align_of, offset_of, size_of};
        assert_eq!(size_of::<HandleInfo>(), 16);
        assert_eq!(align_of::<HandleInfo>(), 4);
        assert_eq!(offset_of!(HandleInfo, handle), 0);
        assert_eq!(offset_of!(HandleInfo, obj_type), 4);
        assert_eq!(offset_of!(HandleInfo, rights), 8);
        assert_eq!(offset_of!(HandleInfo, unused), 12);
    }

    /// The words the kernel promises as zero. Both structs are copied out
    /// whole, so a word left unwritten is whatever the kernel's own memory
    /// held; nothing in this file read either of them before. And
    /// `INVALID_HANDLE` is `0` by number, which is what makes a zeroed
    /// `HandleInfo.handle` say "no handle here" to its caller.
    #[test]
    fn the_words_promised_as_zero_are_zero() {
        assert_eq!(INVALID_HANDLE, 0);
        let handle = Handle::new(crate::task::Job::root(), Rights::DEFAULT_JOB);
        assert_eq!(handle.get_info().padding, 0, "padding carries kernel bytes");
        let info = handle.get_handle_info();
        assert_eq!(info.unused, 0, "unused carries kernel bytes");
        // `get_handle_info` leaves the handle value for its caller to fill
        // in, which is what its doc promises; zero is the value that says so.
        assert_eq!(info.handle, 0);
    }

    /// `koid` and `related_koid` answer two different questions, and the only
    /// test that ever built a `HandleBasicInfo` read neither: the first is the
    /// object's own id, the second the id of the object it is paired with --
    /// a channel's other end, a thread's process, a job's parent -- and zero
    /// when it has none. Swapping them leaves two plausible koids in the
    /// struct, which is why it needs two objects whose ids differ to see it
    /// at all.
    ///
    /// A channel pair is what that costs least: a process would do as well,
    /// but each one takes one of the 127 hosted address-space windows, and
    /// this suite already runs out of them at sixteen threads.
    #[test]
    fn the_two_koids_are_the_object_and_the_one_it_is_paired_with() {
        // Both ends stay alive here: a channel reports its peer through a
        // `Weak`, so a dropped peer answers zero and proves nothing.
        let (left, right) = crate::ipc::Channel::create();
        let info = Handle::new(left.clone(), Rights::DEFAULT_CHANNEL).get_info();
        assert_eq!(info.koid, left.id());
        assert_eq!(
            info.related_koid,
            right.id(),
            "a channel is paired with its other end"
        );

        // And an object with nothing on the other side answers zero rather
        // than repeating its own koid.
        let event = crate::signal::Event::new();
        let info = Handle::new(event.clone(), Rights::DEFAULT_EVENT).get_info();
        assert_eq!(info.koid, event.id());
        assert_eq!(info.related_koid, 0);
    }

    /// The rights both info structs report. `test_get_info` read `obj_type`
    /// and `props` and never these, so every handle in the system could have
    /// reported no rights at all: `zx_object_get_info(ZX_INFO_HANDLE_BASIC)`
    /// is how a process asks what a handle it holds is allowed to do.
    #[test]
    fn both_info_structs_report_the_handles_own_rights() {
        let obj = crate::task::Job::root();
        let handle = Handle::new(obj.clone(), Rights::DEFAULT_JOB);
        assert_eq!(handle.get_info().rights, Rights::DEFAULT_JOB.bits());
        assert_eq!(handle.get_handle_info().rights, Rights::DEFAULT_JOB.bits());

        // A narrower handle on the same object reports the narrower set, so
        // the number comes from the handle and not from the object.
        let narrow = Handle::new(obj, Rights::READ);
        assert_eq!(narrow.get_info().rights, Rights::READ.bits());
        assert_ne!(narrow.get_info().rights, Rights::DEFAULT_JOB.bits());
    }
}
