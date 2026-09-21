//! POSIX advisory record locks — `fcntl(2)` F_GETLK / F_SETLK / F_SETLKW.
//!
//! One global table keyed by the file's `(dev, inode)` identity holds byte
//! ranges per owning process. Package managers (`apk add` uses a lock file,
//! dpkg's `/var/lib/dpkg/lock`) and databases take whole-file write locks
//! through this API and treat `ENOSYS` as either "no locking, race freely"
//! or a hard error — both bad answers.
//!
//! Two deliberate simplifications, both safe on this single-user system:
//!
//! * Locks are dropped when the owning **process dies** (a conflict scan
//!   ignores and prunes entries whose owner pid no longer resolves), not on
//!   the infamous POSIX "close of any fd to the file" trigger. Locks held by
//!   a live process that closed the file linger until it exits.
//! * Open-file-description locks (`F_OFD_*`) share the classic table with
//!   the process as owner, which collapses their one behavioural difference
//!   (per-description ownership) into per-process ownership.

use crate::error::{LxError, LxResult};
use alloc::vec::Vec;
use hashbrown::HashMap;
use lazy_static::lazy_static;
use lock::Mutex;
use zircon_object::object::KoID;
use zircon_object::task::ROOT_JOB;

/// `l_type` values from `<fcntl.h>`.
pub const F_RDLCK: i16 = 0;
/// Exclusive (write) lock.
pub const F_WRLCK: i16 = 1;
/// Unlock.
pub const F_UNLCK: i16 = 2;

/// A file's identity in the lock table: `(st_dev, st_ino)`.
pub type FileKey = (usize, usize);

/// One held lock: `[start, end)` with `end == u64::MAX` meaning "to EOF and
/// beyond" (`l_len == 0`).
#[derive(Debug, Clone, Copy)]
struct Held {
    exclusive: bool,
    start: u64,
    end: u64,
    owner: KoID,
}

/// A lock request/probe after `l_whence` resolution.
#[derive(Debug, Clone, Copy)]
pub struct LockRequest {
    /// True for `F_WRLCK`, false for `F_RDLCK`.
    pub exclusive: bool,
    /// Absolute first byte.
    pub start: u64,
    /// One past the last byte; `u64::MAX` = unbounded (`l_len == 0`).
    pub end: u64,
    /// Owning process.
    pub owner: KoID,
}

/// What `F_GETLK` reports about a conflicting lock.
#[derive(Debug, Clone, Copy)]
pub struct ConflictInfo {
    /// `F_RDLCK` or `F_WRLCK`.
    pub type_: i16,
    /// Absolute first byte of the conflicting lock.
    pub start: u64,
    /// `l_len` encoding: 0 for unbounded, else the byte count.
    pub len: u64,
    /// Owner pid.
    pub pid: KoID,
}

/// Resolve a `struct flock`'s `l_start` and `l_len` against an already
/// resolved `l_whence` base into the absolute `[start, end)` range the lock
/// table speaks, with `u64::MAX` for "to the end of the file and beyond".
///
/// Three things make this worth writing down rather than inlining.
///
/// A **negative `l_len` is legal** and means the range *before* `l_start`,
/// which POSIX spells out and which programs do use. Treating it as an error,
/// or as its absolute value, locks a different part of the file than the one
/// the caller asked for -- and a record lock over the wrong range is a data
/// corruption bug in every program that relies on `fcntl` locking (sqlite,
/// dpkg, apk) with no error anywhere.
///
/// The arithmetic **overflows**: all three inputs are a full `i64` straight
/// out of userspace. `base + l_start` can leave the range, and `-l_len` is
/// not representable when `l_len` is `i64::MIN`. Linux answers `EOVERFLOW`;
/// wrapping instead silently locks somewhere else entirely.
///
/// And the end is **exclusive** here while Linux's `fl_end` is inclusive, so
/// the `- 1` in its bounds checks does not appear.
pub fn resolve_range(base: i64, l_start: i64, l_len: i64) -> LxResult<(u64, u64)> {
    let start = base.checked_add(l_start).ok_or(LxError::EOVERFLOW)?;
    let (start, end) = if l_len == 0 {
        // To the end of the file, and anything appended to it later.
        (start, None)
    } else if l_len > 0 {
        (
            start,
            Some(start.checked_add(l_len).ok_or(LxError::EOVERFLOW)?),
        )
    } else {
        // The range before `l_start`: [start + l_len, start).
        let first = start.checked_add(l_len).ok_or(LxError::EOVERFLOW)?;
        (first, Some(start))
    };
    if start < 0 {
        return Err(LxError::EINVAL);
    }
    Ok((start as u64, end.map_or(u64::MAX, |e| e as u64)))
}

lazy_static! {
    static ref LOCKS: Mutex<HashMap<FileKey, Vec<Held>>> = Mutex::new(HashMap::new());
}

fn ranges_overlap(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> bool {
    a_start < b_end && b_start < a_end
}

/// Two locks conflict when their ranges overlap, the owners differ, and at
/// least one side is exclusive — fcntl(2)'s compatibility rule.
fn conflicts(held: &Held, req: &LockRequest) -> bool {
    held.owner != req.owner
        && (held.exclusive || req.exclusive)
        && ranges_overlap(held.start, held.end, req.start, req.end)
}

/// Remove `[start, end)` from a held range, returning the surviving pieces
/// (0, 1 or 2). This is what lets an unlock or a re-lock punch a hole in the
/// middle of an existing lock, per POSIX.
fn subtract(held: &Held, start: u64, end: u64) -> Vec<Held> {
    let mut out = Vec::new();
    if !ranges_overlap(held.start, held.end, start, end) {
        out.push(*held);
        return out;
    }
    if held.start < start {
        out.push(Held {
            end: start,
            ..*held
        });
    }
    if held.end > end {
        out.push(Held {
            start: end,
            ..*held
        });
    }
    out
}

fn owner_alive(owner: KoID) -> bool {
    ROOT_JOB.find_process(owner).is_some()
}

/// Drop every entry whose owner no longer exists (exit-time cleanup, done
/// lazily on each table visit).
fn prune_dead(locks: &mut Vec<Held>) {
    locks.retain(|l| owner_alive(l.owner));
}

/// Release every lock `owner` holds, on any file. Called when the process
/// terminates, so that a pid recycled onto an unrelated live process can
/// never resurrect a dead owner's locks: Firefox's profile lock
/// (`.parentlock`, `F_SETLK`) is exactly that pattern -- one browser exits,
/// its pid is handed to the next launch's helper, and the lazy "is the
/// owner alive?" prune above then keeps the stale lock, so the second
/// browser reports "Firefox is already running".
pub fn release_owner(owner: KoID) {
    let mut table = LOCKS.lock();
    table.retain(|_, locks| {
        locks.retain(|l| l.owner != owner);
        !locks.is_empty()
    });
}

/// `F_GETLK`: describe the first lock blocking `req`, or `None` when the
/// request would succeed.
pub fn getlk(key: FileKey, req: &LockRequest) -> Option<ConflictInfo> {
    let mut table = LOCKS.lock();
    let locks = table.get_mut(&key)?;
    prune_dead(locks);
    locks
        .iter()
        .find(|held| conflicts(held, req))
        .map(|held| ConflictInfo {
            type_: if held.exclusive { F_WRLCK } else { F_RDLCK },
            start: held.start,
            len: if held.end == u64::MAX {
                0
            } else {
                held.end - held.start
            },
            pid: held.owner,
        })
}

/// `F_SETLK` (one attempt): acquire, convert or release `req`'s range.
/// Returns false on conflict — the caller answers `EAGAIN` or, for
/// `F_SETLKW`, retries. An acquisition first carves the owner's existing
/// locks out of the range (POSIX replace semantics), an unlock only carves.
pub fn setlk(key: FileKey, req: &LockRequest, unlock: bool) -> bool {
    let mut table = LOCKS.lock();
    let locks = table.entry(key).or_default();
    prune_dead(locks);
    if !unlock && locks.iter().any(|held| conflicts(held, req)) {
        return false;
    }
    let mut next: Vec<Held> = Vec::with_capacity(locks.len() + 1);
    for held in locks.iter() {
        if held.owner == req.owner {
            next.extend(subtract(held, req.start, req.end));
        } else {
            next.push(*held);
        }
    }
    if !unlock {
        next.push(Held {
            exclusive: req.exclusive,
            start: req.start,
            end: req.end,
            owner: req.owner,
        });
    }
    if next.is_empty() {
        table.remove(&key);
    } else {
        *table.get_mut(&key).unwrap() = next;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(exclusive: bool, start: u64, end: u64, owner: KoID) -> LockRequest {
        LockRequest {
            exclusive,
            start,
            end,
            owner,
        }
    }

    fn held(exclusive: bool, start: u64, end: u64, owner: KoID) -> Held {
        Held {
            exclusive,
            start,
            end,
            owner,
        }
    }

    #[test]
    fn read_locks_share_write_locks_exclude() {
        let shared = held(false, 0, 100, 1);
        let exclusive = held(true, 0, 100, 1);
        // Same owner never conflicts with itself.
        assert!(!conflicts(&exclusive, &req(true, 0, 100, 1)));
        // Two readers coexist.
        assert!(!conflicts(&shared, &req(false, 0, 100, 2)));
        // A writer excludes readers and writers alike.
        assert!(conflicts(&shared, &req(true, 0, 100, 2)));
        assert!(conflicts(&exclusive, &req(false, 0, 100, 2)));
        // Disjoint ranges never conflict.
        assert!(!conflicts(&exclusive, &req(true, 100, 200, 2)));
    }

    #[test]
    fn subtract_splits_middle_and_trims_edges() {
        let h = held(true, 10, 50, 1);
        // Hole in the middle → two pieces.
        let pieces = subtract(&h, 20, 30);
        assert_eq!(pieces.len(), 2);
        assert_eq!((pieces[0].start, pieces[0].end), (10, 20));
        assert_eq!((pieces[1].start, pieces[1].end), (30, 50));
        // Trim the head.
        let pieces = subtract(&h, 0, 20);
        assert_eq!(pieces.len(), 1);
        assert_eq!((pieces[0].start, pieces[0].end), (20, 50));
        // Full cover → nothing left.
        assert!(subtract(&h, 0, 100).is_empty());
        // Disjoint → untouched.
        let pieces = subtract(&h, 50, 60);
        assert_eq!(pieces.len(), 1);
        assert_eq!((pieces[0].start, pieces[0].end), (10, 50));
    }

    #[test]
    fn release_owner_forgets_every_lock_of_that_owner_only() {
        let key_a: FileKey = (usize::MAX - 1, 41);
        let key_b: FileKey = (usize::MAX - 1, 42);
        {
            let mut table = LOCKS.lock();
            table
                .entry(key_a)
                .or_default()
                .extend([held(true, 0, u64::MAX, 7), held(false, 0, 10, 8)]);
            table.entry(key_b).or_default().push(held(true, 0, 10, 7));
        }
        release_owner(7);
        let table = LOCKS.lock();
        // key_b held only 7's lock: the whole entry is gone.
        assert!(!table.contains_key(&key_b));
        // key_a keeps 8's lock and nothing of 7's.
        let left = &table[&key_a];
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].owner, 8);
    }

    #[test]
    fn unbounded_ranges_use_u64_max_end() {
        let h = held(true, 100, u64::MAX, 1);
        assert!(conflicts(&h, &req(false, 1000, 1001, 2)));
        assert!(!conflicts(&h, &req(true, 0, 100, 2)));
    }
}

#[cfg(test)]
mod range_tests {
    //! `fcntl(2)` record locks, the range half.
    //!
    //! The three fields of a `struct flock` are a full `i64` each, straight
    //! out of userspace, and a negative `l_len` is **legal**: it names the
    //! range *before* `l_start`. Get any of it wrong and the program locks a
    //! different part of the file than the one it asked for, with no error
    //! anywhere -- which for the programs that rely on `fcntl` locking
    //! (sqlite, dpkg, apk) is a data corruption bug, not an inconvenience.

    use super::*;

    fn ok(base: i64, start: i64, len: i64) -> (u64, u64) {
        resolve_range(base, start, len).unwrap()
    }

    #[test]
    fn a_plain_forward_range_is_what_it_says() {
        assert_eq!(ok(0, 10, 5), (10, 15));
        assert_eq!(ok(0, 0, 1), (0, 1));
    }

    #[test]
    fn the_end_is_exclusive() {
        // Linux's `fl_end` is the last byte; this table's `end` is one past
        // it. A lock of one byte at 10 must not reach byte 11, or two
        // programs locking adjacent single bytes would conflict.
        let (start, end) = ok(0, 10, 1);
        assert_eq!((start, end), (10, 11));
        assert!(
            !ranges_overlap(start, end, 11, 12),
            "10 and 11 must not clash"
        );
        assert!(ranges_overlap(start, end, 10, 20), "10 and 10 must clash");
    }

    #[test]
    fn a_length_of_zero_runs_to_the_end_of_the_file_and_beyond() {
        // This is how every whole-file lock is written, and it has to cover
        // bytes appended after the lock was taken.
        assert_eq!(ok(0, 0, 0), (0, u64::MAX));
        assert_eq!(ok(0, 4096, 0), (4096, u64::MAX));
    }

    #[test]
    fn a_negative_length_names_the_range_before_the_start() {
        // POSIX says so. Taking the absolute value instead would lock
        // [100, 110) where the caller asked for [90, 100) -- a lock on the
        // wrong bytes, with nothing reported.
        assert_eq!(ok(0, 100, -10), (90, 100));
        assert_eq!(ok(0, 1, -1), (0, 1));
    }

    #[test]
    fn a_negative_length_covers_exactly_the_same_bytes_as_its_positive_twin() {
        // Locking [90, 100) has one spelling from each side, and they have
        // to agree byte for byte.
        for &(start, len) in &[(100i64, -10i64), (50, -50), (4096, -4096)] {
            let back = ok(0, start, len);
            let forward = ok(0, start + len, -len);
            assert_eq!(back, forward, "l_start {} l_len {}", start, len);
        }
    }

    #[test]
    fn the_whence_base_is_added_to_the_start() {
        // SEEK_CUR and SEEK_END arrive here as a base. Ignoring it locks the
        // wrong end of the file.
        assert_eq!(ok(1000, 10, 5), (1010, 1015));
        assert_eq!(ok(1000, -10, 5), (990, 995));
        assert_eq!(ok(1000, 0, -100), (900, 1000));
    }

    #[test]
    fn a_range_starting_before_the_file_is_refused() {
        // A lock cannot cover negative offsets, however it was spelled.
        assert!(matches!(resolve_range(0, -1, 5), Err(LxError::EINVAL)));
        assert!(matches!(resolve_range(10, -20, 5), Err(LxError::EINVAL)));
        assert!(
            matches!(resolve_range(0, 5, -10), Err(LxError::EINVAL)),
            "a backwards range that runs off the front"
        );
    }

    #[test]
    fn a_start_that_leaves_the_range_of_an_offset_is_refused_not_wrapped() {
        // `base + l_start` on two full i64s. Wrapping lands the lock at some
        // small offset and it silently blocks an unrelated part of the file.
        assert!(matches!(
            resolve_range(1, i64::MAX, 0),
            Err(LxError::EOVERFLOW)
        ));
        assert!(matches!(
            resolve_range(i64::MAX, i64::MAX, 0),
            Err(LxError::EOVERFLOW)
        ));
    }

    #[test]
    fn an_end_that_leaves_the_range_of_an_offset_is_refused_not_wrapped() {
        assert!(matches!(
            resolve_range(0, i64::MAX, i64::MAX),
            Err(LxError::EOVERFLOW)
        ));
        assert!(matches!(
            resolve_range(0, i64::MAX - 1, 2),
            Err(LxError::EOVERFLOW)
        ));
        // One less and it fits exactly.
        assert_eq!(
            ok(0, i64::MAX - 1, 1),
            ((i64::MAX - 1) as u64, i64::MAX as u64)
        );
    }

    #[test]
    fn the_most_negative_length_is_refused_rather_than_negated() {
        // `-i64::MIN` is not representable: negating it overflows. This is
        // the one a fuzzer finds first, and it is one `fcntl` call away from
        // userspace.
        assert!(resolve_range(0, 0, i64::MIN).is_err());
        assert!(resolve_range(i64::MAX, 0, i64::MIN).is_err());
        // And with a base that would make it land in range, it is still
        // refused rather than wrapped to a positive length.
        assert!(matches!(
            resolve_range(0, i64::MAX, i64::MIN),
            Err(LxError::EINVAL) | Err(LxError::EOVERFLOW)
        ));
    }

    #[test]
    fn a_backwards_range_that_leaves_the_offset_range_is_refused_not_wrapped() {
        // `start + l_len` with a negative `l_len` can underflow just as the
        // forward case overflows, and a wrap there comes back **positive**:
        // a lock up near 2^63 that passes every later check and silently
        // covers bytes nobody asked about. `l_whence` only ever hands this
        // function a non-negative base today, so these values are not
        // reachable from `fcntl`; this is the invariant the function owes
        // its caller, and what a future `l_whence` case would break.
        assert!(matches!(
            resolve_range(i64::MIN, 0, -1),
            Err(LxError::EOVERFLOW)
        ));
        assert!(matches!(
            resolve_range(i64::MIN + 5, 0, -10),
            Err(LxError::EOVERFLOW)
        ));
    }

    #[test]
    fn nothing_userspace_can_send_gets_through_unresolved() {
        // Sweep the awkward values in all three fields: every combination
        // has to come back as a range or an error, never a panic and never a
        // range whose end is before its start.
        let edges = [
            i64::MIN,
            i64::MIN + 1,
            -4096,
            -1,
            0,
            1,
            4096,
            i64::MAX - 1,
            i64::MAX,
        ];
        for &base in &edges {
            for &start in &edges {
                for &len in &edges {
                    if let Ok((s, e)) = resolve_range(base, start, len) {
                        let where_ = || alloc::format!("base {} start {} len {}", base, start, len);
                        assert!(s <= e, "{} gave [{}, {})", where_(), s, e);
                        // Every byte of an accepted range has to be one an
                        // `off_t` can name. A wrap lands inside `u64` but
                        // outside that, which is how a silently relocated
                        // lock would look.
                        assert!(s <= i64::MAX as u64, "{} starts past off_t", where_());
                        assert!(
                            e <= i64::MAX as u64 || e == u64::MAX,
                            "{} ends past off_t at {}",
                            where_(),
                            e
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn an_empty_range_is_never_produced_from_a_non_zero_length() {
        // A zero-width lock conflicts with nothing, so a request that
        // collapsed to one would silently do nothing at all.
        for &len in &[1i64, 4096, -1, -4096] {
            let (s, e) = ok(8192, 0, len);
            assert!(e > s, "len {} collapsed to [{}, {})", len, s, e);
        }
    }
}
