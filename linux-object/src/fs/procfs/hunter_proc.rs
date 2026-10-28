//! `/proc/hunter`: the security subsystem's live report when read, and its
//! control plane when written. Split out of `procfs.rs` for the size ratchet.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use rcore_fs::vfs::{FileSystem, FsError, INode, Metadata, PollStatus, Result};

use super::{file_metadata, seq_read_at, ProcFS};

/// `/proc/hunter` — security subsystem status and recent intrusion-detection
/// event ring, rendered on each read by the `hunter` crate.
fn proc_hunter_content() -> String {
    hunter::render_report()
}

/// `/proc/hunter`: read, the live report; written, a script in hunter's
/// control language (`mode exec enforce`, `seal`, ... -- see
/// `hunter::control`), which is how an operator tightens the running system.
///
/// Only root writes it. The mode bits say so too, but the check that matters
/// is here, on the credentials of the process doing the `write(2)`: a file
/// descriptor opened by root and handed to an unprivileged child must not
/// carry the right to unseal anything with it (it cannot unseal anyway --
/// the latch is one-way -- but before the seal it could relax every domain).
///
/// Each `write(2)` is one whole script, applied all or nothing: a script that
/// does not parse is `EINVAL` and changes nothing. `echo`, `printf` and
/// `hunterctl` all hand over a script in one write.
pub(super) struct ProcHunterINode;

impl ProcHunterINode {
    const INODE: usize = 17;
}

/// Whether the writer may drive hunter's control plane: root, or the kernel
/// itself (no user thread on this CPU, which is also every host test).
pub(super) fn may_control_hunter(ucred: Option<(i32, u32, u32)>) -> bool {
    ucred.is_none_or(|(_, euid, _)| euid == 0)
}

impl INode for ProcHunterINode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        seq_read_at(proc_hunter_content, offset, buf)
    }

    fn seq_text(&self) -> Option<Result<Vec<u8>>> {
        Some(Ok(proc_hunter_content().into_bytes()))
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> Result<usize> {
        let ucred = crate::process::current_ucred();
        if !may_control_hunter(ucred) {
            return Err(FsError::NoPermission);
        }
        let text = core::str::from_utf8(buf).map_err(|_| FsError::InvalidParam)?;
        let pid = ucred.map(|(pid, _, _)| pid as u64).unwrap_or(0);
        hunter::control::apply_script(text, "/proc/hunter", pid)
            .map_err(|_| FsError::InvalidParam)?;
        Ok(buf.len())
    }

    /// `O_TRUNC` is a no-op, as for `comm`: `echo seal > /proc/hunter` opens
    /// with it, and the default (ENOSYS) would fail the open before the write.
    fn resize(&self, _len: usize) -> Result<()> {
        Ok(())
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: true,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(file_metadata(Self::INODE, 0o644))
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(ProcFS)
    }
}
