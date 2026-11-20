//! What the kernel still reads of an NFS mount: its device string and its
//! options, to refuse a malformed `mount(2)` with `EINVAL` and to resolve
//! the server's name before the mount (`addr=`).
//!
//! The client is not the kernel's: `tools/eclipse-nfs` serves every NFS
//! mount, started by the mount helper. Both read these strings with the
//! same code, the `nfs3` crate (`vendor/nfs3-rs`).

pub use ::nfs3::source;

impl From<::nfs3::errno::Errno> for crate::error::LxError {
    fn from(e: ::nfs3::errno::Errno) -> Self {
        crate::error::LxError::from_errno(e.0)
    }
}
