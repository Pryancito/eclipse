//! `semctl` through the syscall: what its first argument names, which of its
//! fourteen commands needs what, and the two stamps a set keeps.
//!
//! `sem_wiring_tests` next door covers `semget` and `semop`; this is the
//! third and last of the three, and the one with the most ways to be wrong
//! because **one call does fourteen different things**. Its pure halves are
//! covered elsewhere again -- `semctl_subject` and `semctl_access` have
//! their own tests, and so do `getall_values`, `setall_values` and
//! `setval_from_arg` -- and again none of them can see whether the syscall
//! asks, in what order, or what it does with the answer.
//!
//! Five things are this file's:
//!
//! The first is **what the first argument names**, which is not the same for
//! every command. `IPC_INFO` and `SEM_INFO` ignore it, `SEM_STAT` and
//! `SEM_STAT_ANY` take an INDEX into the table, and everything else takes an
//! id. Looking an id up first is what made `semctl(0, 0, SEM_INFO)` answer
//! `EINVAL`, which busybox `ipcs -s` reports as "kernel not configured".
//!
//! The second is **what a command returns**, which is not always zero: a
//! stat by index answers with the ID it found there, which is how the caller
//! learns it, and the `*_INFO`s answer with the highest index in use so a
//! caller knows how far to walk.
//!
//! The third is **which permission each command needs**. Reading a value or
//! the `semid_ds` needs read, writing one needs write, and `IPC_SET` and
//! `IPC_RMID` ask a different question altogether -- who may CHANGE the set
//! -- and answer `EPERM` rather than `EACCES`. `SEM_STAT_ANY` is `SEM_STAT`
//! with no check at all. Each of those needs a set that grants one thing and
//! not the other, or the question has only one answer.
//!
//! The fourth is **that what `IPC_SET` wrote is what the next call obeys**.
//! Reading the mode back proves only that it was stored; the test changes
//! the mode and then shows the very call that was allowed before being
//! refused.
//!
//! The fifth is the pair of stamps `semop` leaves and only `semctl` can
//! read: `sem_otime`, the time of the last SUCCESSFUL operation, and
//! `sempid`, the process that made it. Both are invisible from
//! `sem_wiring_tests`, where a mutation run left them alive for exactly that
//! reason.
//!
//! And one refusal that used to be a kernel panic: `num` comes straight from
//! userspace and used to index the set, so `semctl(id, 9999, GETVAL)` from
//! any process at all took the machine down.
//!
//! Fixed pids live in 47_30x and 47_31x, keys in 47_300_0xx.

extern crate std;

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use core::time::Duration;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// `IPC_CREAT`, and the mode a set is made with when the test is not about
/// modes.
const CREAT: usize = 0o1000;
const EVERYBODY: usize = 0o666;

/// `IPC_NOWAIT` in a `sembuf`.
const NOWAIT: i16 = SemFlags::IPC_NOWAIT.bits();

/// A semaphore number past the end of every set below.
const NO_SUCH_SEMAPHORE: usize = 9_999;
/// A `semctl` command no `<linux/sem.h>` defines.
const A_COMMAND_OF_NOBODY: usize = 999;

/// Keys of this file's own, one set each.
const KEY: usize = 47_300_000;

/// How long a probe keeps asking before it gives up.
const TRIES: usize = 300;
const A_MOMENT: Duration = Duration::from_millis(10);
/// How long a waiter is watched taking nothing while it parks.
const WHILE_IT_PARKS: usize = 60;

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// What this process acts as from here on. The group goes first: dropping
/// the effective uid takes away the privilege `set_resgid` needs.
fn acting_as(sys: &Syscall<'_>, euid: u32, egid: u32) {
    let lp = sys.linux_process();
    lp.set_resgid(egid, egid, egid).unwrap();
    lp.set_resuid(euid, euid, euid).unwrap();
}

fn semget(sys: &Syscall<'_>, key: usize, nsems: usize, flags: usize) -> SysResult {
    sys.sys_semget(key, nsems, flags)
}

fn semctl(sys: &Syscall<'_>, id: usize, num: usize, cmd: SemctlCmds, arg: usize) -> SysResult {
    sys.sys_semctl(id, num, cmd as usize, arg)
}

async fn semop(sys: &Syscall<'_>, id: usize, ops: &[SemBuf]) -> SysResult {
    sys.sys_semop(id, (ops.as_ptr() as usize).into(), ops.len())
        .await
}

/// The `Syscall` the harness lends, as a `&mut` one. Every field is `Copy`.
fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// `semctl` reached through the syscall table instead of by name.
async fn by_number(sys: &Syscall<'_>, args: [usize; 6]) -> isize {
    let mut own = as_mut(sys);
    own.syscall(crate::Sys::SEMCTL as u32, args).await
}

/// A `semid_ds` to hand the kernel as a buffer.
///
/// The struct has padding a test cannot name, so one cannot be built here;
/// this takes a copy of ANOTHER set's. That is the better buffer anyway: a
/// command that writes nothing leaves recognisably the wrong set's numbers
/// behind instead of zeroes.
fn a_semid_ds_of_another_set(id: usize) -> SemidDs {
    *sem_lookup(id).unwrap().semid_ds.lock()
}

/// What `semctl(id, num, GETVAL)` answers.
fn value_of(sys: &Syscall<'_>, id: usize, num: usize) -> SysResult {
    semctl(sys, id, num, SemctlCmds::GETVAL, 0)
}

/// Whether semaphore `num` of `id` holds exactly zero, asked with `semop`
/// alone so a set that refuses `semctl` can still be looked at.
async fn holds_zero(sys: &Syscall<'_>, id: usize, num: u16) -> bool {
    semop(sys, id, &[SemBuf::new(num, 0, NOWAIT)]).await == Ok(0)
}

/// A thread of `proc` parked in `semop(id, ops)`, on its own host thread.
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

/// Watch semaphore `num` stay at zero for a while, which is the time the
/// waiter needs to park and, at once, the proof that it took nothing.
async fn nothing_moves_while_the_waiter_parks(sys: &Syscall<'_>, id: usize, num: u16) {
    for _ in 0..WHILE_IT_PARKS {
        assert!(
            holds_zero(sys, id, num).await,
            "a semaphore nobody has posted to did not stay at zero"
        );
        async_std::task::sleep(A_MOMENT).await;
    }
}

/// A command this kernel does not know is refused before anything else is
/// looked at -- the buffer included.
///
/// The order shows in the only place it can: an unknown command with an
/// unreadable buffer is `EINVAL`, and a command that IS known, with the same
/// unreadable buffer, is `EFAULT`. A caller probing for a command it is not
/// sure of reads `EINVAL` as "this kernel does not have it" and `EFAULT` as
/// "you passed a bad pointer", and they are not interchangeable.
#[test]
fn a_command_this_kernel_does_not_know_is_refused_before_the_buffer_is_touched() {
    const MINE: usize = KEY + 1;
    let proc = a_process(47_300, "semctl-unknown");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let id = semget(sys, MINE, 1, CREAT | EVERYBODY).unwrap();

            assert_eq!(
                sys.sys_semctl(id, 0, A_COMMAND_OF_NOBODY, 0),
                Err(LxError::EINVAL),
                "a command number nobody defines was accepted"
            );
            assert_eq!(
                semctl(sys, id, 0, SemctlCmds::IPC_STAT, 0),
                Err(LxError::EFAULT),
                "a null semid_ds buffer was written to anyway"
            );
            // And the set is untouched: an unknown command is not a
            // sideways IPC_RMID.
            assert_eq!(
                value_of(sys, id, 0),
                Ok(0),
                "the set did not survive an unknown command"
            );
        })
    });
}

/// What the first argument names depends on the command, and what the call
/// RETURNS depends on it too.
///
/// Three different readings of one register. `SEM_INFO` ignores it, which is
/// why `semctl(0, 0, SEM_INFO)` has to work -- looking it up as an id made
/// busybox `ipcs -s` print "kernel not configured". `SEM_STAT` reads it as an
/// index and answers with the ID it found, which is the only way a caller
/// walking the table learns the ids. `IPC_STAT` reads it as an id and
/// answers zero.
#[test]
fn what_the_first_argument_names_and_what_comes_back_depend_on_the_command() {
    const MINE: usize = KEY + 2;
    const SEVEN: usize = 7;
    let proc = a_process(47_301, "semctl-subject");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let id = semget(sys, MINE, SEVEN, CREAT | EVERYBODY).unwrap();
            let mut info = SemInfo::default();
            let at_info = &mut info as *mut SemInfo as usize;

            // The first argument is ignored, so the zero that used to be an
            // id lookup is fine. The answer is the highest index in use,
            // which is at least this set's own.
            let highest = semctl(sys, 0, 0, SemctlCmds::SEM_INFO, at_info).unwrap();
            assert!(
                highest >= id - 1,
                "SEM_INFO answered {}, below this set's own index {}",
                highest,
                id - 1
            );
            assert_eq!(
                semctl(sys, 0, 0, SemctlCmds::IPC_INFO, at_info),
                Ok(highest),
                "IPC_INFO and SEM_INFO did not agree on the highest index"
            );

            // An index, and the id comes back. The buffer starts as another
            // set's, so a command that wrote nothing would leave its numbers.
            let other = semget(sys, KEY + 3, 1, CREAT | EVERYBODY).unwrap();
            let mut ds = a_semid_ds_of_another_set(other);
            let at_ds = &mut ds as *mut SemidDs as usize;
            assert_eq!(
                semctl(sys, id - 1, 0, SemctlCmds::SEM_STAT, at_ds),
                Ok(id),
                "SEM_STAT did not read its argument as an index, or did not answer the id"
            );
            assert_eq!(ds.nsems, SEVEN, "SEM_STAT filled in the wrong set");
            assert_eq!(ds.perm.key as usize, MINE, "SEM_STAT filled in another key");

            // The same command without the permission check, same answer.
            assert_eq!(
                semctl(sys, id - 1, 0, SemctlCmds::SEM_STAT_ANY, at_ds),
                Ok(id),
                "SEM_STAT_ANY is SEM_STAT by index and did not answer the same"
            );

            // An id, and zero comes back.
            let mut by_id = a_semid_ds_of_another_set(other);
            let at_by_id = &mut by_id as *mut SemidDs as usize;
            assert_eq!(
                semctl(sys, id, 0, SemctlCmds::IPC_STAT, at_by_id),
                Ok(0),
                "IPC_STAT did not read its argument as an id, or did not answer zero"
            );
            assert_eq!(by_id.nsems, SEVEN, "IPC_STAT filled in the wrong set");

            // And through the table, where the four arguments have to land
            // in the four places the entry names.
            let mut numbered = a_semid_ds_of_another_set(other);
            assert_eq!(
                by_number(
                    sys,
                    [
                        id,
                        0,
                        SemctlCmds::IPC_STAT as usize,
                        &mut numbered as *mut SemidDs as usize,
                        0,
                        0
                    ]
                )
                .await,
                0,
                "semctl by number did not go through"
            );
            assert_eq!(
                numbered.nsems, SEVEN,
                "semctl by number put its arguments somewhere else"
            );
        })
    });
}

/// `SEM_INFO` is `IPC_INFO` with two of the ten fields replaced by what
/// actually exists.
///
/// The eight limits are the same in both and come from the crate that
/// ENFORCES them -- a limit reported but not enforced is a bug this pair has
/// had: `IPC_INFO` said 32000 while `semget` refused past 256, so
/// PostgreSQL's sizing was told yes and then no. The two that differ are
/// `semusz` (how many sets) and `semaem` (how many semaphores between them),
/// and with a set of seven in existence the counts cannot still be the
/// limits.
#[test]
fn sem_info_replaces_two_of_the_limits_with_what_exists() {
    const MINE: usize = KEY + 4;
    let proc = a_process(47_302, "semctl-info");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            semget(sys, MINE, 7, CREAT | EVERYBODY).unwrap();

            let mut limits = SemInfo::default();
            semctl(
                sys,
                0,
                0,
                SemctlCmds::IPC_INFO,
                &mut limits as *mut SemInfo as usize,
            )
            .unwrap();
            let mut counts = SemInfo::default();
            semctl(
                sys,
                0,
                0,
                SemctlCmds::SEM_INFO,
                &mut counts as *mut SemInfo as usize,
            )
            .unwrap();

            // IPC_INFO reports the limits and nothing else, so these two are
            // the constants `seminfo` carries.
            assert_eq!(limits.semusz, 20, "IPC_INFO moved semusz off its limit");
            assert_eq!(limits.semaem, SEMVMX, "IPC_INFO moved semaem off its limit");
            // SEM_INFO puts what exists in those two places.
            assert!(
                counts.semusz >= 1,
                "SEM_INFO counted no sets while one was in front of it"
            );
            assert!(
                counts.semaem >= 7,
                "SEM_INFO counted {} semaphores with a set of seven in existence",
                counts.semaem
            );
            assert_ne!(
                counts.semaem, SEMVMX,
                "SEM_INFO left semaem on the limit instead of counting"
            );
            assert!(
                counts.semaem >= counts.semusz,
                "SEM_INFO counted fewer semaphores than sets"
            );

            // The eight that are limits either way, and that come from the
            // crate that enforces them.
            assert_eq!(limits.semmni, counts.semmni);
            assert_eq!(limits.semmsl, counts.semmsl);
            assert_eq!(limits.semopm, counts.semopm);
            assert_eq!(limits.semvmx, counts.semvmx);
            assert_eq!(
                limits.semmni as usize,
                linux_object::ipc::SEMMNI,
                "the reported SEMMNI is not the one semget enforces"
            );
            assert_eq!(
                limits.semmsl as usize,
                linux_object::ipc::SEMMSL,
                "the reported SEMMSL is not the one semget enforces"
            );
            assert_eq!(
                limits.semopm as usize, SEMOPM,
                "the reported SEMOPM is not the one semop enforces"
            );
            assert_eq!(
                limits.semvmx, SEMVMX,
                "the reported SEMVMX is not the one semop enforces"
            );
        })
    });
}

/// Reading something and writing something need different permissions, and
/// the commands split the way Linux splits them.
///
/// Two sets, because a mode that grants both answers the inverted question
/// the same way. Every refusal below is a call that WOULD have succeeded:
/// the sets exist, the numbers are in range and the values are legal.
#[test]
fn reading_a_value_and_writing_one_need_different_permissions() {
    const READABLE: usize = KEY + 5;
    const WRITABLE: usize = KEY + 6;
    const A_USER: u32 = 1_000;
    let proc = a_process(47_303, "semctl-access");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            acting_as(sys, A_USER, A_USER);
            let readable = semget(sys, READABLE, 2, CREAT | 0o444).unwrap();
            let writable = semget(sys, WRITABLE, 2, CREAT | 0o222).unwrap();
            let mut values = [0u16; 2];
            let at_values = values.as_mut_ptr() as usize;
            let mut ds = a_semid_ds_of_another_set(readable);
            let at_ds = &mut ds as *mut SemidDs as usize;

            // Read only: everything that reads goes through.
            assert_eq!(value_of(sys, readable, 0), Ok(0));
            assert_eq!(semctl(sys, readable, 0, SemctlCmds::GETPID, 0), Ok(0));
            assert_eq!(semctl(sys, readable, 0, SemctlCmds::GETNCNT, 0), Ok(0));
            assert_eq!(semctl(sys, readable, 0, SemctlCmds::GETZCNT, 0), Ok(0));
            assert_eq!(
                semctl(sys, readable, 0, SemctlCmds::GETALL, at_values),
                Ok(0),
                "GETALL needed more than read"
            );
            assert_eq!(
                semctl(sys, readable, 0, SemctlCmds::IPC_STAT, at_ds),
                Ok(0),
                "IPC_STAT needed more than read"
            );
            // And nothing that writes does.
            assert_eq!(
                semctl(sys, readable, 0, SemctlCmds::SETVAL, 1),
                Err(LxError::EACCES),
                "a read-only set took a SETVAL"
            );
            assert_eq!(
                semctl(sys, readable, 0, SemctlCmds::SETALL, at_values),
                Err(LxError::EACCES),
                "a read-only set took a SETALL"
            );
            assert_eq!(
                value_of(sys, readable, 0),
                Ok(0),
                "a refused write reached the semaphore anyway"
            );

            // Write only: the mirror.
            assert_eq!(
                semctl(sys, writable, 0, SemctlCmds::SETVAL, 5),
                Ok(0),
                "a write-only set refused a SETVAL"
            );
            assert_eq!(
                semctl(sys, writable, 0, SemctlCmds::SETALL, at_values),
                Ok(0),
                "a write-only set refused a SETALL"
            );
            for cmd in [
                SemctlCmds::GETVAL,
                SemctlCmds::GETPID,
                SemctlCmds::GETNCNT,
                SemctlCmds::GETZCNT,
            ] {
                assert_eq!(
                    semctl(sys, writable, 0, cmd, 0),
                    Err(LxError::EACCES),
                    "a write-only set answered {:?}",
                    cmd
                );
            }
            assert_eq!(
                semctl(sys, writable, 0, SemctlCmds::GETALL, at_values),
                Err(LxError::EACCES),
                "a write-only set answered GETALL"
            );
            assert_eq!(
                semctl(sys, writable, 0, SemctlCmds::IPC_STAT, at_ds),
                Err(LxError::EACCES),
                "a write-only set answered IPC_STAT"
            );
        })
    });
}

/// The two commands that CHANGE a set ask who may change it, not who may use
/// it, and answer `EPERM`.
///
/// The set below grants read and write to everybody, so `may_access` says
/// yes to the stranger for every other command -- and `IPC_SET` and
/// `IPC_RMID` still have to refuse, because the question they ask is
/// `may_control`. Nothing asked at all before: the `semid_ds` userspace
/// handed in rewrote `uid` and `gid`, so naming the id was enough to take
/// the set over.
///
/// `SEM_STAT_ANY` is the third of the commands with no `ipcperms` question,
/// and the only one where that makes it MORE permissive: on a set with no
/// mode bits at all, `SEM_STAT` is `EACCES` and `SEM_STAT_ANY` goes through.
#[test]
fn changing_a_set_asks_who_owns_it_and_answers_eperm() {
    const OWNED: usize = KEY + 7;
    const UNREADABLE: usize = KEY + 8;
    const AN_OWNER: u32 = 1_000;
    const A_STRANGER: u32 = 1_001;

    let owner = a_process(47_304, "semctl-owner");
    let (owned, unreadable) = on_a_thread(&owner, move |sys| {
        Box::pin(async move {
            acting_as(sys, AN_OWNER, AN_OWNER);
            (
                semget(sys, OWNED, 1, CREAT | EVERYBODY).unwrap(),
                semget(sys, UNREADABLE, 1, CREAT).unwrap(),
            )
        })
    });

    let stranger = a_process(47_305, "semctl-stranger");
    on_a_thread(&stranger, move |sys| {
        Box::pin(async move {
            acting_as(sys, A_STRANGER, A_STRANGER);
            let mut ds = a_semid_ds_of_another_set(owned);
            let at_ds = &mut ds as *mut SemidDs as usize;

            // 0666, so every ipcperms question answers yes for anybody.
            assert_eq!(
                value_of(sys, owned, 0),
                Ok(0),
                "a 0666 set refused a stranger a GETVAL"
            );
            assert_eq!(semctl(sys, owned, 0, SemctlCmds::SETVAL, 3), Ok(0));
            // And the two that are not an ipcperms question at all.
            assert_eq!(
                semctl(sys, owned, 0, SemctlCmds::IPC_SET, at_ds),
                Err(LxError::EPERM),
                "a stranger took a set over with IPC_SET"
            );
            assert_eq!(
                semctl(sys, owned, 0, SemctlCmds::IPC_RMID, 0),
                Err(LxError::EPERM),
                "a stranger removed somebody else's set"
            );
            // Still there, and still holding what was put in it.
            assert_eq!(
                value_of(sys, owned, 0),
                Ok(3),
                "the set did not survive the two refusals"
            );

            // A set with no mode bits: the stat with the check is refused
            // and the one without it is not.
            assert_eq!(
                semctl(sys, unreadable - 1, 0, SemctlCmds::SEM_STAT, at_ds),
                Err(LxError::EACCES),
                "a set with no mode bits was stat-ed by a stranger"
            );
            assert_eq!(
                semctl(sys, unreadable - 1, 0, SemctlCmds::SEM_STAT_ANY, at_ds),
                Ok(unreadable),
                "SEM_STAT_ANY asked the permission question it is defined not to ask"
            );
        })
    });
}

/// What `IPC_SET` wrote is what the next call obeys.
///
/// Reading the mode back would prove only that it was stored somewhere. The
/// test instead makes a call, changes the mode, and makes the same call
/// again: `SETVAL` goes from `Ok` to `EACCES` with nothing else different.
/// Only the low nine bits are taken, as `semctl(2)` says.
#[test]
fn the_mode_ipc_set_writes_is_the_one_the_next_call_obeys() {
    const MINE: usize = KEY + 9;
    const A_USER: u32 = 1_000;
    let proc = a_process(47_306, "semctl-ipc-set");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            acting_as(sys, A_USER, A_USER);
            let id = semget(sys, MINE, 1, CREAT | EVERYBODY).unwrap();

            // Allowed to begin with.
            assert_eq!(semctl(sys, id, 0, SemctlCmds::SETVAL, 4), Ok(0));

            let mut ds = a_semid_ds_of_another_set(id);
            semctl(
                sys,
                id,
                0,
                SemctlCmds::IPC_STAT,
                &mut ds as *mut SemidDs as usize,
            )
            .unwrap();
            // Read only from here on, with bits above the low nine that
            // IPC_SET has to drop.
            ds.perm.mode = 0o7400;
            assert_eq!(
                semctl(
                    sys,
                    id,
                    0,
                    SemctlCmds::IPC_SET,
                    &mut ds as *mut SemidDs as usize
                ),
                Ok(0),
                "the owner could not change its own set"
            );

            assert_eq!(
                semctl(sys, id, 0, SemctlCmds::SETVAL, 1),
                Err(LxError::EACCES),
                "the mode IPC_SET wrote is not the one the next call obeys"
            );
            assert_eq!(
                value_of(sys, id, 0),
                Ok(4),
                "read is still granted, and the refused SETVAL changed nothing"
            );

            let mut after = a_semid_ds_of_another_set(id);
            semctl(
                sys,
                id,
                0,
                SemctlCmds::IPC_STAT,
                &mut after as *mut SemidDs as usize,
            )
            .unwrap();
            assert_eq!(
                after.perm.mode, 0o400,
                "IPC_SET kept bits above the low nine"
            );
            // The creator ids are not IPC_SET's to move, which is what keeps
            // control with whoever made the set.
            assert_eq!(after.perm.cuid, A_USER, "IPC_SET rewrote the creator uid");
        })
    });
}

/// The whole set is written and read in one call, and a value out of range
/// refuses the whole array.
///
/// Both commands used to fall into the per-semaphore arm below and answer
/// `EINVAL`, so a set could be initialised only one `SETVAL` at a time and
/// never read back at once -- which is how every program that uses a
/// semaphore set as a vector of counters starts up. `SETALL` judges every
/// value before it applies any (`semctl_main`: the range loop runs ahead of
/// the assignment loop), so an array with one bad value leaves the set
/// exactly as it was.
#[test]
fn a_whole_set_is_written_and_read_in_one_call() {
    const MINE: usize = KEY + 10;
    const THREE: usize = 3;
    /// One past the end of the buffer, to catch a write that goes too far.
    const A_SENTINEL: u16 = 0xbeef;
    let proc = a_process(47_307, "semctl-all");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let id = semget(sys, MINE, THREE, CREAT | EVERYBODY).unwrap();

            let good = [1u16, 2, 3];
            assert_eq!(
                semctl(sys, id, 0, SemctlCmds::SETALL, good.as_ptr() as usize),
                Ok(0),
                "SETALL was refused"
            );

            let mut read = [0u16, 0, 0, A_SENTINEL];
            assert_eq!(
                semctl(sys, id, 0, SemctlCmds::GETALL, read.as_mut_ptr() as usize),
                Ok(0),
                "GETALL was refused"
            );
            assert_eq!(&read[..THREE], &good, "GETALL did not read the set back");
            assert_eq!(
                read[THREE], A_SENTINEL,
                "GETALL wrote past the end of the set"
            );

            // One value past SEMVMX refuses the array, and the set keeps the
            // three it had.
            let bad = [9u16, SEMVMX as u16 + 1, 9];
            assert_eq!(
                semctl(sys, id, 0, SemctlCmds::SETALL, bad.as_ptr() as usize),
                Err(LxError::ERANGE),
                "SETALL took a value past SEMVMX"
            );
            let mut again = [0u16; THREE];
            semctl(sys, id, 0, SemctlCmds::GETALL, again.as_mut_ptr() as usize).unwrap();
            assert_eq!(
                again, good,
                "a refused SETALL applied the values it had already walked past"
            );

            // SETALL stamps the caller on every semaphore it wrote, as
            // `semctl_main` does.
            let us = sys.zircon_process().id() as usize;
            for num in 0..THREE {
                assert_eq!(
                    semctl(sys, id, num, SemctlCmds::GETPID, 0),
                    Ok(us),
                    "SETALL did not stamp semaphore {}",
                    num
                );
            }
        })
    });
}

/// A semaphore number outside the set is refused, and used to be a kernel
/// panic.
///
/// `num` comes straight from userspace and this arm used to index the set
/// with it, so `semctl(id, 9999, GETVAL)` from any process at all took the
/// machine down. semctl(2) says `EINVAL`, and its sibling `semop` had the
/// check all along.
#[test]
fn a_semaphore_number_outside_the_set_is_refused_and_not_a_panic() {
    const MINE: usize = KEY + 11;
    const TWO: usize = 2;
    let proc = a_process(47_308, "semctl-num");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let id = semget(sys, MINE, TWO, CREAT | EVERYBODY).unwrap();

            for cmd in [
                SemctlCmds::GETVAL,
                SemctlCmds::GETPID,
                SemctlCmds::GETNCNT,
                SemctlCmds::GETZCNT,
                SemctlCmds::SETVAL,
            ] {
                assert_eq!(
                    semctl(sys, id, NO_SUCH_SEMAPHORE, cmd, 0),
                    Err(LxError::EINVAL),
                    "{:?} took a semaphore number far outside the set",
                    cmd
                );
                // The boundary, which is what an off-by-one gets wrong.
                assert_eq!(
                    semctl(sys, id, TWO, cmd, 0),
                    Err(LxError::EINVAL),
                    "{:?} took the first number outside the set",
                    cmd
                );
                assert_eq!(
                    semctl(sys, id, TWO - 1, cmd, 0),
                    Ok(0),
                    "{:?} refused the last number inside the set",
                    cmd
                );
            }
        })
    });
}

/// `SETVAL`'s argument is the `int` the caller passed, not the register it
/// arrived in.
///
/// `arg` is the `int val` member of `union semun`, and a variadic argument
/// arrives in a whole register: reading all 64 bits of it turns the `-1` a
/// program passes by mistake into 4294967295 and sets the semaphore to it,
/// instead of answering. Linux reads the `int`, and `semctl(2)` says
/// `ERANGE` for anything outside `[0, SEMVMX]`.
#[test]
fn the_value_setval_takes_is_the_int_the_caller_passed() {
    const MINE: usize = KEY + 12;
    let proc = a_process(47_309, "semctl-setval");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let id = semget(sys, MINE, 1, CREAT | EVERYBODY).unwrap();
            assert_eq!(semctl(sys, id, 0, SemctlCmds::SETVAL, 6), Ok(0));

            // All ones is -1 read as an int, which is out of range.
            assert_eq!(
                semctl(sys, id, 0, SemctlCmds::SETVAL, usize::MAX),
                Err(LxError::ERANGE),
                "a negative value was taken"
            );
            assert_eq!(
                value_of(sys, id, 0),
                Ok(6),
                "a refused SETVAL reached the semaphore"
            );

            // The ceiling is allowed and one more is not.
            assert_eq!(
                semctl(sys, id, 0, SemctlCmds::SETVAL, SEMVMX as usize),
                Ok(0),
                "SEMVMX itself was refused"
            );
            assert_eq!(value_of(sys, id, 0), Ok(SEMVMX as usize));
            assert_eq!(
                semctl(sys, id, 0, SemctlCmds::SETVAL, SEMVMX as usize + 1),
                Err(LxError::ERANGE),
                "a value past SEMVMX was taken"
            );
            assert_eq!(value_of(sys, id, 0), Ok(SEMVMX as usize));

            // Only the low 32 bits are the `int`: a register whose top half
            // is rubbish still sets the value its bottom half names.
            assert_eq!(
                semctl(sys, id, 0, SemctlCmds::SETVAL, 0x1_0000_0007),
                Ok(0),
                "a register with a high half set was refused"
            );
            assert_eq!(
                value_of(sys, id, 0),
                Ok(7),
                "SETVAL read more than the int it was given"
            );
        })
    });
}

/// What a set reports is who last operated it and when, and only `semctl`
/// can read either.
///
/// `sem_otime` is the time of the last SUCCESSFUL operation
/// (`do_smart_update`), and it used to be stamped before the loop that
/// applies one: a `semop` that answered `ERANGE` or `EAGAIN`, or one that
/// blocked for ever, moved it, so `ipcs -s` and any watchdog that reads it
/// to tell "in use" from "wedged" got the wrong answer. `sempid` is the
/// other half, and the two processes below make it say something: whoever
/// operated the set LAST is the one named.
#[test]
fn what_a_set_reports_is_who_last_operated_it_and_when() {
    const MINE: usize = KEY + 13;
    let first = a_process(47_310, "semctl-stamps-first");
    let (id, first_pid) = on_a_thread(&first, move |sys| {
        Box::pin(async move {
            let id = semget(sys, MINE, 1, CREAT | EVERYBODY).unwrap();
            let mut fresh = a_semid_ds_of_another_set(id);
            semctl(
                sys,
                id,
                0,
                SemctlCmds::IPC_STAT,
                &mut fresh as *mut SemidDs as usize,
            )
            .unwrap();
            assert_eq!(
                fresh.otime, 0,
                "a set nobody has operated already carried an otime"
            );

            // A refused operation is not a successful one.
            assert_eq!(
                semop(sys, id, &[SemBuf::new(0, -1, NOWAIT)]).await,
                Err(LxError::EAGAIN)
            );
            let mut after_refusal = a_semid_ds_of_another_set(id);
            semctl(
                sys,
                id,
                0,
                SemctlCmds::IPC_STAT,
                &mut after_refusal as *mut SemidDs as usize,
            )
            .unwrap();
            assert_eq!(
                after_refusal.otime, 0,
                "a semop that answered EAGAIN stamped the set anyway"
            );

            assert_eq!(semop(sys, id, &[SemBuf::new(0, 3, 0)]).await, Ok(0));
            let mut stamped = a_semid_ds_of_another_set(id);
            semctl(
                sys,
                id,
                0,
                SemctlCmds::IPC_STAT,
                &mut stamped as *mut SemidDs as usize,
            )
            .unwrap();
            assert_ne!(stamped.otime, 0, "a successful semop did not stamp the set");

            let us = sys.zircon_process().id() as usize;
            assert_eq!(
                semctl(sys, id, 0, SemctlCmds::GETPID, 0),
                Ok(us),
                "the set did not name the process that operated it"
            );
            (id, us)
        })
    });

    let second = a_process(47_311, "semctl-stamps-second");
    on_a_thread(&second, move |sys| {
        Box::pin(async move {
            let us = sys.zircon_process().id() as usize;
            assert_ne!(us, first_pid, "the two processes have one pid");
            assert_eq!(
                semctl(sys, id, 0, SemctlCmds::GETPID, 0),
                Ok(first_pid),
                "the set named this process before it had operated it"
            );
            assert_eq!(semop(sys, id, &[SemBuf::new(0, -1, NOWAIT)]).await, Ok(0));
            assert_eq!(
                semctl(sys, id, 0, SemctlCmds::GETPID, 0),
                Ok(us),
                "the set still names the process that operated it before"
            );
        })
    });
}

/// The waiters on a semaphore are counted, and the count of those waiting
/// for a zero is a separate number.
///
/// `GETNCNT` and `GETZCNT` are two adjacent commands answering two counts of
/// the same shape, which is exactly the pair a swap hides. This kernel does
/// not keep the second one, so `GETZCNT` is zero whatever is parked -- and
/// that is itself the thing to pin down, because a `GETNCNT` wired into
/// `GETZCNT`'s place would read zero with a waiter in front of it.
#[test]
fn the_waiters_on_a_semaphore_are_counted_and_the_zero_waiters_are_a_separate_number() {
    const MINE: usize = KEY + 14;
    let maker = a_process(47_312, "semctl-ncnt-maker");
    let id = on_a_thread(&maker, move |sys| {
        Box::pin(async move {
            let id = semget(sys, MINE, 1, CREAT | EVERYBODY).unwrap();
            assert_eq!(
                semctl(sys, id, 0, SemctlCmds::GETNCNT, 0),
                Ok(0),
                "a semaphore nobody is waiting on counted a waiter"
            );
            id
        })
    });

    let waiting = a_process(47_313, "semctl-ncnt-waiter");
    let waiter = a_waiter(&waiting, id, Vec::from([SemBuf::new(0, -1, 0)]));

    let watching = a_process(47_314, "semctl-ncnt-watcher");
    on_a_thread(&watching, move |sys| {
        Box::pin(async move {
            let mut counted = 0;
            for _ in 0..TRIES {
                if semctl(sys, id, 0, SemctlCmds::GETNCNT, 0) == Ok(1) {
                    counted = 1;
                    break;
                }
                async_std::task::sleep(A_MOMENT).await;
            }
            assert_eq!(counted, 1, "the parked waiter was never counted");
            assert_eq!(
                semctl(sys, id, 0, SemctlCmds::GETZCNT, 0),
                Ok(0),
                "GETZCNT answered the count of the other kind of waiter"
            );
            // Let it go, so the thread does not outlive the test.
            assert_eq!(semop(sys, id, &[SemBuf::new(0, 1, 0)]).await, Ok(0));
        })
    });

    assert_eq!(waiter.join().unwrap(), Ok(0));
}

/// Removing a set makes its id and its key name nothing, and wakes what was
/// parked on it with `EIDRM`.
///
/// Three things in one command, and the waiter is the one that cannot be
/// seen any other way: `SemArray::remove` marks every semaphore removed and
/// fires its event bus, `sem_unregister` takes the id out of the system
/// table, and the key goes with it. A process still parked in `semop` when
/// `ipcrm -s` runs has to come back, and come back with `EIDRM` rather than
/// a success it did not get.
#[test]
fn removing_a_set_wakes_what_was_parked_on_it_and_leaves_nothing_behind() {
    const MINE: usize = KEY + 15;
    let maker = a_process(47_315, "semctl-rmid-maker");
    let id = on_a_thread(&maker, move |sys| {
        Box::pin(async move { semget(sys, MINE, 1, CREAT | EVERYBODY).unwrap() })
    });

    let waiting = a_process(47_316, "semctl-rmid-waiter");
    let waiter = a_waiter(&waiting, id, Vec::from([SemBuf::new(0, -1, 0)]));

    let removing = a_process(47_317, "semctl-rmid-remover");
    on_a_thread(&removing, move |sys| {
        Box::pin(async move {
            nothing_moves_while_the_waiter_parks(sys, id, 0).await;
            assert_eq!(
                semctl(sys, id, 0, SemctlCmds::IPC_RMID, 0),
                Ok(0),
                "the set could not be removed"
            );

            // The id names nothing, to the process that removed it as much
            // as to anybody else.
            assert_eq!(
                value_of(sys, id, 0),
                Err(LxError::EINVAL),
                "a removed set still answered by id"
            );
            assert_eq!(
                semop(sys, id, &[SemBuf::new(0, 1, NOWAIT)]).await,
                Err(LxError::EINVAL),
                "a removed set still took operations"
            );
            // And so does the key, so a program that asks whether the set is
            // there is told it is not.
            assert_eq!(
                semget(sys, MINE, 0, 0),
                Err(LxError::ENOENT),
                "the key of a removed set still named it"
            );
            // A second removal has nothing to remove.
            assert_eq!(
                semctl(sys, id, 0, SemctlCmds::IPC_RMID, 0),
                Err(LxError::EINVAL),
                "a set was removed twice"
            );
        })
    });

    assert_eq!(
        waiter.join().unwrap(),
        Err(LxError::EIDRM),
        "the process parked on the set was not woken into EIDRM"
    );
}
