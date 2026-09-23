//! Linux Inter-Process Communication
#![deny(missing_docs)]
mod msgqueue;
mod semary;
mod shared_mem;

pub use self::msgqueue::*;
pub use self::semary::*;
pub use self::shared_mem::*;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use bitflags::*;
use kernel_hal::sync::Mutex;

/// Semaphore table in a process
#[derive(Default)]
pub struct SemProc {
    /// Semaphore arrays
    arrays: BTreeMap<SemId, Arc<SemArray>>,
    /// Undo operations when process terminates
    undos: BTreeMap<(SemId, SemNum), SemOp>,
}

/// Shared_memory table in a process
#[derive(Default, Clone)]
pub struct ShmProc {
    /// Shared_memory identifier sets
    shm_identifiers: BTreeMap<ShmId, ShmIdentifier>,
}

bitflags! {
    /// ipc get bit flags
    struct IpcGetFlag: usize {
        const CREAT = 1 << 9;
        const EXCLUSIVE = 1 << 10;
        const NO_WAIT = 1 << 11;
    }
}

/// structure specifies the access permissions on the semaphore set
///
/// struct ipc_perm
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IpcPerm {
    /// Key supplied to semget(2)
    pub key: u32,
    /// Effective UID of owner
    pub uid: u32,
    /// Effective GID of owner
    pub gid: u32,
    /// Effective UID of creator
    pub cuid: u32,
    /// Effective GID of creator
    pub cgid: u32,
    /// Permissions
    pub mode: u32,
    /// Sequence number
    pub __seq: u32,
    /// pad1
    pub __pad1: usize,
    /// pad2
    pub __pad2: usize,
}

impl IpcPerm {
    /// Whether `euid` may change this object: `IPC_SET` and `IPC_RMID`, which
    /// Linux allows only to the owner, the creator or a privileged caller and
    /// answers with `EPERM` otherwise (`ipcctl_obtain_check`, `ipc/util.c`).
    ///
    /// Nothing here asked. `msgctl`, `semctl` and `shmctl` each read a
    /// `*_ds` out of userspace and copied its `uid`, `gid` and `mode` straight
    /// into the object, so any process that could name the id took the queue,
    /// the segment or the semaphore set over. These three fields were written
    /// and then only ever read back out by `IPC_STAT` and `/proc/sysvipc` --
    /// never to decide anything -- and there is no `EACCES` anywhere in this
    /// module.
    pub fn may_control(&self, euid: u32) -> bool {
        euid == 0 || euid == self.uid || euid == self.cuid
    }
}

/// Semaphore set identifier (in a process)
type SemId = usize;
/// Shared memory identifier. System-wide: `shmget(2)`'s id means the same
/// segment in every process, which is how two programs with no common
/// ancestor share memory. See [`shared_mem::shm_register`].
pub type ShmId = usize;

/// Semaphore number (in an array)
type SemNum = u16;

/// Semaphore operation value
type SemOp = i16;

impl SemProc {
    /// Insert the `array` and return its ID
    pub fn add(&mut self, array: Arc<SemArray>) -> SemId {
        let id = self.get_free_id();
        self.arrays.insert(id, array);
        id
    }

    /// Remove an `array` by ID
    ///
    /// The undo records for that set go with it. They used to stay behind, and
    /// then the exit-time replay in `Drop` looked up an id that was no longer
    /// in `arrays` — an index into a `BTreeMap` that is not there, i.e. a
    /// kernel panic that any process could arrange with `semop(.., SEM_UNDO)`
    /// followed by `semctl(.., IPC_RMID)` and `exit`.
    pub fn remove(&mut self, id: SemId) {
        self.arrays.remove(&id);
        self.undos.retain(|&(undo_id, _), _| undo_id != id);
    }

    /// Get a free ID
    fn get_free_id(&self) -> SemId {
        (0..).find(|i| !self.arrays.contains_key(i)).unwrap()
    }

    /// Get an semaphore set by `id`
    pub fn get(&self, id: SemId) -> Option<Arc<SemArray>> {
        self.arrays.get(&id).cloned()
    }

    /// Does this process owe any SEM_UNDO adjustment?
    ///
    /// A `fork(2)` child must owe none: a plain fork does not share SEM_UNDO
    /// state, only `CLONE_SYSVSEM` does. The sets themselves do come along.
    pub fn owes_no_undo(&self) -> bool {
        self.undos.is_empty()
    }

    /// Add an undo operation
    ///
    /// The record is the adjustment that will put the semaphore back where it
    /// was, so it is the *negation* of the operation and it accumulates across
    /// calls. Saturating, because `SemOp` is an `i16` and a loop of
    /// `semop(+1, SEM_UNDO)` reaches its end in 32768 syscalls — cheap for any
    /// process, and an arithmetic overflow panic in a debug kernel.
    pub fn add_undo(&mut self, id: SemId, num: SemNum, op: SemOp) {
        let old_val = *self.undos.get(&(id, num)).unwrap_or(&0);
        let new_val = old_val.saturating_sub(op);
        self.undos.insert((id, num), new_val);
    }
}

/// Fork the semaphore table. Clear undo info.
impl Clone for SemProc {
    fn clone(&self) -> Self {
        SemProc {
            arrays: self.arrays.clone(),
            undos: BTreeMap::default(),
        }
    }
}

/// Auto perform semaphores undo on drop
///
/// This runs while a process is being torn down, which is the worst possible
/// place to panic, and it used to have three ways to do it: indexing `arrays`
/// with an id whose set had been removed, indexing the set with a semaphore
/// number it no longer has, and an `unimplemented!()` for every undo value but
/// `1` and `0` — so a single `semop(+1, SEM_UNDO)` (value `-1`) or two waits in
/// a row (value `2`) took the kernel down on `exit`. `SEM_UNDO` is the normal,
/// careful way to use a System V semaphore, so the programs that hit this were
/// the well-behaved ones.
///
/// What semop(2) actually asks for is that the accumulated adjustment be added
/// to `semval`, whatever its sign, which is what happens now.
impl Drop for SemProc {
    fn drop(&mut self) {
        for (&(id, num), &adj) in self.undos.iter() {
            debug!("semundo: id: {}, num: {}, adj: {}", id, num, adj);
            let Some(sem_array) = self.arrays.get(&id) else {
                continue;
            };
            let Some(sem) = sem_array.get_sem(num as usize) else {
                continue;
            };
            sem.adjust(adj as isize);
        }
    }
}

impl ShmProc {
    /// Record that this process is using the segment `id` names.
    ///
    /// The id comes from [`shared_mem::shm_register`] and is the same number
    /// in every process; this map is only what THIS process has attached, so
    /// `shmdt(addr)` can find its way back to the segment.
    pub fn add(&mut self, id: ShmId, shared_guard: Arc<Mutex<ShmGuard>>) {
        let shm_identifier = ShmIdentifier {
            addr: 0,
            guard: shared_guard,
        };
        self.shm_identifiers.entry(id).or_insert(shm_identifier);
    }

    /// Get an semaphore set by `id`
    pub fn get(&self, id: ShmId) -> Option<ShmIdentifier> {
        self.shm_identifiers.get(&id).cloned()
    }

    /// Used to set Virtual Addr
    pub fn set(&mut self, id: ShmId, shm_id: ShmIdentifier) {
        self.shm_identifiers.insert(id, shm_id);
    }

    /// get id from virtaddr
    pub fn get_id(&self, addr: usize) -> Option<ShmId> {
        for (key, value) in &self.shm_identifiers {
            if value.addr == addr {
                return Some(*key);
            }
        }
        None
    }

    /// Pop Shared Area
    pub fn pop(&mut self, id: ShmId) {
        self.shm_identifiers.remove(&id);
    }
}

/// `SemProc` is a process's own view of the System V semaphores it is using,
/// and its `Drop` is the `SEM_UNDO` replay: the adjustments a dying process
/// promised to put back so a semaphore it was holding does not stay held for
/// ever.
///
/// It ran during process teardown and had three ways to panic there, all of
/// them reachable by an ordinary process:
///
/// - indexing `arrays` with an id whose set had been removed by
///   `semctl(.., IPC_RMID)` — the undo record stayed behind;
/// - `unimplemented!()` for every accumulated value but `1` and `0`, so a
///   single `semop(+1, SEM_UNDO)` (value `-1`), or two waits in a row (value
///   `2`), took the machine down on `exit`;
/// - an `i16` overflow in the accumulator, 32768 syscalls away.
///
/// `SEM_UNDO` is the careful way to use a System V semaphore — it is what
/// keeps a crash from wedging the set — so the programs that tripped this were
/// the well-written ones.
#[cfg(test)]
mod sem_proc_tests {
    use super::*;
    use crate::ipc::semary::test_globals::lock as test_lock;

    /// `IPC_CREAT`, as userspace spells it.
    const CREAT: usize = 0o1000;

    /// A private set of `n` semaphores, all starting at zero.
    fn set(n: usize) -> Arc<SemArray> {
        SemArray::get_or_create(0, n, CREAT | 0o666, 0, 0).unwrap()
    }

    #[test]
    fn exiting_after_ipc_rmid_does_not_take_the_kernel_with_it() {
        let _guard = test_lock();
        // semop(.., SEM_UNDO); semctl(.., IPC_RMID); exit. Three ordinary
        // syscalls, and the third one used to panic inside `Drop`.
        let mut proc = SemProc::default();
        let id = proc.add(set(1));
        proc.add_undo(id, 0, -1);
        proc.remove(id);
        drop(proc);
    }

    #[test]
    fn removing_a_set_drops_its_undo_records_and_leaves_the_others() {
        let _guard = test_lock();
        let mut proc = SemProc::default();
        let a = proc.add(set(1));
        let b = proc.add(set(1));
        proc.add_undo(a, 0, -1);
        proc.add_undo(b, 0, -1);
        proc.remove(a);
        assert_eq!(proc.undos.len(), 1);
        assert!(proc.undos.contains_key(&(b, 0)));
    }

    #[test]
    fn an_undo_of_a_wait_gives_the_resource_back() {
        let _guard = test_lock();
        // semop(-1) takes one; the undo puts it back.
        let array = set(1);
        let mut proc = SemProc::default();
        let id = proc.add(array.clone());
        array.get_sem(0).unwrap().set(3);
        proc.add_undo(id, 0, -1);
        drop(proc);
        assert_eq!(array.get_sem(0).unwrap().get(), 4);
    }

    #[test]
    fn an_undo_of_a_post_takes_the_resource_away_instead_of_panicking() {
        let _guard = test_lock();
        // This is the one `unimplemented!()` caught: op = +1 records an undo
        // of -1, which is not 1 and not 0.
        let array = set(1);
        let mut proc = SemProc::default();
        let id = proc.add(array.clone());
        array.get_sem(0).unwrap().set(3);
        proc.add_undo(id, 0, 1);
        drop(proc);
        assert_eq!(array.get_sem(0).unwrap().get(), 2);
    }

    #[test]
    fn undo_records_accumulate_across_operations() {
        let _guard = test_lock();
        // Two waits in a row record 2, which was the other `unimplemented!()`.
        let array = set(1);
        let mut proc = SemProc::default();
        let id = proc.add(array.clone());
        proc.add_undo(id, 0, -1);
        proc.add_undo(id, 0, -1);
        proc.add_undo(id, 0, -1);
        assert_eq!(proc.undos[&(id, 0)], 3);
        drop(proc);
        assert_eq!(array.get_sem(0).unwrap().get(), 3);
    }

    #[test]
    fn a_wait_and_a_post_cancel_out_and_nothing_is_undone() {
        let _guard = test_lock();
        let array = set(1);
        let mut proc = SemProc::default();
        let id = proc.add(array.clone());
        array.get_sem(0).unwrap().set(5);
        proc.add_undo(id, 0, -1);
        proc.add_undo(id, 0, 1);
        assert_eq!(proc.undos[&(id, 0)], 0);
        drop(proc);
        assert_eq!(array.get_sem(0).unwrap().get(), 5);
    }

    #[test]
    fn each_semaphore_of_a_set_keeps_its_own_undo() {
        let _guard = test_lock();
        let array = set(3);
        let mut proc = SemProc::default();
        let id = proc.add(array.clone());
        proc.add_undo(id, 0, -1);
        proc.add_undo(id, 2, -2);
        drop(proc);
        assert_eq!(array.get_sem(0).unwrap().get(), 1);
        assert_eq!(array.get_sem(1).unwrap().get(), 0);
        assert_eq!(array.get_sem(2).unwrap().get(), 2);
    }

    #[test]
    fn undo_accumulation_saturates_instead_of_overflowing() {
        let _guard = test_lock();
        // The accumulator is an i16 and each `semop(+1, SEM_UNDO)` walks it
        // one step down: 32768 syscalls, which any process can afford, and a
        // subtraction overflow panic in a debug kernel.
        let mut proc = SemProc::default();
        let id = proc.add(set(1));
        for _ in 0..40_000 {
            proc.add_undo(id, 0, 1);
        }
        assert_eq!(proc.undos[&(id, 0)], i16::MIN);
        // And the same from the other end.
        proc.add_undo(id, 0, i16::MIN);
        proc.add_undo(id, 0, i16::MIN);
        assert_eq!(proc.undos[&(id, 0)], i16::MAX);
    }

    #[test]
    fn an_undo_naming_a_semaphore_the_set_does_not_have_is_skipped() {
        let _guard = test_lock();
        // Nothing reaches `add_undo` today without `semop` having bounds-
        // checked the number first, but `Drop` must not be the place that
        // finds out otherwise.
        let mut proc = SemProc::default();
        let id = proc.add(set(1));
        proc.add_undo(id, 9, -1);
        drop(proc);
    }

    #[test]
    fn an_undo_naming_a_set_this_process_never_had_is_skipped() {
        let _guard = test_lock();
        let mut proc = SemProc::default();
        proc.add_undo(77, 0, -1);
        drop(proc);
    }

    #[test]
    fn fork_shares_the_sets_but_inherits_no_undo_records() {
        let _guard = test_lock();
        // semop(2): "the child does not inherit its parent's semadj values".
        // Otherwise both halves of a fork undo the same operation.
        let array = set(1);
        let mut parent = SemProc::default();
        let id = parent.add(array.clone());
        parent.add_undo(id, 0, -1);
        let child = parent.clone();
        assert!(child.undos.is_empty());
        assert!(Arc::ptr_eq(&child.get(id).unwrap(), &array));
        drop(child);
        assert_eq!(
            array.get_sem(0).unwrap().get(),
            0,
            "the child undid nothing"
        );
        drop(parent);
        assert_eq!(array.get_sem(0).unwrap().get(), 1, "the parent undid once");
    }

    #[test]
    fn ids_are_handed_out_from_the_lowest_free_one() {
        let _guard = test_lock();
        let mut proc = SemProc::default();
        assert_eq!(proc.add(set(1)), 0);
        assert_eq!(proc.add(set(1)), 1);
        assert_eq!(proc.add(set(1)), 2);
        proc.remove(1);
        assert!(proc.get(1).is_none());
        assert_eq!(proc.add(set(1)), 1, "the hole is filled before the end");
    }
}
