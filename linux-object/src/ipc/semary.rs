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
}

impl SemArray {
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
}

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

    /// for IPC_SET
    /// see man semctl(2)
    pub fn set(&self, new: &SemidDs) {
        let mut lock = self.semid_ds.lock();
        lock.perm.uid = new.perm.uid;
        lock.perm.gid = new.perm.gid;
        lock.perm.mode = new.perm.mode & 0x1ff;
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
    pub fn get_or_create(key: u32, nsems: usize, flags: usize) -> Result<Arc<Self>, LxError> {
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
                    uid: 0,
                    gid: 0,
                    cuid: 0,
                    cgid: 0,
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

    use super::test_globals::lock as test_lock;

    /// `IPC_CREAT`, as userspace spells it.
    const CREAT: usize = 0o1000;
    /// `IPC_EXCL`.
    const EXCL: usize = 0o2000;

    #[test]
    fn a_private_set_is_not_filed_under_any_key() {
        let _guard = test_lock();
        // Hold it alive: the bug needed a live set to alias onto.
        let _private = SemArray::get_or_create(0, 4, CREAT | 0o666).unwrap();
        // This used to hand back `_private`, because that is where the old
        // code had filed it.
        for key in 1u32..=4 {
            assert_eq!(
                SemArray::get_or_create(key, 1, 0).err(),
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
        let _private = SemArray::get_or_create(0, 1, CREAT | 0o666).unwrap();
        assert!(!KEY2SEM.read().contains_key(&0));
    }

    #[test]
    fn a_private_set_never_needs_ipc_creat() {
        let _guard = test_lock();
        // semget(2): with IPC_PRIVATE a new set is created whatever the flags
        // say, so the ENOENT rule must not reach this path.
        let a = SemArray::get_or_create(0, 2, 0o666).unwrap();
        assert_eq!(a.len(), 2);
    }

    #[test]
    fn two_private_sets_are_different_sets() {
        let _guard = test_lock();
        let a = SemArray::get_or_create(0, 1, CREAT | 0o666).unwrap();
        let b = SemArray::get_or_create(0, 1, CREAT | 0o666).unwrap();
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
        assert_eq!(
            SemArray::get_or_create(0x5e_0001, 1, 0o666).err(),
            Some(LxError::ENOENT)
        );
        // And with IPC_CREAT it does create it.
        let made = SemArray::get_or_create(0x5e_0001, 1, CREAT | 0o666).unwrap();
        assert_eq!(made.len(), 1);
        // Now the probe finds it.
        let found = SemArray::get_or_create(0x5e_0001, 1, 0o666).unwrap();
        assert!(Arc::ptr_eq(&made, &found));
    }

    #[test]
    fn the_same_key_comes_back_as_the_same_set() {
        let _guard = test_lock();
        let a = SemArray::get_or_create(0x5e_0002, 3, CREAT | 0o666).unwrap();
        let b = SemArray::get_or_create(0x5e_0002, 3, CREAT | 0o666).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        a.get_sem(2).unwrap().set(5);
        assert_eq!(b.get_sem(2).unwrap().get(), 5);
    }

    #[test]
    fn creat_and_excl_together_on_an_existing_key_is_eexist() {
        let _guard = test_lock();
        let _a = SemArray::get_or_create(0x5e_0003, 1, CREAT | 0o666).unwrap();
        assert_eq!(
            SemArray::get_or_create(0x5e_0003, 1, CREAT | EXCL | 0o666).err(),
            Some(LxError::EEXIST)
        );
    }

    #[test]
    fn excl_without_creat_returns_the_set_that_is_there() {
        let _guard = test_lock();
        // semget(2): IPC_EXCL only means anything alongside IPC_CREAT.
        let a = SemArray::get_or_create(0x5e_0004, 1, CREAT | 0o666).unwrap();
        let b = SemArray::get_or_create(0x5e_0004, 1, EXCL | 0o666).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn asking_an_existing_set_for_more_semaphores_than_it_has_is_einval() {
        let _guard = test_lock();
        let _a = SemArray::get_or_create(0x5e_0005, 2, CREAT | 0o666).unwrap();
        // The caller walks away believing in four semaphores and gets EFBIG
        // from semop on the two that do not exist.
        assert_eq!(
            SemArray::get_or_create(0x5e_0005, 4, CREAT | 0o666).err(),
            Some(LxError::EINVAL)
        );
    }

    #[test]
    fn asking_for_fewer_or_none_attaches_to_the_set_that_is_there() {
        let _guard = test_lock();
        // nsems == 0 is "don't care", and it is what every attach-only caller
        // passes.
        let a = SemArray::get_or_create(0x5e_0006, 3, CREAT | 0o666).unwrap();
        let b = SemArray::get_or_create(0x5e_0006, 0, 0o666).unwrap();
        let c = SemArray::get_or_create(0x5e_0006, 3, 0o666).unwrap();
        assert!(Arc::ptr_eq(&a, &b) && Arc::ptr_eq(&a, &c));
    }

    #[test]
    fn a_semaphore_number_past_the_end_is_none() {
        let _guard = test_lock();
        // This is the one that panicked the kernel: `semctl` fed a userspace
        // number straight into the old `Index` impl.
        let a = SemArray::get_or_create(0, 2, CREAT | 0o666).unwrap();
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
        let a = SemArray::get_or_create(0, 0, CREAT | 0o666).unwrap();
        assert!(a.is_empty());
        assert_eq!(a.len(), 0);
        assert!(a.get_sem(0).is_none());
    }

    #[test]
    fn dropping_the_last_reference_frees_the_key() {
        let _guard = test_lock();
        let a = SemArray::get_or_create(0x5e_0007, 1, CREAT | 0o666).unwrap();
        drop(a);
        // The table holds a Weak, so the key is gone with the set and the
        // probe says so.
        assert_eq!(
            SemArray::get_or_create(0x5e_0007, 1, 0o666).err(),
            Some(LxError::ENOENT)
        );
    }

    #[test]
    fn removing_a_set_frees_its_key_even_while_it_is_still_referenced() {
        let _guard = test_lock();
        let a = SemArray::get_or_create(0x5e_0008, 1, CREAT | 0o666).unwrap();
        a.remove();
        assert_eq!(
            SemArray::get_or_create(0x5e_0008, 1, 0o666).err(),
            Some(LxError::ENOENT)
        );
        // A fresh create under the same key is a different set.
        let b = SemArray::get_or_create(0x5e_0008, 1, CREAT | 0o666).unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn removing_a_private_set_touches_nobody_elses_key() {
        let _guard = test_lock();
        // A private set carries key 0. `remove` used to take key 0 out of the
        // table, which after the fix is nobody's key — but a set filed under a
        // real key must survive its neighbour's removal either way.
        let keyed = SemArray::get_or_create(0x5e_0009, 1, CREAT | 0o666).unwrap();
        let private = SemArray::get_or_create(0, 1, CREAT | 0o666).unwrap();
        private.remove();
        let still_there = SemArray::get_or_create(0x5e_0009, 1, 0o666).unwrap();
        assert!(Arc::ptr_eq(&keyed, &still_there));
    }

    #[test]
    fn the_semid_ds_records_the_key_the_mode_and_the_count() {
        let _guard = test_lock();
        let a = SemArray::get_or_create(0x5e_000a, 3, CREAT | 0o1666).unwrap();
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
        let a = SemArray::get_or_create(0, 1, CREAT | 0o666).unwrap();
        assert_eq!(a.semid_ds.lock().perm.key, 0);
    }

    #[test]
    fn otime_and_ctime_are_stamped_where_semop_and_semctl_say() {
        let _guard = test_lock();
        let a = SemArray::get_or_create(0, 1, CREAT | 0o666).unwrap();
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
        let a = SemArray::get_or_create(0x5e_000b, 1, CREAT | 0o666).unwrap();
        let mut ds = *a.semid_ds.lock();
        ds.perm.uid = 1000;
        ds.perm.gid = 1001;
        ds.perm.mode = 0o7654;
        a.set(&ds);
        let now = *a.semid_ds.lock();
        assert_eq!(now.perm.uid, 1000);
        assert_eq!(now.perm.gid, 1001);
        assert_eq!(now.perm.mode, 0o654);
        assert_eq!(now.perm.key, 0x5e_000b, "IPC_SET never moves the key");
    }
}

/// The one lock every test that reaches `KEY2SEM` takes first.
#[cfg(test)]
pub(super) mod test_globals {
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
