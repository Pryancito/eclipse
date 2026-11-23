//! POSIX advisory record locks — `fcntl(2)` F_GETLK / F_SETLK / F_SETLKW.
//!
//! One global table keyed by the file's `(dev, inode)` identity holds byte
//! ranges per owning process. Package managers (`apk add` uses a lock file,
//! dpkg's `/var/lib/dpkg/lock`) and databases take whole-file write locks
//! through this API and treat `ENOSYS` as either "no locking, race freely"
//! or a hard error — both bad answers.
//!
//! A process's locks on a file go when it closes ANY descriptor of that
//! file (`close`, `close_range`, the descriptor a `dup2`/`dup3` replaces,
//! and the close-on-exec ones an `execve` closes: [`release_file_owner`]),
//! and all of them when it dies ([`release_owner`]; a conflict scan also
//! prunes entries whose owner pid no longer resolves).
//!
//! Open-file-description locks (`F_OFD_*`) share the table, owned by the
//! description (`ofd`) instead of the process: two descriptions of one file
//! conflict even inside one process, a `dup` or a `fork` shares the lock,
//! and the last close of the description (`File`'s `Drop`) releases it.
//! `F_GETLK` reports an `l_pid` of -1 for one, as Linux does.

use crate::error::{LxError, LxResult};
use crate::kmem::Charge;
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
///
/// Each one is charged to the process that took it (Linux's
/// `filelock_cache` is `SLAB_ACCOUNT`) and given back when it goes, by any
/// road: unlocked, carved, merged, pruned, or released on close or exit.
/// Locking every other byte of a file made one entry per byte, kept for
/// as long as the file was open, with nothing to stop it.
#[derive(Debug)]
struct Held {
    exclusive: bool,
    start: u64,
    end: u64,
    owner: KoID,
    ofd: bool,
    charge: Charge,
}

/// What one held lock costs its taker.
pub const HELD_COST: usize = core::mem::size_of::<Held>();

/// A lock request/probe after `l_whence` resolution.
#[derive(Debug, Clone, Copy)]
pub struct LockRequest {
    /// True for `F_WRLCK`, false for `F_RDLCK`.
    pub exclusive: bool,
    /// Absolute first byte.
    pub start: u64,
    /// One past the last byte; `u64::MAX` = unbounded (`l_len == 0`).
    pub end: u64,
    /// Owning process, or the description's address when `ofd`.
    pub owner: KoID,
    /// An open-file-description lock: `owner` names a `File`, not a process.
    pub ofd: bool,
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
    /// Owner pid; [`OFD_PID`] for a description's lock.
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
    !same_owner(held, req)
        && (held.exclusive || req.exclusive)
        && ranges_overlap(held.start, held.end, req.start, req.end)
}

/// What `F_GETLK` puts in `l_pid` for a description's lock: -1 once it is
/// cut to the `int` of `struct flock`.
pub const OFD_PID: KoID = KoID::MAX;

/// Whether any `F_OFD_*` lock was ever taken, so a close need not look.
static OFD_EVER_TAKEN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn same_owner(held: &Held, req: &LockRequest) -> bool {
    held.owner == req.owner && held.ofd == req.ofd
}

fn owner_alive(held: &Held) -> bool {
    // A description's lock goes with its last close, not with a process.
    held.ofd || ROOT_JOB.find_process(held.owner).is_some()
}

/// Drop every entry whose owner no longer exists (exit-time cleanup, done
/// lazily on each table visit).
fn prune_dead(locks: &mut Vec<Held>) {
    locks.retain(owner_alive);
}

/// The last close of the open file description at `description`: every
/// `F_OFD_*` lock it holds goes (`locks_remove_file`).
pub fn release_description(description: usize) {
    // Called from every `File`'s drop: one load when no process ever took a
    // description's lock, which is nearly always.
    if !OFD_EVER_TAKEN.load(core::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let owner = description as KoID;
    let mut table = LOCKS.lock();
    table.retain(|_, locks| {
        locks.retain(|l| !(l.ofd && l.owner == owner));
        !locks.is_empty()
    });
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
        locks.retain(|l| l.ofd || l.owner != owner);
        !locks.is_empty()
    });
}

/// Release all locks held by `owner` on the file identified by `key`.
///
/// POSIX requires that closing ANY file descriptor referring to a file in a
/// process releases all POSIX record locks held by that process on that file.
pub fn release_file_owner(key: FileKey, owner: KoID) {
    let mut table = LOCKS.lock();
    if let Some(locks) = table.get_mut(&key) {
        locks.retain(|l| l.ofd || l.owner != owner);
        if locks.is_empty() {
            table.remove(&key);
        }
    }
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
            pid: if held.ofd { OFD_PID } else { held.owner },
        })
}

/// `F_SETLK` (one attempt): acquire, convert or release `req`'s range.
/// `Ok(false)` on conflict -- the caller answers `EAGAIN` or, for
/// `F_SETLKW`, retries. An acquisition first carves the owner's existing
/// locks out of the range (POSIX replace semantics), an unlock only carves.
///
/// As `posix_lock_inode` does, the owner's locks of the same type that
/// touch or overlap the new one merge into it, so a file locked a byte at
/// a time in order holds one lock, not one per byte. And as it does, the
/// two locks a request may need (the new one, and the far piece of a lock
/// it splits) are taken up front: without them the request is `ENOLCK`
/// and the table is as it was.
pub fn setlk(key: FileKey, req: &LockRequest, unlock: bool) -> LxResult<bool> {
    let mut table = LOCKS.lock();
    let locks = table.entry(key).or_default();
    prune_dead(locks);
    if !unlock && locks.iter().any(|held| conflicts(held, req)) {
        return Ok(false);
    }
    let merges = |h: &Held, start: u64, end: u64| {
        !unlock
            && same_owner(h, req)
            && h.exclusive == req.exclusive
            && h.start <= end
            && start <= h.end
    };
    // The owner's locks never overlap each other, so at most one holds the
    // whole range with room on both sides; one of the same type merges
    // instead. And a lock that merges lends the new one its charge, so
    // filling the gap between two locks needs no room at all.
    let splits = locks
        .iter()
        .filter(|h| same_owner(h, req) && h.start < req.start && h.end > req.end)
        .filter(|h| !merges(h, req.start, req.end))
        .count();
    let lends = locks.iter().any(|h| merges(h, req.start, req.end));
    let mut spare = Vec::new();
    let mut next: Vec<Held> = Vec::new();
    let need = splits + (!unlock && !lends) as usize;
    if spare.try_reserve_exact(need).is_err() || next.try_reserve(locks.len() + need).is_err() {
        return Err(LxError::ENOLCK);
    }
    for _ in 0..need {
        spare.push(Charge::try_new(HELD_COST).map_err(|_| LxError::ENOLCK)?);
    }
    let (mut start, mut end) = (req.start, req.end);
    let mut lent = None;
    for held in locks.drain(..) {
        if !same_owner(&held, req) {
            next.push(held);
        } else if merges(&held, start, end) {
            // Same type, touching or overlapping: one lock now.
            start = start.min(held.start);
            end = end.max(held.end);
            lent = lent.or(Some(held.charge));
        } else if !ranges_overlap(held.start, held.end, req.start, req.end) {
            next.push(held);
        } else {
            // Carve `req`'s range out: 0, 1 or 2 pieces survive.
            let Held {
                exclusive,
                start: from,
                end: to,
                owner,
                ofd,
                charge,
            } = held;
            let mut charge = Some(charge);
            let piece = |start, end, charge| Held {
                exclusive,
                start,
                end,
                owner,
                ofd,
                charge,
            };
            if from < req.start {
                next.push(piece(from, req.start, charge.take().unwrap()));
            }
            if to > req.end {
                let charge = charge.take().or_else(|| spare.pop()).unwrap();
                next.push(piece(req.end, to, charge));
            }
        }
    }
    if !unlock {
        if req.ofd {
            OFD_EVER_TAKEN.store(true, core::sync::atomic::Ordering::Relaxed);
        }
        next.push(Held {
            exclusive: req.exclusive,
            start,
            end,
            owner: req.owner,
            ofd: req.ofd,
            charge: lent.or_else(|| spare.pop()).unwrap(),
        });
    }
    if next.is_empty() {
        table.remove(&key);
    } else {
        *locks = next;
    }
    Ok(true)
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
            ofd: false,
        }
    }

    fn held(exclusive: bool, start: u64, end: u64, owner: KoID) -> Held {
        Held {
            exclusive,
            start,
            end,
            owner,
            ofd: false,
            charge: Charge::none(),
        }
    }

    fn ranges(key: FileKey) -> Vec<(bool, u64, u64)> {
        let mut v: Vec<_> = LOCKS
            .lock()
            .get(&key)
            .map(|l| l.iter().map(|h| (h.exclusive, h.start, h.end)).collect())
            .unwrap_or_default();
        v.sort_by_key(|r| r.1);
        v
    }

    /// A description's lock: owned by the `File`, not by the process.
    #[test]
    fn a_description_lock_conflicts_inside_one_process_and_goes_with_its_close() {
        let key = (0xd0fd, 1);
        let pid = 9_001;
        let ofd = |exclusive, at: usize| LockRequest {
            exclusive,
            start: 0,
            end: 10,
            owner: at as KoID,
            ofd: true,
        };
        assert!(setlk(key, &ofd(true, 0x1000), false).unwrap());
        assert!(
            !setlk(key, &ofd(true, 0x2000), false).unwrap(),
            "two descriptions conflict"
        );
        assert!(
            setlk(key, &ofd(true, 0x1000), false).unwrap(),
            "the same one re-locks"
        );
        let posix = req(false, 0, 10, pid);
        assert_eq!(getlk(key, &posix).map(|c| c.pid as i32), Some(-1));
        // An owner koid that happens to equal the address is not the owner.
        assert!(getlk(key, &req(false, 0, 10, 0x1000)).is_some());
        release_owner(0x1000);
        assert!(
            getlk(key, &posix).is_some(),
            "a process exit is not its close"
        );
        release_description(0x1000);
        assert!(getlk(key, &posix).is_none());
        assert!(setlk(key, &ofd(true, 0x2000), true).unwrap());
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
    fn an_unlock_splits_the_middle_and_trims_the_edges() {
        let key = (usize::MAX - 2, 1);
        let pid = 0; // never a live process, but `ofd` keeps it from the prune
        let r = |exclusive, start, end| LockRequest {
            exclusive,
            start,
            end,
            owner: pid,
            ofd: true,
        };
        assert_eq!(setlk(key, &r(true, 10, 50), false), Ok(true));
        // A hole in the middle: two pieces.
        assert_eq!(setlk(key, &r(true, 20, 30), true), Ok(true));
        assert_eq!(ranges(key), [(true, 10, 20), (true, 30, 50)]);
        // Trim the head.
        assert_eq!(setlk(key, &r(true, 0, 15), true), Ok(true));
        assert_eq!(ranges(key), [(true, 15, 20), (true, 30, 50)]);
        // Disjoint: untouched.
        assert_eq!(setlk(key, &r(true, 50, 60), true), Ok(true));
        assert_eq!(ranges(key), [(true, 15, 20), (true, 30, 50)]);
        // Full cover: nothing left, and the file leaves the table.
        assert_eq!(setlk(key, &r(true, 0, 100), true), Ok(true));
        assert!(!LOCKS.lock().contains_key(&key));
    }

    #[test]
    fn locks_of_one_type_that_touch_merge_and_another_type_carves() {
        let key = (usize::MAX - 2, 2);
        let r = |exclusive, start, end| LockRequest {
            exclusive,
            start,
            end,
            owner: 0,
            ofd: true,
        };
        // A byte at a time, in order: one lock.
        for b in 0..100 {
            assert_eq!(setlk(key, &r(true, b, b + 1), false), Ok(true));
        }
        assert_eq!(ranges(key), [(true, 0, 100)]);
        // A read lock in the middle converts that part only.
        assert_eq!(setlk(key, &r(false, 40, 60), false), Ok(true));
        assert_eq!(
            ranges(key),
            [(true, 0, 40), (false, 40, 60), (true, 60, 100)]
        );
        // Writing it back merges all three again.
        assert_eq!(setlk(key, &r(true, 40, 60), false), Ok(true));
        assert_eq!(ranges(key), [(true, 0, 100)]);
        assert_eq!(setlk(key, &r(true, 0, u64::MAX), true), Ok(true));
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
