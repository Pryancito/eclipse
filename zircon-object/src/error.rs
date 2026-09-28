/// The type returned by kernel objects methods.
pub type ZxResult<T = ()> = Result<T, ZxError>;

/// Zircon statuses are signed 32 bit integers. The space of values is
/// divided as follows:
/// - The zero value is for the OK status.
/// - Negative values are defined by the system, in this file.
/// - Positive values are reserved for protocol-specific error values,
///   and will never be defined by the system.
#[allow(non_camel_case_types)]
#[allow(clippy::upper_case_acronyms)]
#[repr(i32)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ZxError {
    /// Success.
    OK = 0,

    // ======= Internal failures =======
    /// The system encountered an otherwise unspecified error
    /// while performing the operation.
    INTERNAL = -1,

    /// The operation is not implemented, supported,
    /// or enabled.
    NOT_SUPPORTED = -2,

    /// The system was not able to allocate some resource
    /// needed for the operation.
    NO_RESOURCES = -3,

    /// The system was not able to allocate memory needed
    /// for the operation.
    NO_MEMORY = -4,

    // -5 used to be ZX_ERR_CALL_FAILED.
    /// The system call was interrupted, but should be
    /// retried.  This should not be seen outside of the VDSO.
    INTERNAL_INTR_RETRY = -6,

    // ======= Parameter errors =======
    /// an argument is invalid, ex. null pointer
    INVALID_ARGS = -10,

    /// A specified handle value does not refer to a handle.
    BAD_HANDLE = -11,

    /// The subject of the operation is the wrong type to
    /// perform the operation.
    /// Example: Attempting a message_read on a thread handle.
    WRONG_TYPE = -12,

    /// The specified syscall number is invalid.
    BAD_SYSCALL = -13,

    /// An argument is outside the valid range for this
    /// operation.
    OUT_OF_RANGE = -14,

    /// A caller provided buffer is too small for
    /// this operation.
    BUFFER_TOO_SMALL = -15,

    // ======= Precondition or state errors =======
    /// operation failed because the current state of the
    /// object does not allow it, or a precondition of the operation is
    /// not satisfied
    BAD_STATE = -20,

    /// The time limit for the operation elapsed before
    /// the operation completed.
    TIMED_OUT = -21,

    /// The operation cannot be performed currently but
    /// potentially could succeed if the caller waits for a prerequisite
    /// to be satisfied, for example waiting for a handle to be readable
    /// or writable.
    /// Example: Attempting to read from a channel that has no
    /// messages waiting but has an open remote will return ZX_ERR_SHOULD_WAIT.
    /// Attempting to read from a channel that has no messages waiting
    /// and has a closed remote end will return ZX_ERR_PEER_CLOSED.
    SHOULD_WAIT = -22,

    /// The in-progress operation (e.g. a wait) has been
    /// canceled.
    CANCELED = -23,

    /// The operation failed because the remote end of the
    /// subject of the operation was closed.
    PEER_CLOSED = -24,

    /// The requested entity is not found.
    NOT_FOUND = -25,

    /// An object with the specified identifier
    /// already exists.
    /// Example: Attempting to create a file when a file already exists
    /// with that name.
    ALREADY_EXISTS = -26,

    /// The operation failed because the named entity
    /// is already owned or controlled by another entity. The operation
    /// could succeed later if the current owner releases the entity.
    ALREADY_BOUND = -27,

    /// The subject of the operation is currently unable
    /// to perform the operation.
    /// Note: This is used when there's no direct way for the caller to
    /// observe when the subject will be able to perform the operation
    /// and should thus retry.
    UNAVAILABLE = -28,

    // ======= Permission check errors =======
    /// The caller did not have permission to perform
    /// the specified operation.
    ACCESS_DENIED = -30,

    // ======= Input-output errors =======
    /// Otherwise unspecified error occurred during I/O.
    IO = -40,

    /// The entity the I/O operation is being performed on
    /// rejected the operation.
    /// Example: an I2C device NAK'ing a transaction or a disk controller
    /// rejecting an invalid command, or a stalled USB endpoint.
    IO_REFUSED = -41,

    /// The data in the operation failed an integrity
    /// check and is possibly corrupted.
    /// Example: CRC or Parity error.
    IO_DATA_INTEGRITY = -42,

    /// The data in the operation is currently unavailable
    /// and may be permanently lost.
    /// Example: A disk block is irrecoverably damaged.
    IO_DATA_LOSS = -43,

    /// The device is no longer available (has been
    /// unplugged from the system, powered down, or the driver has been
    /// unloaded,
    IO_NOT_PRESENT = -44,

    /// More data was received from the device than expected.
    /// Example: a USB "babble" error due to a device sending more data than
    /// the host queued to receive.
    IO_OVERRUN = -45,

    /// An operation did not complete within the required timeframe.
    /// Example: A USB isochronous transfer that failed to complete due to an overrun or underrun.
    IO_MISSED_DEADLINE = -46,

    /// The data in the operation is invalid parameter or is out of range.
    /// Example: A USB transfer that failed to complete with TRB Error
    IO_INVALID = -47,

    // ======== Filesystem Errors ========
    /// Path name is too long.
    BAD_PATH = -50,

    /// Object is not a directory or does not support
    /// directory operations.
    /// Example: Attempted to open a file as a directory or
    /// attempted to do directory operations on a file.
    NOT_DIR = -51,

    /// Object is not a regular file.
    NOT_FILE = -52,

    /// This operation would cause a file to exceed a
    /// filesystem-specific size limit
    FILE_BIG = -53,

    /// Filesystem or device space is exhausted.
    NO_SPACE = -54,

    /// Directory is not empty.
    NOT_EMPTY = -55,

    // ======== Flow Control ========
    // These are not errors, as such, and will never be returned
    // by a syscall or public API.  They exist to allow callbacks
    // to request changes in operation.
    /// Do not call again.
    /// Example: A notification callback will be called on every
    /// event until it returns something other than ZX_OK.
    /// This status allows differentiation between "stop due to
    /// an error" and "stop because the work is done."
    STOP = -60,

    /// Advance to the next item.
    /// Example: A notification callback will use this response
    /// to indicate it did not "consume" an item passed to it,
    /// but by choice, not due to an error condition.
    NEXT = -61,

    /// Ownership of the item has moved to an asynchronous worker.
    ///
    /// Unlike ZX_ERR_STOP, which implies that iteration on an object
    /// should stop, and ZX_ERR_NEXT, which implies that iteration
    /// should continue to the next item, ZX_ERR_ASYNC implies
    /// that an asynchronous worker is responsible for continuing iteration.
    ///
    /// Example: A notification callback will be called on every
    /// event, but one event needs to handle some work asynchronously
    /// before it can continue. ZX_ERR_ASYNC implies the worker is
    /// responsible for resuming iteration once its work has completed.
    ASYNC = -62,

    // ======== Network-related errors ========
    /// Specified protocol is not
    /// supported.
    PROTOCOL_NOT_SUPPORTED = -70,

    /// Host is unreachable.
    ADDRESS_UNREACHABLE = -71,

    /// Address is being used by someone else.
    ADDRESS_IN_USE = -72,

    /// Socket is not connected.
    NOT_CONNECTED = -73,

    /// Remote peer rejected the connection.
    CONNECTION_REFUSED = -74,

    /// Connection was reset.
    CONNECTION_RESET = -75,

    /// Connection was aborted.
    CONNECTION_ABORTED = -76,
}

use kernel_hal::user::Error;

impl From<Error> for ZxError {
    fn from(e: Error) -> Self {
        match e {
            Error::InvalidUtf8 => ZxError::INVALID_ARGS,
            Error::InvalidPointer => ZxError::INVALID_ARGS,
            Error::BufferTooSmall => ZxError::BUFFER_TOO_SMALL,
            Error::InvalidLength => ZxError::INVALID_ARGS,
            Error::InvalidVectorAddress => ZxError::NOT_FOUND,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::vec::Vec;

    /// Every value, exactly as Zircon's `errors.h` defines it.
    ///
    /// These numbers are an ABI: a syscall returns one of them to a userspace
    /// program that was built against Fuchsia's headers, and `linux-object`
    /// maps them on to errnos for the Linux personality. A wrong one is a
    /// program told the wrong thing -- `NOT_FOUND` where the file was really
    /// unreadable, `SHOULD_WAIT` where the peer had closed -- and it cannot be
    /// caught by anything downstream, because every downstream reader believes
    /// this table. Nothing in the tree pinned them.
    const ABI: &[(ZxError, i32)] = &[
        (ZxError::OK, 0),
        (ZxError::INTERNAL, -1),
        (ZxError::NOT_SUPPORTED, -2),
        (ZxError::NO_RESOURCES, -3),
        (ZxError::NO_MEMORY, -4),
        (ZxError::INTERNAL_INTR_RETRY, -6),
        (ZxError::INVALID_ARGS, -10),
        (ZxError::BAD_HANDLE, -11),
        (ZxError::WRONG_TYPE, -12),
        (ZxError::BAD_SYSCALL, -13),
        (ZxError::OUT_OF_RANGE, -14),
        (ZxError::BUFFER_TOO_SMALL, -15),
        (ZxError::BAD_STATE, -20),
        (ZxError::TIMED_OUT, -21),
        (ZxError::SHOULD_WAIT, -22),
        (ZxError::CANCELED, -23),
        (ZxError::PEER_CLOSED, -24),
        (ZxError::NOT_FOUND, -25),
        (ZxError::ALREADY_EXISTS, -26),
        (ZxError::ALREADY_BOUND, -27),
        (ZxError::UNAVAILABLE, -28),
        (ZxError::ACCESS_DENIED, -30),
        (ZxError::IO, -40),
        (ZxError::IO_REFUSED, -41),
        (ZxError::IO_DATA_INTEGRITY, -42),
        (ZxError::IO_DATA_LOSS, -43),
        (ZxError::IO_NOT_PRESENT, -44),
        (ZxError::IO_OVERRUN, -45),
        (ZxError::IO_MISSED_DEADLINE, -46),
        (ZxError::IO_INVALID, -47),
        (ZxError::BAD_PATH, -50),
        (ZxError::NOT_DIR, -51),
        (ZxError::NOT_FILE, -52),
        (ZxError::FILE_BIG, -53),
        (ZxError::NO_SPACE, -54),
        (ZxError::NOT_EMPTY, -55),
        (ZxError::STOP, -60),
        (ZxError::NEXT, -61),
        (ZxError::ASYNC, -62),
        (ZxError::PROTOCOL_NOT_SUPPORTED, -70),
        (ZxError::ADDRESS_UNREACHABLE, -71),
        (ZxError::ADDRESS_IN_USE, -72),
        (ZxError::NOT_CONNECTED, -73),
        (ZxError::CONNECTION_REFUSED, -74),
        (ZxError::CONNECTION_RESET, -75),
        (ZxError::CONNECTION_ABORTED, -76),
    ];

    #[test]
    fn every_status_is_the_number_zircon_defines() {
        for (e, want) in ABI {
            assert_eq!(*e as i32, *want, "{e:?}");
        }
    }

    #[test]
    fn no_two_statuses_share_a_number() {
        // A copy-pasted arm with the neighbour's number gives two names for one
        // value, and then `match` on the status silently takes the first arm
        // for both -- which looks like the operation returning the wrong error
        // and not like a typo in a table.
        let mut seen: Vec<i32> = ABI.iter().map(|(e, _)| *e as i32).collect();
        let n = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), n, "dos estados comparten numero");
        assert_eq!(n, 46, "OK y 45 errores");
    }

    #[test]
    fn the_table_above_covers_every_variant_of_the_enum() {
        // A variant added to the enum and not to the table would be a number
        // nothing checks, which is the whole hole this file had. Rust cannot
        // enumerate an enum without a derive, so the count is taken from the
        // source: every variant of `ZxError` is one `NAME = n,` line, and they
        // are the only such lines in the file outside this module.
        let src = include_str!("error.rs");
        let decls = src
            .split("#[cfg(test)]")
            .next()
            .unwrap()
            .lines()
            .map(str::trim)
            .filter(|l| {
                l.ends_with(',')
                    && l.contains(" = ")
                    && l.split(" = ").next().map(|n| {
                        !n.is_empty()
                            && n.bytes()
                                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                    }) == Some(true)
            })
            .count();
        assert_eq!(
            decls,
            ABI.len(),
            "el enum tiene {} variantes y la tabla {}",
            decls,
            ABI.len()
        );
    }

    #[test]
    fn ok_is_the_only_status_that_is_not_negative() {
        // The doc comment at the top of this file states the rule: zero is OK,
        // the system defines the negatives, and the positives belong to
        // protocols and will NEVER be defined here. A positive added by
        // mistake would be read by a caller as a protocol's own value.
        for (e, _) in ABI {
            let v = *e as i32;
            if matches!(e, ZxError::OK) {
                assert_eq!(v, 0);
            } else {
                assert!(v < 0, "{:?} vale {}, que no es negativo", e, v);
            }
        }
        assert_eq!(ZxError::OK as i32, 0);
    }

    #[test]
    fn a_status_is_a_plain_i32_the_syscall_layer_can_return() {
        // `repr(i32)`, so it goes back to userspace as the four bytes a Zircon
        // program expects. A wider repr and the value read on the other side
        // is whatever half of it the register held.
        assert_eq!(core::mem::size_of::<ZxError>(), 4);
        assert_eq!(core::mem::align_of::<ZxError>(), 4);
        // And `ZxResult` carries it without growing: a niche-free enum would
        // make every kernel result one word wider.
        assert_eq!(
            core::mem::size_of::<ZxResult<()>>(),
            core::mem::size_of::<ZxError>()
        );
    }

    #[test]
    fn a_bad_user_pointer_is_not_reported_as_a_bad_length() {
        // The bridge from `kernel_hal::user`: these are the errors a syscall
        // gets when it touches a userspace buffer, and the status it hands back
        // is what the program's libc turns into an errno. Three different
        // causes collapse on to INVALID_ARGS on purpose -- the point of the
        // test is the two that must NOT.
        assert_eq!(ZxError::from(Error::InvalidUtf8), ZxError::INVALID_ARGS);
        assert_eq!(ZxError::from(Error::InvalidPointer), ZxError::INVALID_ARGS);
        assert_eq!(ZxError::from(Error::InvalidLength), ZxError::INVALID_ARGS);
        assert_eq!(
            ZxError::from(Error::BufferTooSmall),
            ZxError::BUFFER_TOO_SMALL,
            "un buffer corto tiene su propio estado, que es el que dice cuanto pedir"
        );
        assert_eq!(
            ZxError::from(Error::InvalidVectorAddress),
            ZxError::NOT_FOUND
        );
        // Nothing in the bridge maps on to OK: every one of these is a failure,
        // and a mapping that returned OK would make the syscall report success
        // after touching memory it could not read.
        for e in [
            Error::InvalidUtf8,
            Error::InvalidPointer,
            Error::BufferTooSmall,
            Error::InvalidLength,
            Error::InvalidVectorAddress,
        ] {
            assert_ne!(ZxError::from(e), ZxError::OK, "{e:?}");
        }
    }

    #[test]
    fn a_status_compares_and_copies_by_value() {
        // `ZxResult` is returned by nearly every method in this crate and
        // compared with `==` all over it; `Copy` is what lets an error be
        // matched and then returned again without a clone.
        let a = ZxError::BAD_STATE;
        let b = a;
        assert_eq!(a, b);
        assert_ne!(ZxError::BAD_STATE, ZxError::BAD_HANDLE);
        let r: ZxResult<u32> = Err(ZxError::TIMED_OUT);
        assert_eq!(r, Err(ZxError::TIMED_OUT));
        assert_eq!(ZxResult::<u32>::Ok(7), Ok(7));
        // Debug names the variant, which is what every log line in the kernel
        // prints when a syscall fails.
        assert_eq!(format!("{:?}", ZxError::PEER_CLOSED), "PEER_CLOSED");
    }
}
