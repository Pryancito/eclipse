use {
    crate::{ZxError, ZxResult},
    bitflags::bitflags,
    core::convert::TryFrom,
};

bitflags! {
    /// Rights are associated with handles and convey privileges to perform actions on
    /// either the associated handle or the object associated with the handle.
    #[derive(Default)]
    pub struct Rights: u32 {
        /// Allows handle duplication via `zx_handle_duplicate()`.
        #[allow(clippy::identity_op)]
        const DUPLICATE = 1 << 0;

        /// Allows handle transfer via `zx_channel_write()`.
        const TRANSFER = 1 << 1;

        /// Allows reading of data from containers (channels, sockets, VM objects, etc).
        /// Allows mapping as readable if `Rights::MAP` is also present.
        const READ = 1 << 2;

        /// Allows writing of data to containers (channels, sockets, VM objects, etc).
        /// Allows mapping as writeable if `Rights::MAP` is also present.
        const WRITE = 1 << 3;

        /// Allows mapping as executable if `Rights::MAP` is also present.
        const EXECUTE = 1 << 4;

        /// Allows mapping of a VM object into an address space.
        const MAP = 1 << 5;

        /// Allows property inspection via `zx_object_get_property()`.
        const GET_PROPERTY = 1 << 6;

        /// Allows property modification via `zx_object_set_property()`.
        const SET_PROPERTY = 1 << 7;

        /// Allows enumerating child objects via `zx_object_get_info()` and `zx_object_get_child()`.
        const ENUMERATE = 1 << 8;

        /// Allows termination of task objects via `zx_task_kill()`.
        const DESTROY = 1 << 9;

        /// Allows policy modification via `zx_job_set_policy()`.
        const SET_POLICY = 1 << 10;

        /// Allows policy inspection via `zx_job_get_policy()`.
        const GET_POLICY = 1 << 11;

        /// Allows use of `zx_object_signal()`.
        const SIGNAL = 1 << 12;

        /// Allows use of `zx_object_signal_peer()`.
        const SIGNAL_PEER = 1 << 13;

        /// Allows use of `zx_object_wait_one()`, `zx_object_wait_many()`, and other waiting primitives.
        const WAIT = 1 << 14;

        /// Allows inspection via `zx_object_get_info()`.
        const INSPECT = 1 << 15;

        /// Allows creation of processes, subjobs, etc.
        const MANAGE_JOB = 1 << 16;

        /// Allows creation of threads, etc.
        const MANAGE_PROCESS = 1 << 17;

        /// Allows suspending/resuming threads, etc.
        const MANAGE_THREAD = 1 << 18;

        /// Not used.
        const APPLY_PROFILE = 1 << 19;

        /// Allows managing socket disposition and thresholds.
        const MANAGE_SOCKET = 1 << 20;

        /// Allows operations on child VMARs and mappings.
        const OP_CHILDREN = 1 << 21;

        /// Allows resizing an object.
        const RESIZE = 1 << 22;

        /// Allows attaching a VMO.
        const ATTACH_VMO = 1 << 23;

        /// Allows managing a VMO.
        const MANAGE_VMO = 1 << 24;

        /// Used to duplicate a handle with the same rights.
        const SAME_RIGHTS = 1 << 31;


        /// TRANSFER | DUPLICATE | WAIT | INSPECT
        const BASIC = Self::TRANSFER.bits | Self::DUPLICATE.bits | Self::WAIT.bits | Self::INSPECT.bits;

        /// READ ｜ WRITE
        const IO = Self::READ.bits | Self::WRITE.bits;

        /// GET_PROPERTY ｜ SET_PROPERTY
        const PROPERTY = Self::GET_PROPERTY.bits | Self::SET_PROPERTY.bits;

        /// GET_POLICY ｜ SET_POLICY
        const POLICY = Self::GET_POLICY.bits | Self::SET_POLICY.bits;

        /// BASIC & !Self::DUPLICATE | IO | SIGNAL | SIGNAL_PEER
        const DEFAULT_CHANNEL = Self::BASIC.bits & !Self::DUPLICATE.bits | Self::IO.bits | Self::SIGNAL.bits | Self::SIGNAL_PEER.bits;

        /// BASIC | IO | PROPERTY | ENUMERATE | DESTROY | SIGNAL | MANAGE_PROCESS | MANAGE_THREAD
        const DEFAULT_PROCESS = Self::BASIC.bits | Self::IO.bits | Self::PROPERTY.bits | Self::ENUMERATE.bits | Self::DESTROY.bits
            | Self::SIGNAL.bits | Self::MANAGE_PROCESS.bits | Self::MANAGE_THREAD.bits;

        /// BASIC | IO | PROPERTY | DESTROY | SIGNAL | MANAGE_THREAD
        const DEFAULT_THREAD = Self::BASIC.bits | Self::IO.bits | Self::PROPERTY.bits | Self::DESTROY.bits | Self::SIGNAL.bits | Self::MANAGE_THREAD.bits;

        /// BASIC | IO | PROPERTY | MAP | SIGNAL
        const DEFAULT_VMO = Self::BASIC.bits | Self::IO.bits | Self::PROPERTY.bits | Self::MAP.bits | Self::SIGNAL.bits;

        /// (BASIC & !WAIT) | OP_CHILDREN
        const DEFAULT_VMAR = Self::BASIC.bits & !Self::WAIT.bits | Self::OP_CHILDREN.bits;

        /// BASIC | IO | PROPERTY | POLICY | ENUMERATE | DESTROY | SIGNAL | MANAGE_JOB | MANAGE_PROCESS | MANAGE_THREAD
        const DEFAULT_JOB = Self::BASIC.bits | Self::IO.bits | Self::PROPERTY.bits | Self::POLICY.bits | Self::ENUMERATE.bits
            | Self::DESTROY.bits | Self::SIGNAL.bits | Self::MANAGE_JOB.bits | Self::MANAGE_PROCESS.bits | Self::MANAGE_THREAD.bits;

        /// (BASIC & !WAIT) | WRITE | GET_PROPERTY
        const DEFAULT_RESOURCE = (Self::BASIC.bits & !Self::WAIT.bits) | Self::WRITE.bits | Self::GET_PROPERTY.bits;

        /// BASIC | WRITE | SIGNAL
        const DEFAULT_DEBUGLOG = Self::BASIC.bits | Self::WRITE.bits | Self::SIGNAL.bits;

        /// TRANSFER | INSPECT
        const DEFAULT_SUSPEND_TOKEN = Self::TRANSFER.bits | Self::INSPECT.bits;

        /// (BASIC & !WAIT) | IO
        const DEFAULT_PORT = (Self::BASIC.bits & !Self::WAIT.bits) | Self::IO.bits;

        /// BASIC | WRITE | SIGNAL
        const DEFAULT_TIMER = Self::BASIC.bits | Self::WRITE.bits | Self::SIGNAL.bits;

        /// BASIC | IO | SIGNAL | PROPERTY | MAP
        const DEFAULT_CLOCK = Self::BASIC.bits | Self::IO.bits | Self::SIGNAL.bits | Self::PROPERTY.bits | Self::MAP.bits;

        /// BASIC | IO | SIGNAL
        const DEFAULT_COUNTER = Self::BASIC.bits | Self::IO.bits | Self::SIGNAL.bits;

        /// BASIC | SIGNAL
        const DEFAULT_EVENT = Self::BASIC.bits | Self::SIGNAL.bits;

        /// WAIT | DUPLICATE | TRANSFER
        const DEFAULT_SYSTEM_EVENT_LOW_MEMORY = Self::WAIT.bits | Self::DUPLICATE.bits | Self::TRANSFER.bits;

        /// BASIC | SIGNAL ｜ SIGNAL_PEER
        const DEFAULT_EVENTPAIR = Self::BASIC.bits | Self::SIGNAL.bits | Self::SIGNAL_PEER.bits;

        /// BASIC | IO | SIGNAL | SIGNAL_PEER
        const DEFAULT_FIFO = Self::BASIC.bits | Self::IO.bits | Self::SIGNAL.bits | Self::SIGNAL_PEER.bits;

        /// BASIC | IO | PROPERTY | SIGNAL | SIGNAL_PEER | MANAGE_SOCKET
        const DEFAULT_SOCKET = Self::BASIC.bits | Self::IO.bits | Self::PROPERTY.bits | Self::SIGNAL.bits | Self::SIGNAL_PEER.bits | Self::MANAGE_SOCKET.bits;

        /// BASIC | PROPERTY | SIGNAL
        const DEFAULT_STREAM = Self::BASIC.bits | Self::PROPERTY.bits | Self::SIGNAL.bits;

        /// (BASIC & !WAIT) | IO | PROPERTY | MAP
        const DEFAULT_BTI = (Self::BASIC.bits & !Self::WAIT.bits) | Self::IO.bits | Self::PROPERTY.bits | Self::MAP.bits;

        /// BASIC without WAIT: TRANSFER | DUPLICATE | INSPECT (Fuchsia IOMMU).
        /// Used to incorrectly install with `DEFAULT_CHANNEL` (IO/SIGNAL_PEER).
        const DEFAULT_IOMMU = Self::BASIC.bits & !Self::WAIT.bits;

        /// BASIC | IO | SIGNAL
        const DEFAULT_INTERRUPT = Self::BASIC.bits | Self::IO.bits | Self::SIGNAL.bits;

        /// BASIC | IO
        const DEFAULT_DEVICE = Self::BASIC.bits | Self::IO.bits;

        /// BASIC | IO | SIGNAL
        const DEFAULT_PCI_INTERRUPT = Self::BASIC.bits | Self::IO.bits | Self::SIGNAL.bits;

        /// TRANSFER | PROPERTY | INSPECT
        const DEFAULT_EXCEPTION = Self::TRANSFER.bits | Self::PROPERTY.bits | Self::INSPECT.bits;

        /// TRANSFER | DUPLICATE | WRITE | INSPECT | MANAGE_PROCESS
        ///
        /// `MANAGE_PROCESS`, not `MANAGE_THREAD`: `sys_vcpu_create` is the one
        /// caller that asks a `Guest` handle for either, and it asks for
        /// `MANAGE_PROCESS`. With `MANAGE_THREAD` here, every
        /// `zx_vcpu_create` on a handle from `zx_guest_create` answered
        /// `ACCESS_DENIED` -- no VCPU could be made, so the hypervisor was
        /// unusable from userspace -- and nothing in the tree ever asked a
        /// `Guest` for `MANAGE_THREAD`.
        const DEFAULT_GUEST = Self::TRANSFER.bits | Self::DUPLICATE.bits | Self::WRITE.bits | Self::INSPECT.bits | Self::MANAGE_PROCESS.bits;

        /// BASIC | IO | EXECUTE | SIGNAL
        const DEFAULT_VCPU = Self::BASIC.bits | Self::IO.bits | Self::EXECUTE.bits | Self::SIGNAL.bits;
    }
}

impl TryFrom<u32> for Rights {
    type Error = ZxError;

    fn try_from(x: u32) -> ZxResult<Self> {
        Self::from_bits(x).ok_or(ZxError::INVALID_ARGS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_try_from() {
        assert_eq!(Err(ZxError::INVALID_ARGS), Rights::try_from(0xffff_ffff));
        assert_eq!(Ok(Rights::SAME_RIGHTS), Rights::try_from(1 << 31));
    }

    /// A default right set is only useful if it covers what the syscalls ask of
    /// that kind of handle. `DEFAULT_GUEST` did not: `sys_vcpu_create` asks a
    /// `Guest` for `MANAGE_PROCESS` and the set granted `MANAGE_THREAD`, so
    /// `zx_vcpu_create` on a handle straight out of `zx_guest_create` could
    /// only ever answer `ACCESS_DENIED`.
    ///
    /// Spelled out rather than compared against the syscall crate, which this
    /// one cannot see.
    #[test]
    fn a_guest_handle_can_make_a_vcpu() {
        assert!(
            Rights::DEFAULT_GUEST.contains(Rights::MANAGE_PROCESS),
            "sys_vcpu_create asks a Guest for MANAGE_PROCESS"
        );
        assert!(
            !Rights::DEFAULT_GUEST.contains(Rights::MANAGE_THREAD),
            "nothing asks a Guest for MANAGE_THREAD"
        );
    }

    #[test]
    fn an_iommu_handle_is_not_a_channel() {
        // Used to install with DEFAULT_CHANNEL (IO | SIGNAL_PEER).
        assert!(!Rights::DEFAULT_IOMMU.contains(Rights::IO));
        assert!(!Rights::DEFAULT_IOMMU.contains(Rights::SIGNAL_PEER));
        assert!(Rights::DEFAULT_IOMMU.contains(Rights::DUPLICATE));
        assert!(Rights::DEFAULT_IOMMU.contains(Rights::INSPECT));
    }

    /// Every `DEFAULT_*` set names rights that exist, and none of them carries
    /// `SAME_RIGHTS`, which is the marker `zx_handle_duplicate` reads and not a
    /// right an object can hold.
    #[test]
    fn no_default_set_carries_the_same_rights_marker() {
        for (name, rights) in [
            ("CHANNEL", Rights::DEFAULT_CHANNEL),
            ("PROCESS", Rights::DEFAULT_PROCESS),
            ("THREAD", Rights::DEFAULT_THREAD),
            ("VMO", Rights::DEFAULT_VMO),
            ("VMAR", Rights::DEFAULT_VMAR),
            ("JOB", Rights::DEFAULT_JOB),
            ("RESOURCE", Rights::DEFAULT_RESOURCE),
            ("DEBUGLOG", Rights::DEFAULT_DEBUGLOG),
            ("SUSPEND_TOKEN", Rights::DEFAULT_SUSPEND_TOKEN),
            ("PORT", Rights::DEFAULT_PORT),
            ("TIMER", Rights::DEFAULT_TIMER),
            ("CLOCK", Rights::DEFAULT_CLOCK),
            ("COUNTER", Rights::DEFAULT_COUNTER),
            ("EVENT", Rights::DEFAULT_EVENT),
            ("EVENTPAIR", Rights::DEFAULT_EVENTPAIR),
            ("FIFO", Rights::DEFAULT_FIFO),
            ("SOCKET", Rights::DEFAULT_SOCKET),
            ("STREAM", Rights::DEFAULT_STREAM),
            ("BTI", Rights::DEFAULT_BTI),
            ("IOMMU", Rights::DEFAULT_IOMMU),
            ("INTERRUPT", Rights::DEFAULT_INTERRUPT),
            ("DEVICE", Rights::DEFAULT_DEVICE),
            ("PCI_INTERRUPT", Rights::DEFAULT_PCI_INTERRUPT),
            ("EXCEPTION", Rights::DEFAULT_EXCEPTION),
            ("GUEST", Rights::DEFAULT_GUEST),
            ("VCPU", Rights::DEFAULT_VCPU),
        ] {
            assert!(
                !rights.contains(Rights::SAME_RIGHTS),
                "DEFAULT_{} carries SAME_RIGHTS",
                name
            );
            assert!(!rights.is_empty(), "DEFAULT_{} is empty", name);
        }
    }

    /// A channel handle is not duplicatable, which is what makes a channel a
    /// point-to-point link rather than a broadcast.
    #[test]
    fn a_channel_handle_cannot_be_duplicated() {
        assert!(!Rights::DEFAULT_CHANNEL.contains(Rights::DUPLICATE));
        assert!(Rights::DEFAULT_CHANNEL.contains(Rights::TRANSFER));
    }
    /// Every right is its own bit. Nothing else in the tree says so, and the
    /// one test that happens to notice a collision only notices *one*:
    /// `a_handle_one_right_short_opens_nothing` asks for `READ | WRITE`, so
    /// putting `READ` on `WRITE`'s bit fails it, while putting `WAIT` on
    /// `INSPECT`'s bit passes everything green.
    ///
    /// Two rights sharing a bit is not a cosmetic mistake: it hands out an
    /// authority nobody granted. A handle created with one of them opens
    /// every door the other one guards, because `get_object_with_rights`
    /// compares bits and has no idea two names mean the same one.
    #[test]
    fn no_two_rights_share_a_bit() {
        let named: &[(&str, Rights)] = &[
            ("DUPLICATE", Rights::DUPLICATE),
            ("TRANSFER", Rights::TRANSFER),
            ("READ", Rights::READ),
            ("WRITE", Rights::WRITE),
            ("EXECUTE", Rights::EXECUTE),
            ("MAP", Rights::MAP),
            ("GET_PROPERTY", Rights::GET_PROPERTY),
            ("SET_PROPERTY", Rights::SET_PROPERTY),
            ("ENUMERATE", Rights::ENUMERATE),
            ("DESTROY", Rights::DESTROY),
            ("SET_POLICY", Rights::SET_POLICY),
            ("GET_POLICY", Rights::GET_POLICY),
            ("SIGNAL", Rights::SIGNAL),
            ("SIGNAL_PEER", Rights::SIGNAL_PEER),
            ("WAIT", Rights::WAIT),
            ("INSPECT", Rights::INSPECT),
            ("MANAGE_JOB", Rights::MANAGE_JOB),
            ("MANAGE_PROCESS", Rights::MANAGE_PROCESS),
            ("MANAGE_THREAD", Rights::MANAGE_THREAD),
            ("APPLY_PROFILE", Rights::APPLY_PROFILE),
            ("MANAGE_SOCKET", Rights::MANAGE_SOCKET),
            ("OP_CHILDREN", Rights::OP_CHILDREN),
            ("RESIZE", Rights::RESIZE),
            ("ATTACH_VMO", Rights::ATTACH_VMO),
            ("MANAGE_VMO", Rights::MANAGE_VMO),
            ("SAME_RIGHTS", Rights::SAME_RIGHTS),
        ];
        let mut union = Rights::empty();
        for (name, right) in named {
            assert_eq!(right.bits().count_ones(), 1, "{} is not a single bit", name);
            assert!(
                !union.contains(*right),
                "{} shares its bit with a right named earlier",
                name
            );
            union |= *right;
        }
        assert_eq!(
            union.bits().count_ones(),
            named.len() as u32,
            "the rights above do not add up to one bit each"
        );
    }

    /// A default right set is only useful if it covers what the syscalls ask
    /// of that kind of handle: a right left out of the set is an operation
    /// that can only ever answer `ACCESS_DENIED` on a handle straight out of
    /// the call that creates the object.
    ///
    /// Each row names the caller that asks, so the table says *why* rather
    /// than repeating the definitions above -- a test that restated the set
    /// would move along with it and catch nothing. `zircon-syscall` is the
    /// crate that asks, and this one cannot see it, so the callers are named
    /// rather than called.
    #[test]
    fn the_rights_the_syscalls_ask_for_are_in_each_default_set() {
        let asked: &[(&str, Rights, Rights, &str)] = &[
            ("VMO", Rights::DEFAULT_VMO, Rights::MAP, "sys_vmar_map"),
            (
                "VMO",
                Rights::DEFAULT_VMO,
                Rights::IO,
                "sys_vmo_read / sys_vmo_write",
            ),
            ("BTI", Rights::DEFAULT_BTI, Rights::MAP, "sys_bti_pin"),
            (
                "FIFO",
                Rights::DEFAULT_FIFO,
                Rights::SIGNAL_PEER,
                "sys_object_signal_peer",
            ),
            (
                "FIFO",
                Rights::DEFAULT_FIFO,
                Rights::WRITE,
                "sys_fifo_write",
            ),
            (
                "EVENTPAIR",
                Rights::DEFAULT_EVENTPAIR,
                Rights::SIGNAL_PEER,
                "sys_object_signal_peer",
            ),
            (
                "SOCKET",
                Rights::DEFAULT_SOCKET,
                Rights::MANAGE_SOCKET,
                "sys_socket_set_disposition",
            ),
            (
                "SOCKET",
                Rights::DEFAULT_SOCKET,
                Rights::WRITE,
                "sys_socket_write",
            ),
            (
                "SOCKET",
                Rights::DEFAULT_SOCKET,
                Rights::GET_PROPERTY,
                "sys_object_get_property",
            ),
            (
                "CHANNEL",
                Rights::DEFAULT_CHANNEL,
                Rights::SIGNAL_PEER,
                "sys_object_signal_peer",
            ),
            (
                "JOB",
                Rights::DEFAULT_JOB,
                Rights::SET_POLICY,
                "sys_job_set_policy",
            ),
            (
                "JOB",
                Rights::DEFAULT_JOB,
                Rights::MANAGE_JOB,
                "sys_job_create",
            ),
            ("JOB", Rights::DEFAULT_JOB, Rights::DESTROY, "sys_task_kill"),
            (
                "JOB",
                Rights::DEFAULT_JOB,
                Rights::ENUMERATE,
                "ZX_INFO_JOB_CHILDREN",
            ),
            (
                "THREAD",
                Rights::DEFAULT_THREAD,
                Rights::MANAGE_THREAD,
                "sys_task_suspend",
            ),
            (
                "THREAD",
                Rights::DEFAULT_THREAD,
                Rights::WRITE,
                "sys_process_start",
            ),
            (
                "PROCESS",
                Rights::DEFAULT_PROCESS,
                Rights::MANAGE_THREAD,
                "sys_thread_create",
            ),
            (
                "PROCESS",
                Rights::DEFAULT_PROCESS,
                Rights::WRITE,
                "sys_process_start",
            ),
            (
                "PROCESS",
                Rights::DEFAULT_PROCESS,
                Rights::SET_PROPERTY,
                "sys_object_set_property",
            ),
            (
                "PROCESS",
                Rights::DEFAULT_PROCESS,
                Rights::ENUMERATE,
                "ZX_INFO_PROCESS_THREADS",
            ),
            (
                "VCPU",
                Rights::DEFAULT_VCPU,
                Rights::EXECUTE,
                "sys_vcpu_resume",
            ),
            (
                "VCPU",
                Rights::DEFAULT_VCPU,
                Rights::SIGNAL,
                "sys_vcpu_interrupt",
            ),
            (
                "GUEST",
                Rights::DEFAULT_GUEST,
                Rights::MANAGE_PROCESS,
                "sys_vcpu_create",
            ),
            (
                "TIMER",
                Rights::DEFAULT_TIMER,
                Rights::WRITE,
                "sys_timer_set / sys_timer_cancel",
            ),
            (
                "DEBUGLOG",
                Rights::DEFAULT_DEBUGLOG,
                Rights::WRITE,
                "sys_debuglog_write",
            ),
            (
                "PORT",
                Rights::DEFAULT_PORT,
                Rights::WRITE,
                "sys_port_queue",
            ),
            ("PORT", Rights::DEFAULT_PORT, Rights::READ, "sys_port_wait"),
            (
                "INTERRUPT",
                Rights::DEFAULT_INTERRUPT,
                Rights::SIGNAL,
                "sys_interrupt_trigger",
            ),
            (
                "EXCEPTION",
                Rights::DEFAULT_EXCEPTION,
                Rights::TRANSFER,
                "handing the exception on",
            ),
        ];
        for (kind, set, needed, caller) in asked {
            assert!(
                set.contains(*needed),
                "DEFAULT_{} is missing {:?}, which {} asks for",
                kind,
                needed,
                caller
            );
        }
    }

    /// The five sets that leave `WAIT` out leave it out on purpose: these are
    /// the handles you do not wait on with `zx_object_wait_one`, which is the
    /// call that asks for `WAIT`.
    ///
    /// A port has its own `zx_port_wait`; a VMAR, a resource and a BTI raise
    /// no signals to wait for; and a suspend token is a receipt you drop to
    /// resume the thread, not something that ever becomes readable. Handing
    /// them `WAIT` does not open a door -- it parks the caller on a wait that
    /// nothing will ever satisfy, which reads as a hang rather than an error.
    #[test]
    fn the_handles_you_do_not_wait_on_do_not_carry_wait() {
        for (name, rights) in [
            ("PORT", Rights::DEFAULT_PORT),
            ("VMAR", Rights::DEFAULT_VMAR),
            ("RESOURCE", Rights::DEFAULT_RESOURCE),
            ("BTI", Rights::DEFAULT_BTI),
            ("SUSPEND_TOKEN", Rights::DEFAULT_SUSPEND_TOKEN),
        ] {
            assert!(
                !rights.contains(Rights::WAIT),
                "DEFAULT_{} carries WAIT, so zx_object_wait_one on it would park forever",
                name
            );
        }
        // And the ones that are waited on that way do carry it, so the test
        // above is not passing for want of anyone holding WAIT at all.
        for (name, rights) in [
            ("EVENT", Rights::DEFAULT_EVENT),
            ("CHANNEL", Rights::DEFAULT_CHANNEL),
            ("PROCESS", Rights::DEFAULT_PROCESS),
        ] {
            assert!(
                rights.contains(Rights::WAIT),
                "DEFAULT_{} cannot be waited on",
                name
            );
        }
    }

    /// An exception handle is not duplicatable, and that is what makes
    /// "the last handle" mean anything: letting go of it is what resumes the
    /// faulting thread. With a second copy in play, the handler dropping its
    /// end would leave the thread parked on a handle it cannot see, which is
    /// the one failure mode a debugger cannot diagnose from the outside.
    ///
    /// A suspend token is the same shape for the same reason.
    #[test]
    fn the_handles_whose_last_drop_means_something_cannot_be_duplicated() {
        assert!(!Rights::DEFAULT_EXCEPTION.contains(Rights::DUPLICATE));
        assert!(!Rights::DEFAULT_SUSPEND_TOKEN.contains(Rights::DUPLICATE));
        // Both are still transferable: handing the exception to another
        // process is how a debugger further up the chain gets it.
        assert!(Rights::DEFAULT_EXCEPTION.contains(Rights::TRANSFER));
        assert!(Rights::DEFAULT_SUSPEND_TOKEN.contains(Rights::TRANSFER));
    }
}
