//! System V semaphores through the syscall: a set made in one process and
//! operated from another, an operation array that lands whole or not at all,
//! and the park a blocked `semop` goes into.
//!
//! This file's two syscalls are between them the whole of System V
//! semaphores, and until now **no test named either**. What `ipc.rs` already
//! covers next door is every pure piece of them: `semget_nsems_tests` owns
//! the count a new set may ask for, `semop_plan_tests` owns `plan_semop`,
//! `sembuf_layout_tests` owns the struct userspace writes and
//! `ipc_access_tests` owns `may_access`. Not one of those can see whether
//! the syscall ASKS its question, in what order, or what it does with the
//! answer.
//!
//! Five pieces of wiring are what this file is for.
//!
//! The first is that **an id means the same set in every process**. That is
//! the whole point of a System V id (sysvipc(7)) and it was not true here:
//! the id was an index into the calling process's own table, so an id handed
//! to another program named a different set there or nothing at all, and a
//! set whose creator exited took every value it held with it. The registry
//! `sem_register` keeps is what fixed it, and only a test that makes a set
//! in one process and operates it from another can tell.
//!
//! The second is **order**. `semop` judges how many operations it was given
//! before it reads the array, so a count of zero is `EINVAL` and a count
//! past `SEMOPM` is `E2BIG` even with nothing at all at the pointer -- and a
//! count of one at the same pointer is `EFAULT`, which is what makes the
//! other two mean anything. `semget` judges `nsems` before it goes near the
//! key, so a refused count leaves the key naming nothing.
//!
//! The third is **atomicity**: "the operations are performed either as a
//! complete unit, or not at all". `plan_semop` decides the whole array
//! against one snapshot, but whether the decision REACHES the semaphores --
//! all of it on `Apply` and none of it on `WouldBlock` -- is this file's to
//! say. An array that cannot proceed must leave every semaphore exactly as
//! it found it, which the op-at-a-time loop this code replaced did not.
//!
//! The fourth is **which permission an operation needs**, and it is the one
//! that needs an asymmetric fixture. Read is enough to wait for a zero;
//! altering a semaphore needs write. A set that grants only one of the two
//! separates them: with a mode that grants both, inverting the question
//! changes nothing and the test passes either way.
//!
//! The fifth is the **park**: a `semop` that cannot proceed and did not ask
//! for `IPC_NOWAIT` blocks until somebody else's operation makes room, and
//! `IPC_NOWAIT` is read off the operation that is IN THE WAY rather than the
//! first one in the array. Both need two threads, so the waiter below goes
//! on its own host thread and the operation that releases it is made from
//! another process entirely.
//!
//! One trap: `KEY2SEM` and the id registry are global to this binary, and a
//! registered set is never dropped. Every set here gets a key of its own, so
//! no test is handed the set another one made.
//!
//! Fixed pids live in 47_28x and 47_29x, keys in 47_280_0xx.

extern crate std;

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use core::time::Duration;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// `IPC_CREAT`: make the set if the key names none.
const CREAT: usize = 0o1000;
/// `IPC_EXCL`: with `IPC_CREAT`, refuse a key that already names one.
const EXCL: usize = 0o2000;
/// The mode a set is created with when the test is not about modes.
const EVERYBODY: usize = 0o666;
/// The access a probe asks for: read and write, folded the way `ipcperms`
/// folds it. Zero would ask for nothing and pass every check.
const READ_AND_WRITE: usize = 0o600;

/// `IPC_NOWAIT` in a `sembuf`: answer `EAGAIN` rather than park.
const NOWAIT: i16 = SemFlags::IPC_NOWAIT.bits();
/// A `sem_flg` bit no `semop` recognises.
const A_FLAG_OF_NOBODY: i16 = 0x1;

/// An id the registry can never answer: ids are handed out from 1 and
/// counted up, and never reused.
const NO_SUCH_SET: usize = usize::MAX;
/// A semaphore number past the end of every set below.
const NO_SUCH_SEMAPHORE: u16 = 9_999;

/// Keys of this file's own, one set each.
const KEY: usize = 47_280_000;

/// How long a probe keeps asking before it gives up: 3 seconds, well inside
/// the 20 the harness gives the whole call.
const TRIES: usize = 300;
/// The gap between two tries, long enough that they are not a spin.
const A_MOMENT: Duration = Duration::from_millis(10);
/// How long a waiter is watched taking nothing while it parks.
const WHILE_IT_PARKS: usize = 60;

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// What this process acts as from here on.
///
/// The order is not free: dropping the effective uid to something other than
/// root takes away the privilege `set_resgid` needs, so the group goes first.
fn acting_as(sys: &Syscall<'_>, euid: u32, egid: u32, groups: &[u32]) {
    let lp = sys.linux_process();
    lp.set_groups(Vec::from(groups));
    lp.set_resgid(egid, egid, egid).unwrap();
    lp.set_resuid(euid, euid, euid).unwrap();
}

fn semget(sys: &Syscall<'_>, key: usize, nsems: usize, flags: usize) -> SysResult {
    sys.sys_semget(key, nsems, flags)
}

/// `semop(id, ops, ops.len())`, with the array where the kernel reads it
/// from: the caller's own memory, which is all `UserInPtr` names here.
async fn semop(sys: &Syscall<'_>, id: usize, ops: &[SemBuf]) -> SysResult {
    sys.sys_semop(id, (ops.as_ptr() as usize).into(), ops.len())
        .await
}

/// A `semop` with nothing readable at the pointer, for the refusals that are
/// decided before it is read.
async fn semop_at_nothing(sys: &Syscall<'_>, id: usize, num_ops: usize) -> SysResult {
    sys.sys_semop(id, 0usize.into(), num_ops).await
}

/// The `Syscall` the harness lends, as a `&mut` one. Every field is `Copy`.
fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// A call reached through the syscall table instead of by name.
async fn by_number(sys: &Syscall<'_>, which: crate::Sys, args: [usize; 6]) -> isize {
    let mut own = as_mut(sys);
    own.syscall(which as u32, args).await
}

/// Whether semaphore `num` of `id` holds exactly zero, asked without
/// changing it: an operation of zero is "wait until this one is zero", so
/// with `IPC_NOWAIT` it answers the question outright. Needs read access
/// only, which is what makes it the probe a read-only set can take.
async fn holds_zero(sys: &Syscall<'_>, id: usize, num: u16) -> bool {
    semop(sys, id, &[SemBuf::new(num, 0, NOWAIT)]).await == Ok(0)
}

/// Whether semaphore `num` of `id` held exactly `value`: taking that much
/// out of it succeeds and leaves it at zero.
///
/// Destructive on purpose. "It had at least this much" is all one operation
/// can see; the zero behind it is what makes the answer exact.
async fn held_exactly(sys: &Syscall<'_>, id: usize, num: u16, value: i16) -> bool {
    semop(sys, id, &[SemBuf::new(num, -value, NOWAIT)]).await == Ok(0)
        && holds_zero(sys, id, num).await
}

/// A thread of `proc` parked in `semop(id, ops)`, on its own host thread.
///
/// `on_a_thread` blocks until the call it makes comes back, so a wait and
/// the operation that releases it cannot both be made from the test's own
/// thread. `semop` has no timeout of its own, so a wait nothing releases
/// ends as the harness's 20-second panic -- a failure, if a blunt one.
fn a_waiter(
    proc: &Arc<Process>,
    id: usize,
    ops: Vec<SemBuf>,
) -> std::thread::JoinHandle<SysResult> {
    let proc = proc.clone();
    std::thread::spawn(move || {
        on_a_thread(&proc, move |sys| {
            Box::pin(async move { semop(sys, id, &ops).await })
        })
    })
}

/// Watch semaphore `num` stay at zero for a while.
///
/// This is both the settle -- the waiter needs time to park -- and an
/// assertion: a kernel that applied a plan it had decided could not proceed
/// would have let the waiter take something, and there is nothing there to
/// take.
async fn nothing_moves_while_the_waiter_parks(sys: &Syscall<'_>, id: usize, num: u16) {
    for _ in 0..WHILE_IT_PARKS {
        assert!(
            holds_zero(sys, id, num).await,
            "a semaphore nobody has posted to did not stay at zero"
        );
        async_std::task::sleep(A_MOMENT).await;
    }
}

/// Wait for semaphore `num` to be back at zero, which is how the test sees
/// that the parked waiter woke and took what was posted.
async fn the_waiter_took_it(sys: &Syscall<'_>, id: usize, num: u16) -> bool {
    for _ in 0..TRIES {
        if holds_zero(sys, id, num).await {
            return true;
        }
        async_std::task::sleep(A_MOMENT).await;
    }
    false
}

/// `nsems` is judged before the key is looked at, so a count the kernel will
/// not take leaves the key naming nothing at all.
///
/// Three refusals, from two different places: a negative count and one past
/// `SEMMSL` come from `semget_nsems`, and a count of zero gets as far as
/// `get_or_create`, which will not make an empty set. The last one is the
/// one that could have left a trace, since it is decided inside the function
/// that does the inserting: an empty set used to be created, take an id and
/// a key, and answer `EFBIG` to every operation on it for ever.
#[test]
fn a_count_of_semaphores_the_kernel_will_not_take_makes_no_set() {
    const MINE: usize = KEY + 1;
    let proc = a_process(47_280, "semget-count");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            // `nsems` is an `int`: all ones is -1, not a huge count.
            for (nsems, why) in [
                (usize::MAX, "a negative count"),
                (linux_object::ipc::SEMMSL + 1, "a count past SEMMSL"),
                (0, "a count of zero"),
            ] {
                assert_eq!(
                    semget(sys, MINE, nsems, CREAT | EVERYBODY),
                    Err(LxError::EINVAL),
                    "{} was taken",
                    why
                );
                assert_eq!(
                    semget(sys, MINE, 0, 0),
                    Err(LxError::ENOENT),
                    "{} left the key naming a set",
                    why
                );
            }
            // And the key was free the whole time, which is what the three
            // ENOENTs above are worth anything against.
            assert!(
                semget(sys, MINE, 1, CREAT | EVERYBODY).is_ok(),
                "the key could not be used after three refusals"
            );
        })
    });
}

/// A key already in use answers with the id it already has, and the ways of
/// asking that are not that.
///
/// `semget(key, 0, 0)` is how a program asks whether a set is there, so a
/// key that names none has to be `ENOENT` and not a silent create;
/// `IPC_EXCL` is how it asks to be the one that made it; and a caller that
/// asks for more semaphores than the set has is refused rather than handed
/// an id and left believing in semaphores that do not exist.
#[test]
fn a_key_already_in_use_answers_with_the_id_it_already_has() {
    const MINE: usize = KEY + 2;
    const ANOTHER: usize = KEY + 3;
    const FREE: usize = KEY + 4;
    let proc = a_process(47_281, "semget-again");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            assert_eq!(
                semget(sys, FREE, 0, 0),
                Err(LxError::ENOENT),
                "a key that names no set was not ENOENT"
            );

            let id = semget(sys, MINE, 2, CREAT | EVERYBODY).unwrap();
            assert_eq!(
                semget(sys, MINE, 2, CREAT | EVERYBODY),
                Ok(id),
                "the same key asked for again answered with another id"
            );
            assert_eq!(
                semget(sys, MINE, 0, 0),
                Ok(id),
                "a lookup of an existing key did not find it"
            );
            assert_eq!(
                semget(sys, MINE, 1, 0),
                Ok(id),
                "asking for fewer semaphores than the set has was refused"
            );
            assert_eq!(
                semget(sys, MINE, 3, 0),
                Err(LxError::EINVAL),
                "asking for more semaphores than the set has was granted"
            );
            assert_eq!(
                semget(sys, MINE, 2, CREAT | EXCL | EVERYBODY),
                Err(LxError::EEXIST),
                "IPC_EXCL on a key that already names a set was not EEXIST"
            );

            // A second key is a second set, so the id is not a constant.
            let other = semget(sys, ANOTHER, 1, CREAT | EVERYBODY).unwrap();
            assert_ne!(other, id, "two keys answered with one id");
        })
    });
}

/// The owner, group and creator a new set records are the caller's own, and
/// they are recorded truthfully enough to decide who may use the set
/// afterwards.
///
/// Read back through `IPC_STAT` this would prove very little: the check that
/// matters reads those fields, so the test asks the check. The set made
/// `0600` is reachable by the uid that made it and by nobody else, and the
/// one made `0060` is reachable through the group -- as the acting group of
/// a process and as a supplementary group of one, which is the other half of
/// what `semget` passes along. A kernel that filed either set under root's
/// ids, or under none, refuses the caller that should get in.
#[test]
fn the_ids_a_new_set_records_are_the_callers_own() {
    const OWNED: usize = KEY + 5;
    const SHARED: usize = KEY + 6;
    const A_USER: u32 = 1_000;
    const A_GROUP: u32 = 2_000;
    const SOMEBODY_ELSE: u32 = 1_001;
    const ANOTHER_GROUP: u32 = 3_000;

    let maker = a_process(47_282, "semget-ids-maker");
    let shared = on_a_thread(&maker, move |sys| {
        Box::pin(async move {
            acting_as(sys, A_USER, A_GROUP, &[]);
            let owned = semget(sys, OWNED, 1, CREAT | 0o600).unwrap();
            let shared = semget(sys, SHARED, 1, CREAT | 0o060).unwrap();
            // The owner of a `0600` set gets in, which it only can if the
            // uid written down is the one that was acting.
            assert_eq!(
                semget(sys, OWNED, 0, READ_AND_WRITE),
                Ok(owned),
                "the user that made a 0600 set could not reach it"
            );
            shared
        })
    });

    let stranger = a_process(47_283, "semget-ids-group");
    on_a_thread(&stranger, move |sys| {
        Box::pin(async move {
            acting_as(sys, SOMEBODY_ELSE, A_GROUP, &[]);
            // Group, not owner: a `0600` set grants the group nothing. The
            // classes do not fall through, so this is EACCES and not a retry
            // against the other bits.
            assert_eq!(
                semget(sys, OWNED, 0, READ_AND_WRITE),
                Err(LxError::EACCES),
                "a 0600 set let somebody in through its group"
            );
            // And the `0060` set is the mirror: the group is all it grants.
            assert_eq!(
                semget(sys, SHARED, 0, READ_AND_WRITE),
                Ok(shared),
                "a 0060 set refused the group it was made for"
            );
        })
    });

    let outsider = a_process(47_284, "semget-ids-outsider");
    on_a_thread(&outsider, move |sys| {
        Box::pin(async move {
            acting_as(sys, SOMEBODY_ELSE, ANOTHER_GROUP, &[]);
            assert_eq!(
                semget(sys, SHARED, 0, READ_AND_WRITE),
                Err(LxError::EACCES),
                "a 0060 set let a process of another group in"
            );
            // The same process, now holding the group on its supplementary
            // list and acting as something else. That list is the second
            // thing `semget` hands the check, and the ordinary way a set is
            // shared through a group.
            acting_as(sys, SOMEBODY_ELSE, ANOTHER_GROUP, &[A_GROUP]);
            assert_eq!(
                semget(sys, SHARED, 0, READ_AND_WRITE),
                Ok(shared),
                "a supplementary group did not count as being in the group"
            );
        })
    });
}

/// A set made by one process is the same set, under the same id, in another
/// -- and it is still there once the process that made it has gone.
///
/// This is the property a System V id exists for, and the one this kernel
/// did not have: ids were per-process indices, so a semid passed to another
/// program through a file or an argument named something else there, and the
/// only strong reference to a set was the creator's own table. A setup
/// program that made a set, filled it and exited took every value with it.
#[test]
fn a_set_made_by_one_process_is_the_same_set_in_another() {
    const MINE: usize = KEY + 7;
    let maker = a_process(47_285, "sem-shared-maker");
    let id = on_a_thread(&maker, move |sys| {
        Box::pin(async move {
            let id = semget(sys, MINE, 2, CREAT | EVERYBODY).unwrap();
            assert_eq!(
                semop(sys, id, &[SemBuf::new(0, 5, 0)]).await,
                Ok(0),
                "the process that made the set could not post to it"
            );
            id
        })
    });

    let other = a_process(47_286, "sem-shared-user");
    on_a_thread(&other, move |sys| {
        Box::pin(async move {
            // The key resolves to the same id from here, which is how a
            // program that was not told the id finds the set.
            assert_eq!(
                semget(sys, MINE, 0, 0),
                Ok(id),
                "the key named a different set in another process"
            );
            // And the five the other process posted are really in it.
            assert!(
                held_exactly(sys, id, 0, 5).await,
                "the value another process posted was not there"
            );
            // An id nobody has been given names nothing, from here as from
            // anywhere.
            assert_eq!(
                semop(sys, NO_SUCH_SET, &[SemBuf::new(0, 1, 0)]).await,
                Err(LxError::EINVAL),
                "an id that names no set was operated on"
            );
        })
    });
}

/// How many operations the call carries is judged before the array is read,
/// and the array is read before the id is looked up.
///
/// Both refusals below are answered with a null pointer, which the third
/// call shows would be `EFAULT` if the array were read first. Getting this
/// backwards costs a program its probe: `semop(id, NULL, 0)` is how a
/// caller asks whether the set is still there without touching it, and
/// `EFAULT` is not the answer it is looking for. The id coming last is the
/// other half: a malformed array is refused whoever it was aimed at.
#[test]
fn how_many_operations_there_are_is_judged_before_the_array_is_read() {
    const MINE: usize = KEY + 8;
    let proc = a_process(47_287, "semop-order");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let id = semget(sys, MINE, 1, CREAT | EVERYBODY).unwrap();

            assert_eq!(
                semop_at_nothing(sys, id, 0).await,
                Err(LxError::EINVAL),
                "an empty operation array was not EINVAL"
            );
            assert_eq!(
                semop_at_nothing(sys, id, SEMOPM + 1).await,
                Err(LxError::E2BIG),
                "more operations than SEMOPM was not E2BIG"
            );
            // The same pointer, with a count the kernel will take: now the
            // array IS read, and there is nothing at it.
            assert_eq!(
                semop_at_nothing(sys, id, 1).await,
                Err(LxError::EFAULT),
                "a null operation array was read anyway"
            );
            // Read before the id is looked up: an unreadable array aimed at
            // a set that does not exist is still EFAULT.
            assert_eq!(
                semop_at_nothing(sys, NO_SUCH_SET, 1).await,
                Err(LxError::EFAULT),
                "the id was looked up before the array was read"
            );
            // A flag bit nobody defines is EINVAL for the WHOLE array, and
            // nothing in it is applied.
            assert_eq!(
                semop(sys, id, &[SemBuf::new(0, 1, A_FLAG_OF_NOBODY)]).await,
                Err(LxError::EINVAL),
                "an unknown sem_flg bit was accepted"
            );
            assert!(
                holds_zero(sys, id, 0).await,
                "an operation refused for its flags was applied anyway"
            );
        })
    });
}

/// An array that cannot proceed leaves every semaphore exactly as it found
/// them, and the operations inside one array see each other in order.
///
/// Atomicity is semop(2)'s central promise and the hardest thing about it to
/// test, because the only difference between keeping it and breaking it is
/// what is visible AFTERWARDS. The code this replaced applied each operation
/// as it went: a `[+3, -1]` that had to block left the `+3` where every
/// other process could see it and then waited for ever holding it.
///
/// The in-order half is the same two operations twice, in the two orders:
/// `[+1, -1]` on a semaphore at zero goes through because the second sees
/// the first, and `[-1, +1]` cannot start.
#[test]
fn an_array_that_cannot_proceed_leaves_every_semaphore_as_it_was() {
    const MINE: usize = KEY + 9;
    let proc = a_process(47_288, "semop-atomic");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let id = semget(sys, MINE, 2, CREAT | EVERYBODY).unwrap();
            assert_eq!(semop(sys, id, &[SemBuf::new(0, 5, 0)]).await, Ok(0));

            // Semaphore 0 can take the three; semaphore 1 has nothing to
            // give. The whole array is refused.
            assert_eq!(
                semop(
                    sys,
                    id,
                    &[SemBuf::new(0, 3, NOWAIT), SemBuf::new(1, -1, NOWAIT)]
                )
                .await,
                Err(LxError::EAGAIN),
                "an array with an operation that cannot proceed was taken"
            );
            assert!(
                holds_zero(sys, id, 1).await,
                "the semaphore that blocked the array was changed"
            );
            // Still five and not eight: the operation that could have
            // proceeded did not.
            assert!(
                held_exactly(sys, id, 0, 5).await,
                "an operation of a refused array was applied anyway"
            );

            // Both semaphores are at zero again. The same pair of
            // operations, in the two orders.
            assert_eq!(
                semop(
                    sys,
                    id,
                    &[SemBuf::new(0, 1, NOWAIT), SemBuf::new(0, -1, NOWAIT)]
                )
                .await,
                Ok(0),
                "a release followed by a take did not see its own release"
            );
            assert!(
                holds_zero(sys, id, 0).await,
                "a release and a take left something behind"
            );
            assert_eq!(
                semop(
                    sys,
                    id,
                    &[SemBuf::new(0, -1, NOWAIT), SemBuf::new(0, 1, NOWAIT)]
                )
                .await,
                Err(LxError::EAGAIN),
                "a take was served out of a release that comes after it"
            );
            assert!(
                holds_zero(sys, id, 0).await,
                "the release of a refused array was applied"
            );
        })
    });
}

/// Whether an operation alters a semaphore decides which permission the call
/// needs: read to wait for a zero, write to change anything.
///
/// Nothing asked at all before this, so naming the id was the whole check --
/// a set created `0600` could be operated by any process that could name it.
/// The two sets below are what makes the question visible: with a mode that
/// grants both, read and write are the same answer and inverting the
/// question changes nothing. Each refusal here is also a call that would
/// have SUCCEEDED without the check, which is what separates "refused" from
/// "could not have worked anyway".
#[test]
fn whether_an_operation_alters_a_semaphore_decides_which_permission_it_needs() {
    const READABLE: usize = KEY + 10;
    const WRITABLE: usize = KEY + 11;
    const A_USER: u32 = 1_000;
    let proc = a_process(47_289, "semop-access");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            acting_as(sys, A_USER, A_USER, &[]);
            let readable = semget(sys, READABLE, 1, CREAT | 0o444).unwrap();
            let writable = semget(sys, WRITABLE, 1, CREAT | 0o222).unwrap();

            // Read only. Waiting for a zero is allowed, and the semaphore
            // is at zero, so the call goes through.
            assert!(
                holds_zero(sys, readable, 0).await,
                "waiting for a zero needed more than read"
            );
            assert_eq!(
                semop(sys, readable, &[SemBuf::new(0, 1, 0)]).await,
                Err(LxError::EACCES),
                "a read-only set was altered"
            );
            assert!(
                holds_zero(sys, readable, 0).await,
                "the refused operation was applied to a read-only set"
            );

            // Write only. The mirror, and the refusal is a call that would
            // have answered Ok: the semaphore is at zero, so without the
            // check "wait until it is zero" returns at once.
            assert_eq!(
                semop(sys, writable, &[SemBuf::new(0, 0, NOWAIT)]).await,
                Err(LxError::EACCES),
                "a write-only set could be read"
            );
            assert_eq!(
                semop(sys, writable, &[SemBuf::new(0, 1, 0)]).await,
                Ok(0),
                "a write-only set could not be altered"
            );
            // Proved with another altering operation, since reading it is
            // the thing this set does not allow: the one that was posted
            // comes back out, and a second take does not.
            assert_eq!(
                semop(sys, writable, &[SemBuf::new(0, -1, NOWAIT)]).await,
                Ok(0),
                "what was posted to a write-only set was not there"
            );
            assert_eq!(
                semop(sys, writable, &[SemBuf::new(0, -1, NOWAIT)]).await,
                Err(LxError::EAGAIN),
                "a write-only set gave out more than was posted to it"
            );
        })
    });
}

/// A `semop` that cannot proceed and did not ask for `IPC_NOWAIT` parks
/// until somebody else's operation makes room.
///
/// The waiter and the process that releases it have no relation to each
/// other beyond the id, which is the case this is for: two unrelated
/// programs holding one lock. The waiter is watched taking nothing for as
/// long as the semaphore stays at zero, so the release that follows is
/// really what ends the wait and not something that had already happened.
#[test]
fn a_wait_that_cannot_proceed_parks_until_another_process_releases_it() {
    const MINE: usize = KEY + 12;
    let maker = a_process(47_290, "semop-park-maker");
    let id = on_a_thread(&maker, move |sys| {
        Box::pin(async move { semget(sys, MINE, 1, CREAT | EVERYBODY).unwrap() })
    });

    let waiting = a_process(47_291, "semop-park-waiter");
    let waiter = a_waiter(&waiting, id, Vec::from([SemBuf::new(0, -1, 0)]));

    let posting = a_process(47_292, "semop-park-poster");
    on_a_thread(&posting, move |sys| {
        Box::pin(async move {
            nothing_moves_while_the_waiter_parks(sys, id, 0).await;
            assert_eq!(
                semop(sys, id, &[SemBuf::new(0, 1, 0)]).await,
                Ok(0),
                "the release could not be posted"
            );
            assert!(
                the_waiter_took_it(sys, id, 0).await,
                "the parked waiter never took what was released"
            );
        })
    });

    assert_eq!(
        waiter.join().unwrap(),
        Ok(0),
        "the wait did not come back once the semaphore was released"
    );
}

/// `IPC_NOWAIT` is read off the operation that is IN THE WAY, not the first
/// one in the array.
///
/// semop(2) is explicit about it, and it is the one thing about the flag a
/// single-operation array cannot show. Both arrays below are the same two
/// operations with the blocking one second; they differ only in which of the
/// two carries the flag, and they differ in outcome: `EAGAIN` on the spot
/// against a wait that another process has to end. A kernel reading the
/// flag off the first operation gets both of them backwards.
#[test]
fn the_operation_in_the_way_is_the_one_that_decides_whether_the_call_waits() {
    const MINE: usize = KEY + 13;
    let maker = a_process(47_293, "semop-nowait-maker");
    let id = on_a_thread(&maker, move |sys| {
        Box::pin(async move { semget(sys, MINE, 2, CREAT | EVERYBODY).unwrap() })
    });

    // Semaphore 1 is at zero, so the leading "wait for zero" proceeds and
    // the take on semaphore 0 is what cannot. It carries the flag.
    let asked_not_to_wait = a_process(47_294, "semop-nowait-asked");
    on_a_thread(&asked_not_to_wait, move |sys| {
        Box::pin(async move {
            assert_eq!(
                semop(sys, id, &[SemBuf::new(1, 0, 0), SemBuf::new(0, -1, NOWAIT)]).await,
                Err(LxError::EAGAIN),
                "IPC_NOWAIT on the operation in the way did not answer EAGAIN"
            );
        })
    });

    // The same array with the flag on the other operation: the one in the
    // way did not ask, so the call waits.
    let waiting = a_process(47_295, "semop-nowait-waiter");
    let waiter = a_waiter(
        &waiting,
        id,
        Vec::from([SemBuf::new(1, 0, NOWAIT), SemBuf::new(0, -1, 0)]),
    );

    let posting = a_process(47_296, "semop-nowait-poster");
    on_a_thread(&posting, move |sys| {
        Box::pin(async move {
            nothing_moves_while_the_waiter_parks(sys, id, 0).await;
            assert_eq!(semop(sys, id, &[SemBuf::new(0, 1, 0)]).await, Ok(0));
            assert!(
                the_waiter_took_it(sys, id, 0).await,
                "the waiter that did not ask for IPC_NOWAIT never woke"
            );
        })
    });

    assert_eq!(
        waiter.join().unwrap(),
        Ok(0),
        "IPC_NOWAIT on an operation that was not in the way ended the wait"
    );
}

/// A semaphore number outside the set is refused, and so is an addition that
/// would carry one past the ceiling -- and neither leaves a trace.
///
/// `num` comes straight from userspace, and indexing with it is how
/// `semctl`'s sibling used to panic the kernel from any process. `SEMVMX` is
/// the other bound: a counting semaphore released more often than it is
/// taken answers rather than wrapping into a negative value.
#[test]
fn a_semaphore_outside_the_set_and_a_value_past_the_ceiling_are_both_refused() {
    const MINE: usize = KEY + 14;
    let proc = a_process(47_297, "semop-bounds");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let id = semget(sys, MINE, 1, CREAT | EVERYBODY).unwrap();

            assert_eq!(
                semop(sys, id, &[SemBuf::new(NO_SUCH_SEMAPHORE, 1, 0)]).await,
                Err(LxError::EFBIG),
                "a semaphore number far outside the set was accepted"
            );
            // One past the end of a set of one, which is the boundary rather
            // than a wild number.
            assert_eq!(
                semop(sys, id, &[SemBuf::new(1, 1, 0)]).await,
                Err(LxError::EFBIG),
                "the first semaphore number outside the set was accepted"
            );
            // And an array whose SECOND operation is out of range takes
            // none of it, not even the first.
            assert_eq!(
                semop(sys, id, &[SemBuf::new(0, 1, 0), SemBuf::new(1, 1, 0)]).await,
                Err(LxError::EFBIG),
                "an array naming a semaphore outside the set was accepted"
            );
            assert!(
                holds_zero(sys, id, 0).await,
                "the first operation of an out-of-range array was applied"
            );

            // The ceiling itself is allowed; one more is not.
            assert_eq!(
                semop(sys, id, &[SemBuf::new(0, SEMVMX as i16, 0)]).await,
                Ok(0),
                "a semaphore could not be raised to SEMVMX"
            );
            assert_eq!(
                semop(sys, id, &[SemBuf::new(0, 1, 0)]).await,
                Err(LxError::ERANGE),
                "a semaphore was carried past SEMVMX"
            );
            assert!(
                held_exactly(sys, id, 0, SEMVMX as i16).await,
                "the refused addition moved the semaphore anyway"
            );
        })
    });
}

/// The arguments arrive in the places the syscall table names.
///
/// `semget(key, nsems, flags)` and `semop(id, ops, nops)` each take three
/// registers of plain integers, and `sys_semop`'s middle one is a pointer
/// while the others are not -- which is the only thing that makes a swap
/// visible from outside. The set is made by number and then operated by
/// number, and the proof that the second call landed is read with the named
/// one.
#[test]
fn the_arguments_arrive_in_the_order_the_syscall_table_names_them() {
    const MINE: usize = KEY + 15;
    let proc = a_process(47_298, "sem-by-number");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let made = by_number(
                sys,
                crate::Sys::SEMGET,
                [MINE, 1, CREAT | EVERYBODY, 0, 0, 0],
            )
            .await;
            assert!(made > 0, "semget by number did not make a set: {}", made);
            let id = made as usize;
            assert_eq!(
                semget(sys, MINE, 0, 0),
                Ok(id),
                "the set semget made by number is not the one its key names"
            );

            let ops = [SemBuf::new(0, 7, 0)];
            let posted = by_number(
                sys,
                crate::Sys::SEMOP,
                [id, ops.as_ptr() as usize, ops.len(), 0, 0, 0],
            )
            .await;
            assert_eq!(posted, 0, "semop by number did not go through");
            assert!(
                held_exactly(sys, id, 0, 7).await,
                "semop by number did not put the value where it was asked to"
            );
        })
    });
}
