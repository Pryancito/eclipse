//! `/proc/<pid>/kmem` and `/proc/<pid>/kmem_limit`: what a process holds in
//! the kernel's heap and how much it may (`crate::kmem`). Linux has no such
//! files; its memory cgroups' `memory.kmem.*` were the nearest, and are gone.
//!
//! `kmem` reads, one `key: value` per line, in bytes and counts:
//!
//! ```text
//! charged: 1052672
//! peak: 4198400
//! limit: 536870912
//! refused_quota: 0
//! refused_reserve: 0
//! inflight_fds: 0
//! inotify_watches: 0
//! ```
//!
//! `inflight_fds` is how many descriptors the process has sent over unix
//! sockets that nobody has received yet (`crate::kmem::InFlight`).
//!
//! `kmem_limit` reads the limit as set, `0` for the default share, and takes
//! a new one in bytes (`0`: back to the default). Lowering it is the owner's
//! to do; raising it needs `CAP_SYS_RESOURCE`, as raising a hard rlimit does.

use super::*;
use crate::process::{LinuxProcess, CAP_SYS_RESOURCE};
use core::fmt::Write;

/// The most of a write the file reads: a 64-bit number and change.
const NUMBUF: usize = 24;

fn account(proc: &Process) -> Result<&Arc<crate::kmem::KmemAccount>> {
    proc.try_linux()
        .map(LinuxProcess::kmem)
        .ok_or(FsError::EntryNotFound)
}

/// `/proc/<pid>/kmem`.
pub(super) fn text(proc: &Process) -> String {
    let mut out = String::new();
    if let Ok(acct) = account(proc) {
        let (quota, reserve) = acct.refused();
        let _ = write!(
            out,
            "charged: {}\npeak: {}\nlimit: {}\nrefused_quota: {}\nrefused_reserve: {}\ninflight_fds: {}\ninotify_watches: {}\n",
            acct.charged(),
            acct.peak(),
            acct.limit(),
            quota,
            reserve,
            acct.inflight(),
            acct.inotify_watches(),
        );
    }
    out
}

/// `/proc/<pid>/kmem_limit`.
pub(super) fn limit_text(proc: &Process) -> String {
    alloc::format!("{}\n", account(proc).map_or(0, |a| a.limit_setting()))
}

/// A write of `kmem_limit` by the process making the call.
pub(super) fn write(proc: &Process, buf: &[u8]) -> Result<usize> {
    let capable = current_process()
        .and_then(|caller| caller.try_linux().map(|lp| lp.capable(CAP_SYS_RESOURCE)))
        .unwrap_or(true);
    write_as(proc, buf, capable)
}

/// [`write`] with the caller's capability as an argument, for the host tests.
fn write_as(proc: &Process, buf: &[u8], capable: bool) -> Result<usize> {
    let len = buf.len().min(NUMBUF);
    let limit = parse(&buf[..len])?;
    let acct = account(proc)?;
    if !capable && raises(acct.limit(), limit, crate::kmem::default_limit()) {
        return Err(FsError::NoPermission);
    }
    acct.set_limit(limit);
    Ok(len)
}

/// Whether setting `new` (`0`: the default share, `default` bytes now) lifts
/// a limit that is `now` bytes in force.
fn raises(now: usize, new: usize, default: usize) -> bool {
    let new = if new == 0 { default } else { new };
    new > now
}

/// A decimal byte count, spaces and a newline around it allowed.
fn parse(buf: &[u8]) -> Result<usize> {
    let text = core::str::from_utf8(buf).map_err(|_| FsError::InvalidParam)?;
    let text = text.split('\0').next().unwrap_or_default().trim();
    text.parse::<usize>().map_err(|_| FsError::InvalidParam)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_limit_is_a_decimal_byte_count() {
        assert_eq!(parse(b"1048576\n"), Ok(1048576));
        assert_eq!(parse(b"  0 "), Ok(0));
        assert_eq!(parse(b"12\0junk"), Ok(12));
        assert_eq!(parse(b"-1"), Err(FsError::InvalidParam));
        assert_eq!(parse(b"1M"), Err(FsError::InvalidParam));
        assert_eq!(parse(b""), Err(FsError::InvalidParam));
    }

    #[test]
    fn only_a_higher_limit_is_a_raise() {
        assert!(!raises(100, 50, 400));
        assert!(!raises(100, 100, 400));
        assert!(raises(100, 101, 400));
        // Back to the default is a raise when the default is above.
        assert!(raises(100, 0, 400));
        assert!(!raises(400, 0, 100));
    }
}
