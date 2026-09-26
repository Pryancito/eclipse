//! Linux Inter-Process Communication
#![deny(missing_docs)]
mod msgqueue;
mod semary;
#[cfg(test)]
pub(crate) use self::semary::test_globals as sem_test_globals;
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

/// A process's own view of the System V shared memory it is using: the
/// segments it knows by id, and where each `shmat` put one.
///
/// An attachment is a mapping in the address space, so it lives and dies
/// with the address space: `exit` and `execve` tear every one of them down,
/// and `fork` copies every one of them into the child. Linux accounts each
/// of those on the segment (`shm_close` and `shm_open` in ipc/shm.c move
/// `shm_nattch`), and this table is what does it here: dropping it is the
/// detach of everything attached, and [`inherited`](Self::inherited) is the
/// attach of the copy. The count used to move only on an explicit `shmat`
/// or `shmdt`, so a process that died or exec'd with a segment attached
/// left `shm_nattch` one too high for ever -- and `ipcs -m`, or a program
/// that reads `shm_nattch` to decide whether a segment is still in use
/// (PostgreSQL's postmaster does exactly that to tell a stale segment from
/// a live one), saw phantom users.
///
/// One entry per attachment, not per segment: a segment may be attached
/// more than once (shmat(2) allows it, and a program mapping the same
/// buffer at two addresses does it), and a table keyed by id kept only the
/// last address, so `shmdt` of the first found nothing, unmapped nothing
/// and answered 0.
#[derive(Default)]
pub struct ShmProc {
    /// The segments this process has named through `shmget` or `shmat`,
    /// attached or not.
    segments: BTreeMap<ShmId, Arc<Mutex<ShmGuard>>>,
    /// Where each attachment is mapped, by the address `shmat` returned.
    attached: BTreeMap<usize, ShmId>,
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

    /// The `r`/`w` bits a caller is asking for, out of the mode word it
    /// passed to `shmget`/`msgget`/`semget` or the access it needs.
    ///
    /// `ipcperms` folds owner, group and other down onto one another
    /// (`(flag >> 6) | (flag >> 3) | flag`) before comparing, because the
    /// caller does not know yet which of the three classes it will land in:
    /// asking for `0640` is asking to read and write, whoever you turn out to
    /// be.
    pub fn requested_mode(flags: usize) -> u32 {
        (((flags >> 6) | (flags >> 3) | flags) & 0o7) as u32
    }

    /// Whether `euid`/`egid` may use this object for `want` (`IPC_R`, `IPC_W`
    /// or both), the way `ipcperms` (`ipc/util.c`) decides it.
    ///
    /// This is the check that was missing everywhere. [`may_control`] answers
    /// who may *change* an object, and since the tanda that added it the
    /// owner and creator ids have been recorded truthfully -- but nothing
    /// asked them who may *use* one. A segment created `0600` could be
    /// attached, read and written by any process that could name its id,
    /// because knowing the id was the whole check: `shmat`, `msgsnd`,
    /// `msgrcv`, `semop` and every `IPC_STAT` went straight through, and so
    /// did a `shmget`/`msgget`/`semget` that resolved an existing key.
    ///
    /// The classes do not fall through: landing in "owner" and being refused
    /// there does **not** then try "group" or "other", which is why a mode
    /// like `0066` refuses its own owner. Linux is deliberate about that, and
    /// so is every other `mode`-based check in Unix.
    ///
    /// euid 0 passes, standing in for `CAP_IPC_OWNER`.
    pub fn may_access(&self, euid: u32, egid: u32, want: u32) -> bool {
        if euid == 0 {
            return true;
        }
        let granted = if euid == self.cuid || euid == self.uid {
            self.mode >> 6
        } else if egid == self.cgid || egid == self.gid {
            self.mode >> 3
        } else {
            self.mode
        };
        want & !granted & 0o7 == 0
    }
}

/// Read access, as `ipcperms` counts it (`S_IRUGO` folded onto one class).
pub const IPC_R: u32 = 0o4;
/// Write access. `semop` needs it for any operation that alters a semaphore,
/// `msgsnd` always, `shmat` unless the caller asked for `SHM_RDONLY`.
pub const IPC_W: u32 = 0o2;

/// Semaphore set identifier. System-wide, like [`ShmId`]: `semget(2)`'s id
/// means the same set in every process. See [`semary::sem_register`].
pub type SemId = usize;
/// Shared memory identifier. System-wide: `shmget(2)`'s id means the same
/// segment in every process, which is how two programs with no common
/// ancestor share memory. See [`shared_mem::shm_register`].
pub type ShmId = usize;

/// Semaphore number (in an array)
type SemNum = u16;

/// Semaphore operation value
type SemOp = i16;

impl SemProc {
    /// Record that this process is using the set `id` names.
    ///
    /// The id comes from [`semary::sem_register`] and is the same number in
    /// every process; this table is what THIS process has used, so its
    /// `SEM_UNDO` records have a set to replay against at exit. It used to
    /// hand out its own ids, indices from 0 into this very map, so the same
    /// number named a different set in every process.
    pub fn add(&mut self, id: SemId, array: Arc<SemArray>) {
        self.arrays.insert(id, array);
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
            // Under the set-wide lock, as `exit_sem` replays the whole undo
            // list under `sem_lock_and_putref(sma)`. Without it the adjustment
            // was lost the same way a `SETVAL` was: `sys_semop` plans against
            // a snapshot and writes absolute values, so an undo applied
            // between the two vanishes -- and a vanished undo is a semaphore
            // held for ever by a process that no longer exists, which is the
            // exact failure `SEM_UNDO` is for.
            let _atomic = sem_array.semop_guard();
            sem.adjust(adj as isize);
        }
    }
}

impl ShmProc {
    /// Record that this process is using the segment `id` names.
    ///
    /// The id comes from [`shared_mem::shm_register`] and is the same number
    /// in every process; this map is only what THIS process knows, so
    /// `shmctl` can still find a segment it created after the id was retired.
    pub fn add(&mut self, id: ShmId, shared_guard: Arc<Mutex<ShmGuard>>) {
        self.segments.entry(id).or_insert(shared_guard);
    }

    /// The segment `id` names for this process, with the address of one of
    /// its attachments (0 when it is not attached).
    pub fn get(&self, id: ShmId) -> Option<ShmIdentifier> {
        let guard = self.segments.get(&id)?.clone();
        let addr = self
            .attached
            .iter()
            .find(|(_, &i)| i == id)
            .map(|(&addr, _)| addr)
            .unwrap_or(0);
        Some(ShmIdentifier { addr, guard })
    }

    /// Record an attachment of `id` at `addr`, what `shmat` just mapped.
    ///
    /// The accounting on the segment (`nattch`, `atime`, `lpid`) is the
    /// caller's, through [`ShmGuard::attach`]; this only remembers where.
    pub fn attach(&mut self, id: ShmId, shared_guard: Arc<Mutex<ShmGuard>>, addr: usize) {
        self.segments.insert(id, shared_guard);
        self.attached.insert(addr, id);
    }

    /// Record a segment and, when `shm_id.addr` is not 0, an attachment of it
    /// there. The older spelling of [`attach`](Self::attach).
    pub fn set(&mut self, id: ShmId, shm_id: ShmIdentifier) {
        if shm_id.addr != 0 {
            self.attach(id, shm_id.guard, shm_id.addr);
        } else {
            self.segments.insert(id, shm_id.guard);
        }
    }

    /// The segment attached at `addr`, if any: what `shmdt(addr)` asks.
    pub fn get_id(&self, addr: usize) -> Option<ShmId> {
        self.attached.get(&addr).copied()
    }

    /// Forget the attachment at `addr` -- that one only: the same segment
    /// attached elsewhere stays attached there, and the segment itself stays
    /// known. Returns what was attached, or `None` when nothing was, which
    /// shmdt(2) answers with `EINVAL`.
    ///
    /// The accounting on the segment is the caller's, through
    /// [`ShmGuard::detach`], because it carries the pid.
    pub fn detach(&mut self, addr: usize) -> Option<ShmIdentifier> {
        let id = self.attached.remove(&addr)?;
        let guard = self.segments.get(&id)?.clone();
        Some(ShmIdentifier { addr, guard })
    }

    /// Forget the segment `id` and every attachment of it, without
    /// accounting anything on the segment.
    pub fn pop(&mut self, id: ShmId) {
        self.segments.remove(&id);
        self.attached.retain(|_, i| *i != id);
    }

    /// How many attachments this process holds.
    pub fn attachment_count(&self) -> usize {
        self.attached.len()
    }

    /// The table a `fork` gives the child: the same segments at the same
    /// addresses, because the address space was copied with them mapped,
    /// and each attachment counted once more on its segment (`shm_open`).
    pub fn inherited(&self) -> Self {
        for id in self.attached.values() {
            if let Some(guard) = self.segments.get(id) {
                guard.lock().account_attach();
            }
        }
        ShmProc {
            segments: self.segments.clone(),
            attached: self.attached.clone(),
        }
    }
}

impl Drop for ShmProc {
    /// The address space that held these attachments is gone (`exit`,
    /// `execve`): each of them is a detach on its segment (`shm_close`).
    /// A segment only known, never attached, is not.
    fn drop(&mut self) {
        for id in self.attached.values() {
            if let Some(guard) = self.segments.get(id) {
                guard.lock().account_detach();
            }
        }
    }
}

/// System V shared memory attachments are mappings, and Linux counts them
/// as the address space does: `shm_nattch` goes up on `shmat` and on the
/// copy a `fork` makes, and down on `shmdt`, on `exit` and on `execve`.
/// Here it moved only on the two explicit calls, so every process that died
/// or exec'd while attached left the count one too high for ever. And a
/// segment attached twice was recorded once, under its id, at the last
/// address only.
#[cfg(test)]
mod shm_proc_tests {
    use super::*;
    use zircon_object::vm::VmObject;

    extern crate std;

    const PID: u32 = 42;
    const ID: ShmId = 9;
    const HERE: usize = 0x7f00_0000;
    const THERE: usize = 0x7f10_0000;

    fn a_segment() -> Arc<Mutex<ShmGuard>> {
        Arc::new(Mutex::new(ShmGuard {
            shared_guard: VmObject::new_paged(1),
            shmid_ds: Mutex::new(Default::default()),
        }))
    }

    fn nattch(guard: &Arc<Mutex<ShmGuard>>) -> usize {
        guard.lock().shmid_ds.lock().nattch
    }

    /// `shmat`, as `sys_shmat` does it: account on the segment, record in
    /// the process.
    fn shmat(proc: &mut ShmProc, guard: &Arc<Mutex<ShmGuard>>, addr: usize) {
        guard.lock().attach(PID);
        proc.attach(ID, guard.clone(), addr);
    }

    /// The process dies with the segment attached: the segment must see the
    /// detach, or it counts a user that no longer exists.
    #[test]
    fn dropping_the_table_detaches_everything_it_had_attached() {
        let guard = a_segment();
        let mut proc = ShmProc::default();
        shmat(&mut proc, &guard, HERE);
        shmat(&mut proc, &guard, THERE);
        assert_eq!(nattch(&guard), 2);
        drop(proc);
        assert_eq!(nattch(&guard), 0, "exit must detach every attachment");
        assert_ne!(
            guard.lock().shmid_ds.lock().dtime,
            0,
            "and it is a detach, with its time"
        );
    }

    /// A segment the process only named (`shmget`) is not attached, so its
    /// death detaches nothing from it.
    #[test]
    fn a_segment_only_known_is_not_detached_at_exit() {
        let guard = a_segment();
        let mut proc = ShmProc::default();
        proc.add(ID, guard.clone());
        // Somebody else is attached; that is who the count belongs to.
        guard.lock().attach(PID + 1);
        drop(proc);
        assert_eq!(nattch(&guard), 1, "a known segment is not an attached one");
    }

    /// `fork` copies the mapping, so the child is one more user of the
    /// segment, and its own death is one fewer.
    #[test]
    fn a_fork_attaches_the_child_and_its_exit_detaches_it() {
        let guard = a_segment();
        let mut parent = ShmProc::default();
        shmat(&mut parent, &guard, HERE);
        let child = parent.inherited();
        assert_eq!(nattch(&guard), 2, "the child holds the mapping too");
        assert_eq!(child.get_id(HERE), Some(ID));
        drop(child);
        assert_eq!(nattch(&guard), 1, "the child's exit is a detach");
        drop(parent);
        assert_eq!(nattch(&guard), 0);
    }

    /// shmat(2) allows attaching the same segment more than once. Recorded
    /// once, under the id, `shmdt` of the first address found nothing and
    /// unmapped nothing.
    #[test]
    fn a_segment_attached_twice_is_recorded_at_both_addresses() {
        let guard = a_segment();
        let mut proc = ShmProc::default();
        shmat(&mut proc, &guard, HERE);
        shmat(&mut proc, &guard, THERE);
        assert_eq!(proc.get_id(HERE), Some(ID));
        assert_eq!(proc.get_id(THERE), Some(ID));
        assert_eq!(proc.attachment_count(), 2);

        // Detaching one leaves the other attached, and the segment known.
        let gone = proc.detach(HERE).expect("HERE was attached");
        assert_eq!(gone.addr, HERE);
        assert!(Arc::ptr_eq(&gone.guard, &guard));
        assert_eq!(proc.get_id(HERE), None);
        assert_eq!(proc.get_id(THERE), Some(ID), "the other attachment stays");
        assert!(proc.get(ID).is_some(), "and the segment is still known");
        assert_eq!(proc.attachment_count(), 1);
    }

    /// `shmdt(addr)` of an address nothing is attached at is `EINVAL`: the
    /// table has nothing to give back.
    #[test]
    fn detaching_an_address_nothing_is_attached_at_finds_nothing() {
        let guard = a_segment();
        let mut proc = ShmProc::default();
        shmat(&mut proc, &guard, HERE);
        assert!(proc.detach(THERE).is_none());
        assert!(proc.detach(HERE + 0x1000).is_none(), "not even inside it");
        assert_eq!(proc.attachment_count(), 1, "and nothing was forgotten");
    }

    /// The explicit `shmdt` accounts through the caller (it carries the
    /// pid); once detached, the table's own drop must not count it again.
    #[test]
    fn an_explicit_detach_is_not_counted_twice_at_exit() {
        let guard = a_segment();
        let mut proc = ShmProc::default();
        shmat(&mut proc, &guard, HERE);
        proc.detach(HERE).expect("attached");
        guard.lock().detach(PID);
        assert_eq!(nattch(&guard), 0);
        // Somebody else attaches; our exit must leave them alone.
        guard.lock().attach(PID + 1);
        drop(proc);
        assert_eq!(nattch(&guard), 1);
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
        let id = 7;
        proc.add(id, set(1));
        proc.add_undo(id, 0, -1);
        proc.remove(id);
        drop(proc);
    }

    #[test]
    fn removing_a_set_drops_its_undo_records_and_leaves_the_others() {
        let _guard = test_lock();
        let mut proc = SemProc::default();
        let (a, b) = (7, 8);
        proc.add(a, set(1));
        proc.add(b, set(1));
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
        let id = 7;
        proc.add(id, array.clone());
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
        let id = 7;
        proc.add(id, array.clone());
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
        let id = 7;
        proc.add(id, array.clone());
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
        let id = 7;
        proc.add(id, array.clone());
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
        let id = 7;
        proc.add(id, array.clone());
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
        let id = 7;
        proc.add(id, set(1));
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
        let id = 7;
        proc.add(id, set(1));
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
        let id = 7;
        parent.add(id, array.clone());
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
    fn the_table_records_sets_under_the_id_it_is_given() {
        let _guard = test_lock();
        // The id is the system-wide one `sem_register` handed out, not a
        // number of this table's own choosing: two processes recording the
        // same set do it under the same number.
        let array = set(1);
        let mut a = SemProc::default();
        let mut b = SemProc::default();
        a.add(4242, array.clone());
        b.add(4242, array.clone());
        assert!(Arc::ptr_eq(&a.get(4242).unwrap(), &array));
        assert!(Arc::ptr_eq(&b.get(4242).unwrap(), &array));
        assert!(a.get(0).is_none(), "nothing was ever numbered from 0");
        a.remove(4242);
        assert!(a.get(4242).is_none());
        assert!(b.get(4242).is_some(), "each table is its own process's");
    }
}

/// Who may *use* a System V object, as opposed to who may change it.
///
/// The tanda that added [`IpcPerm::may_control`] made the owner and creator
/// ids truthful, and its own note ended "there is no `EACCES` anywhere in
/// this module". This is that `EACCES`: `ipcperms` (`ipc/util.c`), which
/// every access path in Linux goes through and none went through here.
#[cfg(test)]
mod ipc_access_tests {
    use super::*;

    const OWNER: u32 = 1000;
    const OWNER_GROUP: u32 = 50;
    const STRANGER: u32 = 1001;
    const STRANGER_GROUP: u32 = 51;

    /// A segment/queue/set created by `OWNER`:`OWNER_GROUP` with `mode`.
    fn perm(mode: u32) -> IpcPerm {
        IpcPerm {
            key: 1,
            uid: OWNER,
            gid: OWNER_GROUP,
            cuid: OWNER,
            cgid: OWNER_GROUP,
            mode,
            ..IpcPerm::default()
        }
    }

    /// The three classes, each answering for itself.
    #[test]
    fn the_owner_the_group_and_everyone_else_are_three_separate_classes() {
        let p = perm(0o640);
        assert!(p.may_access(OWNER, OWNER_GROUP, IPC_R | IPC_W));
        // The group may read and not write...
        assert!(p.may_access(STRANGER, OWNER_GROUP, IPC_R));
        assert!(!p.may_access(STRANGER, OWNER_GROUP, IPC_W));
        // ...and everyone else neither.
        assert!(!p.may_access(STRANGER, STRANGER_GROUP, IPC_R));
        assert!(!p.may_access(STRANGER, STRANGER_GROUP, IPC_W));
    }

    /// This is the hole: a `0600` object could be attached, read and written
    /// by anyone who could name its id, because naming it *was* the check.
    #[test]
    fn a_private_object_is_closed_to_everyone_but_its_owner() {
        let p = perm(0o600);
        assert!(p.may_access(OWNER, OWNER_GROUP, IPC_R | IPC_W));
        assert!(!p.may_access(STRANGER, OWNER_GROUP, IPC_R));
        assert!(!p.may_access(STRANGER, STRANGER_GROUP, IPC_R));
    }

    /// The classes do not fall through: being refused as the owner does not
    /// then try "group" or "other", so `0066` refuses its own owner. Linux is
    /// deliberate about that, and so is every other mode check in Unix.
    #[test]
    fn a_class_that_refuses_does_not_fall_through_to_the_next() {
        let p = perm(0o066);
        assert!(!p.may_access(OWNER, OWNER_GROUP, IPC_R));
        assert!(!p.may_access(OWNER, OWNER_GROUP, IPC_W));
        // ...while a stranger in the group, and even one outside it, may.
        assert!(p.may_access(STRANGER, OWNER_GROUP, IPC_R | IPC_W));
        assert!(p.may_access(STRANGER, STRANGER_GROUP, IPC_R | IPC_W));
    }

    /// `ipcperms` tries the creator's ids as well as the current owner's, so
    /// an `IPC_SET` that hands the object to somebody else does not lock the
    /// creator out of it.
    #[test]
    fn the_creator_is_tried_as_well_as_the_owner() {
        let mut p = perm(0o600);
        p.uid = STRANGER;
        p.gid = STRANGER_GROUP;
        assert!(p.may_access(STRANGER, STRANGER_GROUP, IPC_R | IPC_W));
        assert!(p.may_access(OWNER, 0, IPC_R | IPC_W), "still the creator");
        // The same for the group class.
        let mut g = perm(0o060);
        g.gid = STRANGER_GROUP;
        assert!(g.may_access(STRANGER, STRANGER_GROUP, IPC_R | IPC_W));
        assert!(g.may_access(STRANGER, OWNER_GROUP, IPC_R | IPC_W));
    }

    /// euid 0 stands in for `CAP_IPC_OWNER`.
    #[test]
    fn root_passes_whatever_the_mode_says() {
        assert!(perm(0).may_access(0, 0, IPC_R | IPC_W));
        assert!(perm(0o600).may_access(0, 999, IPC_R | IPC_W));
    }

    /// A mode word with nothing in it grants nothing -- but asking for
    /// nothing is still granted, which is what a bare `shmget(key, 0, 0)`
    /// existence probe does.
    #[test]
    fn asking_for_nothing_is_granted_and_a_zero_mode_grants_nothing() {
        let p = perm(0);
        assert!(p.may_access(STRANGER, STRANGER_GROUP, 0));
        assert!(!p.may_access(STRANGER, STRANGER_GROUP, IPC_R));
        assert!(!p.may_access(OWNER, OWNER_GROUP, IPC_R));
    }

    /// The caller does not know which class it will land in, so the mode word
    /// it passes to `*get` is folded onto one before it is compared.
    #[test]
    fn requested_mode_folds_the_three_classes_onto_one() {
        assert_eq!(IpcPerm::requested_mode(0), 0);
        assert_eq!(IpcPerm::requested_mode(0o400), IPC_R);
        assert_eq!(IpcPerm::requested_mode(0o040), IPC_R);
        assert_eq!(IpcPerm::requested_mode(0o004), IPC_R);
        assert_eq!(IpcPerm::requested_mode(0o200), IPC_W);
        assert_eq!(IpcPerm::requested_mode(0o640), IPC_R | IPC_W);
        assert_eq!(IpcPerm::requested_mode(0o666), IPC_R | IPC_W);
        // `IPC_CREAT`/`IPC_EXCL` ride in the same word and are not access.
        assert_eq!(IpcPerm::requested_mode(0o3000), 0);
        assert_eq!(IpcPerm::requested_mode(0o1600), IPC_R | IPC_W);
    }

    /// The two bits this kernel distinguishes.
    #[test]
    fn read_and_write_are_the_bits_ipcperms_compares() {
        assert_eq!(IPC_R, 0o4);
        assert_eq!(IPC_W, 0o2);
        let read_only = perm(0o400);
        assert!(read_only.may_access(OWNER, OWNER_GROUP, IPC_R));
        assert!(!read_only.may_access(OWNER, OWNER_GROUP, IPC_W));
        assert!(!read_only.may_access(OWNER, OWNER_GROUP, IPC_R | IPC_W));
    }
}
