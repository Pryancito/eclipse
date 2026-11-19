//! The Linux `errno` every failure of this client becomes.
//!
//! A number and not the kernel's `LxError`, so the same code serves the
//! kernel (which turns it into its own) and a process (which hands it to
//! the kernel as a served filesystem's answer). The names are the ones the
//! code was written with; the values are Linux's on every architecture
//! Eclipse runs on.

/// A Linux `errno`, positive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Errno(pub i32);

/// What every fallible call of this crate returns.
pub type LxResult<T> = Result<T, Errno>;

macro_rules! errnos {
    ($($name:ident = $value:expr,)*) => {
        impl Errno {
            $(
                #[allow(missing_docs)]
                pub const $name: Errno = Errno($value);
            )*
        }
    };
}

errnos! {
    // Not an error: what `NFS3_OK` would map to, which nothing should ask.
    EUNDEF = 0,
    EPERM = 1,
    ENOENT = 2,
    EIO = 5,
    ENXIO = 6,
    EAGAIN = 11,
    ENOMEM = 12,
    EACCES = 13,
    EEXIST = 17,
    EXDEV = 18,
    ENODEV = 19,
    ENOTDIR = 20,
    EISDIR = 21,
    EINVAL = 22,
    EFBIG = 27,
    ENOSPC = 28,
    EROFS = 30,
    EMLINK = 31,
    ENAMETOOLONG = 36,
    ENOTEMPTY = 39,
    EREMOTE = 66,
    EOPNOTSUPP = 95,
    EADDRNOTAVAIL = 99,
    ENOTCONN = 107,
    ETIMEDOUT = 110,
    ECONNREFUSED = 111,
    ESTALE = 116,
    EDQUOT = 122,
}

impl Errno {
    /// The number.
    pub const fn get(self) -> i32 {
        self.0
    }
}
