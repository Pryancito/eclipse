//! Linux error codes
use core::fmt;
use rcore_fs::vfs::FsError;
use zircon_object::ZxError;

/// Linux Result defination
pub type LxResult<T = ()> = Result<T, LxError>;
/// SysResult Result defination (same as Linux Result)
pub type SysResult = LxResult<usize>;

/// Linux error codes defination
#[allow(dead_code)]
#[repr(isize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LxError {
    /// Undefined
    EUNDEF = 0,
    /// Operation not permitted
    EPERM = 1,
    /// No such file or directory
    ENOENT = 2,
    /// No such process
    ESRCH = 3,
    /// Interrupted system call
    EINTR = 4,
    /// I/O error
    EIO = 5,
    /// No such device or address
    ENXIO = 6,
    /// Arg list too long
    E2BIG = 7,
    /// Exec format error
    ENOEXEC = 8,
    /// Bad file number
    EBADF = 9,
    /// No child processes
    ECHILD = 10,
    /// Try again
    EAGAIN = 11,
    /// Out of memory
    ENOMEM = 12,
    /// Permission denied
    EACCES = 13,
    /// Bad address
    EFAULT = 14,
    /// Block device required
    ENOTBLK = 15,
    /// Device or resource busy
    EBUSY = 16,
    /// File exists
    EEXIST = 17,
    /// Cross-device link
    EXDEV = 18,
    /// No such device
    ENODEV = 19,
    /// Not a directory
    ENOTDIR = 20,
    /// Is a directory
    EISDIR = 21,
    /// Invalid argument
    EINVAL = 22,
    /// File table overflow
    ENFILE = 23,
    /// Too many open files
    EMFILE = 24,
    /// Not a tty device
    ENOTTY = 25,
    /// Text file busy
    ETXTBSY = 26,
    /// File too large
    EFBIG = 27,
    /// No space left on device
    ENOSPC = 28,
    /// Illegal seek
    ESPIPE = 29,
    /// Read-only file system
    EROFS = 30,
    /// Too many links
    EMLINK = 31,
    /// Broken pipe
    EPIPE = 32,
    /// Math argument out of domain
    EDOM = 33,
    /// Math result not representable
    ERANGE = 34,
    /// Resource deadlock would occur
    EDEADLK = 35,
    /// Filename too long
    ENAMETOOLONG = 36,
    /// No record locks available
    ENOLCK = 37,
    /// Function not implemented
    ENOSYS = 38,
    /// Directory not empty
    ENOTEMPTY = 39,
    /// Too many symbolic links encountered
    ELOOP = 40,
    /// No message of desired type
    ENOMSG = 42,
    /// Identifier removed
    EIDRM = 43,
    /// No data available (e.g. no such extended attribute)
    ENODATA = 61,
    /// Timer expired -- the errno `drm_syncobj_wait` returns on timeout, which
    /// Mesa checks for explicitly (`errno == ETIME` -> `VK_TIMEOUT`); any other
    /// errno there is treated as a lost device.
    ETIME = 62,
    /// File descriptor in bad state -- the fd is valid, the object behind it
    /// is not in a state that allows the operation. ALSA returns it from
    /// `writei`/`readi` on a stream that is neither prepared nor running.
    EBADFD = 77,
    /// Value too large for defined data type. `fcntl(2)`'s record locks
    /// return it when `l_whence` + `l_start` + `l_len` resolve to a range
    /// that will not fit an `off_t`.
    EOVERFLOW = 75,
    /// Socket operation on non-socket
    ENOTSOCK = 88,
    /// Protocol not available
    ENOPROTOOPT = 92,
    /// Operation not supported
    EOPNOTSUPP = 95,
    /// Protocol family not supported
    EPFNOSUPPORT = 96,
    /// Address family not supported by protocol
    EAFNOSUPPORT = 97,
    /// Connection reset by peer
    ECONNRESET = 104,
    /// No buffer space available
    ENOBUFS = 105,
    /// Transport endpoint is already connected
    EISCONN = 106,
    /// Transport endpoint is not connected
    ENOTCONN = 107,
    /// Connection timeout
    ETIMEDOUT = 110,
    /// Connection refused
    ECONNREFUSED = 111,
    /// Operation now in progress (non-blocking connect)
    EINPROGRESS = 115,
    /// Address already in use
    EADDRINUSE = 98,
    /// Cannot assign requested address
    EADDRNOTAVAIL = 99,
    /// Message too long
    EMSGSIZE = 90,
    /// Operation already in progress
    EALREADY = 114,
}

#[allow(non_snake_case)]
impl fmt::Display for LxError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::LxError::*;
        let explain = match self {
            EPERM => "Operation not permitted",
            ENOENT => "No such file or directory",
            ESRCH => "No such process",
            EINTR => "Interrupted system call",
            EIO => "I/O error",
            ENXIO => "No such device or address",
            E2BIG => "Argument list too long",
            ENOEXEC => "Exec format error",
            EBADF => "Bad file number",
            ECHILD => "No child processes",
            EAGAIN => "Try again",
            ENOMEM => "Out of memory",
            EACCES => "Permission denied",
            EFAULT => "Bad address",
            ENOTBLK => "Block device required",
            EBUSY => "Device or resource busy",
            EEXIST => "File exists",
            EXDEV => "Cross-device link",
            ENODEV => "No such device",
            ENOTDIR => "Not a directory",
            EISDIR => "Is a directory",
            EINVAL => "Invalid argument",
            ENFILE => "File table overflow",
            EMFILE => "Too many open files",
            ENOTTY => "Not a typewriter",
            ETXTBSY => "Text file busy",
            EFBIG => "File too large",
            ENOSPC => "No space left on device",
            ESPIPE => "Illegal seek",
            EROFS => "Read-only file system",
            EMLINK => "Too many links",
            EPIPE => "Broken pipe",
            EDOM => "Math argument out of domain of func",
            ERANGE => "Math result not representable",
            EDEADLK => "Resource deadlock would occur",
            ENAMETOOLONG => "File name too long",
            ENOLCK => "No record locks available",
            EOVERFLOW => "Value too large for defined data type",
            ENOSYS => "Function not implemented",
            ENOTEMPTY => "Directory not empty",
            ELOOP => "Too many symbolic links encountered",
            ENOMSG => "No message of desired type",
            EIDRM => "Identifier removed",
            ENODATA => "No data available",
            ETIME => "Timer expired",
            EBADFD => "File descriptor in bad state",
            ENOTSOCK => "Socket operation on non-socket",
            ENOPROTOOPT => "Protocol not available",
            EOPNOTSUPP => "Operation not supported",
            EPFNOSUPPORT => "Protocol family not supported",
            EAFNOSUPPORT => "Address family not supported by protocol",
            ECONNRESET => "Connection reset by peer",
            ENOBUFS => "No buffer space available",
            EISCONN => "Transport endpoint is already connected",
            ENOTCONN => "Transport endpoint is not connected",
            ETIMEDOUT => "Connection timed out",
            ECONNREFUSED => "Connection refused",
            EINPROGRESS => "Operation in progress",
            EADDRINUSE => "Address already in use",
            EADDRNOTAVAIL => "Cannot assign requested address",
            EMSGSIZE => "Message too long",
            EALREADY => "Operation already in progress",
            _ => "Unknown error",
        };
        write!(f, "{}", explain)
    }
}

impl From<ZxError> for LxError {
    fn from(e: ZxError) -> Self {
        match e {
            ZxError::INVALID_ARGS => LxError::EINVAL,
            ZxError::NOT_SUPPORTED => LxError::ENOSYS,
            ZxError::ALREADY_EXISTS => LxError::EEXIST,
            ZxError::SHOULD_WAIT => LxError::EAGAIN,
            ZxError::PEER_CLOSED => LxError::EPIPE,
            ZxError::BAD_HANDLE => LxError::EBADF,
            ZxError::TIMED_OUT => LxError::ETIMEDOUT,
            ZxError::STOP => LxError::ESRCH,
            ZxError::BAD_STATE => LxError::EAGAIN,
            // Physical-frame exhaustion must surface as ENOMEM to the caller,
            // never take the kernel down: seen live as multi-CPU panics when
            // foot's render load drained the frame pool ("frame_alloc FAILED
            // ... 1646 MiB used / 1647 MiB managed" followed by one panic per
            // CPU that hit the exhausted allocator).
            ZxError::NO_MEMORY => LxError::ENOMEM,
            ZxError::NO_RESOURCES => LxError::ENOMEM,
            ZxError::ACCESS_DENIED => LxError::EACCES,
            ZxError::NOT_FOUND => LxError::ENOENT,
            ZxError::OUT_OF_RANGE => LxError::EINVAL,
            ZxError::BUFFER_TOO_SMALL => LxError::EINVAL,
            ZxError::UNAVAILABLE => LxError::EBUSY,
            ZxError::CANCELED => LxError::EINTR,
            ZxError::NOT_DIR => LxError::ENOTDIR,
            ZxError::NOT_FILE => LxError::EISDIR,
            ZxError::FILE_BIG => LxError::EFBIG,
            ZxError::NO_SPACE => LxError::ENOSPC,
            ZxError::IO => LxError::EIO,
            // Anything else is still a real error for the caller, not a
            // reason to bring the machine down: default to EIO and log it.
            other => {
                log::error!("ZxError -> LxError fallback: {:?} mapped to EIO", other);
                LxError::EIO
            }
        }
    }
}

impl From<FsError> for LxError {
    fn from(error: FsError) -> Self {
        match error {
            FsError::NotSupported => LxError::ENOSYS,
            FsError::NotFile => LxError::EISDIR,
            FsError::IsDir => LxError::EISDIR,
            FsError::NotDir => LxError::ENOTDIR,
            FsError::EntryNotFound => LxError::ENOENT,
            FsError::EntryExist => LxError::EEXIST,
            FsError::NotSameFs => LxError::EXDEV,
            FsError::InvalidParam => LxError::EINVAL,
            // ENOSPC, not ENOMEM: this is "the filesystem is full", and it is
            // the errno every caller actually tests for. A `write` that
            // answers ENOMEM instead makes a program report the machine out of
            // memory and, worse, retry -- musl's stdio, package managers and
            // sqlite all branch on ENOSPC specifically and on nothing else.
            FsError::NoDeviceSpace => LxError::ENOSPC,
            FsError::DirRemoved => LxError::ENOENT,
            FsError::DirNotEmpty => LxError::ENOTEMPTY,
            FsError::WrongFs => LxError::EINVAL,
            FsError::DeviceError => LxError::EIO,
            FsError::IOCTLError => LxError::EINVAL,
            // ENODEV, not EINVAL: this is "the device is not there", and
            // callers act on the difference -- mesa's nouveau winsys tests
            // specifically for -ENODEV to decide a channel was killed, and a
            // driver that carefully returns ENODEV only for userspace to read
            // EINVAL makes every such check silently wrong.
            FsError::NoDevice => LxError::ENODEV,
            FsError::Again => LxError::EAGAIN,
            FsError::TimedOut => LxError::ETIME,
            FsError::SymLoop => LxError::ELOOP,
            FsError::Busy => LxError::EBUSY,
            FsError::ReadOnly => LxError::EROFS,
            FsError::Interrupted => LxError::EINTR,
            FsError::NoPermission => LxError::EACCES,
            FsError::OpNotSupported => LxError::EOPNOTSUPP,
            FsError::BadAddress => LxError::EFAULT,
            FsError::BadState => LxError::EBADFD,
            FsError::Broken => LxError::EPIPE,
            FsError::NoSuchDeviceOrAddress => LxError::ENXIO,
        }
    }
}

use kernel_hal::user::Error;

impl From<Error> for LxError {
    fn from(e: Error) -> Self {
        match e {
            Error::InvalidUtf8 => LxError::EINVAL,
            Error::InvalidPointer => LxError::EFAULT,
            Error::BufferTooSmall => LxError::ENOBUFS,
            Error::InvalidLength => LxError::EINVAL,
            Error::InvalidVectorAddress => LxError::EINVAL,
        }
    }
}

#[cfg(test)]
mod errno_tests {
    use super::*;

    /// The numbers, checked against `asm-generic/errno.h` (which x86_64,
    /// aarch64 and riscv64 all use). These are the values userspace compares
    /// against, so a table that drifts is a program taking the wrong branch.
    #[test]
    fn the_numbers_are_the_ones_userspace_knows() {
        for (err, n) in [
            (LxError::EPERM, 1),
            (LxError::ENOENT, 2),
            (LxError::ESRCH, 3),
            (LxError::EINTR, 4),
            (LxError::EIO, 5),
            (LxError::ENXIO, 6),
            (LxError::EBADF, 9),
            (LxError::EAGAIN, 11),
            (LxError::ENOMEM, 12),
            (LxError::EACCES, 13),
            (LxError::EFAULT, 14),
            (LxError::EBUSY, 16),
            (LxError::EEXIST, 17),
            (LxError::EXDEV, 18),
            (LxError::ENODEV, 19),
            (LxError::ENOTDIR, 20),
            (LxError::EISDIR, 21),
            (LxError::EINVAL, 22),
            (LxError::ENOTTY, 25),
            (LxError::EFBIG, 27),
            (LxError::ENOSPC, 28),
            (LxError::ESPIPE, 29),
            (LxError::EROFS, 30),
            (LxError::EPIPE, 32),
            (LxError::ERANGE, 34),
            (LxError::ENOSYS, 38),
            (LxError::ENOTEMPTY, 39),
            (LxError::ELOOP, 40),
            (LxError::ETIME, 62),
            (LxError::EOVERFLOW, 75),
            (LxError::EBADFD, 77),
            (LxError::ENOTSOCK, 88),
            (LxError::EOPNOTSUPP, 95),
            (LxError::ETIMEDOUT, 110),
            (LxError::ECONNREFUSED, 111),
            (LxError::EALREADY, 114),
            (LxError::EINPROGRESS, 115),
        ] {
            assert_eq!(err as isize, n, "{:?}", err);
        }
    }

    #[test]
    fn a_full_filesystem_says_so_instead_of_blaming_memory() {
        // ENOSPC is the errno every caller tests for when a write fails --
        // musl's stdio, package managers and sqlite branch on it and on
        // nothing else. Answering ENOMEM made a full disk look like a machine
        // out of memory, which is both the wrong message and, for anything
        // that retries on ENOMEM, the wrong action.
        assert_eq!(LxError::from(FsError::NoDeviceSpace), LxError::ENOSPC);
        assert_ne!(LxError::from(FsError::NoDeviceSpace), LxError::ENOMEM);
    }

    /// Every `FsError` the filesystem layer can raise, and the errno it must
    /// reach userspace as. Written out rather than derived so a change to the
    /// mapping has to be made twice, on purpose.
    #[test]
    fn every_filesystem_error_maps_to_the_errno_it_means() {
        for (fs, lx) in [
            (FsError::NotSupported, LxError::ENOSYS),
            (FsError::NotFile, LxError::EISDIR),
            (FsError::IsDir, LxError::EISDIR),
            (FsError::NotDir, LxError::ENOTDIR),
            (FsError::EntryNotFound, LxError::ENOENT),
            (FsError::EntryExist, LxError::EEXIST),
            (FsError::NotSameFs, LxError::EXDEV),
            (FsError::InvalidParam, LxError::EINVAL),
            (FsError::NoDeviceSpace, LxError::ENOSPC),
            (FsError::DirRemoved, LxError::ENOENT),
            (FsError::DirNotEmpty, LxError::ENOTEMPTY),
            (FsError::WrongFs, LxError::EINVAL),
            (FsError::DeviceError, LxError::EIO),
            (FsError::IOCTLError, LxError::EINVAL),
            (FsError::NoDevice, LxError::ENODEV),
            (FsError::Again, LxError::EAGAIN),
            (FsError::TimedOut, LxError::ETIME),
            (FsError::SymLoop, LxError::ELOOP),
            (FsError::Busy, LxError::EBUSY),
            (FsError::ReadOnly, LxError::EROFS),
            (FsError::Interrupted, LxError::EINTR),
            (FsError::NoPermission, LxError::EACCES),
            (FsError::OpNotSupported, LxError::EOPNOTSUPP),
            (FsError::BadAddress, LxError::EFAULT),
            (FsError::BadState, LxError::EBADFD),
            (FsError::Broken, LxError::EPIPE),
            (FsError::NoSuchDeviceOrAddress, LxError::ENXIO),
        ] {
            let name = alloc::format!("{:?}", fs);
            assert_eq!(LxError::from(fs), lx, "{}", name);
        }
    }

    #[test]
    fn a_missing_device_is_enodev_and_not_einval() {
        // mesa's nouveau winsys tests for -ENODEV specifically to decide a
        // channel was killed; a driver that carefully returns it only for
        // userspace to read EINVAL makes every such check silently wrong.
        assert_eq!(LxError::from(FsError::NoDevice), LxError::ENODEV);
    }

    #[test]
    fn running_out_of_frames_is_enomem_and_never_a_panic() {
        // Seen live as one panic per CPU when foot's render load drained the
        // frame pool. It has to surface to the caller as an errno.
        assert_eq!(LxError::from(ZxError::NO_MEMORY), LxError::ENOMEM);
        assert_eq!(LxError::from(ZxError::NO_RESOURCES), LxError::ENOMEM);
    }

    #[test]
    fn the_kernel_errors_userspace_acts_on_map_to_their_posix_names() {
        for (zx, lx) in [
            (ZxError::INVALID_ARGS, LxError::EINVAL),
            (ZxError::NOT_SUPPORTED, LxError::ENOSYS),
            (ZxError::ALREADY_EXISTS, LxError::EEXIST),
            (ZxError::SHOULD_WAIT, LxError::EAGAIN),
            (ZxError::PEER_CLOSED, LxError::EPIPE),
            (ZxError::BAD_HANDLE, LxError::EBADF),
            (ZxError::TIMED_OUT, LxError::ETIMEDOUT),
            (ZxError::ACCESS_DENIED, LxError::EACCES),
            (ZxError::NOT_FOUND, LxError::ENOENT),
            (ZxError::NOT_DIR, LxError::ENOTDIR),
            (ZxError::NOT_FILE, LxError::EISDIR),
            (ZxError::FILE_BIG, LxError::EFBIG),
            (ZxError::NO_SPACE, LxError::ENOSPC),
            (ZxError::UNAVAILABLE, LxError::EBUSY),
            (ZxError::CANCELED, LxError::EINTR),
            (ZxError::IO, LxError::EIO),
        ] {
            assert_eq!(LxError::from(zx), lx, "{:?}", zx);
        }
    }

    #[test]
    fn an_unmapped_kernel_error_is_still_an_error_and_not_a_panic() {
        // The fallback exists so a new ZxError can never take the machine
        // down; EIO is a real errno for the caller.
        assert_eq!(LxError::from(ZxError::INTERNAL), LxError::EIO);
        assert_eq!(LxError::from(ZxError::WRONG_TYPE), LxError::EIO);
    }

    #[test]
    fn reading_userspace_memory_badly_is_efault_and_not_einval() {
        // EFAULT is what a program's own fault handler and its test suite
        // expect from a bad pointer; EINVAL reads as "your arguments were
        // wrong", which sends the author looking in the wrong place.
        assert_eq!(LxError::from(Error::InvalidPointer), LxError::EFAULT);
        assert_eq!(LxError::from(Error::InvalidUtf8), LxError::EINVAL);
        assert_eq!(LxError::from(Error::BufferTooSmall), LxError::ENOBUFS);
        assert_eq!(LxError::from(Error::InvalidLength), LxError::EINVAL);
        assert_eq!(LxError::from(Error::InvalidVectorAddress), LxError::EINVAL);
    }

    #[test]
    fn no_errno_is_zero_because_zero_is_success() {
        // `-(err as isize)` is how a failure reaches the caller's register, so
        // an errno of 0 would negate to 0 and read as a successful call.
        for err in [
            LxError::from(FsError::NoDeviceSpace),
            LxError::from(FsError::EntryNotFound),
            LxError::from(ZxError::NO_MEMORY),
            LxError::from(ZxError::INTERNAL),
            LxError::from(Error::InvalidPointer),
        ] {
            assert!(err as isize > 0, "{:?} would negate to a success", err);
        }
    }
}
