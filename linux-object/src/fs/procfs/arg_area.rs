//! `/proc/<pid>/cmdline` and `environ`, read out of the process's memory.
//!
//! Both were copies of what `execve` had been handed. Linux reads them from
//! where the image keeps its strings ([`ArgArea`]), so what a program has
//! written over its own `argv` is what `ps` shows: `setproctitle`, a daemon
//! blanking a password it was given on the command line, and the
//! interpreter's argument vector for a `#!` script instead of the script's.

use super::*;
use crate::loader::ArgArea;
use alloc::vec;

/// Up to `buf.len()` bytes of a process's memory at an address, and how
/// many there were.
type ReadMemory<'a> = &'a dyn Fn(usize, &mut [u8]) -> usize;

/// The page `setproctitle` mode reads a title from (`get_mm_proctitle`).
const TITLE_PAGE: usize = 4096;

/// `get_mm_cmdline`: `/proc/<pid>/cmdline` from `pos` into `buf`.
///
/// The arguments, `[arg_start, arg_end)`, unless the last of them no longer
/// ends in a NUL: then a title has been written over them, and the file is
/// that title up to its first NUL within a page. The title may run on over
/// the environment, and only when the environment comes right after.
pub(super) fn cmdline_at(read: ReadMemory, area: ArgArea, pos: usize, buf: &mut [u8]) -> usize {
    let ArgArea {
        arg_start, arg_end, ..
    } = area;
    if arg_start >= arg_end {
        return 0;
    }
    let end = if area.env_start == arg_end && area.env_end >= area.env_start {
        area.env_end
    } else {
        arg_end
    };
    let len = end - arg_start;
    if pos >= len {
        return 0;
    }
    let count = buf.len().min(len - pos);
    let mut last = [0u8];
    if read(arg_end - 1, &mut last) == 1 && last[0] != 0 {
        return title_at(read, arg_start, pos, &mut buf[..count]);
    }
    let from = arg_start + pos;
    if from >= arg_end {
        return 0;
    }
    read(from, &mut buf[..count.min(arg_end - from)])
}

/// `get_mm_proctitle`: the title at `arg_start`, its NUL included when it
/// has one within the page, from `pos`.
fn title_at(read: ReadMemory, arg_start: usize, pos: usize, buf: &mut [u8]) -> usize {
    let mut page = vec![0u8; TITLE_PAGE];
    let got = read(arg_start, &mut page);
    let len = page[..got]
        .iter()
        .position(|&b| b == 0)
        .map_or(got, |nul| nul + 1);
    if len <= pos {
        return 0;
    }
    let n = (len - pos).min(buf.len());
    buf[..n].copy_from_slice(&page[pos..pos + n]);
    n
}

/// `environ_read`: `[env_start, env_end)` from `pos` into `buf`.
pub(super) fn environ_at(read: ReadMemory, area: ArgArea, pos: usize, buf: &mut [u8]) -> usize {
    let len = area.env_end.saturating_sub(area.env_start);
    if pos >= len {
        return 0;
    }
    let count = buf.len().min(len - pos);
    read(area.env_start + pos, &mut buf[..count])
}

/// `proc`'s `cmdline`, or its `environ`, read out of its memory; `None`
/// when the process has no [`ArgArea`] and the copy kept at `execve` stands
/// in.
pub(super) fn read(proc: &Process, environ: bool) -> Option<Vec<u8>> {
    let area = proc.try_linux()?.arg_area()?;
    // A zombie has no memory left to read (`get_mm_cmdline` finds no `mm`).
    if let Status::Exited(_) = proc.status() {
        return Some(Vec::new());
    }
    let vmar = proc.vmar();
    let read = |addr: usize, buf: &mut [u8]| vmar.read_memory(addr, buf).unwrap_or(0);
    let mut out = Vec::new();
    let mut chunk = vec![0u8; TITLE_PAGE];
    loop {
        let n = if environ {
            environ_at(&read, area, out.len(), &mut chunk)
        } else {
            cmdline_at(&read, area, out.len(), &mut chunk)
        };
        if n == 0 {
            return Some(out);
        }
        out.extend_from_slice(&chunk[..n]);
    }
}

#[cfg(test)]
mod tests;
