//! The words of the table's calls between a driver in a process and the
//! kernel (`ECLIPSE_SYNCOBJ`): an op, the words it is given, and the words
//! of its answer. Both ends are built from this, so they cannot disagree.
//!
//! A fence's landing zone travels as its physical address: the driver's
//! mapping of it is not the kernel's.

use alloc::vec::Vec;
use core::convert::TryFrom;

/// `poll_pending`: `[]`, the fences still pending. Bit [`TIMED_OUT`] of the
/// answer says some of the driver's fences timed out since it last asked.
pub const POLL_PENDING: u32 = 0;
/// `has_pending`: `[]`, 1 or 0.
pub const HAS_PENDING: u32 = 1;
/// `attach_hw_fence`: `[handle, point, phys, gpu_va, payload, ctx, binary]`,
/// 1 or 0.
pub const ATTACH: u32 = 2;
/// `abandon_fences`: `[phys]`, how many.
pub const ABANDON: u32 = 3;
/// `timeline_signal`: `[handle, point]`, 1 or 0.
pub const SIGNAL: u32 = 4;
/// `pending_hw_fences`: `[handle, point]`, how many; four words each in the
/// answer: `[phys, gpu_va, payload, ctx]`, phys 0 and ctx [`NOT_MINE`] for
/// a fence of another driver.
pub const PENDING_HW: u32 = 5;
/// `without_fence`: [`waits`], the index or [`NONE`].
pub const WITHOUT_FENCE: u32 = 6;
/// `first_unusable_by`: `[pid, handles...]`, the handle or [`NONE`].
pub const FIRST_UNUSABLE: u32 = 7;
/// `reached_by_timeout`: [`waits`], 1 or 0.
pub const REACHED_BY_TIMEOUT: u32 = 8;
/// `describe`: [`waits`], the bytes of the line, in the answer's words.
pub const DESCRIBE: u32 = 9;
/// `wait_ready`: `[wait_all]` then [`waits`], a [`ready_word`].
pub const WAIT_READY: u32 = 10;
/// The driver's fences that timed out since it last asked: `[]`, how many;
/// five words each: `[ctx, phys, payload, handle, point]`.
pub const TIMEOUTS: u32 = 11;
/// `stats_line`: `[]`, the bytes of the line.
pub const STATS: u32 = 12;
/// `resv::fences`: `[phys, write]`, how many; two words each:
/// `[handle, point]`.
pub const RESV_FENCES: u32 = 13;
/// `resv::forget`: `[phys]`, 0.
pub const RESV_FORGET: u32 = 14;

/// The most words a call's input or answer has.
pub const WORDS_MAX: usize = 4096;
/// "No such": the answer of [`WITHOUT_FENCE`] and [`FIRST_UNUSABLE`] when
/// every handle is fine.
///
/// An answer is the value of a system call, and a value from -4095 to -1
/// is an `errno` to the C library: `u64::MAX` here was -1, so "every
/// handle is fine" reached the driver as a refusal (`EINVAL`), and it took
/// the first handle of every EXEC for one that is not the caller's
/// (`ENOENT`): no NVK device could be made on a served card. Every answer
/// stays at or below `i64::MAX` ([`ANSWER_MAX`]).
pub const NONE: u64 = ANSWER_MAX;
/// The largest answer a call gives: one that never reads as negative.
pub const ANSWER_MAX: u64 = i64::MAX as u64;
/// The ctx of a fence another driver attached.
pub const NOT_MINE: u32 = u32::MAX;
/// The bit of [`POLL_PENDING`]'s answer that says there are timeouts to ask
/// for.
pub const TIMED_OUT: u64 = 1 << 62;

/// The words of a wait list: `[n, has_points, handles(n), points(n)?]`.
pub fn waits(handles: &[u32], points: Option<&[u64]>) -> Vec<u64> {
    let mut w = Vec::with_capacity(2 + handles.len() * 2);
    w.push(handles.len() as u64);
    w.push(points.is_some() as u64);
    w.extend(handles.iter().map(|&h| h as u64));
    if let Some(p) = points {
        w.extend(p.iter().take(handles.len()).copied());
        // A short list of points is padded as the table reads it: point 1.
        w.extend(core::iter::repeat_n(
            1,
            handles.len().saturating_sub(p.len()),
        ));
    }
    w
}

/// A wait list from its words, or `None` for words that are not one.
pub fn waits_of(w: &[u64]) -> Option<(Vec<u32>, Option<Vec<u64>>)> {
    let n = *w.first()? as usize;
    let has_points = *w.get(1)? != 0;
    let handles = w.get(2..2usize.checked_add(n)?)?;
    let handles = handles
        .iter()
        .map(|&h| u32::try_from(h).ok())
        .collect::<Option<Vec<u32>>>()?;
    let points = if has_points {
        Some(w.get(2 + n..2 + 2 * n)?.to_vec())
    } else {
        None
    };
    Some((handles, points))
}

/// What `wait_ready` said: `0` look again later, `1 | index << 8` signaled,
/// `2` a handle is gone, `3` its deadline passed.
pub fn ready_word(r: Option<Result<u32, super::WaitOutcome>>) -> u64 {
    use super::WaitOutcome;
    match r {
        None => 0,
        Some(Ok(i))
        | Some(Err(WaitOutcome::Signaled {
            first_signaled_index: i,
        })) => 1 | (i as u64) << 8,
        Some(Err(WaitOutcome::Invalid)) => 2,
        Some(Err(WaitOutcome::Timeout)) => 3,
    }
}

/// [`ready_word`] read back.
pub fn ready_of(w: u64) -> Option<Result<u32, super::WaitOutcome>> {
    use super::WaitOutcome;
    match w & 0xff {
        1 => Some(Ok((w >> 8) as u32)),
        2 => Some(Err(WaitOutcome::Invalid)),
        3 => Some(Err(WaitOutcome::Timeout)),
        _ => None,
    }
}

/// Bytes in words, as a line travels.
pub fn words_of_bytes(b: &[u8]) -> Vec<u64> {
    b.chunks(8)
        .map(|c| {
            let mut w = [0u8; 8];
            w[..c.len()].copy_from_slice(c);
            u64::from_le_bytes(w)
        })
        .collect()
}

/// The `len` bytes in `words`.
pub fn bytes_of_words(words: &[u64], len: usize) -> Vec<u8> {
    let mut b: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    b.truncate(len);
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// What the C library does with a system call's value: -4095 to -1 is
    /// an error.
    fn reads_as_errno(w: u64) -> bool {
        (-4095..=-1).contains(&(w as i64))
    }

    #[test]
    fn no_answer_reads_as_an_errno_or_as_negative() {
        for w in [
            NONE,
            TIMED_OUT,
            TIMED_OUT | WORDS_MAX as u64,
            TIMED_OUT | u32::MAX as u64,
            ready_word(Some(Ok(u32::MAX))),
        ] {
            assert!(!reads_as_errno(w), "{:#x}", w);
            assert!(w <= ANSWER_MAX, "{:#x}", w);
        }
        // `NONE` is no index and no handle.
        assert!(NONE > u32::MAX as u64);
        assert_eq!(TIMED_OUT & (u32::MAX as u64), 0);
    }

    #[test]
    fn a_wait_list_comes_back_as_it_went() {
        let w = waits(&[3, 9], Some(&[5, 7]));
        assert_eq!(waits_of(&w), Some((vec![3, 9], Some(vec![5, 7]))));
        let w = waits(&[4], None);
        assert_eq!(waits_of(&w), Some((vec![4], None)));
        // Points short of the handles are point 1, as the table reads them.
        assert_eq!(
            waits_of(&waits(&[1, 2], Some(&[8]))),
            Some((vec![1, 2], Some(vec![8, 1])))
        );
        assert_eq!(waits_of(&[3, 0, 1]), None, "fewer handles than it says");
        assert_eq!(waits_of(&[1, 0, u64::MAX]), None, "not a handle");
        assert_eq!(waits_of(&[u64::MAX, 0]), None);
    }

    #[test]
    fn what_wait_ready_says_comes_back_as_it_went() {
        use super::super::WaitOutcome;
        for r in [
            None,
            Some(Ok(0)),
            Some(Ok(7)),
            Some(Err(WaitOutcome::Invalid)),
            Some(Err(WaitOutcome::Timeout)),
        ] {
            let back = ready_of(ready_word(r.as_ref().map(|r| match r {
                Ok(i) => Ok(*i),
                Err(WaitOutcome::Invalid) => Err(WaitOutcome::Invalid),
                Err(_) => Err(WaitOutcome::Timeout),
            })));
            assert_eq!(format_of(&back), format_of(&r));
        }
    }

    fn format_of(r: &Option<Result<u32, super::super::WaitOutcome>>) -> alloc::string::String {
        use super::super::WaitOutcome;
        alloc::format!(
            "{:?}",
            r.as_ref().map(|r| match r {
                Ok(i) => *i as i64,
                Err(WaitOutcome::Invalid) => -2,
                Err(WaitOutcome::Timeout) => -3,
                Err(WaitOutcome::Signaled { .. }) => -1,
            })
        )
    }

    #[test]
    fn a_line_comes_back_as_it_went() {
        let line = b"0x3:1/0+1(ctx2)";
        let w = words_of_bytes(line);
        assert_eq!(w.len(), 2);
        assert_eq!(bytes_of_words(&w, line.len()), line);
    }
}
