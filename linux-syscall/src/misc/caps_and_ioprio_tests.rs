//! What a program asks about its own privilege, and what priority the disk
//! should serve it at: `capget`, `capset`, `ioprio_get` and `ioprio_set`,
//! asked through the syscall.
//!
//! Four syscalls, and until now **not one test named any of them**. The
//! pure halves are well covered next door -- `capability_tests` and
//! `cap_user_data_layout_tests` in this file's parent own the version
//! table, the subset rules and the struct layout; `ioprio_tests` owns the
//! class/level word, the nice derivation and the fold -- and every one of
//! those functions answers a question correctly without anybody checking
//! that the syscall ASKS it.
//!
//! Three pieces of wiring are what this file is for.
//!
//! The first is the **split of a 64-bit set across two 32-bit elements**.
//! `published_capabilities` hands back one `u64`; the caller reads an array
//! of `struct __user_cap_data_struct`. Put the halves the wrong way round,
//! or write the low half into both, and `cap_get_flag` reports capabilities
//! this kernel does not grant and withholds ones it does -- with every pure
//! test still green, because none of them can see the array.
//!
//! The second is the **negotiation protocol**, which is a write on the way
//! out of a refusal. An unknown version is `EINVAL` *and* the preferred
//! version written back into the caller's header; a null `data` is answered
//! 0 whatever the header said, which is how libcap asks what this kernel
//! speaks. Both halves are observable only from outside.
//!
//! The third is **order**. `ioprio_set` judges the word before it looks any
//! task up, so asking for real time without the capability is `EPERM` even
//! when the pid names nobody; swap the two and the same call becomes
//! `ESRCH`, and a program that probes for permission learns the wrong thing.
//! `capget` tests its null `data` before it tests the pid, so the probe
//! works with a pid that would otherwise be `EINVAL`.
//!
//! One measured trap, the same one the resource-limit batch found: a
//! caller whose real and effective user ids are EQUAL cannot see those two
//! comparisons swapped. `may_set_ioprio_of` asks both, so the caller here
//! holds a real uid and an effective uid that differ, and there is a target
//! for each of them.
//!
//! Fixed pids live in 47_22x and 47_23x.

extern crate std;

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::{LinuxProcess, CAP_LAST_CAP};
use linux_object::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Thread, ROOT_JOB};

/// An ordinary user. Root is the only holder of any capability here, so
/// "not root" and "powerless" are the same thing.
const A_USER: u32 = 1000;

/// Somebody else again, who shares no id with the caller.
const ANOTHER_USER: u32 = 2000;

/// The caller's EFFECTIVE user, kept different from its real one so the
/// two comparisons `may_set_ioprio_of` makes cannot pass for each other.
const A_THIRD_USER: u32 = 3000;

/// A user id nothing else in this binary uses.
///
/// `ioprio_get(IOPRIO_WHO_USER, ...)` walks every live process, so a test
/// about one user's tasks has to own that user outright: another test's
/// process with the same uid and a live thread would join the fold.
const A_LONE_USER: u32 = 47_221;

/// `IOPRIO_WHO_PROCESS`, `IOPRIO_WHO_PGRP` and `IOPRIO_WHO_USER` as
/// userspace spells them: 1, 2 and 3, one above `setpriority`'s.
const WHO_PROCESS: usize = 1;
const WHO_PGRP: usize = 2;
const WHO_USER: usize = 3;

/// A `which` that is none of the three.
const WHO_NOTHING: usize = 4;

/// What an out-parameter holds before a call, so "left alone" cannot pass
/// for "written".
const A_SEED: u32 = 0xdead_beef;

/// A pid no process and no thread of this binary has.
const A_PID_OF_NOBODY: i32 = 47_239;

/// Every capability there is, as `published_capabilities` builds it for
/// root. Written out here from `CAP_LAST_CAP` rather than asked for, so
/// the two halves below are a claim about the struct and not a tautology.
const EVERY_CAPABILITY: u64 = (1u64 << (CAP_LAST_CAP + 1)) - 1;

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// A process whose three user ids are all `uid`, so its REAL uid -- the one
/// every gate here compares against -- is `uid`.
fn a_process_of(pid: KoID, name: &str, uid: u32) -> Arc<Process> {
    let proc = a_process(pid, name);
    proc.linux().set_resuid(uid, uid, uid).unwrap();
    proc
}

/// A process with one live thread, which is what `priority_targets` needs:
/// it walks `threads()`, and a process that never made one has none, so a
/// target built without this is `ESRCH` for a reason that has nothing to do
/// with what the test is asking.
///
/// The `Arc` goes back to the caller because the thread lives as long as
/// somebody holds it.
fn a_process_with_a_thread(pid: KoID, name: &str, uid: u32) -> (Arc<Process>, Arc<Thread>) {
    let proc = a_process_of(pid, name, uid);
    let thread = Thread::create_linux(&proc).unwrap();
    (proc, thread)
}

/// The `Syscall` the harness lends, as a `&mut` one, for the calls that
/// want one. Every field is `Copy`.
fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// A header as userspace fills it in.
fn a_header(version: u32, pid: i32) -> CapUserHeader {
    CapUserHeader { version, pid }
}

/// Two data elements seeded, so an element the call does not write shows up
/// as one.
fn seeded_data() -> [CapUserData; 2] {
    [CapUserData {
        effective: A_SEED,
        permitted: A_SEED,
        inheritable: A_SEED,
    }; 2]
}

/// `capget(2)` with a real `data` array.
fn capget(sys: &Syscall<'_>, hdr: &mut CapUserHeader, data: &mut [CapUserData; 2]) -> SysResult {
    let at = data.as_mut_ptr() as usize;
    sys.sys_capget(((hdr as *mut CapUserHeader) as usize).into(), at.into())
}

/// `capget(&hdr, NULL)`: the version probe, which carries no data at all.
fn capget_probe(sys: &Syscall<'_>, hdr: &mut CapUserHeader) -> SysResult {
    sys.sys_capget(((hdr as *mut CapUserHeader) as usize).into(), 0usize.into())
}

/// `capset(2)`.
fn capset(sys: &Syscall<'_>, hdr: &mut CapUserHeader, data: &[CapUserData; 2]) -> SysResult {
    sys.sys_capset(
        ((hdr as *mut CapUserHeader) as usize).into(),
        (data.as_ptr() as usize).into(),
    )
}

/// The sets a process asks to hold, as the two elements of the array.
fn sets(effective: u64, permitted: u64, inheritable: u64) -> [CapUserData; 2] {
    let half = |v: u64, shift: u32| (v >> shift) as u32;
    [
        CapUserData {
            effective: half(effective, 0),
            permitted: half(permitted, 0),
            inheritable: half(inheritable, 0),
        },
        CapUserData {
            effective: half(effective, 32),
            permitted: half(permitted, 32),
            inheritable: half(inheritable, 32),
        },
    ]
}

/// One `ioprio` word as the syscall returns it.
fn ioprio_get(sys: &Syscall<'_>, which: usize, who: usize) -> SysResult {
    sys.sys_ioprio_get(which, who)
}

fn ioprio_set(sys: &Syscall<'_>, which: usize, who: usize, ioprio: u16) -> SysResult {
    sys.sys_ioprio_set(which, who, ioprio as usize)
}

/// The call as the dispatch table makes it, by number, so the order of its
/// arguments is under test too.
async fn by_number(sys: &Syscall<'_>, number: u32, args: [usize; 6]) -> isize {
    let mut own = as_mut(sys);
    own.syscall(number, args).await
}

#[test]
fn capget_publishes_every_capability_to_root_and_none_to_anybody_else() {
    let root = a_process(47_220, "capget-root");
    let (them, _thread) = a_process_with_a_thread(47_221, "capget-user", A_USER);
    let their_pid = them.id() as i32;
    on_a_thread(&root, move |sys| {
        Box::pin(async move {
            let mut hdr = a_header(LINUX_CAPABILITY_VERSION_3, 0);
            let mut data = seeded_data();
            assert_eq!(capget(sys, &mut hdr, &mut data), Ok(0), "capget failed");

            // The 64-bit set, cut in two: the low 32 bits in the first
            // element, the high ones in the second. The high half of
            // `CAP_LAST_CAP = 40` is nine bits, not thirty-two, so an
            // element holding the low half twice is visible from here.
            assert_eq!(
                data[0].effective, EVERY_CAPABILITY as u32,
                "the low half of the effective set"
            );
            assert_eq!(
                data[1].effective,
                (EVERY_CAPABILITY >> 32) as u32,
                "the high half of the effective set"
            );
            assert_ne!(
                data[1].effective, data[0].effective,
                "both elements hold the same half"
            );
            assert_ne!(
                data[1].effective,
                u32::MAX,
                "the high half is every bit, so the set was not cut at 64"
            );
            assert_eq!(
                data[0].permitted, data[0].effective,
                "permitted != effective"
            );
            assert_eq!(
                data[1].permitted, data[1].effective,
                "permitted != effective"
            );
            // Nothing is inheritable here, and the seed proves the field
            // was written rather than left behind.
            assert_eq!(data[0].inheritable, 0, "the low half of inheritable");
            assert_eq!(data[1].inheritable, 0, "the high half of inheritable");

            // The same call about a process that is not root: the sets are
            // empty, which is a different answer and not an error.
            let mut hdr = a_header(LINUX_CAPABILITY_VERSION_3, their_pid);
            let mut data = seeded_data();
            assert_eq!(capget(sys, &mut hdr, &mut data), Ok(0), "capget failed");
            for (i, elem) in data.iter().enumerate() {
                assert_eq!(elem.effective, 0, "element {} of an ordinary user", i);
                assert_eq!(elem.permitted, 0, "element {} of an ordinary user", i);
            }
        })
    });
}

#[test]
fn an_unknown_header_version_is_written_back_as_the_preferred_one() {
    let proc = a_process(47_222, "capget-version");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            // What libcap sends first: version 0, no data. The answer is
            // not an error, and the header comes back saying what this
            // kernel speaks.
            let mut hdr = a_header(0, 0);
            assert_eq!(capget_probe(sys, &mut hdr), Ok(0), "the probe was refused");
            assert_eq!(
                hdr.version, LINUX_CAPABILITY_VERSION_3,
                "the probe did not learn the version"
            );
            assert_eq!(hdr.pid, 0, "the probe changed the pid");

            // The same unknown version with real data IS an error, and
            // writes the version back just the same.
            let mut hdr = a_header(0, 0);
            let mut data = seeded_data();
            assert_eq!(
                capget(sys, &mut hdr, &mut data),
                Err(LxError::EINVAL),
                "an unknown version was accepted"
            );
            assert_eq!(
                hdr.version, LINUX_CAPABILITY_VERSION_3,
                "the version was not written back"
            );
            assert_eq!(
                data[0].effective, A_SEED,
                "a refused capget wrote the data anyway"
            );

            // A version this kernel knows is left alone: the write-back is
            // the unknown case and not something every call does.
            let mut hdr = a_header(LINUX_CAPABILITY_VERSION_2, 0);
            let mut data = seeded_data();
            assert_eq!(capget(sys, &mut hdr, &mut data), Ok(0), "v2 was refused");
            assert_eq!(
                hdr.version, LINUX_CAPABILITY_VERSION_2,
                "a known version was rewritten"
            );

            // And `capset` plays the same protocol, except that its probe
            // is an error too: there is no data to answer 0 about.
            let mut hdr = a_header(0, 0);
            let data = sets(0, 0, 0);
            assert_eq!(
                capset(sys, &mut hdr, &data),
                Err(LxError::EINVAL),
                "capset accepted an unknown version"
            );
            assert_eq!(
                hdr.version, LINUX_CAPABILITY_VERSION_3,
                "capset did not write the version back"
            );
        })
    });
}

#[test]
fn the_first_version_transfers_one_element_and_both_calls_ignore_the_second() {
    let root = a_process(47_223, "capget-v1");
    on_a_thread(&root, move |sys| {
        Box::pin(async move {
            let mut hdr = a_header(LINUX_CAPABILITY_VERSION_1, 0);
            let mut data = seeded_data();
            assert_eq!(capget(sys, &mut hdr, &mut data), Ok(0), "v1 was refused");
            assert_eq!(
                data[0].effective, EVERY_CAPABILITY as u32,
                "v1 did not fill its one element"
            );
            assert_eq!(
                data[1].effective, A_SEED,
                "v1 wrote a second element the caller never provided"
            );

            // The mirror in `capset`: a word past the one element version 1
            // transfers is not part of the request. Read it and this call
            // would be asking for capabilities nobody holds -- `EPERM`.
            let mut hdr = a_header(LINUX_CAPABILITY_VERSION_1, 0);
            let mut data = sets(0, 0, 0);
            data[1].permitted = u32::MAX;
            assert_eq!(
                capset(sys, &mut hdr, &data),
                Ok(0),
                "capset read a second element version 1 does not send"
            );
        })
    });
}

#[test]
fn a_capget_pid_that_names_nobody_is_esrch_and_a_negative_one_never_gets_that_far() {
    let root = a_process(47_224, "capget-pid");
    on_a_thread(&root, move |sys| {
        Box::pin(async move {
            let mut hdr = a_header(LINUX_CAPABILITY_VERSION_3, A_PID_OF_NOBODY);
            let mut data = seeded_data();
            assert_eq!(
                capget(sys, &mut hdr, &mut data),
                Err(LxError::ESRCH),
                "a pid of nobody was answered"
            );

            // `pid < 0` is `EINVAL`, and it is a different error from the
            // one above: userspace can tell "no such task" from "that is
            // not a task id".
            let mut hdr = a_header(LINUX_CAPABILITY_VERSION_3, -1);
            let mut data = seeded_data();
            assert_eq!(
                capget(sys, &mut hdr, &mut data),
                Err(LxError::EINVAL),
                "a negative pid was looked up"
            );

            // But the null-data probe is tested FIRST, so it works with a
            // pid that would otherwise be refused. libcap probes with a
            // zeroed header, and a kernel that checked the pid first would
            // still answer; one that checked a NEGATIVE pid first would
            // not.
            let mut hdr = a_header(0, -1);
            assert_eq!(
                capget_probe(sys, &mut hdr),
                Ok(0),
                "the probe was refused for its pid"
            );
            assert_eq!(
                hdr.version, LINUX_CAPABILITY_VERSION_3,
                "the probe did not learn the version"
            );
            // And the pid it sent is still there: the write-back replaces
            // the version and nothing else, so a probe made with a pid in
            // the header does not have it taken away.
            assert_eq!(hdr.pid, -1, "the write-back cleared the pid");

            // Its own process id names itself, exactly like 0 does.
            let mine = sys.zircon_process().id() as i32;
            let mut hdr = a_header(LINUX_CAPABILITY_VERSION_3, mine);
            let mut data = seeded_data();
            assert_eq!(capget(sys, &mut hdr, &mut data), Ok(0), "capget of self");
            assert_eq!(
                data[0].effective, EVERY_CAPABILITY as u32,
                "capget by its own pid answered somebody else"
            );
        })
    });
}

#[test]
fn capset_may_only_name_the_caller_and_any_other_pid_is_eperm() {
    let root = a_process(47_225, "capset-pid");
    let (them, _thread) = a_process_with_a_thread(47_226, "capset-other", A_USER);
    let their_pid = them.id() as i32;
    on_a_thread(&root, move |sys| {
        Box::pin(async move {
            let data = sets(0, 0, 0);

            let mut hdr = a_header(LINUX_CAPABILITY_VERSION_3, 0);
            assert_eq!(capset(sys, &mut hdr, &data), Ok(0), "capset of 0 refused");

            let mine = sys.zircon_process().id() as i32;
            let mut hdr = a_header(LINUX_CAPABILITY_VERSION_3, mine);
            assert_eq!(
                capset(sys, &mut hdr, &data),
                Ok(0),
                "capset of self refused"
            );

            // Another process, which exists: still `EPERM`. `capset` has
            // never been able to change anybody else's sets, and answering
            // 0 told `setcap`-style tooling that it had.
            let mut hdr = a_header(LINUX_CAPABILITY_VERSION_3, their_pid);
            assert_eq!(
                capset(sys, &mut hdr, &data),
                Err(LxError::EPERM),
                "capset of another process was accepted"
            );

            // And a negative pid is `EPERM` as well, not the `EINVAL`
            // `capget` gives it: -1 is just another process that is not
            // the caller.
            let mut hdr = a_header(LINUX_CAPABILITY_VERSION_3, -1);
            assert_eq!(
                capset(sys, &mut hdr, &data),
                Err(LxError::EPERM),
                "capset of a negative pid"
            );
        })
    });
}

#[test]
fn capset_lets_a_set_shrink_and_refuses_one_that_grows() {
    let root = a_process(47_227, "capset-root");
    let them = a_process_of(47_228, "capset-user", A_USER);
    on_a_thread(&root, move |sys| {
        Box::pin(async move {
            let mut hdr = a_header(LINUX_CAPABILITY_VERSION_3, 0);

            // Root holds everything, so it may ask for everything.
            assert_eq!(
                capset(sys, &mut hdr, &sets(EVERY_CAPABILITY, EVERY_CAPABILITY, 0)),
                Ok(0),
                "root may not keep what it holds"
            );

            // And while it holds them it may make the lot inheritable: its
            // bounding set is every capability there is, which is what
            // `PR_CAPBSET_READ` reports. A kernel that passed an empty
            // bounding set in here would refuse this. It has to come
            // before the drop: the sets are kept, so a root that has given
            // everything away no longer holds what it would be passing on,
            // and Linux refuses it then too.
            assert_eq!(
                capset(
                    sys,
                    &mut hdr,
                    &sets(EVERY_CAPABILITY, EVERY_CAPABILITY, EVERY_CAPABILITY)
                ),
                Ok(0),
                "root may not make what it holds inheritable"
            );
            assert_eq!(
                capset(sys, &mut hdr, &sets(0, 0, 0)),
                Ok(0),
                "root may not drop what it holds"
            );

            // What is refused even of root: an effective set wider than
            // the permitted set it is being made effective out of.
            assert_eq!(
                capset(sys, &mut hdr, &sets(EVERY_CAPABILITY, 0, 0)),
                Err(LxError::EPERM),
                "an effective set outside the permitted one was accepted"
            );

            // Giving the lot away is what a daemon dropping privilege
            // does, and it is a door that only opens one way: these sets
            // are what the process KEEPS, so each call is judged against
            // what the one before it left behind, and once the permitted
            // set is empty not even root asks it back.
            assert_eq!(
                capset(sys, &mut hdr, &sets(0, 0, 0)),
                Ok(0),
                "root may not drop what it holds"
            );
            assert_eq!(
                capset(sys, &mut hdr, &sets(0, EVERY_CAPABILITY, 0)),
                Err(LxError::EPERM),
                "a permitted set came back after it had been dropped"
            );
        })
    });
    on_a_thread(&them, move |sys| {
        Box::pin(async move {
            let mut hdr = a_header(LINUX_CAPABILITY_VERSION_3, 0);

            // Somebody who holds nothing may still say so.
            assert_eq!(
                capset(sys, &mut hdr, &sets(0, 0, 0)),
                Ok(0),
                "an empty request from an ordinary user"
            );

            // What they may not do is grant themselves one bit. Nothing
            // checked this, so any process could capset itself every
            // capability and be told it had them.
            assert_eq!(
                capset(sys, &mut hdr, &sets(0, 1, 0)),
                Err(LxError::EPERM),
                "an ordinary user granted itself a capability"
            );
            assert_eq!(
                capset(sys, &mut hdr, &sets(0, 0, 1)),
                Err(LxError::EPERM),
                "an ordinary user granted itself an inheritable capability"
            );
        })
    });
}

#[test]
fn the_io_priority_set_on_a_task_is_the_one_read_back_from_it() {
    let root = a_process(47_229, "ioprio-roundtrip");
    let (them, thread) = a_process_with_a_thread(47_230, "ioprio-other", A_USER);
    let their_tid = thread.id() as usize;
    let _held = them;
    on_a_thread(&root, move |sys| {
        Box::pin(async move {
            let mine = ioprio_value(IOPRIO_CLASS_RT, 3);
            assert_eq!(
                ioprio_set(sys, WHO_PROCESS, 0, mine),
                Ok(0),
                "ioprio_set on myself"
            );
            assert_eq!(
                ioprio_get(sys, WHO_PROCESS, 0),
                Ok(mine as usize),
                "the word set on myself did not come back"
            );

            // `who == 0` is mine and nobody else's: the other task still
            // reports that it never set one.
            assert_eq!(
                ioprio_get(sys, WHO_PROCESS, their_tid),
                Ok(0),
                "setting mine moved somebody else's"
            );

            // Then the same round trip aimed at that task by its id.
            let theirs = ioprio_value(IOPRIO_CLASS_IDLE, 0);
            assert_eq!(
                ioprio_set(sys, WHO_PROCESS, their_tid, theirs),
                Ok(0),
                "ioprio_set on another task"
            );
            assert_eq!(
                ioprio_get(sys, WHO_PROCESS, their_tid),
                Ok(theirs as usize),
                "the word set on another task did not come back"
            );
            assert_eq!(
                ioprio_get(sys, WHO_PROCESS, 0),
                Ok(mine as usize),
                "setting theirs moved mine"
            );
        })
    });
}

/// One task reports the word it set, so a task that never set one is
/// `IOPRIO_CLASS_NONE`, 0, whatever its nice (`get_task_raw_ioprio`); a
/// group counts it at the level that nice is served at. This reported the
/// derived level for one task too. Linux 6.18, measured: nice 7 and nothing
/// set is 0 for the process and best-effort 5 for its group.
#[test]
fn a_task_that_never_set_one_reports_none_and_its_group_the_level_it_is_served_at() {
    let root = a_process(47_231, "ioprio-derived");
    let (them, thread) = a_process_with_a_thread(47_232, "ioprio-fresh", A_USER);
    thread.set_sched(thread.sched_policy(), 7, 0);
    let their_tid = thread.id() as usize;
    let their_group = them.id() as usize;
    let _held = them;
    on_a_thread(&root, move |sys| {
        Box::pin(async move {
            assert_eq!(
                ioprio_get(sys, WHO_PROCESS, their_tid),
                Ok(0),
                "a fresh task's word is none"
            );
            // Nice 7 is best-effort level 5, `(7 + 20) / 5`: a task that
            // never asked still has a priority the disk serves it at, and
            // its group's fold has to compare that.
            assert_eq!(
                ioprio_get(sys, WHO_PGRP, their_group),
                Ok(ioprio_value(IOPRIO_CLASS_BE, 5) as usize),
                "a fresh task's group"
            );
        })
    });
}

#[test]
fn real_time_is_refused_without_the_capability_before_any_task_is_looked_up() {
    let them = a_process_of(47_233, "ioprio-stranger", A_USER);
    on_a_thread(&them, move |sys| {
        Box::pin(async move {
            let rt = ioprio_value(IOPRIO_CLASS_RT, 0);
            let be = ioprio_value(IOPRIO_CLASS_BE, 0);
            let nobody = A_PID_OF_NOBODY as usize;

            // The word is judged first, so this is `EPERM` and not the
            // `ESRCH` the pid has coming. A program that asks whether it
            // may have real-time I/O gets an answer about permission.
            assert_eq!(
                ioprio_set(sys, WHO_PROCESS, nobody, rt),
                Err(LxError::EPERM),
                "real time without the capability"
            );

            // The same call with a class anybody may ask for gets as far
            // as the lookup, which is how we know the order above is the
            // order and not the pid being accepted.
            assert_eq!(
                ioprio_set(sys, WHO_PROCESS, nobody, be),
                Err(LxError::ESRCH),
                "a pid of nobody"
            );

            // `which` is judged after the word for the same reason.
            assert_eq!(
                ioprio_set(sys, WHO_NOTHING, 0, rt),
                Err(LxError::EPERM),
                "the word is judged after which"
            );
            assert_eq!(
                ioprio_set(sys, WHO_NOTHING, 0, be),
                Err(LxError::EINVAL),
                "a which that is none of the three"
            );

            // `ioprio_get` has no word to judge, so for it `which` comes
            // first and a pid of nobody never gets looked at.
            assert_eq!(
                ioprio_get(sys, WHO_NOTHING, nobody),
                Err(LxError::EINVAL),
                "ioprio_get accepted a which of its own"
            );
            assert_eq!(
                ioprio_get(sys, WHO_PROCESS, nobody),
                Err(LxError::ESRCH),
                "ioprio_get of a pid of nobody"
            );

            // And a level past the eight a class has is refused whoever
            // asks, which is the other half of `ioprio_check_cap`.
            assert_eq!(
                ioprio_set(sys, WHO_PROCESS, 0, ioprio_value(IOPRIO_CLASS_BE, 8)),
                Err(LxError::EINVAL),
                "a level past the last one"
            );
        })
    });
}

#[test]
fn the_best_priority_across_a_users_tasks_is_the_one_reported() {
    let root = a_process(47_234, "ioprio-fold");
    let (first, one) = a_process_with_a_thread(47_235, "ioprio-fold-a", A_LONE_USER);
    let (second, two) = a_process_with_a_thread(47_236, "ioprio-fold-b", A_LONE_USER);
    let (one_tid, two_tid) = (one.id() as usize, two.id() as usize);
    let _held = (first, second);
    on_a_thread(&root, move |sys| {
        Box::pin(async move {
            let worse = ioprio_value(IOPRIO_CLASS_BE, 6);
            let better = ioprio_value(IOPRIO_CLASS_RT, 2);
            assert_eq!(ioprio_set(sys, WHO_PROCESS, one_tid, worse), Ok(0), "set a");
            assert_eq!(
                ioprio_set(sys, WHO_PROCESS, two_tid, better),
                Ok(0),
                "set b"
            );

            // Of a user's tasks, `ioprio_get` reports the best one, not the
            // last one it walked past and not the caller's.
            assert_eq!(
                ioprio_get(sys, WHO_USER, A_LONE_USER as usize),
                Ok(better as usize),
                "the fold did not pick the best of the two"
            );

            // Order should not matter: make the other one the best and ask
            // again.
            let best_yet = ioprio_value(IOPRIO_CLASS_RT, 1);
            assert_eq!(
                ioprio_set(sys, WHO_PROCESS, one_tid, best_yet),
                Ok(0),
                "set a again"
            );
            assert_eq!(
                ioprio_get(sys, WHO_USER, A_LONE_USER as usize),
                Ok(best_yet as usize),
                "the fold depends on the order it walked"
            );

            // A user nobody runs as has no tasks, which is `ESRCH` rather
            // than a default word.
            assert_eq!(
                ioprio_get(sys, WHO_USER, ANOTHER_USER as usize + 40_000),
                Err(LxError::ESRCH),
                "a user with no tasks"
            );
        })
    });
}

#[test]
fn either_of_the_callers_two_user_ids_may_set_a_task_and_a_strangers_is_eperm() {
    let caller = a_process(47_237, "ioprio-creds");
    // The caller's REAL uid and its EFFECTIVE uid differ, because
    // `may_set_ioprio_of` compares the target's real uid against both and
    // a caller whose ids are equal cannot see the two swapped.
    caller
        .linux()
        .set_resuid(A_USER, A_THIRD_USER, A_THIRD_USER)
        .unwrap();
    let creds = caller.linux().credentials();
    assert_ne!(
        creds.ruid, creds.euid,
        "the caller's two user ids must differ for this test to see anything"
    );

    let (by_real, real_thread) = a_process_with_a_thread(47_238, "ioprio-by-real", A_USER);
    let (by_effective, eff_thread) = a_process_with_a_thread(47_240, "ioprio-by-eff", A_THIRD_USER);
    let (stranger, strange_thread) =
        a_process_with_a_thread(47_241, "ioprio-stranger", ANOTHER_USER);
    let real_tid = real_thread.id() as usize;
    let eff_tid = eff_thread.id() as usize;
    let strange_tid = strange_thread.id() as usize;
    let _held = (by_real, by_effective, stranger);
    on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let word = ioprio_value(IOPRIO_CLASS_IDLE, 0);

            // A task whose real uid is the caller's REAL uid.
            assert_eq!(
                ioprio_set(sys, WHO_PROCESS, real_tid, word),
                Ok(0),
                "a task of the caller's real user was refused"
            );
            // And one whose real uid is the caller's EFFECTIVE uid. Drop
            // either comparison and one of these two becomes `EPERM`.
            assert_eq!(
                ioprio_set(sys, WHO_PROCESS, eff_tid, word),
                Ok(0),
                "a task of the caller's effective user was refused"
            );
            // Somebody who shares neither id, and the caller holds no
            // capability to go over the top with.
            assert_eq!(
                ioprio_set(sys, WHO_PROCESS, strange_tid, word),
                Err(LxError::EPERM),
                "a stranger's task was accepted"
            );

            // Reading is not writing: `ioprio_get` asks nobody's
            // permission, so the stranger's priority is readable.
            assert_eq!(
                ioprio_get(sys, WHO_PROCESS, strange_tid),
                Ok(0),
                "ioprio_get asked for permission"
            );
        })
    });
}

#[test]
fn the_two_io_priority_calls_answer_from_their_own_numbers_with_their_arguments_in_order() {
    let root = a_process(47_242, "ioprio-by-number");
    on_a_thread(&root, move |sys| {
        Box::pin(async move {
            let word = ioprio_value(IOPRIO_CLASS_BE, 7) as usize;

            // Three arguments, in the order `which`, `who`, `ioprio`. Swap
            // `which` and `who` and `ioprio_which(0)` refuses the call;
            // swap `who` and `ioprio` and the word names a task.
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::IOPRIO_SET as u32,
                    [WHO_PROCESS, 0, word, 0, 0, 0]
                )
                .await,
                0,
                "ioprio_set by its own number"
            );
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::IOPRIO_GET as u32,
                    [WHO_PROCESS, 0, 0, 0, 0, 0]
                )
                .await,
                word as isize,
                "ioprio_get by its own number did not answer what was set"
            );

            // The same two numbers with the arguments the other way round,
            // which is the shape a mutant leaves behind.
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::IOPRIO_GET as u32,
                    [0, WHO_PROCESS, 0, 0, 0, 0]
                )
                .await,
                -(LxError::EINVAL as isize),
                "ioprio_get read its arguments the other way round"
            );
        })
    });
}

#[test]
fn a_process_without_a_thread_is_esrch_and_not_a_default_priority() {
    let root = a_process(47_243, "ioprio-threadless");
    // A process with no thread at all: `priority_targets` walks `threads()`
    // and finds nothing, which is the same `ESRCH` a pid of nobody gets.
    let empty = a_process_of(47_244, "ioprio-empty", A_LONE_USER + 1);
    let empty_pid = empty.id() as usize;
    let _held = empty;
    on_a_thread(&root, move |sys| {
        Box::pin(async move {
            assert_eq!(
                ioprio_get(sys, WHO_PROCESS, empty_pid),
                Err(LxError::ESRCH),
                "a process id with no thread of that id"
            );
            assert_eq!(
                ioprio_set(
                    sys,
                    WHO_PROCESS,
                    empty_pid,
                    ioprio_value(IOPRIO_CLASS_IDLE, 0)
                ),
                Err(LxError::ESRCH),
                "ioprio_set on a process id with no thread of that id"
            );

            // The caller itself always has one: its own harness thread.
            // Without this the test above would pass on a kernel that
            // answered `ESRCH` to everything.
            assert!(
                ioprio_get(sys, WHO_PROCESS, 0).is_ok(),
                "the caller's own task was not found"
            );
        })
    });
}
