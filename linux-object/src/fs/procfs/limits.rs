//! `/proc/<pid>/limits`, which was not there. `cat /proc/<pid>/limits` is how
//! a daemon that runs out of descriptors gets looked at from outside, and
//! what supervisors and `prlimit`-less scripts read a process's limits from.

use super::*;
use crate::process::{LinuxProcess, RLIM_INFINITY, RLIM_NLIMITS};
use alloc::string::ToString;
use core::fmt::Write;

/// Each limit's row name and unit, in resource order (`lnames` in
/// `fs/proc/base.c`). The two priorities have no unit.
const ROWS: [(&str, &str); RLIM_NLIMITS] = [
    ("Max cpu time", "seconds"),
    ("Max file size", "bytes"),
    ("Max data size", "bytes"),
    ("Max stack size", "bytes"),
    ("Max core file size", "bytes"),
    ("Max resident set", "bytes"),
    ("Max processes", "processes"),
    ("Max open files", "files"),
    ("Max locked memory", "bytes"),
    ("Max address space", "bytes"),
    ("Max file locks", "locks"),
    ("Max pending signals", "signals"),
    ("Max msgqueue size", "bytes"),
    ("Max nice priority", ""),
    ("Max realtime priority", ""),
    ("Max realtime timeout", "us"),
];

/// The file as `proc_pid_limits` prints it: a header, then one row per
/// limit with its soft and hard values, `unlimited` for `RLIM_INFINITY`.
pub(super) fn proc_pid_limits(lp: &LinuxProcess) -> String {
    let value = |v: u64| {
        if v == RLIM_INFINITY {
            String::from("unlimited")
        } else {
            v.to_string()
        }
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<25} {:<20} {:<20} {:<10}",
        "Limit", "Soft Limit", "Hard Limit", "Units"
    );
    for (resource, (name, unit)) in ROWS.iter().enumerate() {
        let limit = lp.rlimit(resource, None, false).unwrap_or_default();
        let _ = write!(
            out,
            "{:<25} {:<20} {:<20} ",
            name,
            value(limit.cur),
            value(limit.max)
        );
        if !unit.is_empty() {
            let _ = write!(out, "{:<10}", unit);
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod row_tests;
#[cfg(test)]
mod tests;
