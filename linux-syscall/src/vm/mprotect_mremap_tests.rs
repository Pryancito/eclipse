//! What `mprotect` and `mremap` answer when they refuse, through the
//! syscall.
//!
//! Both bodies end in a `match` that turns a Zircon error into an errno,
//! and neither match had ever run: every test in the tree asked them
//! something that worked. The errnos matter more than usual here, because
//! the two mistakes they stand for are both silent:
//!
//! 1. **An `mprotect` that failed and reported success.** Swallowing the
//!    error told SpiderMonkey its `mprotect(RX)` had worked while part of
//!    the range still lacked EXECUTE, and the next indirect call
//!    instruction-fetched a non-executable page. A hole in the range is
//!    `ENOMEM`, which is what Linux answers.
//! 2. **An empty `mprotect` that answered `EINVAL`.** `mm/mprotect.c` is
//!    `if (!len) return 0;` before Linux has looked at a single VMA.
//!    Falling through instead reached `VmAddressRegion::protect`, which
//!    refuses a zero length.
//!
//! And `mremap`'s refusals: a new length past what this kernel will map, an
//! old address that is not mapped at all, and a shrink in place, which is
//! the one that gives pages back without moving anything.
//!
//! Fixed pids live in 48_90x here.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::{LinuxProcess, RLimit};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

const PAGE: usize = PAGE_SIZE;
/// `MAP_PRIVATE | MAP_ANONYMOUS`, and `PROT_READ | PROT_WRITE`.
const ANON_PRIVATE: usize = 0x22;
const RW: usize = 0x3;
/// `MREMAP_MAYMOVE`.
const MREMAP_MAYMOVE: usize = 1;

fn a_process(pid: KoID) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "t", LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// What the process has mapped now.
fn mapped(proc: &Process) -> usize {
    proc.vmar().mapped_bytes_in(0, usize::MAX)
}

/// `RLIMIT_AS` at what is mapped now plus `pages`.
fn room_for(proc: &Arc<Process>, pages: usize) {
    let cur = (mapped(proc) + pages * PAGE) as u64;
    proc.linux()
        .rlimit(RLIMIT_AS, Some(RLimit { cur, max: cur }), true)
        .unwrap();
}

async fn anon(sys: &Syscall<'_>, pages: usize) -> SysResult {
    sys.sys_mmap(
        0,
        pages * PAGE,
        RW,
        ANON_PRIVATE,
        FileDesc::from(-1i32 as usize),
        0,
    )
    .await
}

/// An address inside the user address space that is mapped by nobody: one
/// the kernel itself picked, handed back, and then took away again.
async fn a_free_address(sys: &Syscall<'_>, pages: usize) -> usize {
    let at = anon(sys, pages).await.expect("a fresh mapping");
    sys.sys_munmap(at, pages * PAGE).expect("and it goes away");
    at
}

#[test]
fn an_mprotect_of_nothing_answers_before_it_looks_at_a_mapping() {
    // The same address twice, and only the length differs: with a zero
    // length the answer is success without a VMA being consulted, and with
    // one page it is the refusal that proves no VMA was there.
    let proc = a_process(48_900);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let nowhere = a_free_address(sys, 1).await;

            assert_eq!(
                sys.sys_mprotect(nowhere, 0, RW),
                Ok(0),
                "an empty mprotect is success, whatever is or is not there"
            );
            assert_eq!(
                sys.sys_mprotect(nowhere, PAGE, RW),
                Err(LxError::ENOMEM),
                "and one page of the same address is not mapped"
            );
        })
    });
}

#[test]
fn an_mprotect_over_a_range_that_is_not_mapped_is_enomem() {
    // A hole in the range is `ENOMEM` in Linux, and the thing that must
    // never happen is `Ok(0)`: a caller that asked for EXECUTE and was told
    // yes will fetch an instruction from a page that does not have it.
    let proc = a_process(48_901);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let at = anon(sys, 2).await.expect("two pages");
            assert_eq!(
                sys.sys_mprotect(at, 2 * PAGE, 0x1),
                Ok(0),
                "a range that is all mapped goes through"
            );

            let nowhere = a_free_address(sys, 2).await;
            assert_eq!(
                sys.sys_mprotect(nowhere, 2 * PAGE, 0x1),
                Err(LxError::ENOMEM),
                "and a range that is mapped by nobody is refused"
            );

            // The mapping is still as permissive as it was asked to be: the
            // refusal above changed nothing.
            assert_eq!(sys.sys_mprotect(at, 2 * PAGE, RW), Ok(0));
        })
    });
}

#[test]
fn an_mremap_past_what_the_kernel_will_map_is_enomem_and_nothing_moves() {
    // The cap is read before the address space is touched, so the old
    // mapping has to come out of it unchanged -- both its address and the
    // pages the process has.
    let proc = a_process(48_902);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let at = anon(sys, 1).await.expect("one page");
            let held = mapped(sys.zircon_process());

            // `EINVAL`, from `mremap_args`: a `new_len` that is not anchored
            // anywhere yet is bounded against the address space as a whole,
            // and one byte past it is not a length at all. It read `ENOMEM`
            // while `MAX_MMAP_LEN` was an arbitrary number below the address
            // space, which gave a second, later refusal to reach.
            assert_eq!(
                sys.sys_mremap(at, PAGE, MAX_MMAP_LEN + PAGE, MREMAP_MAYMOVE, 0),
                Err(LxError::EINVAL),
                "a new length past the address space"
            );
            assert_eq!(
                mapped(sys.zircon_process()),
                held,
                "and the address space is as it was"
            );
            assert_eq!(
                sys.sys_mremap(at, PAGE, 2 * PAGE, MREMAP_MAYMOVE, 0),
                Ok(at),
                "the same mapping still remaps"
            );
        })
    });
}

#[test]
fn an_mremap_of_something_that_is_not_there_is_efault() {
    // `NOT_FOUND` is `EFAULT`, which is what `mremap(2)` names for an old
    // range that is not mapped. Answering `ENOMEM` instead would read, to
    // an allocator, as "out of memory, try later" for an address that will
    // never be there.
    let proc = a_process(48_903);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let nowhere = a_free_address(sys, 1).await;
            assert_eq!(
                sys.sys_mremap(nowhere, PAGE, 2 * PAGE, MREMAP_MAYMOVE, 0),
                Err(LxError::EFAULT),
                "an old address that is mapped by nobody"
            );
        })
    });
}

#[test]
fn an_mremap_that_shrinks_in_place_hands_the_tail_back() {
    // A shrink needs no `MREMAP_MAYMOVE` and must not move: the answer is
    // the address that went in, and the pages dropped off the end have to
    // leave the address space, or `ulimit -v` counts them for ever.
    let proc = a_process(48_904);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let at = anon(sys, 4).await.expect("four pages");
            let held = mapped(sys.zircon_process());

            assert_eq!(
                sys.sys_mremap(at, 4 * PAGE, 2 * PAGE, 0, 0),
                Ok(at),
                "a shrink stays where it was"
            );
            assert_eq!(
                held - mapped(sys.zircon_process()),
                2 * PAGE,
                "and the two pages of the tail are gone"
            );
            assert_eq!(
                sys.sys_mprotect(at, 2 * PAGE, RW),
                Ok(0),
                "what is left is still mapped"
            );
            assert_eq!(
                sys.sys_mprotect(at + 2 * PAGE, 2 * PAGE, RW),
                Err(LxError::ENOMEM),
                "and the tail is not"
            );
        })
    });
}

#[test]
fn an_mremap_that_grows_past_the_as_rlimit_is_enomem() {
    // `vma_to_resize()` adds `new_len - old_len` to what is mapped, so the
    // limit is asked about the difference and not about the whole new
    // length. With room for exactly one more page, growing by one works and
    // growing by two does not.
    let proc = a_process(48_905);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let at = anon(sys, 2).await.expect("two pages");
            room_for(&proc_of(sys), 1);

            assert_eq!(
                sys.sys_mremap(at, 2 * PAGE, 5 * PAGE, MREMAP_MAYMOVE, 0),
                Err(LxError::ENOMEM),
                "growing by three pages with room for one"
            );
            assert_eq!(
                sys.sys_mremap(at, 2 * PAGE, 3 * PAGE, MREMAP_MAYMOVE, 0),
                Ok(at),
                "and growing by exactly the room there is"
            );
        })
    });
}

/// The process the calling thread belongs to, which `room_for` needs as an
/// `Arc`.
fn proc_of(sys: &Syscall<'_>) -> Arc<Process> {
    sys.zircon_process().clone()
}

/// Whether hunter's W^X tracker can still answer precisely for this pid.
/// It fails CLOSED: a pid it has forgotten, or one whose interval list
/// saturated, answers "ever writable" for every address. An address far
/// past anything this test mapped is the probe -- if that comes back
/// writable, the tracker is in its fail-closed state and nothing can be
/// concluded from it, so the assertions that follow are skipped instead of
/// failing for a reason that has nothing to do with the code under test.
fn the_tracker_still_answers(pid: KoID, nowhere: usize) -> bool {
    !hunter::wx::is_ever_writable(pid, nowhere, PAGE)
}

#[test]
fn a_shrink_in_place_retires_the_tail_from_the_wx_history() {
    // The pages that drop off the end have to leave hunter's
    // ever-writable history with them. If they did not, the next mapping
    // the allocator puts there would be refused an `mprotect(PROT_EXEC)`
    // as a two-step W-then-X bypass, for writes that happened to a
    // mapping that no longer exists.
    let proc = a_process(48_906);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let pid = sys.zircon_process().id();
            let at = anon(sys, 4).await.expect("four pages");
            let nowhere = at + 1024 * PAGE;
            assert!(
                hunter::wx::is_ever_writable(pid, at, 4 * PAGE),
                "a writable mapping goes into the history"
            );
            let precise = the_tracker_still_answers(pid, nowhere);

            assert_eq!(
                sys.sys_mremap(at, 4 * PAGE, 2 * PAGE, 0, 0),
                Ok(at),
                "a shrink stays where it was"
            );
            if precise {
                assert!(
                    !hunter::wx::is_ever_writable(pid, at + 2 * PAGE, 2 * PAGE),
                    "the tail's writable history goes with the pages"
                );
                assert!(
                    hunter::wx::is_ever_writable(pid, at, 2 * PAGE),
                    "and the part that stays keeps its own"
                );
            }
        })
    });
}

#[test]
fn a_grow_in_place_keeps_the_history_of_the_part_that_stays() {
    // The other direction, and the one that matters for the bypass: a
    // range that was writable once must still be known to have been
    // writable after it is resized. Treating a resize in place as a move
    // retires the old range's history and re-records only what the
    // mapping's CURRENT flags say, so a program could mmap(RW), write
    // code, mprotect(R), mremap to resize -- and arrive at
    // mprotect(RX) with a clean record.
    let proc = a_process(48_907);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let pid = sys.zircon_process().id();
            let at = anon(sys, 2).await.expect("two pages");
            let nowhere = at + 1024 * PAGE;
            let precise = the_tracker_still_answers(pid, nowhere);

            // Take the write bit away, which is what makes the two
            // branches tell different stories: the resize reads the
            // CURRENT flags, and they now say "not writable".
            assert_eq!(sys.sys_mprotect(at, 2 * PAGE, 0x1), Ok(0));
            assert_eq!(
                sys.sys_mremap(at, 2 * PAGE, 4 * PAGE, MREMAP_MAYMOVE, 0),
                Ok(at),
                "a grow with room after it stays where it was"
            );
            if precise {
                assert!(
                    hunter::wx::is_ever_writable(pid, at, 2 * PAGE),
                    "the range was writable once and the resize must not forget it"
                );
                assert!(
                    !hunter::wx::is_ever_writable(pid, at + 2 * PAGE, 2 * PAGE),
                    "and the pages the grow added were never writable"
                );
            }
        })
    });
}
