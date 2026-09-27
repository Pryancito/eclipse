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
}
