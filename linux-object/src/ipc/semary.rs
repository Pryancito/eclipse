//! Linux semaphore ipc
use super::*;
use crate::error::LxError;
use crate::sync::Semaphore;
use crate::time::*;
use alloc::{collections::BTreeMap, sync::Arc, sync::Weak, vec::Vec};
use kernel_hal::sync::{Mutex, RwLock};
use lazy_static::*;

/// semid data structure
///
/// struct semid_ds
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SemidDs {
    /// Ownership and permissions
    pub perm: IpcPerm,
    /// Last semop time
    pub otime: usize,
    __pad1: usize,
    /// Last change time
    pub ctime: usize,
    __pad2: usize,
    /// number of semaphores in set
    pub nsems: usize,
}

/// A System V semaphore set
pub struct SemArray {
    /// semid data structure
    pub semid_ds: Mutex<SemidDs>,
    sems: Vec<Semaphore>,
    /// Serialises `semop` over the whole set.
    ///
    /// semop(2): "The set of operations contained in `sops` is performed in
    /// array order, and **atomically**, that is, the operations are performed
    /// either as a complete unit, or not at all." Each `Semaphore` has its
    /// own lock, which is enough for one of them and says nothing about the
    /// set -- two callers could interleave halfway through their arrays and
    /// both see a state neither of them ever agreed to. Linux keeps one lock
    /// per set for exactly this; so does this.
    semop_lock: Mutex<()>,
}

impl SemArray {
    /// Take the set-wide `semop` lock. Held across the plan AND the apply,
    /// which is what makes the array atomic.
    pub fn semop_guard(&self) -> kernel_hal::sync::MutexGuard<'_, ()> {
        self.semop_lock.lock()
    }

    /// The value and generation of every semaphore in the set, read under the
    /// caller's [`semop_guard`](Self::semop_guard).
    pub fn snapshot(&self) -> Vec<(isize, u64)> {
        self.sems.iter().map(|s| s.get_versioned()).collect()
    }

    /// Number of semaphores in the set.
    pub fn len(&self) -> usize {
        self.sems.len()
    }

    /// Returns `true` if the set contains no semaphores.
    pub fn is_empty(&self) -> bool {
        self.sems.is_empty()
    }

    /// Returns the semaphore at `idx`, or `None` if out of range.
    ///
    /// This is the only way in. There used to be an `Index<usize>` impl beside
    /// it, and every `sem_array[n]` written through it was a panic waiting for
    /// a semaphore number out of userspace: `semctl` passed one straight
    /// through. The impl is gone rather than fixed, so the next caller cannot
    /// write that line again.
    pub fn get_sem(&self, idx: usize) -> Option<&Semaphore> {
        self.sems.get(idx)
    }
}

lazy_static! {
    static ref KEY2SEM: RwLock<BTreeMap<u32, Weak<SemArray>>> = RwLock::new(BTreeMap::new());
    /// Every semaphore set in the system, under the id `semget(2)` handed
    /// out for it. See [`sem_register`].
    static ref SEMID2SEM: RwLock<BTreeMap<SemId, Arc<SemArray>>> =
        RwLock::new(BTreeMap::new());
    /// The next id to hand out. Ids are never reused, so a stale `semid` kept
    /// by a program whose set was removed names nothing rather than somebody
    /// else's set (Linux gets the same property from the sequence number it
    /// packs into the id).
    static ref NEXT_SEMID: Mutex<SemId> = Mutex::new(1);
}

/// `SEMMNI`: how many sets may exist at once, Linux's own default.
pub const SEMMNI: usize = 32000;

/// Register a set under a **system-wide** id and return it, or return the id
/// it already has.
///
/// `semget(2)` returns an identifier that means the same set in every
/// process, and the set lives until `semctl(IPC_RMID)` whether or not its
/// creator is still around (sysvipc(7)). Neither was true here. The id was an
/// index into the CALLING process's own table, counted from 0 per process, so
/// an id passed to an unrelated program (`ipcrm -s`, a cleanup script, a
/// lock daemon handing its clients a semid through a file) named a different
/// set there or nothing at all. And the only strong references to a set were
/// the tables of the processes that had `semget`-ed it: a setup program that
/// created a set, initialised it with `SETALL` and exited took the set with
/// it, and the daemon that came next got `ENOENT` for the key -- and every
/// value it had been given was gone.
///
/// `msg_get` and `shm_register` already keep their objects this way; the
/// semaphore sets were the last of the three left behind.
pub fn sem_register(array: &Arc<SemArray>) -> Result<SemId, LxError> {
    let mut table = SEMID2SEM.write();
    // `semget` on an existing key answers with the id that key already has,
    // as Linux does -- not a second id for the same set.
    if let Some((&id, _)) = table.iter().find(|(_, a)| Arc::ptr_eq(a, array)) {
        return Ok(id);
    }
    // The table is what keeps a set alive past its creator, so without a
    // bound a loop of `semget(IPC_PRIVATE, ...)` from an ordinary process
    // would pin kernel memory for ever. Linux bounds it the same way and
    // with the same error.
    if table.len() >= SEMMNI {
        return Err(LxError::ENOSPC);
    }
    let mut next = NEXT_SEMID.lock();
    let id = *next;
    *next += 1;
    table.insert(id, array.clone());
    Ok(id)
}

/// `/proc/sysvipc/sem` (Documentation/filesystems/proc.rst): one line per
/// set in the kernel's column layout (`sysvipc_sem_proc_show`, ipc/sem.c),
/// consumed by `ipcs -s`. The key is printed as the signed `int` it is in
/// `struct ipc_perm`, so a key past `i32::MAX` shows negative, as on Linux.
pub fn sem_proc_table() -> alloc::string::String {
    use core::fmt::Write as _;
    let mut out = alloc::string::String::from(
        "       key      semid perms      nsems   uid   gid  cuid  cgid      otime      ctime\n",
    );
    for (id, array) in SEMID2SEM.read().iter() {
        let ds = *array.semid_ds.lock();
        let _ = writeln!(
            out,
            "{:>10} {:>10} {:>5o} {:>10} {:>5} {:>5} {:>5} {:>5} {:>10} {:>10}",
            ds.perm.key as i32,
            id,
            ds.perm.mode,
            ds.nsems,
            ds.perm.uid,
            ds.perm.gid,
            ds.perm.cuid,
            ds.perm.cgid,
            ds.otime,
            ds.ctime,
        );
    }
    out
}

/// The set an id names, from any process.
pub fn sem_lookup(id: SemId) -> Option<Arc<SemArray>> {
    SEMID2SEM.read().get(&id).cloned()
}

/// `ipc_get_maxidx` for the set table: the highest INDEX in use, `None`
/// when there is no set. Ids are handed out from 1 and never reused, so
/// slot `i` is id `i + 1`; `SEM_INFO` and `IPC_INFO` return this and `ipcs
/// -s` walks `SEM_STAT` up to it.
pub fn sem_max_index() -> Option<usize> {
    SEMID2SEM.read().keys().next_back().map(|&id| id - 1)
}

/// `semctl(idx, 0, SEM_STAT, ..)`: the set in slot `idx`, with the id the
/// call returns for it; `None` is `EINVAL`.
pub fn sem_at_index(idx: usize) -> Option<(SemId, Arc<SemArray>)> {
    let id = idx.checked_add(1)?;
    sem_lookup(id).map(|array| (id, array))
}

/// For `SEM_INFO`: how many sets exist (`semusz`) and how many semaphores
/// they hold between them (`semaem`).
pub fn sem_totals() -> (usize, usize) {
    let table = SEMID2SEM.read();
    (table.len(), table.values().map(|a| a.len()).sum())
}

/// `semctl(id, IPC_RMID, ..)`: the id stops naming the set. A process still
/// holding it in its own table keeps the object (its waiters wake into
/// `EIDRM` through [`SemArray::remove`]); nobody can find it again.
pub fn sem_unregister(id: SemId) -> bool {
    SEMID2SEM.write().remove(&id).is_some()
}

lazy_static! {}

impl SemArray {
    fn purge_stale_keys(map: &mut BTreeMap<u32, Weak<SemArray>>) {
        map.retain(|_, weak| weak.strong_count() > 0);
    }

    /// remove semaphores
    ///
    /// A private set carries key 0 and was never filed under it, so for one of
    /// those the first line finds nothing and only the semaphores are woken
    /// into `EIDRM`.
    pub fn remove(&self) {
        let mut key2sem = KEY2SEM.write();
        let key = self.semid_ds.lock().perm.key;
        key2sem.remove(&key);
        for sem in self.sems.iter() {
            sem.remove();
        }
    }

    /// set last semop time
    pub fn otime(&self) {
        self.semid_ds.lock().otime = TimeSpec::now().sec;
    }

    /// set last change time
    pub fn ctime(&self) {
        self.semid_ds.lock().ctime = TimeSpec::now().sec;
    }

    /// `IPC_SET`: owner and permission bits, per semctl(2).
    ///
    /// Only the owner, the creator or a privileged caller may do this; anyone
    /// else gets `EPERM`. Without that check the `semid_ds` userspace handed
    /// in rewrote `uid` and `gid`, so naming the id was enough to take the set
    /// over -- see [`IpcPerm::may_control`].
    pub fn set(&self, new: &SemidDs, euid: u32) -> Result<(), LxError> {
        let mut lock = self.semid_ds.lock();
        if !lock.perm.may_control(euid) {
            return Err(LxError::EPERM);
        }
        lock.perm.uid = new.perm.uid;
        lock.perm.gid = new.perm.gid;
        lock.perm.mode = new.perm.mode & 0x1ff;
        Ok(())
    }

    /// Whether `euid` may `IPC_SET` or `IPC_RMID` this set.
    pub fn may_control(&self, euid: u32) -> bool {
        self.semid_ds.lock().perm.may_control(euid)
    }

    /// Whether `euid`/`egid` may use this set for `want`: `IPC_W` for a
    /// `semop` that alters a semaphore or a `SETVAL`/`SETALL`, `IPC_R` for a
    /// wait-for-zero, a `GET*` or an `IPC_STAT`. See [`IpcPerm::may_access`].
    pub fn may_access(&self, euid: u32, egid: u32, want: u32) -> bool {
        self.semid_ds.lock().perm.may_access(euid, egid, want)
    }

    /// Get the semaphore array with `key`, following semget(2).
    /// If not exist, create a new one with `nsems` elements.
    ///
    /// Three things this gets right that it used not to, all of them things
    /// `msgget` already did:
    ///
    /// - `key == 0` is `IPC_PRIVATE`: a set with no name. It used to be given
    ///   the first free key from 1 upwards and filed under it, so a later
    ///   `semget(1, ..)` from an unrelated process attached to somebody's
    ///   private set. A private set is no longer filed at all.
    /// - A key that does not exist without `IPC_CREAT` is `ENOENT`, not a
    ///   silent create: a bare `semget(key, 0, 0)` is how a program asks
    ///   whether a set is there.
    /// - Asking an existing set for more semaphores than it has is `EINVAL`.
    ///   Without it the caller walked away believing in semaphores that do not
    ///   exist, and every index past the end came back `EFBIG` from `semop`.
    pub fn get_or_create(
        key: u32,
        nsems: usize,
        flags: usize,
        uid: u32,
        gid: u32,
    ) -> Result<Arc<Self>, LxError> {
        let mut key2sem = KEY2SEM.write();
        Self::purge_stale_keys(&mut key2sem);
        let flag = IpcGetFlag::from_bits_truncate(flags);

        if key != 0 {
            // check existence
            if let Some(weak_array) = key2sem.get(&key) {
                if let Some(array) = weak_array.upgrade() {
                    if flag.contains(IpcGetFlag::CREAT) && flag.contains(IpcGetFlag::EXCLUSIVE) {
                        // exclusive
                        return Err(LxError::EEXIST);
                    }
                    if nsems > array.len() {
                        return Err(LxError::EINVAL);
                    }
                    // Somebody else's set. Asking for a mode it will not grant
                    // is EACCES, not a working id (`ipc_check_perms`).
                    if !array.may_access(uid, gid, IpcPerm::requested_mode(flags)) {
                        return Err(LxError::EACCES);
                    }
                    return Ok(array);
                }
            }
            if !flag.contains(IpcGetFlag::CREAT) {
                return Err(LxError::ENOENT);
            }
        }

        // not found, create one
        let mut semaphores = Vec::new();
        for _ in 0..nsems {
            semaphores.push(Semaphore::new(0));
        }

        // insert to global map
        let array = Arc::new(SemArray {
            semid_ds: Mutex::new(SemidDs {
                perm: IpcPerm {
                    key,
                    uid,
                    gid,
                    cuid: uid,
                    cgid: gid,
                    // least significant 9 bits
                    mode: (flags as u32) & 0x1ff,
                    __seq: 0,
                    __pad1: 0,
                    __pad2: 0,
                },
                otime: 0,
                ctime: TimeSpec::now().sec,
                nsems,
                __pad1: 0,
                __pad2: 0,
            }),
            sems: semaphores,
            semop_lock: Mutex::new(()),
        });
        // A private set has no name, so nothing may ever look it up again.
        if key != 0 {
            key2sem.insert(key, Arc::downgrade(&array));
        }
        Ok(array)
    }
}

/// System V semaphore sets had no tests, and `semget` turned out to differ
/// from `msgget` — its own sibling, three files away — on every rule the two
/// share.
///
/// The sharp edge is `IPC_PRIVATE`. A private set is one nobody else can name;
/// this filed it under the first free key from 1 upwards, so the moment a
/// process asked for `semget(1, ..)` it attached to somebody's private set and
/// the two took turns wrecking each other's counts. The same shape, in
/// `shared_mem.rs`, was worse: see the tests there.
#[cfg(test)]
mod sem_tests {
    use super::*;
    use crate::ipc::{IPC_R, IPC_W};

    use super::test_globals::lock as test_lock;

    /// `IPC_CREAT`, as userspace spells it.
    const CREAT: usize = 0o1000;
    /// `IPC_EXCL`.
    const EXCL: usize = 0o2000;
    /// `euid == 0`.
    const ROOT: u32 = 0;
    /// The owner in the ownership tests.
    const OWNER: u32 = 1000;
    /// Somebody else.
    const STRANGER: u32 = 1001;

    /// `semget` as root, which is what every test here that does not care
    /// about ownership wants.
    fn get(key: u32, nsems: usize, flags: usize) -> Result<Arc<SemArray>, LxError> {
        SemArray::get_or_create(key, nsems, flags, ROOT, ROOT)
    }

    #[test]
    fn a_private_set_is_not_filed_under_any_key() {
        let _guard = test_lock();
        // Hold it alive: the bug needed a live set to alias onto.
        let _private = get(0, 4, CREAT | 0o666).unwrap();
        // This used to hand back `_private`, because that is where the old
        // code had filed it.
        for key in 1u32..=4 {
            assert_eq!(
                get(key, 1, 0).err(),
                Some(LxError::ENOENT),
                "a private set must not be reachable as key {}",
                key
            );
        }
    }

    #[test]
    fn a_private_set_leaves_nothing_behind_in_the_key_table() {
        let _guard = test_lock();
        // The invariant the fix rests on, stated where a later edit will trip
        // over it: key 0 is never a row in the table, so no lookup can ever
        // come back with somebody's private set.
        let _private = get(0, 1, CREAT | 0o666).unwrap();
        assert!(!KEY2SEM.read().contains_key(&0));
    }

    #[test]
    fn a_private_set_never_needs_ipc_creat() {
        let _guard = test_lock();
        // semget(2): with IPC_PRIVATE a new set is created whatever the flags
        // say, so the ENOENT rule must not reach this path.
        let a = get(0, 2, 0o666).unwrap();
        assert_eq!(a.len(), 2);
    }

    #[test]
    fn two_private_sets_are_different_sets() {
        let _guard = test_lock();
        let a = get(0, 1, CREAT | 0o666).unwrap();
        let b = get(0, 1, CREAT | 0o666).unwrap();
        assert!(
            !Arc::ptr_eq(&a, &b),
            "every IPC_PRIVATE semget is a fresh set"
        );
        a.get_sem(0).unwrap().set(7);
        assert_eq!(b.get_sem(0).unwrap().get(), 0, "and they share no counts");
    }

    #[test]
    fn a_key_that_does_not_exist_without_ipc_creat_is_enoent() {
        let _guard = test_lock();
        // A bare semget is how a program asks whether a set is there. It used
        // to create one and answer "yes".
        assert_eq!(get(0x5e_0001, 1, 0o666).err(), Some(LxError::ENOENT));
        // And with IPC_CREAT it does create it.
        let made = get(0x5e_0001, 1, CREAT | 0o666).unwrap();
        assert_eq!(made.len(), 1);
        // Now the probe finds it.
        let found = get(0x5e_0001, 1, 0o666).unwrap();
        assert!(Arc::ptr_eq(&made, &found));
    }

    #[test]
    fn the_same_key_comes_back_as_the_same_set() {
        let _guard = test_lock();
        let a = get(0x5e_0002, 3, CREAT | 0o666).unwrap();
        let b = get(0x5e_0002, 3, CREAT | 0o666).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        a.get_sem(2).unwrap().set(5);
        assert_eq!(b.get_sem(2).unwrap().get(), 5);
    }

    #[test]
    fn creat_and_excl_together_on_an_existing_key_is_eexist() {
        let _guard = test_lock();
        let _a = get(0x5e_0003, 1, CREAT | 0o666).unwrap();
        assert_eq!(
            get(0x5e_0003, 1, CREAT | EXCL | 0o666).err(),
            Some(LxError::EEXIST)
        );
    }

    #[test]
    fn excl_without_creat_returns_the_set_that_is_there() {
        let _guard = test_lock();
        // semget(2): IPC_EXCL only means anything alongside IPC_CREAT.
        let a = get(0x5e_0004, 1, CREAT | 0o666).unwrap();
        let b = get(0x5e_0004, 1, EXCL | 0o666).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn asking_an_existing_set_for_more_semaphores_than_it_has_is_einval() {
        let _guard = test_lock();
        let _a = get(0x5e_0005, 2, CREAT | 0o666).unwrap();
        // The caller walks away believing in four semaphores and gets EFBIG
        // from semop on the two that do not exist.
        assert_eq!(
            get(0x5e_0005, 4, CREAT | 0o666).err(),
            Some(LxError::EINVAL)
        );
    }

    #[test]
    fn asking_for_fewer_or_none_attaches_to_the_set_that_is_there() {
        let _guard = test_lock();
        // nsems == 0 is "don't care", and it is what every attach-only caller
        // passes.
        let a = get(0x5e_0006, 3, CREAT | 0o666).unwrap();
        let b = get(0x5e_0006, 0, 0o666).unwrap();
        let c = get(0x5e_0006, 3, 0o666).unwrap();
        assert!(Arc::ptr_eq(&a, &b) && Arc::ptr_eq(&a, &c));
    }

    #[test]
    fn a_semaphore_number_past_the_end_is_none() {
        let _guard = test_lock();
        // This is the one that panicked the kernel: `semctl` fed a userspace
        // number straight into the old `Index` impl.
        let a = get(0, 2, CREAT | 0o666).unwrap();
        assert!(a.get_sem(0).is_some());
        assert!(a.get_sem(1).is_some());
        assert!(a.get_sem(2).is_none());
        assert!(a.get_sem(9999).is_none());
        assert!(a.get_sem(usize::MAX).is_none());
    }

    #[test]
    fn an_empty_set_has_no_semaphore_zero() {
        let _guard = test_lock();
        // semget with nsems == 0 on a *new* key really does make an empty set,
        // so even index 0 has to answer None rather than panic.
        let a = get(0, 0, CREAT | 0o666).unwrap();
        assert!(a.is_empty());
        assert_eq!(a.len(), 0);
        assert!(a.get_sem(0).is_none());
    }

    #[test]
    fn dropping_the_last_reference_frees_the_key() {
        let _guard = test_lock();
        let a = get(0x5e_0007, 1, CREAT | 0o666).unwrap();
        drop(a);
        // The table holds a Weak, so the key is gone with the set and the
        // probe says so.
        assert_eq!(get(0x5e_0007, 1, 0o666).err(), Some(LxError::ENOENT));
    }

    #[test]
    fn removing_a_set_frees_its_key_even_while_it_is_still_referenced() {
        let _guard = test_lock();
        let a = get(0x5e_0008, 1, CREAT | 0o666).unwrap();
        a.remove();
        assert_eq!(get(0x5e_0008, 1, 0o666).err(), Some(LxError::ENOENT));
        // A fresh create under the same key is a different set.
        let b = get(0x5e_0008, 1, CREAT | 0o666).unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn removing_a_private_set_touches_nobody_elses_key() {
        let _guard = test_lock();
        // A private set carries key 0. `remove` used to take key 0 out of the
        // table, which after the fix is nobody's key — but a set filed under a
        // real key must survive its neighbour's removal either way.
        let keyed = get(0x5e_0009, 1, CREAT | 0o666).unwrap();
        let private = get(0, 1, CREAT | 0o666).unwrap();
        private.remove();
        let still_there = get(0x5e_0009, 1, 0o666).unwrap();
        assert!(Arc::ptr_eq(&keyed, &still_there));
    }

    #[test]
    fn the_semid_ds_records_the_key_the_mode_and_the_count() {
        let _guard = test_lock();
        let a = get(0x5e_000a, 3, CREAT | 0o1666).unwrap();
        let ds = *a.semid_ds.lock();
        assert_eq!(ds.perm.key, 0x5e_000a);
        assert_eq!(ds.nsems, 3);
        // Only the low nine bits are permissions; IPC_CREAT is not one of them.
        assert_eq!(ds.perm.mode, 0o666);
        assert_eq!(ds.otime, 0, "no semop has run yet");
    }

    #[test]
    fn a_private_set_records_key_zero() {
        let _guard = test_lock();
        // What `ipcs -s` shows for a private set, and what it used to show
        // instead was the synthesised key it had been filed under.
        let a = get(0, 1, CREAT | 0o666).unwrap();
        assert_eq!(a.semid_ds.lock().perm.key, 0);
    }

    #[test]
    fn otime_and_ctime_are_stamped_where_semop_and_semctl_say() {
        let _guard = test_lock();
        let a = get(0, 1, CREAT | 0o666).unwrap();
        assert_eq!(a.semid_ds.lock().otime, 0);
        a.otime();
        assert_ne!(a.semid_ds.lock().otime, 0);
        let before = a.semid_ds.lock().ctime;
        a.ctime();
        assert!(a.semid_ds.lock().ctime >= before);
    }

    #[test]
    fn ipc_set_takes_the_owner_and_the_low_nine_bits_of_the_mode() {
        let _guard = test_lock();
        let a = get(0x5e_000b, 1, CREAT | 0o666).unwrap();
        let mut ds = *a.semid_ds.lock();
        ds.perm.uid = 1000;
        ds.perm.gid = 1001;
        ds.perm.mode = 0o7654;
        assert_eq!(a.set(&ds, ROOT), Ok(()));
        let now = *a.semid_ds.lock();
        assert_eq!(now.perm.uid, 1000);
        assert_eq!(now.perm.gid, 1001);
        assert_eq!(now.perm.mode, 0o654);
        assert_eq!(now.perm.key, 0x5e_000b, "IPC_SET never moves the key");
    }

    // ---- who may change the set ------------------------------------------

    /// `semget` filled `uid`, `gid`, `cuid` and `cgid` with zeros, whoever
    /// called it, so `semctl(IPC_STAT)` reported every set in the system as
    /// root's and there was nothing for a permission check to read. `msgget`,
    /// three files away, had always recorded its caller.
    #[test]
    fn semget_records_the_caller_as_owner_and_creator() {
        let _guard = test_lock();
        let a = SemArray::get_or_create(0, 1, CREAT | 0o666, OWNER, OWNER + 5).unwrap();
        let perm = a.semid_ds.lock().perm;
        assert_eq!(perm.uid, OWNER);
        assert_eq!(perm.gid, OWNER + 5);
        assert_eq!(perm.cuid, OWNER);
        assert_eq!(perm.cgid, OWNER + 5);
    }

    /// The whole of the old `set`: uid, gid and mode straight out of the
    /// buffer userspace handed in, with nobody asked, so naming the id was
    /// enough to take the set over (semctl(2): `EPERM`).
    #[test]
    fn ipc_set_from_a_stranger_is_eperm_and_changes_nothing() {
        let _guard = test_lock();
        let a = SemArray::get_or_create(0, 1, CREAT | 0o600, OWNER, OWNER).unwrap();
        let mut ds = *a.semid_ds.lock();
        ds.perm.uid = STRANGER;
        ds.perm.mode = 0o666;
        assert_eq!(a.set(&ds, STRANGER), Err(LxError::EPERM));
        let now = a.semid_ds.lock().perm;
        assert_eq!(now.uid, OWNER);
        assert_eq!(now.mode, 0o600);
        assert_eq!(a.set(&ds, OWNER), Ok(()));
        assert_eq!(a.semid_ds.lock().perm.uid, STRANGER);
    }

    /// The same predicate gates `IPC_RMID`, which is how `semctl` decides
    /// whether a caller may destroy somebody else's set.
    #[test]
    fn only_the_owner_the_creator_and_root_may_control_a_set() {
        let _guard = test_lock();
        let a = SemArray::get_or_create(0, 1, CREAT | 0o666, OWNER, OWNER).unwrap();
        assert!(a.may_control(OWNER));
        assert!(a.may_control(ROOT));
        assert!(!a.may_control(STRANGER));
        // Handed over, the creator keeps its rights (ipcctl_obtain_check).
        let mut ds = *a.semid_ds.lock();
        ds.perm.uid = STRANGER;
        assert_eq!(a.set(&ds, OWNER), Ok(()));
        assert!(a.may_control(OWNER));
        assert!(a.may_control(STRANGER));
    }

    // ---------------------------------------------- who may USE the set

    /// `semget` resolving an existing key is `ipc_check_perms`: the set may
    /// be somebody else's, and asking it for a mode it will not grant is
    /// EACCES.
    #[test]
    fn semget_refuses_a_key_that_belongs_to_somebody_else() {
        let _guard = test_lock();
        let owned = SemArray::get_or_create(7701, 2, CREAT | 0o600, OWNER, OWNER).unwrap();
        assert!(Arc::ptr_eq(
            &SemArray::get_or_create(7701, 2, 0o600, OWNER, OWNER).unwrap(),
            &owned
        ));
        assert_eq!(
            SemArray::get_or_create(7701, 2, 0o600, STRANGER, STRANGER).err(),
            Some(LxError::EACCES)
        );
        assert!(
            SemArray::get_or_create(7701, 2, 0, STRANGER, STRANGER).is_ok(),
            "a bare existence probe asks for nothing"
        );
        assert!(SemArray::get_or_create(7701, 2, 0o600, ROOT, ROOT).is_ok());
        drop(owned);
    }

    /// The size check comes first, as it does in `ipcget_public`
    /// (`more_checks` runs before `ipc_check_perms`): a set too small is
    /// EINVAL even for a caller the mode would have refused anyway.
    #[test]
    fn a_set_too_small_is_einval_before_it_is_eacces() {
        let _guard = test_lock();
        let owned = SemArray::get_or_create(7702, 2, CREAT | 0o600, OWNER, OWNER).unwrap();
        assert_eq!(
            SemArray::get_or_create(7702, 9, 0o600, STRANGER, STRANGER).err(),
            Some(LxError::EINVAL)
        );
        drop(owned);
    }

    /// `semop` needs write for anything that alters a semaphore and read for
    /// a wait-for-zero; `SETVAL` write, `GETVAL` read.
    #[test]
    fn a_set_answers_for_itself_who_may_alter_it() {
        let _guard = test_lock();
        let array = SemArray::get_or_create(7703, 1, CREAT | 0o640, OWNER, OWNER).unwrap();
        assert!(array.may_access(OWNER, OWNER, IPC_R | IPC_W));
        assert!(array.may_access(STRANGER, OWNER, IPC_R));
        assert!(!array.may_access(STRANGER, OWNER, IPC_W));
        assert!(!array.may_access(STRANGER, STRANGER, IPC_R));
    }
}

/// The id `semget` hands out was an index into the calling process's own
/// table, and a set lived only as long as some process's table held it. Both
/// are wrong by sysvipc(7): the id is system-wide, and the set stays until
/// `IPC_RMID`.
#[cfg(test)]
mod sem_registry_tests {
    use super::*;
    use crate::ipc::semary::test_globals::lock as test_lock;

    extern crate std;

    /// `IPC_CREAT`, as userspace spells it.
    const CREAT: usize = 0o1000;

    fn clear_ids() {
        SEMID2SEM.write().clear();
    }

    fn private_set() -> Arc<SemArray> {
        SemArray::get_or_create(0, 1, CREAT | 0o666, 0, 0).unwrap()
    }

    /// busybox `ipcs -s`: `maxid = semctl(0, 0, SEM_INFO, &info)`, then
    /// `SEM_STAT` over `0..=maxid`. `SEM_INFO` did not exist, and the id was
    /// looked up before the command was read.
    #[test]
    fn the_index_walk_of_ipcs_finds_every_set() {
        let _guard = test_lock();
        clear_ids();
        assert_eq!(sem_max_index(), None);
        assert_eq!(sem_totals(), (0, 0));
        let a = sem_register(&SemArray::get_or_create(0, 2, CREAT | 0o600, 0, 0).unwrap()).unwrap();
        let b = sem_register(&SemArray::get_or_create(0, 3, CREAT | 0o600, 0, 0).unwrap()).unwrap();
        assert_eq!(sem_max_index(), Some(b - 1));
        assert_eq!(sem_at_index(a - 1).unwrap().0, a);
        assert_eq!(sem_at_index(b - 1).unwrap().0, b);
        assert!(sem_at_index(b).is_none(), "one past the last slot");
        assert!(sem_at_index(usize::MAX).is_none());
        assert_eq!(sem_totals(), (2, 5), "sets, and semaphores in all");
        sem_unregister(a);
        assert!(sem_at_index(a - 1).is_none());
        assert_eq!(sem_max_index(), Some(b - 1));
        assert_eq!(sem_totals(), (1, 3));
    }

    #[test]
    fn an_id_names_the_same_set_from_anywhere() {
        let _guard = test_lock();
        clear_ids();
        let array = private_set();
        let id = sem_register(&array).unwrap();
        // Another process, knowing only the number.
        let found = sem_lookup(id).expect("the id must name the set anywhere");
        assert!(Arc::ptr_eq(&found, &array));
        clear_ids();
    }

    /// A setup program creates the set, sets its values and exits. The daemon
    /// that comes next must find it by key, values and all -- the set used to
    /// die with the last table that held it, and the daemon got `ENOENT`.
    #[test]
    fn a_set_outlives_the_process_that_made_it() {
        let _guard = test_lock();
        clear_ids();
        const KEY: u32 = 0x5e5e_0001;
        let id = {
            let array = SemArray::get_or_create(KEY, 1, CREAT | 0o666, 0, 0).unwrap();
            array.get_sem(0).unwrap().set(7);
            sem_register(&array).unwrap()
            // ...and the creator's reference is gone.
        };
        let again = SemArray::get_or_create(KEY, 0, 0o666, 0, 0)
            .expect("the key must still name the set after its creator exits");
        assert_eq!(again.get_sem(0).unwrap().get(), 7, "with its value");
        assert_eq!(sem_register(&again), Ok(id), "under the same id");
        assert!(sem_lookup(id).is_some());
        // Until IPC_RMID.
        again.remove();
        assert!(sem_unregister(id));
        drop(again);
        assert_eq!(
            SemArray::get_or_create(KEY, 0, 0o666, 0, 0).err(),
            Some(LxError::ENOENT)
        );
        clear_ids();
    }

    #[test]
    fn the_same_set_keeps_the_same_id_and_two_sets_never_share_one() {
        let _guard = test_lock();
        clear_ids();
        let a = private_set();
        let b = private_set();
        let id_a = sem_register(&a).unwrap();
        let id_b = sem_register(&b).unwrap();
        assert_ne!(id_a, id_b);
        assert_eq!(sem_register(&a), Ok(id_a), "asking again is the same id");
        clear_ids();
    }

    /// A stale id kept past `IPC_RMID` must name nothing, never a newer set.
    #[test]
    fn an_id_is_never_reused() {
        let _guard = test_lock();
        clear_ids();
        let old = private_set();
        let old_id = sem_register(&old).unwrap();
        assert!(sem_unregister(old_id));
        assert!(sem_lookup(old_id).is_none());
        let new_id = sem_register(&private_set()).unwrap();
        assert_ne!(new_id, old_id);
        assert!(sem_lookup(old_id).is_none(), "the retired id stays retired");
        clear_ids();
    }

    #[test]
    fn removing_the_id_does_not_take_the_set_from_whoever_holds_it() {
        let _guard = test_lock();
        clear_ids();
        let array = private_set();
        let id = sem_register(&array).unwrap();
        assert!(sem_unregister(id));
        assert!(!sem_unregister(id), "only once");
        // The holder's copy is intact: a process that had it in its table
        // still replays its SEM_UNDO records against it at exit.
        array.get_sem(0).unwrap().set(3);
        assert_eq!(array.get_sem(0).unwrap().get(), 3);
        clear_ids();
    }

    /// `ipcs -s` reads `/proc/sysvipc/sem`, which did not exist: every set
    /// in the system was invisible to it, and to `ipcrm`'s `-a`.
    #[test]
    fn the_proc_table_lists_every_registered_set_in_the_kernels_layout() {
        let _guard = test_lock();
        clear_ids();
        assert_eq!(
            sem_proc_table(),
            "       key      semid perms      nsems   uid   gid  cuid  cgid      otime      ctime\n",
            "an empty system is the header alone"
        );
        // A key past i32::MAX: `struct ipc_perm.key` is an `int`, so Linux
        // prints it negative and `ipcs` reads it back with `%d`.
        let a = SemArray::get_or_create(0xffff_fff0, 3, CREAT | 0o640, 1000, 100).unwrap();
        let b = private_set();
        let id_a = sem_register(&a).unwrap();
        let id_b = sem_register(&b).unwrap();
        let table = sem_proc_table();
        let lines: std::vec::Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3, "a header and one line per set:\n{table}");
        use alloc::string::ToString as _;
        let cols = |line: &str| -> std::vec::Vec<std::string::String> {
            line.split_whitespace().map(|c| c.to_string()).collect()
        };
        let row_a = cols(lines[1]);
        assert_eq!(row_a[0], "-16", "the key, as a signed int");
        assert_eq!(row_a[1], id_a.to_string());
        assert_eq!(row_a[2], "640", "the mode, in octal");
        assert_eq!(row_a[3], "3", "nsems");
        assert_eq!(&row_a[4..8], ["1000", "100", "1000", "100"]);
        assert_eq!(row_a[8], "0", "never operated on");
        assert_ne!(row_a[9], "0", "created now");
        assert_eq!(row_a.len(), 10);
        let row_b = cols(lines[2]);
        assert_eq!(row_b[0], "0", "a private set has key 0");
        assert_eq!(row_b[1], id_b.to_string());
        // A retired id is gone from the table.
        assert!(sem_unregister(id_a));
        assert_eq!(sem_proc_table().lines().count(), 2);
        clear_ids();
    }

    /// The registry is what pins a set past its creator, so it has to be
    /// bounded or `semget(IPC_PRIVATE, ...)` in a loop is unbounded pinned
    /// kernel memory from an ordinary process.
    #[test]
    fn the_number_of_sets_is_bounded() {
        let _guard = test_lock();
        clear_ids();
        {
            let mut table = SEMID2SEM.write();
            for i in 0..SEMMNI {
                table.insert(i + 1_000_000, private_set());
            }
        }
        assert_eq!(sem_register(&private_set()), Err(LxError::ENOSPC));
        clear_ids();
    }
}

/// The one lock every test that reaches `KEY2SEM` takes first.
#[cfg(test)]
pub(crate) mod test_globals {
    extern crate std;

    /// `KEY2SEM` is process-wide and cargo runs a crate's tests in threads, so
    /// two tests picking the same key would see each other's sets — and
    /// `SemProc`'s tests reach the same table through `get_or_create`.
    static LOCK: self::std::sync::Mutex<()> = self::std::sync::Mutex::new(());

    /// Take it. A test that panics while holding it poisons the lock; that is
    /// not a failure for the tests that follow, so step over the poison.
    pub(crate) fn lock() -> self::std::sync::MutexGuard<'static, ()> {
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }
}
