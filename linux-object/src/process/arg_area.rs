//! Where the image a process runs keeps its arguments and environment.
//!
//! `/proc/<pid>/cmdline` and `environ` were the vectors `execve` was handed,
//! kept aside as strings, where Linux reads them back out of the process's
//! own memory (`get_mm_cmdline`, `environ_read`). So a program that rewrote
//! its `argv` in place -- a title set with `setproctitle`, a password blanked
//! after reading it -- still showed what it was started with, and a `#!`
//! script showed the script's argument vector instead of its interpreter's.

use super::*;
use crate::loader::ArgArea;

impl LinuxProcess {
    /// Record where the image just loaded put its argument and environment
    /// strings; `None` when the loader did not say.
    pub fn set_arg_area(&self, area: Option<ArgArea>) {
        self.inner.lock().arg_area = area;
    }

    /// Where the argument and environment strings are, when known. A
    /// process no loader started has none, and `/proc` shows the copy kept
    /// at `execve` instead.
    pub fn arg_area(&self) -> Option<ArgArea> {
        self.inner.lock().arg_area
    }
}
