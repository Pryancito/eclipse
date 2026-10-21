//! System V shared memory through the syscall: a segment made in one process
//! and read in another, where an attach lands, and what a detach leaves
//! behind.
//!
//! These four are the last of `ipc.rs`'s three families. `shmget` and
//! `shmat` were named by exactly one test before this file -- the address
//! space's `RLIMIT_AS` test, which uses them as a fixture to get two pages
//! mapped -- and `shmdt` and `shmctl` by none. Everything covered next door
//! is pure: `shmat_flags_and_place`, `shmctl_subject`, `rounded_attach_addr`
//! and `may_access` all have their own tests, and not one of them can see
//! whether the syscall asks, in what order, or what it does with the answer.
//!
//! What this file is for:
//!
//! **That it really is shared memory.** An id means the same segment in
//! every process, and a byte written through one process's attach is there
//! when another process reads it through its own, at its own address. That
//! is the whole mechanism, and the X11 shared-memory extension is nothing
//! but this: the client calls `shmget(IPC_PRIVATE, ..)`, hands the id to the
//! X server over its socket, and the server calls `shmat` on it.
//!
//! **Order.** `shmat` judges its flags and its address BEFORE it looks the
//! id up and before it asks about permissions, so `SHM_REMAP` with a null
//! address is `EINVAL` on a segment the caller owns, a non-aligned address
//! without `SHM_RND` is `EINVAL` on a segment it may not even read, and
//! `shmat(id, 1, SHM_RND)` rounds down to zero and comes back `EPERM` from
//! the floor under `mmap`, which is a different errno from either.
//!
//! **Which permission an attach needs**, which is `SHM_RDONLY`'s whole job:
//! a read-only attach asks for `IPC_R` and a writable one for `IPC_R |
//! IPC_W`, so a `0444` segment takes the first and refuses the second.
//!
//! **That the address is `shmdt`'s only argument.** It is
//! `SYSCALL_DEFINE1(shmdt, char __user *, shmaddr)`, and this handler was
//! once declared after `shmat`'s shape, so the dispatch fed the address into
//! the second register and nothing read the first: every `shmdt` from a real
//! program detached nothing. Both registers are called here, by number.
//!
//! **That a detach actually unmaps.** It used to drop the tracking entry and
//! leave the segment mapped, so the address stayed taken; here the same
//! address is attached again afterwards, which only a free VMAR slot allows.
//!
//! **The key and the id come apart at `IPC_RMID`.** While an attach remains,
//! the id still works and the key does not -- which is what lets Mesa mark
//! every software frame's segment destroyed before the X server attaches it
//! -- and the last detach is what makes the id go away.
//!
//! An attach is only dereferenceable where the host already keeps a page, so
//! the tests that read or write memory attach at a leaked page of this
//! binary's own and never detach it; the ones that detach attach wherever
//! the kernel chooses and never look at the bytes.
//!
//! Fixed pids live in 47_34x and 47_35x, keys in 47_340_0xx.

extern crate std;

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// `IPC_CREAT` and the mode a segment is made with when the test is not
/// about modes.
const CREAT: usize = 0o1000;
const EVERYBODY: usize = 0o666;
/// A segment anybody may read and nobody may write.
const READ_ONLY: usize = 0o444;
/// A segment with no mode bits at all: its owner may control it, and a
/// stranger may do nothing with it.
const NOBODY: usize = 0;

/// An id the registry can never answer: ids are handed out counting up
/// from one and never reused.
const NO_SUCH_SEGMENT: usize = usize::MAX;
/// A `shmctl` command no `<linux/shm.h>` defines.
const A_COMMAND_OF_NOBODY: usize = 999;

/// Keys of this file's own, one segment each.
const KEY: usize = 47_340_000;

/// What the tests that read memory write into it.
const A_PATTERN: [u8; 8] = [0x5a; 8];
const ANOTHER_PATTERN: [u8; 8] = [0xa5; 8];

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// What this process acts as from here on. The group goes first: dropping
/// the effective uid takes away the privilege `set_resgid` needs.
fn acting_as(sys: &Syscall<'_>, euid: u32, egid: u32, groups: &[u32]) {
    let lp = sys.linux_process();
    lp.set_groups(Vec::from(groups));
    lp.set_resgid(egid, egid, egid).unwrap();
    lp.set_resuid(euid, euid, euid).unwrap();
}

/// A page of this binary's own for a shared mapping to land on.
///
/// The hosted page table maps the frame with `MAP_FIXED` at the user
/// address, so an attach is only dereferenceable at a page-aligned region
/// this process already owns. Leaked, and never touched through this type
/// again.
#[repr(align(4096))]
struct APage([u8; 4096]);

impl APage {
    fn at(&self) -> usize {
        self.0.as_ptr() as usize
    }
}

/// An address an attach can be dereferenced at, one page of its own each
/// time it is called.
fn a_page_of_our_own() -> usize {
    Box::leak(Box::new(APage([0; 4096]))).at()
}

/// Keep `proc`'s address space alive for the rest of the run: a segment
/// mapped over a page of this binary's heap must never be unmapped, or that
/// address is left with nothing behind it.
fn never_torn_down(proc: &Arc<Process>) {
    core::mem::forget(proc.clone());
}

fn shmget(sys: &Syscall<'_>, key: usize, size: usize, shmflg: usize) -> SysResult {
    sys.sys_shmget(key, size, shmflg)
}

fn shmat(sys: &Syscall<'_>, id: usize, addr: usize, shmflg: usize) -> SysResult {
    sys.sys_shmat(id, addr, shmflg)
}

fn shmdt(sys: &Syscall<'_>, addr: usize) -> SysResult {
    sys.sys_shmdt(addr)
}

fn shmctl(sys: &Syscall<'_>, id: usize, cmd: ShmctlCmds, buffer: usize) -> SysResult {
    sys.sys_shmctl(id, cmd as usize, buffer)
}

/// The same `Syscall` with a mutable borrow, which the dispatch table needs.
fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// A syscall made the way userspace makes it: by number, through the table.
async fn by_number(sys: &Syscall<'_>, which: crate::Sys, args: [usize; 6]) -> isize {
    let mut own = as_mut(sys);
    own.syscall(which as u32, args).await
}

/// A `ShmidDs` to hand to `shmctl`, taken from ANOTHER segment so that a
/// command which writes nothing leaves recognisably the wrong numbers
/// behind. The struct has private padding, so a test cannot build one.
fn a_shmid_ds_of_another_segment(id: usize) -> ShmidDs {
    let guard = linux_object::ipc::shm_lookup(id).unwrap();
    let ds = *guard.lock().shmid_ds.lock();
    ds
}

/// What the segment itself says, read without going through `shmctl`.
fn what_the_kernel_keeps(id: usize) -> ShmidDs {
    a_shmid_ds_of_another_segment(id)
}

/// Write a pattern at a user address, as userspace does.
fn write_at(addr: usize, bytes: &[u8]) {
    let mut out: UserOutPtr<u8> = addr.into();
    out.write_array(bytes).unwrap();
}

/// Read bytes back from a user address.
fn read_at(addr: usize, len: usize) -> Vec<u8> {
    let ptr: UserInPtr<u8> = addr.into();
    ptr.read_array(len).unwrap()
}

/// An id means the same segment in every process, and the bytes one writes
/// are the bytes another reads -- at a different address of its own.
///
/// This is the whole of what shared memory is, and the X11 shared-memory
/// extension is this and nothing else. The three processes here share
/// nothing but the key and the id: the second finds the segment by key, the
/// third is handed the id, and each attaches at a page of its own.
#[test]
fn a_segment_made_in_one_process_is_the_same_memory_in_another() {
    const MINE: usize = KEY + 1;

    let maker = a_process(47_340, "shm-maker");
    let (id, first) = on_a_thread(&maker, move |sys| {
        Box::pin(async move {
            let id = shmget(sys, MINE, PAGE_SIZE, CREAT | EVERYBODY).unwrap();
            let at = shmat(sys, id, a_page_of_our_own(), 0).unwrap();
            write_at(at, &A_PATTERN);
            (id, at)
        })
    });
    never_torn_down(&maker);

    // Another process, which knows only the key.
    let finder = a_process(47_341, "shm-finder");
    let second = on_a_thread(&finder, move |sys| {
        Box::pin(async move {
            let same = shmget(sys, MINE, 0, 0).unwrap();
            assert_eq!(same, id, "the key named a different segment");
            let at = shmat(sys, same, a_page_of_our_own(), 0).unwrap();
            assert_eq!(
                read_at(at, A_PATTERN.len()),
                A_PATTERN,
                "the bytes the first process wrote are not here"
            );
            // And back the other way.
            write_at(at, &ANOTHER_PATTERN);
            at
        })
    });
    never_torn_down(&finder);
    assert_ne!(
        first, second,
        "the two processes attached at the same address, so nothing was proved"
    );

    // A third, handed the id and nothing else, which is how the X server
    // gets it.
    let told = a_process(47_342, "shm-told-the-id");
    on_a_thread(&told, move |sys| {
        Box::pin(async move {
            let at = shmat(sys, id, a_page_of_our_own(), 0).unwrap();
            assert_ne!(at, first, "a third attach landed on the first's address");
            assert_ne!(at, second, "a third attach landed on the second's address");
            assert_eq!(
                read_at(at, ANOTHER_PATTERN.len()),
                ANOTHER_PATTERN,
                "the bytes the second process wrote are not here"
            );
        })
    });
    never_torn_down(&told);

    assert_eq!(
        what_the_kernel_keeps(id).nattch,
        3,
        "three attaches were not counted"
    );
}

/// `shmat` judges what it was told to do with the address before it looks
/// the segment up or asks whether the caller may have it.
///
/// Three different errnos say where in the call each refusal happened:
/// `EINVAL` from the placement, `EACCES` from the permission, and `EPERM`
/// from the floor that `SHM_RND` rounded the address down into.
#[test]
fn what_shmat_makes_of_its_address_is_judged_before_the_segment_is_looked_up() {
    const MINE: usize = KEY + 2;
    const NOT_FOR_US: usize = KEY + 3;
    const A_USER: u32 = 1_000;
    const A_STRANGER: u32 = 1_001;

    let owner = a_process(47_343, "shm-place-owner");
    let (mine, not_for_us) = on_a_thread(&owner, move |sys| {
        Box::pin(async move {
            acting_as(sys, A_USER, A_USER, &[]);
            (
                shmget(sys, MINE, PAGE_SIZE, CREAT | EVERYBODY).unwrap(),
                shmget(sys, NOT_FOR_US, PAGE_SIZE, CREAT | NOBODY).unwrap(),
            )
        })
    });

    let proc = a_process(47_344, "shm-place");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            acting_as(sys, A_STRANGER, A_STRANGER, &[]);

            // A segment this caller may not even read, with an address the
            // placement refuses: the answer is the placement's.
            assert_eq!(
                shmat(sys, not_for_us, PAGE_SIZE + 1, 0),
                Err(LxError::EINVAL),
                "a non-aligned address was not judged before the permission"
            );
            // The same segment with an address the placement likes: now the
            // permission answers, which is how we know it was asked at all.
            assert_eq!(
                shmat(sys, not_for_us, 0, 0),
                Err(LxError::EACCES),
                "a segment with no mode bits was attached by a stranger"
            );
            // An id that does not exist, with an address the placement
            // refuses: still the placement's answer, from before the lookup.
            assert_eq!(
                shmat(sys, NO_SUCH_SEGMENT, PAGE_SIZE + 1, 0),
                Err(LxError::EINVAL),
                "a non-aligned address was not judged before the lookup"
            );

            // SHM_REMAP has nothing to take over when the address is null,
            // and that is EINVAL even on a segment the caller may have.
            assert_eq!(
                shmat(sys, mine, 0, SHM_REMAP),
                Err(LxError::EINVAL),
                "SHM_REMAP with a null address was accepted"
            );
            // Without SHM_RND a non-aligned address is refused, rather than
            // quietly mapped somewhere else.
            assert_eq!(
                shmat(sys, mine, PAGE_SIZE + 1, 0),
                Err(LxError::EINVAL),
                "a non-aligned address was mapped anyway"
            );
            // With SHM_RND the same address is rounded DOWN, and one page
            // rounds to zero -- under the floor every `mmap` is held to,
            // which answers EPERM. A kernel that rounded up, or did not
            // round, could not give this answer.
            assert_eq!(
                shmat(sys, mine, PAGE_SIZE + 1, SHM_RND),
                Err(LxError::EPERM),
                "SHM_RND did not round down into the floor"
            );
            assert_eq!(
                shmat(sys, mine, 1, SHM_RND),
                Err(LxError::EPERM),
                "an address of one did not round down to zero"
            );
            // And an aligned address above the floor is taken.
            assert!(
                shmat(sys, mine, a_page_of_our_own(), 0).is_ok(),
                "an aligned address above the floor was refused"
            );
        })
    });
    never_torn_down(&proc);
}

/// `SHM_RDONLY` is not only how the mapping is made: it is what decides
/// which permission the attach asks the segment for.
///
/// A `0444` segment separates the two, which a `0666` one cannot: it takes
/// the read-only attach and refuses the writable one.
#[test]
fn shm_rdonly_decides_which_permission_the_attach_needs() {
    const READABLE: usize = KEY + 4;
    const OPEN_TO_ALL: usize = KEY + 5;
    const A_USER: u32 = 1_000;
    const A_STRANGER: u32 = 1_001;

    let owner = a_process(47_345, "shm-mode-owner");
    let (readable, open_to_all) = on_a_thread(&owner, move |sys| {
        Box::pin(async move {
            acting_as(sys, A_USER, A_USER, &[]);
            (
                shmget(sys, READABLE, PAGE_SIZE, CREAT | READ_ONLY).unwrap(),
                shmget(sys, OPEN_TO_ALL, PAGE_SIZE, CREAT | EVERYBODY).unwrap(),
            )
        })
    });

    let proc = a_process(47_346, "shm-mode");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            acting_as(sys, A_STRANGER, A_STRANGER, &[]);

            // Read-only segment: a writable attach wants IPC_W and there is
            // none, and the read-only attach wants only IPC_R.
            assert_eq!(
                shmat(sys, readable, 0, 0),
                Err(LxError::EACCES),
                "a read-only segment was attached writable"
            );
            assert!(
                shmat(sys, readable, a_page_of_our_own(), SHM_RDONLY).is_ok(),
                "a read-only segment refused a read-only attach"
            );

            // And a segment that grants both takes either attach, which is
            // why it cannot answer the question above.
            assert!(
                shmat(sys, open_to_all, a_page_of_our_own(), 0).is_ok(),
                "a 0666 segment refused a writable attach"
            );
            assert!(
                shmat(sys, open_to_all, a_page_of_our_own(), SHM_RDONLY).is_ok(),
                "a 0666 segment refused a read-only attach"
            );
        })
    });
    never_torn_down(&proc);
}

/// The address is `shmdt`'s ONLY argument, and it arrives in the first
/// register.
///
/// `shmdt` is `SYSCALL_DEFINE1(shmdt, char __user *, shmaddr)`. This handler
/// was once declared `(id, addr, shmflg)`, after `shmat`'s shape, and the
/// dispatch fed it `(a0, a1, a2)`: the address a one-argument call puts in
/// the first register was read as an id, and the address it detached was
/// whatever junk the second register held. Every `shmdt` from a real program
/// therefore detached nothing, and the segment stayed mapped and counted.
#[test]
fn the_address_shmdt_detaches_is_its_first_argument() {
    let proc = a_process(47_347, "shm-shmdt-shape");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            // IPC_PRIVATE: a segment of this caller's own.
            let id = shmget(sys, 0, PAGE_SIZE, CREAT | EVERYBODY).unwrap();
            let at = shmat(sys, id, 0, 0).unwrap();
            assert_eq!(what_the_kernel_keeps(id).nattch, 1);

            // The old shape: the address in the SECOND register, which is
            // what a kernel declaring `(id, addr, ..)` would read. Nothing is
            // attached at whatever the first register holds.
            assert_eq!(
                by_number(sys, crate::Sys::SHMDT, [0, at, 0, 0, 0, 0]).await,
                -(LxError::EINVAL as isize),
                "the address in the second register detached something"
            );
            assert_eq!(
                what_the_kernel_keeps(id).nattch,
                1,
                "a shmdt that answered EINVAL detached anyway"
            );

            // The real shape: the address alone, in the first register.
            assert_eq!(
                by_number(sys, crate::Sys::SHMDT, [at, 0, 0, 0, 0, 0]).await,
                0,
                "the address in the first register detached nothing"
            );
            assert_eq!(
                what_the_kernel_keeps(id).nattch,
                0,
                "the detach was not counted"
            );

            // And an address nothing is attached at is EINVAL, not a silent
            // success, which is what covered the bug above for years.
            assert_eq!(
                shmdt(sys, at),
                Err(LxError::EINVAL),
                "detaching twice at the same address was accepted"
            );
        })
    });
}

/// A detach really unmaps the segment, so the address it was at is free
/// again.
///
/// `shmdt` used to drop its tracking entry and leave the mapping in place:
/// the region stayed writable, its shared frames stayed pinned, and a later
/// attach or `MAP_FIXED` mmap at the same address collided with a mapping
/// nothing admitted to owning. Attaching at the same address afterwards is
/// the cheapest question that tells the two apart, because `shmat` maps
/// without overwrite: a stale mapping would refuse it.
#[test]
fn detaching_unmaps_the_segment_and_leaves_the_address_free() {
    let proc = a_process(47_348, "shm-unmap");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let id = shmget(sys, 0, PAGE_SIZE, CREAT | EVERYBODY).unwrap();
            let first = shmat(sys, id, 0, 0).unwrap();
            assert_eq!(shmdt(sys, first), Ok(0), "the detach failed");

            // The same address again: only a free VMAR slot allows it.
            assert_eq!(
                shmat(sys, id, first, 0),
                Ok(first),
                "the address a detach left behind was still taken"
            );
            assert_eq!(shmdt(sys, first), Ok(0), "the second detach failed");
            assert_eq!(
                what_the_kernel_keeps(id).nattch,
                0,
                "the attaches did not balance"
            );
        })
    });
}

/// One record per attachment: a segment attached twice is detached twice,
/// and each detach takes down the mapping it belongs to.
///
/// The old table kept one address per id, so the second attach overwrote the
/// first's record: the first mapping could never be detached again, and the
/// count never came back to zero.
#[test]
fn a_segment_attached_twice_is_detached_twice() {
    let proc = a_process(47_349, "shm-twice");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let id = shmget(sys, 0, PAGE_SIZE, CREAT | EVERYBODY).unwrap();
            let first = shmat(sys, id, 0, 0).unwrap();
            let second = shmat(sys, id, 0, 0).unwrap();
            assert_ne!(first, second, "the second attach landed on the first");
            assert_eq!(
                what_the_kernel_keeps(id).nattch,
                2,
                "two attaches were not counted"
            );

            assert_eq!(shmdt(sys, first), Ok(0), "the first attach was not known");
            assert_eq!(what_the_kernel_keeps(id).nattch, 1);
            assert_eq!(
                shmdt(sys, first),
                Err(LxError::EINVAL),
                "the first address was detached twice"
            );
            assert_eq!(
                shmdt(sys, second),
                Ok(0),
                "the second attach was forgotten when the first went"
            );
            assert_eq!(
                what_the_kernel_keeps(id).nattch,
                0,
                "the attaches did not balance"
            );
        })
    });
}

/// What a segment reports is who made it, who touched it last, and when:
/// the numbers `ipcs -m` prints and the only ones an attach leaves behind.
#[test]
fn what_a_segment_reports_is_who_last_attached_it_and_when() {
    const MINE: usize = KEY + 6;
    const TWO_PAGES: usize = 2 * PAGE_SIZE;

    let maker = a_process(47_350, "shm-stat-maker");
    let id = on_a_thread(&maker, move |sys| {
        Box::pin(async move { shmget(sys, MINE, TWO_PAGES, CREAT | EVERYBODY).unwrap() })
    });

    let fresh = what_the_kernel_keeps(id);
    assert_eq!(fresh.segsz, TWO_PAGES, "the size was not recorded");
    assert_eq!(fresh.cpid, 47_350, "the creator was not recorded");
    assert_eq!(fresh.nattch, 0, "a segment nobody attached counted one");
    assert_eq!(
        fresh.atime, 0,
        "a segment nobody attached has an attach time"
    );
    assert_eq!(
        fresh.dtime, 0,
        "a segment nobody detached has a detach time"
    );

    // Another process attaches, and then detaches: both stamp their own pid.
    let other = a_process(47_351, "shm-stat-other");
    on_a_thread(&other, move |sys| {
        Box::pin(async move {
            let at = shmat(sys, id, 0, 0).unwrap();
            let attached = what_the_kernel_keeps(id);
            assert_eq!(attached.nattch, 1, "the attach was not counted");
            assert_eq!(attached.lpid, 47_351, "the attacher was not recorded");
            assert_ne!(attached.atime, 0, "the attach left no time behind");
            assert_eq!(
                attached.cpid, 47_350,
                "an attach overwrote who made the segment"
            );
            assert_eq!(shmdt(sys, at), Ok(0));

            let detached = what_the_kernel_keeps(id);
            assert_eq!(detached.nattch, 0, "the detach was not counted");
            assert_eq!(detached.lpid, 47_351, "the detacher was not recorded");
            assert_ne!(detached.dtime, 0, "the detach left no time behind");
        })
    });

    // And `IPC_STAT` is how userspace reads all of it, into a buffer of its
    // own: the one handed in belongs to another segment, so numbers that are
    // not written stay recognisably wrong.
    let reader = a_process(47_352, "shm-stat-reader");
    on_a_thread(&reader, move |sys| {
        Box::pin(async move {
            let elsewhere = shmget(sys, 0, PAGE_SIZE, CREAT | EVERYBODY).unwrap();
            let mut ds = a_shmid_ds_of_another_segment(elsewhere);
            assert_eq!(
                shmctl(
                    sys,
                    id,
                    ShmctlCmds::IPC_STAT,
                    &mut ds as *mut ShmidDs as usize
                ),
                Ok(0),
                "IPC_STAT failed"
            );
            assert_eq!(ds.segsz, TWO_PAGES, "IPC_STAT filled in another segment");
            assert_eq!(ds.cpid, 47_350, "IPC_STAT did not copy the creator");
            assert_eq!(ds.perm.key as usize, MINE, "IPC_STAT did not copy the key");
        })
    });
}

/// `IPC_RMID` takes the key away at once and the id only when the last
/// attach goes.
///
/// This is the MIT-SHM sequence, on every software frame under X11: Mesa
/// does `shmget` + `shmat` + `IPC_RMID` and only then asks the X server to
/// attach the same id. Destroying the segment at `IPC_RMID` made that
/// attach fail with `EINVAL` and Mesa log `Failed to attach to x11 shm`.
#[test]
fn removing_a_segment_keeps_the_id_while_it_is_attached_and_drops_it_at_the_last_detach() {
    const MINE: usize = KEY + 7;

    let proc = a_process(47_353, "shm-rmid");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let id = shmget(sys, MINE, PAGE_SIZE, CREAT | EVERYBODY).unwrap();
            let at = shmat(sys, id, 0, 0).unwrap();
            assert_eq!(
                shmctl(sys, id, ShmctlCmds::IPC_RMID, 0),
                Ok(0),
                "IPC_RMID failed"
            );

            // The key is gone, so nobody can find the segment that way any
            // more -- not even to create it, without IPC_CREAT.
            assert_eq!(
                shmget(sys, MINE, PAGE_SIZE, 0),
                Err(LxError::ENOENT),
                "the key still named the removed segment"
            );
            // The id still works, which is the whole point: the X server is
            // handed the id after the client removed the key.
            let second = shmat(sys, id, 0, 0).unwrap();
            assert_eq!(
                what_the_kernel_keeps(id).nattch,
                2,
                "an attach after IPC_RMID was not counted"
            );
            // A second IPC_RMID while it is still marked is not an error.
            assert_eq!(
                shmctl(sys, id, ShmctlCmds::IPC_RMID, 0),
                Ok(0),
                "a second IPC_RMID on a marked segment failed"
            );

            // The last detach is what makes the id go away.
            assert_eq!(shmdt(sys, at), Ok(0));
            assert!(
                linux_object::ipc::shm_lookup(id).is_some(),
                "the id went away while an attach remained"
            );
            assert_eq!(shmdt(sys, second), Ok(0));
            assert!(
                linux_object::ipc::shm_lookup(id).is_none(),
                "the id outlived its last attach"
            );
            // And now there is nothing to remove or to stat.
            assert_eq!(
                shmctl(sys, id, ShmctlCmds::IPC_RMID, 0),
                Err(LxError::EINVAL),
                "a segment that no longer exists was removed again"
            );
        })
    });
}

/// What the first argument of `shmctl` names depends on the command: the two
/// info commands ignore it, the two `SHM_STAT`s read an INDEX, and
/// everything else reads it as an id.
///
/// Looking the id up for every command made `shmctl(0, SHM_INFO, ..)`
/// `EINVAL` whenever there was no segment 0, which is what busybox `ipcs -m`
/// asks first: "kernel not configured for shared memory".
#[test]
fn what_the_first_argument_of_shmctl_names_depends_on_the_command() {
    const MINE: usize = KEY + 8;
    const NOT_FOR_US: usize = KEY + 9;
    const A_USER: u32 = 1_000;
    const A_STRANGER: u32 = 1_001;

    let owner = a_process(47_354, "shmctl-subject-owner");
    let (id, not_for_us) = on_a_thread(&owner, move |sys| {
        Box::pin(async move {
            acting_as(sys, A_USER, A_USER, &[]);
            (
                shmget(sys, MINE, PAGE_SIZE, CREAT | EVERYBODY).unwrap(),
                shmget(sys, NOT_FOR_US, PAGE_SIZE, CREAT | NOBODY).unwrap(),
            )
        })
    });

    let proc = a_process(47_355, "shmctl-subject");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            acting_as(sys, A_STRANGER, A_STRANGER, &[]);

            // A command nobody defines, before anything else is touched.
            assert_eq!(
                sys.sys_shmctl(id, A_COMMAND_OF_NOBODY, 0),
                Err(LxError::EINVAL),
                "a command number nobody defines was accepted"
            );

            // The info commands ignore the first argument: an id that cannot
            // exist is as good as any.
            let mut counts = ShmInfo::default();
            let highest = shmctl(
                sys,
                NO_SUCH_SEGMENT,
                ShmctlCmds::SHM_INFO,
                &mut counts as *mut ShmInfo as usize,
            )
            .unwrap();
            assert!(
                highest + 1 >= id,
                "SHM_INFO answered index {}, below this segment's id {}",
                highest,
                id
            );

            // An index, and the id comes back: ids are handed out from one,
            // so the index of this segment is one below its id.
            let mut ds = a_shmid_ds_of_another_segment(not_for_us);
            assert_eq!(
                shmctl(
                    sys,
                    id - 1,
                    ShmctlCmds::SHM_STAT,
                    &mut ds as *mut ShmidDs as usize
                ),
                Ok(id),
                "SHM_STAT did not answer the id of the index it was given"
            );
            assert_eq!(
                ds.perm.key as usize, MINE,
                "SHM_STAT filled in another segment"
            );
            // An id, and zero comes back.
            assert_eq!(
                shmctl(
                    sys,
                    id,
                    ShmctlCmds::IPC_STAT,
                    &mut ds as *mut ShmidDs as usize
                ),
                Ok(0),
                "IPC_STAT did not answer zero"
            );

            // A segment with no mode bits: the stat with the check is
            // refused and the one defined not to check is not.
            assert_eq!(
                shmctl(
                    sys,
                    not_for_us - 1,
                    ShmctlCmds::SHM_STAT,
                    &mut ds as *mut ShmidDs as usize
                ),
                Err(LxError::EACCES),
                "a segment with no mode bits was stat-ed by a stranger"
            );
            assert_eq!(
                shmctl(
                    sys,
                    not_for_us - 1,
                    ShmctlCmds::SHM_STAT_ANY,
                    &mut ds as *mut ShmidDs as usize
                ),
                Ok(not_for_us),
                "SHM_STAT_ANY asked the question it is defined not to ask"
            );

            // And the two that change the segment ask who owns it, not who
            // may use it: this one is 0666, so every other command said yes.
            let mut set = a_shmid_ds_of_another_segment(not_for_us);
            assert_eq!(
                shmctl(
                    sys,
                    id,
                    ShmctlCmds::IPC_SET,
                    &mut set as *mut ShmidDs as usize
                ),
                Err(LxError::EPERM),
                "a stranger took a segment over with IPC_SET"
            );
            assert_eq!(
                shmctl(sys, id, ShmctlCmds::IPC_RMID, 0),
                Err(LxError::EPERM),
                "a stranger removed somebody else's segment"
            );
        })
    });
}

/// The two info commands write two DIFFERENT structs into the same buffer:
/// `IPC_INFO` the system's limits, `SHM_INFO` what is actually in use.
///
/// `shm_info` used to stop at `shm_swp`, so its last two fields were never
/// written at all, and the totals it reports used to be zero.
#[test]
fn the_two_info_commands_write_two_different_structs() {
    const MINE: usize = KEY + 10;
    const THREE_PAGES: usize = 3 * PAGE_SIZE;

    let proc = a_process(47_356, "shm-info");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let id = shmget(sys, MINE, THREE_PAGES, CREAT | EVERYBODY).unwrap();

            // The limits, which say nothing about what exists.
            let mut limits = ShmInfo64::default();
            let by_info = shmctl(
                sys,
                NO_SUCH_SEGMENT,
                ShmctlCmds::IPC_INFO,
                &mut limits as *mut ShmInfo64 as usize,
            )
            .unwrap();
            assert_eq!(limits.shmmin, linux_object::ipc::SHMMIN, "SHMMIN");
            assert_eq!(limits.shmmax, linux_object::ipc::SHMMAX, "SHMMAX");
            assert_eq!(limits.shmmni, 4096, "SHMMNI");
            assert_eq!(limits.shmseg, 4096, "SHMSEG");
            assert_eq!(limits.unused, [0; 4], "the reserved tail was not zeroed");

            // And what is in use, which is a different struct of a different
            // size in the same buffer.
            let mut counts = ShmInfo::default();
            let by_counts = shmctl(
                sys,
                NO_SUCH_SEGMENT,
                ShmctlCmds::SHM_INFO,
                &mut counts as *mut ShmInfo as usize,
            )
            .unwrap();
            assert_eq!(
                by_info, by_counts,
                "the two info commands disagreed on the highest index"
            );
            assert!(
                counts.used_ids >= 1,
                "SHM_INFO counted {} segments with one just made",
                counts.used_ids
            );
            assert!(
                counts.shm_tot >= THREE_PAGES / PAGE_SIZE,
                "SHM_INFO counted {} pages with three just asked for",
                counts.shm_tot
            );
            assert_eq!(counts.shm_swp, 0, "there is no swap here");
            assert_eq!(counts.swap_attempts, 0, "the tail was not written");
            assert_eq!(counts.swap_successes, 0, "the tail was not written");

            // The sizes are what make them two commands and not one.
            assert_eq!(core::mem::size_of::<ShmInfo64>(), 72);
            assert_eq!(core::mem::size_of::<ShmInfo>(), 48);
            let _ = id;
        })
    });
}

/// Locking a segment into RAM is a real command the owner may ask for, and a
/// stranger may not.
///
/// There is no swap here, so the answer is yes and nothing happens -- the
/// same honesty as `mlock`. Falling through to `EINVAL` instead made
/// `shmctl(id, SHM_LOCK, 0)` look like an unknown command to everything that
/// probes it, busybox `ipcrm` and postgres's startup among them.
#[test]
fn locking_a_segment_into_ram_is_a_real_command_the_owner_can_ask_for() {
    const MINE: usize = KEY + 11;
    const A_USER: u32 = 1_000;
    const A_STRANGER: u32 = 1_001;

    let owner = a_process(47_357, "shm-lock-owner");
    let id = on_a_thread(&owner, move |sys| {
        Box::pin(async move {
            acting_as(sys, A_USER, A_USER, &[]);
            let id = shmget(sys, MINE, PAGE_SIZE, CREAT | EVERYBODY).unwrap();
            assert_eq!(
                shmctl(sys, id, ShmctlCmds::SHM_LOCK, 0),
                Ok(0),
                "the owner could not lock its own segment"
            );
            assert_eq!(
                shmctl(sys, id, ShmctlCmds::SHM_UNLOCK, 0),
                Ok(0),
                "the owner could not unlock its own segment"
            );
            id
        })
    });

    let proc = a_process(47_358, "shm-lock-stranger");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            acting_as(sys, A_STRANGER, A_STRANGER, &[]);
            // EPERM, not EACCES and not EINVAL: it is ownership that gates
            // these two, and the commands do exist.
            assert_eq!(
                shmctl(sys, id, ShmctlCmds::SHM_LOCK, 0),
                Err(LxError::EPERM),
                "a stranger locked somebody else's segment"
            );
            assert_eq!(
                shmctl(sys, id, ShmctlCmds::SHM_UNLOCK, 0),
                Err(LxError::EPERM),
                "a stranger unlocked somebody else's segment"
            );
        })
    });
}

/// The four calls through the dispatch table, which is the only way
/// userspace makes them: each argument has to arrive in the register the
/// table names.
#[test]
fn the_arguments_arrive_in_the_order_the_syscall_table_names_them() {
    const MINE: usize = KEY + 12;
    const TWO_PAGES: usize = 2 * PAGE_SIZE;

    let proc = a_process(47_359, "shm-by-number");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            // shmget(key, size, shmflg).
            let id = by_number(
                sys,
                crate::Sys::SHMGET,
                [MINE, TWO_PAGES, CREAT | EVERYBODY, 0, 0, 0],
            )
            .await;
            assert!(id > 0, "shmget by number answered {}", id);
            let id = id as usize;
            assert_eq!(
                what_the_kernel_keeps(id).segsz,
                TWO_PAGES,
                "the size did not arrive in the second register"
            );

            // shmat(id, addr, shmflg), with the kernel choosing the address.
            let at = by_number(sys, crate::Sys::SHMAT, [id, 0, 0, 0, 0, 0]).await;
            assert!(at > 0, "shmat by number answered {}", at);
            let at = at as usize;

            // shmctl(id, cmd, buf).
            let mut ds = a_shmid_ds_of_another_segment(id);
            ds.segsz = 0;
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::SHMCTL,
                    [
                        id,
                        ShmctlCmds::IPC_STAT as usize,
                        &mut ds as *mut ShmidDs as usize,
                        0,
                        0,
                        0
                    ]
                )
                .await,
                0,
                "shmctl by number did not go through"
            );
            assert_eq!(
                ds.segsz, TWO_PAGES,
                "the command did not arrive in the second register"
            );

            // shmdt(addr), the one-argument call.
            assert_eq!(
                by_number(sys, crate::Sys::SHMDT, [at, 0, 0, 0, 0, 0]).await,
                0,
                "shmdt by number did not detach"
            );
            assert_eq!(
                what_the_kernel_keeps(id).nattch,
                0,
                "the detach was not counted"
            );
        })
    });
}
