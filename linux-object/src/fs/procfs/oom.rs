//! `/proc/<pid>/oom_score_adj`, the older `oom_adj`, and `oom_score`,
//! measured on Linux 6.18 with glibc and musl (`procfiles-survey`, not in
//! the tree). None of the three was there: a write to the first, which is
//! what `choom`, systemd's `OOMScoreAdjust=` and a browser ranking its
//! renderers do, failed with `ENOENT`, and `ps -o oom_score_adj,oom_score`
//! had nothing to read.

use super::*;
use crate::process::{
    LinuxProcess, OomAdj, CAP_SYS_RESOURCE, INIT_PID, OOM_SCORE_ADJ_MAX, OOM_SCORE_ADJ_MIN,
};
use core::convert::TryFrom;
use zircon_object::object::KoID;

/// `OOM_DISABLE`: the bottom of `oom_adj`'s scale, and `-OOM_DISABLE` the
/// number of its steps above 0 that make `OOM_SCORE_ADJ_MAX`.
const OOM_DISABLE: i32 = -17;
/// `OOM_ADJUST_MAX`: the top of that scale.
const OOM_ADJUST_MAX: i32 = 15;
/// `PROC_NUMBUF - 1`: the most of a write either file reads, and what it
/// says it took of a longer one.
const NUMBUF: usize = 12;

fn adj_of(proc: &Process) -> OomAdj {
    proc.try_linux()
        .map(LinuxProcess::oom_adj)
        .unwrap_or_default()
}

/// `oom_score_adj_read`.
pub(super) fn adj_text(proc: &Process) -> String {
    alloc::format!("{}\n", adj_of(proc).adj)
}

/// `oom_adj_read`.
pub(super) fn legacy_text(proc: &Process) -> String {
    alloc::format!("{}\n", legacy(adj_of(proc).adj))
}

/// The adjustment on `oom_adj`'s scale: in seventeenths of the top,
/// truncated toward zero, and no higher than the old top, which the top
/// fifty-eight of the new scale would pass. Linux also names the very top
/// apart; the cap already gives it 15.
fn legacy(adj: i16) -> i32 {
    (i32::from(adj) * -OOM_DISABLE / i32::from(OOM_SCORE_ADJ_MAX)).min(OOM_ADJUST_MAX)
}

/// `oom_score_adj_write` (`legacy` false) and `oom_adj_write` for the
/// process making the call, which needs `CAP_SYS_RESOURCE` to lower the
/// adjustment past what [`OomAdj::set`] allows. The kernel itself writing
/// has no caller to ask, and may.
pub(super) fn write(proc: &Process, buf: &[u8], legacy: bool) -> Result<usize> {
    let capable = current_process()
        .and_then(|caller| caller.try_linux().map(|lp| lp.capable(CAP_SYS_RESOURCE)))
        .unwrap_or(true);
    write_as(proc, buf, legacy, capable)
}

/// [`write`] with the caller's capability as an argument: a host test has
/// no caller, so with it read inside only the first answer is reachable.
///
/// The first [`NUMBUF`] bytes of the write are read as a number up to their
/// first NUL, and that many is what the write took.
fn write_as(proc: &Process, buf: &[u8], legacy: bool, capable: bool) -> Result<usize> {
    let len = buf.len().min(NUMBUF);
    let adj = parse(&buf[..len], legacy)?;
    let lp = proc.try_linux().ok_or(FsError::EntryNotFound)?;
    if !lp.set_oom_adj(adj, legacy, capable) {
        return Err(FsError::NoPermission);
    }
    Ok(len)
}

/// A write's value on `oom_score_adj`'s scale: `EINVAL` outside the file's
/// own range, which for `oom_adj` is -17 to 15, scaled up the way
/// [`legacy`] scales down so that 15 is the top.
fn parse(buf: &[u8], legacy: bool) -> Result<i16> {
    let value = kstrtoint(buf)?;
    let (low, high) = if legacy {
        (OOM_DISABLE, OOM_ADJUST_MAX)
    } else {
        (i32::from(OOM_SCORE_ADJ_MIN), i32::from(OOM_SCORE_ADJ_MAX))
    };
    if value < low || value > high {
        return Err(FsError::InvalidParam);
    }
    let adj = if !legacy {
        value
    } else if value == OOM_ADJUST_MAX {
        i32::from(OOM_SCORE_ADJ_MAX)
    } else {
        value * i32::from(OOM_SCORE_ADJ_MAX) / -OOM_DISABLE
    };
    Ok(adj as i16)
}

/// `isspace` as the kernel's `ctype` has it, Latin-1's no-break space
/// included.
fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t'..=b'\r' | 0xa0)
}

/// `kstrtoint(strstrip(buf), 0, ..)`, the C string in `buf` up to its first
/// NUL: an `int` written as C writes one, `0x` for hex and a leading `0`
/// for octal, after one sign and with spaces around it. `ERANGE` for one
/// past an `int`, `EINVAL` for anything else that is not a number.
fn kstrtoint(buf: &[u8]) -> Result<i32> {
    let text = buf.split(|&b| b == 0).next().unwrap_or_default();
    let start = text
        .iter()
        .position(|&b| !is_space(b))
        .unwrap_or(text.len());
    let end = text
        .iter()
        .rposition(|&b| !is_space(b))
        .map_or(start, |i| i + 1);
    let text = &text[start..end];
    // `kstrtoll` takes a `-`; `kstrtoull` a `+`; never both.
    let (negative, digits) = match text {
        [b'-', rest @ ..] => (true, rest),
        [b'+', rest @ ..] => (false, rest),
        _ => (false, text),
    };
    // `0x` with no hex digit after it is junk whichever radix reads it.
    let (radix, digits) = match digits {
        [b'0', b'x' | b'X', ..] => (16, &digits[2..]),
        [b'0', ..] => (8, digits),
        _ => (10, digits),
    };
    let mut value: u64 = 0;
    let mut overflow = false;
    let mut taken = 0;
    for &b in digits {
        let Some(digit) = (b as char).to_digit(radix) else {
            break;
        };
        match value
            .checked_mul(radix.into())
            .and_then(|v| v.checked_add(digit.into()))
        {
            Some(v) => value = v,
            None => overflow = true,
        }
        taken += 1;
    }
    if overflow {
        return Err(FsError::OutOfRange);
    }
    if taken == 0 || taken != digits.len() {
        return Err(FsError::InvalidParam);
    }
    let value = if negative {
        -i128::from(value)
    } else {
        i128::from(value)
    };
    i32::try_from(value).map_err(|_| FsError::OutOfRange)
}

/// `proc_oom_score`.
pub(super) fn score_text(proc: &Process) -> String {
    alloc::format!("{}\n", score_of(proc))
}

/// 0 for a process with no memory left, a zombie: a leader that has exited
/// before the rest of its group still counts, by theirs. Anything else by
/// [`score`].
fn score_of(proc: &Process) -> u64 {
    if matches!(proc.status(), Status::Exited(_)) {
        return 0;
    }
    let vmar = proc.vmar();
    let stats = vmar.get_task_stats();
    let held = stats.private_bytes() + stats.shared_bytes() + vmar.page_table_bytes() as u64;
    let (_, total) = kernel_hal::mem::memory_usage();
    score(
        proc.id(),
        adj_of(proc).adj,
        held / PROC_PAGE_SIZE,
        total as u64 / PROC_PAGE_SIZE,
    )
}

/// `oom_badness` as `proc_oom_score` scales it. The pages a process holds
/// (resident, swapped out and in page tables) plus `adj` thousandths of all
/// memory, as a share of all memory, on a scale where a process of no size
/// and no adjustment is 666 and the top is 2000. 0 for `init` and for
/// `OOM_SCORE_ADJ_MIN`, which an OOM killer never picks.
///
/// Never below 0: `adj` thousandths of `total_pages`, thousands taken
/// whole, are less than all of it.
fn score(pid: KoID, adj: i16, held_pages: u64, total_pages: u64) -> u64 {
    if pid == INIT_PID || adj == OOM_SCORE_ADJ_MIN {
        return 0;
    }
    let total = total_pages.max(1) as i64;
    let badness = held_pages as i64 + i64::from(adj) * (total / 1000);
    ((1000 + badness * 1000 / total) * 2 / 3) as u64
}

#[cfg(test)]
mod tests;
