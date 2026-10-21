//! The twenty credential syscalls, asked through the syscall.
//!
//! `set_id`, `groups_size` and `groups_list` are the pure arguments gates
//! and have `id_arg_tests` in `intarg.rs`; the policy itself --
//! `set_resuid` and friends -- lives in `linux-object` with tests of its
//! own. What nothing could see from either side is the wiring, and this is
//! the family where a wiring slip is a privilege change rather than a wrong
//! errno.
//!
//! Three of the things asked here are cases where two calls read the same
//! register two different ways, which is the shape that survives review:
//!
//! * `setuid(-1)` is `EINVAL`, because `(uid_t)-1` is not a user. The same
//!   `-1` through `setreuid` means **leave this one alone**, which is how
//!   every setuid program drops and regains privilege. Route them both
//!   through one gate and `setreuid(-1, 0)` stops working.
//! * `getgroups` reads its `gidsetsize` signed and refuses a negative one;
//!   `setgroups` reads the same register unsigned and caps it.
//! * `setfsuid` **returns the id it replaced and never an error**, so a
//!   refusal shows only as a value that did not move. Hand back the new id
//!   instead and `old = setfsuid(new); ...; setfsuid(old);` leaves the
//!   process holding the wrong filesystem identity for good.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// `(uid_t)-1` as userspace writes it, whatever the register width.
const A_MINUS_ONE: usize = -1_i64 as usize;

/// Somebody who is not root, and a second one with no claim to the first.
const A_USER: usize = 1000;
const ANOTHER_USER: usize = 1001;
const SOMEONE_ELSE: usize = 1002;

/// Six ids with no digit in common, so a swap between any two of them
/// cannot come back looking right.
const A_REAL: usize = 1101;
const AN_EFFECTIVE: usize = 1202;
const A_SAVED: usize = 1303;
const A_REAL_GROUP: usize = 2101;
const AN_EFFECTIVE_GROUP: usize = 2202;
const A_SAVED_GROUP: usize = 2303;

/// A group id for the supplementary set.
const A_GROUP: usize = 2000;

/// A process of its own, so that moving its credentials cannot reach any
/// other test in this binary.
fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

/// The six ids, asked of the process and not of the syscalls under test.
fn ids_of(lp: &LinuxProcess) -> (u32, u32, u32, u32, u32, u32) {
    let creds = lp.credentials();
    (
        creds.ruid, creds.euid, creds.suid, creds.rgid, creds.egid, creds.sgid,
    )
}

/// An out pointer for one `u32`. Built per call: `UserOutPtr` is not `Copy`.
fn out(slot: &mut u32) -> UserOutPtr<u32> {
    UserOutPtr::from(slot as *mut u32 as usize)
}

/// An in pointer for a list of group ids, and an out one for a buffer.
fn list(groups: &[u32]) -> UserInPtr<u32> {
    UserInPtr::from(groups.as_ptr() as usize)
}

fn buffer(slots: &mut [u32]) -> UserOutPtr<u32> {
    UserOutPtr::from(slots.as_mut_ptr() as usize)
}

#[test]
fn minus_one_is_not_an_id_for_setuid_but_setreuid_reads_it_as_leave_this_one_alone() {
    // The whole process here is root, so nothing below is refused for
    // want of privilege: the only thing that can account for the two
    // answers is which call was asked.
    let proc = a_process(47_000, "cred-minus-one");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();

            assert_eq!(
                sys.sys_setuid(A_MINUS_ONE),
                Err(LxError::EINVAL),
                "setuid((uid_t)-1) was accepted"
            );
            assert_eq!(
                sys.sys_setgid(A_MINUS_ONE),
                Err(LxError::EINVAL),
                "setgid((gid_t)-1) was accepted"
            );
            assert_eq!(
                ids_of(&lp),
                (0, 0, 0, 0, 0, 0),
                "a refused set*id moved an id anyway"
            );

            // The same register through the two-argument calls, where it
            // is not a mistake but an instruction.
            assert_eq!(
                sys.sys_setregid(A_MINUS_ONE, A_REAL_GROUP),
                Ok(0),
                "setregid(-1, gid) was refused"
            );
            assert_eq!(
                sys.sys_getgid(),
                Ok(0),
                "setregid(-1, ..) changed the real gid"
            );
            assert_eq!(sys.sys_getegid(), Ok(A_REAL_GROUP));

            assert_eq!(
                sys.sys_setreuid(A_MINUS_ONE, A_USER),
                Ok(0),
                "setreuid(-1, uid) was refused"
            );
            assert_eq!(
                sys.sys_getuid(),
                Ok(0),
                "setreuid(-1, ..) changed the real uid"
            );
            assert_eq!(sys.sys_geteuid(), Ok(A_USER));
        })
    });
}

#[test]
fn each_of_the_six_ids_goes_to_its_own_field_and_comes_back_from_it() {
    // Six different numbers, set through two calls and read back through
    // four, so a pair swapped in either direction shows. The group half
    // goes first on purpose: `setresuid` takes the privilege away, and a
    // `setresgid` after it would be refused for a reason this test is not
    // about.
    let proc = a_process(47_001, "cred-fields");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            assert_eq!(
                sys.sys_setresgid(A_REAL_GROUP, AN_EFFECTIVE_GROUP, A_SAVED_GROUP),
                Ok(0)
            );
            assert_eq!(sys.sys_setresuid(A_REAL, AN_EFFECTIVE, A_SAVED), Ok(0));

            let (mut r, mut e, mut s) = (0u32, 0u32, 0u32);
            assert_eq!(
                sys.sys_getresuid(out(&mut r), out(&mut e), out(&mut s)),
                Ok(0)
            );
            assert_eq!(
                (r, e, s),
                (A_REAL as u32, AN_EFFECTIVE as u32, A_SAVED as u32),
                "the three user ids did not come back one per pointer"
            );

            let (mut rg, mut eg, mut sg) = (0u32, 0u32, 0u32);
            assert_eq!(
                sys.sys_getresgid(out(&mut rg), out(&mut eg), out(&mut sg)),
                Ok(0)
            );
            assert_eq!(
                (rg, eg, sg),
                (
                    A_REAL_GROUP as u32,
                    AN_EFFECTIVE_GROUP as u32,
                    A_SAVED_GROUP as u32
                ),
                "the three group ids did not come back one per pointer"
            );

            // And the four one-number getters, which are the only way most
            // programs ever ask.
            assert_eq!(sys.sys_getuid(), Ok(A_REAL), "getuid is not the real uid");
            assert_eq!(
                sys.sys_geteuid(),
                Ok(AN_EFFECTIVE),
                "geteuid is not the effective uid"
            );
            assert_eq!(sys.sys_getgid(), Ok(A_REAL_GROUP));
            assert_eq!(sys.sys_getegid(), Ok(AN_EFFECTIVE_GROUP));
        })
    });
}

#[test]
fn setfsuid_returns_the_id_it_replaced_and_never_an_error() {
    // `setfsuid(2)` has no way to report a refusal: it returns the old id
    // whatever happens, which is why the idiom is to set it, do the work
    // and set it back. The refusal is asked for at the end, from a caller
    // with no claim to the id, and it has to look exactly like a success
    // that moved nothing.
    let proc = a_process(47_002, "cred-fsuid");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();

            assert_eq!(
                sys.sys_setfsuid(A_USER),
                Ok(0),
                "setfsuid returned the id it was given instead of the one it replaced"
            );
            assert_eq!(
                sys.sys_setfsuid(ANOTHER_USER),
                Ok(A_USER),
                "setfsuid did not report the id in force before it"
            );
            assert_eq!(lp.credentials().fsuid, ANOTHER_USER as u32);
            assert_eq!(
                sys.sys_setfsgid(A_GROUP),
                Ok(0),
                "setfsgid returned the id it was given"
            );
            assert_eq!(lp.credentials().fsgid, A_GROUP as u32);

            // `(uid_t)-1` is the pure query: it reports and changes
            // nothing, which is how a program reads its own fsuid.
            assert_eq!(sys.sys_setfsuid(A_MINUS_ONE), Ok(ANOTHER_USER));
            assert_eq!(
                lp.credentials().fsuid,
                ANOTHER_USER as u32,
                "setfsuid(-1) moved the fsuid"
            );

            // Every set*id drags the fsuid along, so after this the fsuid
            // is A_USER and the caller is no longer privileged.
            lp.set_resuid(A_USER as u32, A_USER as u32, A_USER as u32)
                .unwrap();
            assert_eq!(lp.credentials().fsuid, A_USER as u32);

            assert_eq!(
                sys.sys_setfsuid(SOMEONE_ELSE),
                Ok(A_USER),
                "a refused setfsuid reported an error instead of the old id"
            );
            assert_eq!(
                lp.credentials().fsuid,
                A_USER as u32,
                "a refused setfsuid moved the fsuid anyway"
            );
        })
    });
}

#[test]
fn getgroups_counts_them_with_a_size_of_zero_and_refuses_a_buffer_too_small() {
    // The `gidsetsize == 0` form is the one every caller uses first, to
    // find out how much to allocate, and it must not touch the pointer at
    // all -- which a null one proves, because a write through it would be
    // EFAULT and not a count.
    let proc = a_process(47_003, "cred-getgroups");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let three = [A_GROUP as u32, 7, 9];
            assert_eq!(sys.sys_setgroups(3, list(&three)), Ok(0));

            assert_eq!(
                sys.sys_getgroups(0, UserOutPtr::from(0)),
                Ok(3),
                "a size of zero did not come back as the count"
            );

            let mut buf = [0u32; 5];
            assert_eq!(
                sys.sys_getgroups(2, buffer(&mut buf)),
                Err(LxError::EINVAL),
                "a buffer smaller than the set was accepted"
            );
            assert_eq!(
                buf, [0; 5],
                "a refused getgroups wrote into the buffer anyway"
            );

            assert_eq!(sys.sys_getgroups(5, buffer(&mut buf)), Ok(3));
            assert_eq!(
                buf,
                [A_GROUP as u32, 7, 9, 0, 0],
                "getgroups wrote the wrong entries, or more of them than it said"
            );

            // The signed reading of the same register: negative is EINVAL.
            assert_eq!(
                sys.sys_getgroups(A_MINUS_ONE, buffer(&mut buf)),
                Err(LxError::EINVAL),
                "a negative gidsetsize was read as an enormous unsigned one"
            );
        })
    });
}

#[test]
fn setgroups_asks_for_the_capability_before_it_reads_the_list() {
    // Both orders matter here. The capability comes first, so an
    // unprivileged caller is told about its privilege and not about its
    // pointer -- otherwise `setgroups` is a way to ask the kernel whether
    // an address is readable. And the list is validated before anything
    // is stored, so a `(gid_t)-1` in the middle of it leaves the old set
    // alone rather than half of a new one.
    let proc = a_process(47_004, "cred-setgroups");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();

            let good = [1u32, 2, 3];
            assert_eq!(sys.sys_setgroups(3, list(&good)), Ok(0));

            let with_a_minus_one = [1u32, u32::MAX, 3];
            assert_eq!(
                sys.sys_setgroups(3, list(&with_a_minus_one)),
                Err(LxError::EINVAL),
                "a (gid_t)-1 in the list was accepted"
            );
            assert_eq!(
                lp.groups(),
                alloc::vec![1, 2, 3],
                "a refused setgroups changed the set anyway"
            );

            // Now without CAP_SETGID, and with a pointer that could not be
            // read even if it were asked for.
            lp.set_resuid(A_USER as u32, A_USER as u32, A_USER as u32)
                .unwrap();
            assert_eq!(
                sys.sys_setgroups(3, UserInPtr::from(0)),
                Err(LxError::EPERM),
                "an unprivileged caller was told about its pointer instead of its privilege"
            );
            assert_eq!(
                lp.groups(),
                alloc::vec![1, 2, 3],
                "a refused setgroups changed the set anyway"
            );
        })
    });
}

#[test]
fn umask_returns_the_mask_it_replaced() {
    // The same shape as `setfsuid`: the answer is the old value, and
    // there is no other way to read it. A kernel that returns the new one
    // breaks every program that saves the mask to put it back.
    let proc = a_process(47_005, "cred-umask");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let before = lp.credentials().umask;

            assert_eq!(
                sys.sys_umask(0o027),
                Ok(before as usize),
                "umask did not return the mask it replaced"
            );
            assert_eq!(lp.credentials().umask, 0o027, "umask did not take effect");

            assert_eq!(
                sys.sys_umask(0o077),
                Ok(0o027),
                "umask returned the new mask instead of the old one"
            );
            assert_eq!(lp.credentials().umask, 0o077);
        })
    });
}
