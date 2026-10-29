//! `/proc/hunter_net`: hunter's network monitor. Split out of `procfs.rs` for
//! the size ratchet, like `/proc/hunter`.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use rcore_fs::vfs::{FileSystem, FsError, INode, Metadata, PollStatus, Result};

use super::hunter_proc::may_control_hunter;
use super::{file_metadata, seq_read_at, ProcFS};

/// Read, every inet socket with the process holding it and a verdict on each
/// listener (see `hunter::netmon::render`). Each read takes a fresh snapshot,
/// so the unexpected-listener alerts are as current as whoever is watching:
/// lunarhunter's *Red* tab reads it every two seconds while open.
///
/// Written by root, a script in netmon's language (`expect tcp 22
/// /usr/sbin/sshd`, `alerts off`, ...), all or nothing, which is how the *Allow*
/// button and `/etc/hunter/ports` reach the rules.
///
/// Readable by everyone, as `/proc/net/tcp` is: which program holds which port
/// is what `netstat -p` shows any user on Linux for their own processes, and
/// the table here names programs, not what they send.
pub(super) struct ProcHunterNetINode;

impl ProcHunterNetINode {
    const INODE: usize = 152;
}

fn proc_hunter_net_content() -> String {
    crate::net::netmon::refresh();
    hunter::netmon::render()
}

impl INode for ProcHunterNetINode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        seq_read_at(proc_hunter_net_content, offset, buf)
    }

    fn seq_text(&self) -> Option<Result<Vec<u8>>> {
        Some(Ok(proc_hunter_net_content().into_bytes()))
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> Result<usize> {
        let ucred = crate::process::current_ucred();
        if !may_control_hunter(ucred) {
            return Err(FsError::NoPermission);
        }
        let text = core::str::from_utf8(buf).map_err(|_| FsError::InvalidParam)?;
        let pid = ucred.map(|(pid, _, _)| pid as u64).unwrap_or(0);
        hunter::netmon::apply_script(text, "/proc/hunter_net", pid)
            .map_err(|_| FsError::InvalidParam)?;
        Ok(buf.len())
    }

    /// `O_TRUNC` is a no-op, as for `/proc/hunter`.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Host tests run with no user thread, which is the kernel: it may write.
    #[test]
    fn a_rule_written_here_shows_in_the_table() {
        let node = ProcHunterNetINode;
        let script = b"expect tcp 2222 /usr/sbin/test-sshd\n";
        assert_eq!(node.write_at(0, script).unwrap(), script.len());
        let text = String::from_utf8(node.seq_text().unwrap().unwrap()).unwrap();
        assert!(text.starts_with("hunter netmon\n"), "{}", text);
        assert!(
            text.contains("expect tcp 2222 /usr/sbin/test-sshd\n"),
            "{}",
            text
        );
        let forget = b"forget tcp 2222 /usr/sbin/test-sshd";
        assert_eq!(node.write_at(0, forget).unwrap(), forget.len());
        assert_eq!(
            node.write_at(0, b"expect tcp nope"),
            Err(FsError::InvalidParam)
        );
    }
}
