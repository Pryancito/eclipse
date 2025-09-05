//! Error handling for RedoxFS in Eclipse Kernel

use core::fmt;

/// Error type for RedoxFS operations
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// Key rejected
    KeyRejected,
    /// No such file or directory
    NoEntry,
    /// No key available
    NoKey,
    /// Invalid argument
    InvalidArgument,
    /// Permission denied
    PermissionDenied,
    /// File too large
    FileTooLarge,
    /// No space left
    NoSpaceLeft,
    /// Read only filesystem
    ReadOnlyFilesystem,
    /// Not a directory
    NotDirectory,
    /// Is a directory
    IsDirectory,
    /// Directory not empty
    DirectoryNotEmpty,
    /// Too many links
    TooManyLinks,
    /// Invalid file descriptor
    InvalidFileDescriptor,
    /// Bad file descriptor
    BadFileDescriptor,
    /// Operation not supported
    OperationNotSupported,
    /// Interrupted system call
    Interrupted,
    /// Resource temporarily unavailable
    ResourceUnavailable,
    /// Cross device link
    CrossDeviceLink,
    /// File exists
    FileExists,
    /// Broken pipe
    BrokenPipe,
    /// Connection refused
    ConnectionRefused,
    /// Connection reset
    ConnectionReset,
    /// Connection aborted
    ConnectionAborted,
    /// Not connected
    NotConnected,
    /// Connection in progress
    ConnectionInProgress,
    /// Already connected
    AlreadyConnected,
    /// Address in use
    AddressInUse,
    /// Address not available
    AddressNotAvailable,
    /// Network unreachable
    NetworkUnreachable,
    /// Host unreachable
    HostUnreachable,
    /// Protocol not supported
    ProtocolNotSupported,
    /// Wrong protocol type
    WrongProtocolType,
    /// No protocol option
    NoProtocolOption,
    /// Protocol not available
    ProtocolNotAvailable,
    /// No buffer space
    NoBufferSpace,
    /// Socket not supported
    SocketNotSupported,
    /// Operation not supported on socket
    OperationNotSupportedOnSocket,
    /// Protocol family not supported
    ProtocolFamilyNotSupported,
    /// Address family not supported
    AddressFamilyNotSupported,
    /// Socket type not supported
    SocketTypeNotSupported,
    /// Socket already bound
    SocketAlreadyBound,
    /// Socket not bound
    SocketNotBound,
    /// Socket already connected
    SocketAlreadyConnected,
    /// Socket not connected
    SocketNotConnected,
    /// Cannot send after socket shutdown
    CannotSendAfterSocketShutdown,
    /// Operation already in progress
    OperationAlreadyInProgress,
    /// Operation would block
    OperationWouldBlock,
    /// Connection timed out
    ConnectionTimedOut,
    /// Connection refused
    ConnectionRefused,
    /// Host is down
    HostDown,
    /// Host unreachable
    HostUnreachable,
    /// Network is down
    NetworkDown,
    /// Network unreachable
    NetworkUnreachable,
    /// Connection aborted
    ConnectionAborted,
    /// Connection reset
    ConnectionReset,
    /// No buffer space
    NoBufferSpace,
    /// Socket is connected
    SocketIsConnected,
    /// Socket is not connected
    SocketIsNotConnected,
    /// Cannot send after socket shutdown
    CannotSendAfterSocketShutdown,
    /// Too many references
    TooManyReferences,
    /// Connection timed out
    ConnectionTimedOut,
    /// Connection refused
    ConnectionRefused,
    /// Host is down
    HostDown,
    /// Host unreachable
    HostUnreachable,
    /// Network is down
    NetworkDown,
    /// Network unreachable
    NetworkUnreachable,
    /// Connection aborted
    ConnectionAborted,
    /// Connection reset
    ConnectionReset,
    /// No buffer space
    NoBufferSpace,
    /// Socket is connected
    SocketIsConnected,
    /// Socket is not connected
    SocketIsNotConnected,
    /// Cannot send after socket shutdown
    CannotSendAfterSocketShutdown,
    /// Too many references
    TooManyReferences,
    /// Unknown error
    Unknown(i32),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::KeyRejected => write!(f, "Key rejected"),
            Error::NoEntry => write!(f, "No such file or directory"),
            Error::NoKey => write!(f, "No key available"),
            Error::InvalidArgument => write!(f, "Invalid argument"),
            Error::PermissionDenied => write!(f, "Permission denied"),
            Error::FileTooLarge => write!(f, "File too large"),
            Error::NoSpaceLeft => write!(f, "No space left"),
            Error::ReadOnlyFilesystem => write!(f, "Read only filesystem"),
            Error::NotDirectory => write!(f, "Not a directory"),
            Error::IsDirectory => write!(f, "Is a directory"),
            Error::DirectoryNotEmpty => write!(f, "Directory not empty"),
            Error::TooManyLinks => write!(f, "Too many links"),
            Error::InvalidFileDescriptor => write!(f, "Invalid file descriptor"),
            Error::BadFileDescriptor => write!(f, "Bad file descriptor"),
            Error::OperationNotSupported => write!(f, "Operation not supported"),
            Error::Interrupted => write!(f, "Interrupted system call"),
            Error::ResourceUnavailable => write!(f, "Resource temporarily unavailable"),
            Error::CrossDeviceLink => write!(f, "Cross device link"),
            Error::FileExists => write!(f, "File exists"),
            Error::BrokenPipe => write!(f, "Broken pipe"),
            Error::ConnectionRefused => write!(f, "Connection refused"),
            Error::ConnectionReset => write!(f, "Connection reset"),
            Error::ConnectionAborted => write!(f, "Connection aborted"),
            Error::NotConnected => write!(f, "Not connected"),
            Error::ConnectionInProgress => write!(f, "Connection in progress"),
            Error::AlreadyConnected => write!(f, "Already connected"),
            Error::AddressInUse => write!(f, "Address in use"),
            Error::AddressNotAvailable => write!(f, "Address not available"),
            Error::NetworkUnreachable => write!(f, "Network unreachable"),
            Error::HostUnreachable => write!(f, "Host unreachable"),
            Error::ProtocolNotSupported => write!(f, "Protocol not supported"),
            Error::WrongProtocolType => write!(f, "Wrong protocol type"),
            Error::NoProtocolOption => write!(f, "No protocol option"),
            Error::ProtocolNotAvailable => write!(f, "Protocol not available"),
            Error::NoBufferSpace => write!(f, "No buffer space"),
            Error::SocketNotSupported => write!(f, "Socket not supported"),
            Error::OperationNotSupportedOnSocket => write!(f, "Operation not supported on socket"),
            Error::ProtocolFamilyNotSupported => write!(f, "Protocol family not supported"),
            Error::AddressFamilyNotSupported => write!(f, "Address family not supported"),
            Error::SocketTypeNotSupported => write!(f, "Socket type not supported"),
            Error::SocketAlreadyBound => write!(f, "Socket already bound"),
            Error::SocketNotBound => write!(f, "Socket not bound"),
            Error::SocketAlreadyConnected => write!(f, "Socket already connected"),
            Error::SocketNotConnected => write!(f, "Socket not connected"),
            Error::CannotSendAfterSocketShutdown => write!(f, "Cannot send after socket shutdown"),
            Error::OperationAlreadyInProgress => write!(f, "Operation already in progress"),
            Error::OperationWouldBlock => write!(f, "Operation would block"),
            Error::ConnectionTimedOut => write!(f, "Connection timed out"),
            Error::HostDown => write!(f, "Host is down"),
            Error::NetworkDown => write!(f, "Network is down"),
            Error::SocketIsConnected => write!(f, "Socket is connected"),
            Error::SocketIsNotConnected => write!(f, "Socket is not connected"),
            Error::TooManyReferences => write!(f, "Too many references"),
            Error::Unknown(code) => write!(f, "Unknown error: {}", code),
        }
    }
}

/// Result type for RedoxFS operations
pub type Result<T> = core::result::Result<T, Error>;

/// Convert error code to Error enum
pub fn from_errno(errno: i32) -> Error {
    match errno {
        1 => Error::OperationNotPermitted,
        2 => Error::NoEntry,
        3 => Error::NoProcess,
        4 => Error::Interrupted,
        5 => Error::IOError,
        6 => Error::NoDevice,
        7 => Error::ArgumentListTooLong,
        8 => Error::ExecFormatError,
        9 => Error::BadFileDescriptor,
        10 => Error::NoChildProcess,
        11 => Error::ResourceUnavailable,
        12 => Error::CannotAllocateMemory,
        13 => Error::PermissionDenied,
        14 => Error::BadAddress,
        15 => Error::BlockDeviceRequired,
        16 => Error::DeviceBusy,
        17 => Error::FileExists,
        18 => Error::CrossDeviceLink,
        19 => Error::NoDevice,
        20 => Error::NotDirectory,
        21 => Error::IsDirectory,
        22 => Error::InvalidFileDescriptor,
        23 => Error::FileTableOverflow,
        24 => Error::TooManyOpenFiles,
        25 => Error::NotTTY,
        26 => Error::TextFileBusy,
        27 => Error::FileTooLarge,
        28 => Error::NoSpaceLeft,
        29 => Error::IllegalSeek,
        30 => Error::ReadOnlyFilesystem,
        31 => Error::TooManyLinks,
        32 => Error::BrokenPipe,
        33 => Error::MathArgumentOutOfDomain,
        34 => Error::MathResultNotRepresentable,
        35 => Error::ResourceDeadlockWouldOccur,
        36 => Error::FileNameTooLong,
        37 => Error::NoRecordLocksAvailable,
        38 => Error::FunctionNotImplemented,
        39 => Error::DirectoryNotEmpty,
        40 => Error::TooManySymbolicLinkLevels,
        41 => Error::Unknown(41),
        42 => Error::NoMessageOfDesiredType,
        43 => Error::IdentifierRemoved,
        44 => Error::ChannelNumberOutOfRange,
        45 => Error::Level2NotSynchronized,
        46 => Error::Level3Halted,
        47 => Error::Level3Reset,
        48 => Error::LinkNumberOutOfRange,
        49 => Error::ProtocolDriverNotAttached,
        50 => Error::NoCSIStructureAvailable,
        51 => Error::Level2Halted,
        52 => Error::InvalidExchange,
        53 => Error::InvalidRequestDescriptor,
        54 => Error::ExchangeFull,
        55 => Error::NoAnode,
        56 => Error::InvalidRequestCode,
        57 => Error::InvalidSlot,
        58 => Error::Unknown(58),
        59 => Error::BadFontFileFormat,
        60 => Error::DeviceNotStream,
        61 => Error::NoDataAvailable,
        62 => Error::TimerExpired,
        63 => Error::OutOfStreamsResources,
        64 => Error::MachineIsNotOnNetwork,
        65 => Error::PackageNotInstalled,
        66 => Error::ObjectIsRemote,
        67 => Error::LinkHasBeenSevered,
        68 => Error::AdvertiseError,
        69 => Error::SrmountError,
        70 => Error::CommunicationErrorOnSend,
        71 => Error::ProtocolError,
        72 => Error::Level2Halted,
        73 => Error::InvalidExchange,
        74 => Error::InvalidRequestDescriptor,
        75 => Error::ExchangeFull,
        76 => Error::NoAnode,
        77 => Error::InvalidRequestCode,
        78 => Error::InvalidSlot,
        79 => Error::Unknown(79),
        80 => Error::BadFontFileFormat,
        81 => Error::DeviceNotStream,
        82 => Error::NoDataAvailable,
        83 => Error::TimerExpired,
        84 => Error::OutOfStreamsResources,
        85 => Error::MachineIsNotOnNetwork,
        86 => Error::PackageNotInstalled,
        87 => Error::ObjectIsRemote,
        88 => Error::LinkHasBeenSevered,
        89 => Error::AdvertiseError,
        90 => Error::SrmountError,
        91 => Error::CommunicationErrorOnSend,
        92 => Error::ProtocolError,
        93 => Error::Level2Halted,
        94 => Error::InvalidExchange,
        95 => Error::InvalidRequestDescriptor,
        96 => Error::ExchangeFull,
        97 => Error::NoAnode,
        98 => Error::InvalidRequestCode,
        99 => Error::InvalidSlot,
        100 => Error::Unknown(100),
        101 => Error::BadFontFileFormat,
        102 => Error::DeviceNotStream,
        103 => Error::NoDataAvailable,
        104 => Error::TimerExpired,
        105 => Error::OutOfStreamsResources,
        106 => Error::MachineIsNotOnNetwork,
        107 => Error::PackageNotInstalled,
        108 => Error::ObjectIsRemote,
        109 => Error::LinkHasBeenSevered,
        110 => Error::AdvertiseError,
        111 => Error::SrmountError,
        112 => Error::CommunicationErrorOnSend,
        113 => Error::ProtocolError,
        114 => Error::Level2Halted,
        115 => Error::InvalidExchange,
        116 => Error::InvalidRequestDescriptor,
        117 => Error::ExchangeFull,
        118 => Error::NoAnode,
        119 => Error::InvalidRequestCode,
        120 => Error::InvalidSlot,
        121 => Error::Unknown(121),
        122 => Error::BadFontFileFormat,
        123 => Error::DeviceNotStream,
        124 => Error::NoDataAvailable,
        125 => Error::TimerExpired,
        126 => Error::OutOfStreamsResources,
        127 => Error::MachineIsNotOnNetwork,
        128 => Error::PackageNotInstalled,
        129 => Error::ObjectIsRemote,
        130 => Error::LinkHasBeenSevered,
        131 => Error::AdvertiseError,
        132 => Error::SrmountError,
        133 => Error::CommunicationErrorOnSend,
        134 => Error::ProtocolError,
        135 => Error::Level2Halted,
        136 => Error::InvalidExchange,
        137 => Error::InvalidRequestDescriptor,
        138 => Error::ExchangeFull,
        139 => Error::NoAnode,
        140 => Error::InvalidRequestCode,
        141 => Error::InvalidSlot,
        142 => Error::Unknown(142),
        143 => Error::BadFontFileFormat,
        144 => Error::DeviceNotStream,
        145 => Error::NoDataAvailable,
        146 => Error::TimerExpired,
        147 => Error::OutOfStreamsResources,
        148 => Error::MachineIsNotOnNetwork,
        149 => Error::PackageNotInstalled,
        150 => Error::ObjectIsRemote,
        151 => Error::LinkHasBeenSevered,
        152 => Error::AdvertiseError,
        153 => Error::SrmountError,
        154 => Error::CommunicationErrorOnSend,
        155 => Error::ProtocolError,
        156 => Error::Level2Halted,
        157 => Error::InvalidExchange,
        158 => Error::InvalidRequestDescriptor,
        159 => Error::ExchangeFull,
        160 => Error::NoAnode,
        161 => Error::InvalidRequestCode,
        162 => Error::InvalidSlot,
        163 => Error::Unknown(163),
        164 => Error::BadFontFileFormat,
        165 => Error::DeviceNotStream,
        166 => Error::NoDataAvailable,
        167 => Error::TimerExpired,
        168 => Error::OutOfStreamsResources,
        169 => Error::MachineIsNotOnNetwork,
        170 => Error::PackageNotInstalled,
        171 => Error::ObjectIsRemote,
        172 => Error::LinkHasBeenSevered,
        173 => Error::AdvertiseError,
        174 => Error::SrmountError,
        175 => Error::CommunicationErrorOnSend,
        176 => Error::ProtocolError,
        177 => Error::Level2Halted,
        178 => Error::InvalidExchange,
        179 => Error::InvalidRequestDescriptor,
        180 => Error::ExchangeFull,
        181 => Error::NoAnode,
        182 => Error::InvalidRequestCode,
        183 => Error::InvalidSlot,
        184 => Error::Unknown(184),
        185 => Error::BadFontFileFormat,
        186 => Error::DeviceNotStream,
        187 => Error::NoDataAvailable,
        188 => Error::TimerExpired,
        189 => Error::OutOfStreamsResources,
        190 => Error::MachineIsNotOnNetwork,
        191 => Error::PackageNotInstalled,
        192 => Error::ObjectIsRemote,
        193 => Error::LinkHasBeenSevered,
        194 => Error::AdvertiseError,
        195 => Error::SrmountError,
        196 => Error::CommunicationErrorOnSend,
        197 => Error::ProtocolError,
        198 => Error::Level2Halted,
        199 => Error::InvalidExchange,
        200 => Error::InvalidRequestDescriptor,
        201 => Error::ExchangeFull,
        202 => Error::NoAnode,
        203 => Error::InvalidRequestCode,
        204 => Error::InvalidSlot,
        205 => Error::Unknown(205),
        206 => Error::BadFontFileFormat,
        207 => Error::DeviceNotStream,
        208 => Error::NoDataAvailable,
        209 => Error::TimerExpired,
        210 => Error::OutOfStreamsResources,
        211 => Error::MachineIsNotOnNetwork,
        212 => Error::PackageNotInstalled,
        213 => Error::ObjectIsRemote,
        214 => Error::LinkHasBeenSevered,
        215 => Error::AdvertiseError,
        216 => Error::SrmountError,
        217 => Error::CommunicationErrorOnSend,
        218 => Error::ProtocolError,
        219 => Error::Level2Halted,
        220 => Error::InvalidExchange,
        221 => Error::InvalidRequestDescriptor,
        222 => Error::ExchangeFull,
        223 => Error::NoAnode,
        224 => Error::InvalidRequestCode,
        225 => Error::InvalidSlot,
        226 => Error::Unknown(226),
        227 => Error::BadFontFileFormat,
        228 => Error::DeviceNotStream,
        229 => Error::NoDataAvailable,
        230 => Error::TimerExpired,
        231 => Error::OutOfStreamsResources,
        232 => Error::MachineIsNotOnNetwork,
        233 => Error::PackageNotInstalled,
        234 => Error::ObjectIsRemote,
        235 => Error::LinkHasBeenSevered,
        236 => Error::AdvertiseError,
        237 => Error::SrmountError,
        238 => Error::CommunicationErrorOnSend,
        239 => Error::ProtocolError,
        240 => Error::Level2Halted,
        241 => Error::InvalidExchange,
        242 => Error::InvalidRequestDescriptor,
        243 => Error::ExchangeFull,
        244 => Error::NoAnode,
        245 => Error::InvalidRequestCode,
        246 => Error::InvalidSlot,
        247 => Error::Unknown(247),
        248 => Error::BadFontFileFormat,
        249 => Error::DeviceNotStream,
        250 => Error::NoDataAvailable,
        251 => Error::TimerExpired,
        252 => Error::OutOfStreamsResources,
        253 => Error::MachineIsNotOnNetwork,
        254 => Error::PackageNotInstalled,
        255 => Error::ObjectIsRemote,
        _ => Error::Unknown(errno),
    }
}
