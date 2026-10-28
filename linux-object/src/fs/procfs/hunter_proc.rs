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

/// `/proc/hunter_scan`: hunter's on-demand antivirus scanner.
///
/// Root writes paths, one per line; each write is one batch, scanned before
/// the `write(2)` returns, and replaces the previous batch's results. Reading
/// gives those results in `clamscan` style (`path: OK`, `path: <Name> FOUND`,
/// `path: ERROR <why>`). Paths are resolved like the writer's own `open`
/// (its root and working directory, through links). `hunterctl scan` walks a
/// tree and feeds it in batches.
///
/// The results are one global batch, not one per open file: two scans run at
/// once interleave their results. A scanner is an administrator's tool, and
/// `hunterctl` runs one at a time.
pub(super) struct ProcHunterScanINode;

impl ProcHunterScanINode {
    const INODE: usize = 146;
}

fn proc_hunter_scan_content() -> String {
    hunter::av::render_scan_results()
}

impl INode for ProcHunterScanINode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        seq_read_at(proc_hunter_scan_content, offset, buf)
    }

    fn seq_text(&self) -> Option<Result<Vec<u8>>> {
        Some(Ok(proc_hunter_scan_content().into_bytes()))
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> Result<usize> {
        if !may_control_hunter(crate::process::current_ucred()) {
            return Err(FsError::NoPermission);
        }
        let text = core::str::from_utf8(buf).map_err(|_| FsError::InvalidParam)?;
        let proc = super::current_process();
        hunter::av::begin_scan_batch();
        for path in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let inode = proc.as_ref().and_then(|p| {
                use crate::process::ProcessExt;
                p.linux().lookup_inode(path).ok()
            });
            scan_inode(path, inode.as_deref());
        }
        Ok(buf.len())
    }

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

/// Scans one file into the current batch. Only regular files: a device or a
/// FIFO would be read forever, and a directory has no bytes to scan.
pub(super) fn scan_inode(path: &str, inode: Option<&dyn INode>) {
    let len = inode
        .and_then(|i| i.metadata().ok())
        .filter(|m| m.type_ == rcore_fs::vfs::FileType::File)
        .map(|m| m.size);
    hunter::av::scan_file(path, len, &mut |off, buf| {
        let inode = match inode {
            Some(i) => i,
            None => return false,
        };
        let mut done = 0;
        while done < buf.len() {
            match inode.read_at(off + done, &mut buf[done..]) {
                Ok(0) | Err(_) => return false,
                Ok(n) => done += n,
            }
        }
        true
    });
}
