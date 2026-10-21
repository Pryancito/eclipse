//! `brk(2)` asked for the moves nobody had ever asked it for.
//!
//! `sys_brk` had 28 of its lines at zero, and they are not an odd corner:
//! they are **the whole shrink branch**, the break that stays on the same
//! page, the `RLIMIT_DATA` refusal and the break so far away that the
//! reservation ahead of it would be absurd. The one test this crate had for
//! `brk` asks what a grow does when its 1 MiB chunk does not fit
//! (`as_rlimit_tests`), and the pure helpers that decide all of the above --
//! `brk_target`, `brk_plan`, `brk_within_data_rlimit`, `shrunk_brk` -- have
//! tests of their own at the bottom of `vm.rs`. What had no test was the body
//! that uses them.
//!
//! Two fixes this file records as made live in that gap:
//!
//!  * **`RLIMIT_DATA` was stored, reported back by `prlimit64` and enforced
//!    nowhere**, so `ulimit -d` was a number the shell could set and the
//!    kernel ignored, and the malloc-failure path of everything run under it
//!    never once fired.
//!  * **a break BELOW where the heap starts is a bad argument, not a
//!    shrink**, and `brk(2)` answers it by handing back the old break. Taken
//!    for a shrink it moves the break into the middle of the loaded image,
//!    and the next grow tries to map over the program's own text.
//!
//! Neither had anything holding it: `brk` refuses both by RETURNING THE OLD
//! BREAK rather than an errno, which is how `brk(2)` says no to everything,
//! so a check that stopped working would read as success at the call site.
//! That is what makes these the cheapest possible things to lose.
//!
//! Nothing here reads or writes the heap: that needs a raw pointer and this
//! crate denies `unsafe`. What it asks instead is what the syscall returns,
//! what the process stores, and how many bytes the address space has mapped
//! -- which is enough, because every one of these paths differs in at least
//! one of the three.
//!
//! Fixed pids live in 48_7xx.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::{LinuxProcess, RLimit};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

const PAGE: usize = PAGE_SIZE;

fn a_process(pid: KoID) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "t", LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// Park the break where the heap starts, which is what the loader does
/// before the first `brk`, and hand that address back.
fn a_heap(proc: &Arc<Process>) -> usize {
    let base = linux_object::loader::heap_base(&proc.vmar());
    proc.linux().set_brk(base);
    base
}

/// How many bytes this address space has mapped: what a shrink has to
/// change and what bookkeeping must leave alone.
fn mapped(proc: &Process) -> usize {
    proc.vmar().mapped_bytes_in(0, usize::MAX)
}

#[test]
fn a_break_below_where_the_heap_starts_hands_back_the_old_one() {
    // `brk_target` refuses it and `sys_brk` answers with the break it still
    // has. Nothing in the return value says "refused" -- that is the whole
    // point, and the reason this needs a test: the number comes back looking
    // like the answer to a different question, and the caller's `sbrk`
    // notices only because it is not the one it asked for.
    let proc = a_process(48_700);
    let base = a_heap(&proc);
    let (grown, below, stored) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let grown = sys.sys_brk(base + 3 * PAGE);
            let below = sys.sys_brk(base - PAGE);
            (grown, below, sys.linux_process().brk())
        })
    });

    assert_eq!(grown, Ok(base + 3 * PAGE), "a plain grow of three pages");
    assert_eq!(
        below,
        Ok(base + 3 * PAGE),
        "a break below the heap base was taken for a shrink instead of \
         being handed the old break back"
    );
    assert_eq!(
        stored,
        base + 3 * PAGE,
        "a refused break was stored, which puts it inside the loaded image"
    );
}

#[test]
fn a_break_past_rlimit_data_hands_back_the_old_one() {
    // `check_data_rlimit()`. The limit is measured from the heap base, so a
    // break exactly at it is allowed and one page further is not -- asked as
    // a pair, because a check that compared the wrong way round would still
    // refuse something.
    let proc = a_process(48_701);
    let base = a_heap(&proc);
    let two_pages = (2 * PAGE) as u64;
    proc.linux()
        .rlimit(
            RLIMIT_DATA,
            Some(RLimit {
                cur: two_pages,
                max: two_pages,
            }),
            true,
        )
        .unwrap();
    let (at_the_limit, past_it, stored) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let at_the_limit = sys.sys_brk(base + 2 * PAGE);
            let past_it = sys.sys_brk(base + 3 * PAGE);
            (at_the_limit, past_it, sys.linux_process().brk())
        })
    });

    assert_eq!(
        at_the_limit,
        Ok(base + 2 * PAGE),
        "a break exactly at RLIMIT_DATA was refused"
    );
    assert_eq!(
        past_it,
        Ok(base + 2 * PAGE),
        "a break past RLIMIT_DATA moved, so ulimit -d is still a number the \
         kernel ignores"
    );
    assert_eq!(stored, base + 2 * PAGE, "the refused break was stored");
}

#[test]
fn a_shrink_hands_the_pages_back_and_the_break_moves_with_them() {
    // The branch nobody had ever taken. glibc DOES shrink the break --
    // `systrim()` calls `MORECORE(-extra)` from `free()` once the top chunk
    // passes the 128 KiB trim threshold -- and the unmap is not a tidy-up: it
    // is the only reason the memory a later grow hands back reads as zero.
    //
    // Three things have to move together, and each one alone would look
    // right: the answer, the reservation (`mapped_brk`, or the next grow
    // takes the bookkeeping-only path and hands back the old heap), and the
    // bytes the address space actually holds.
    //
    // The second grow is the reason the reservation exists and had no test
    // either: it stays inside the 1 MiB already mapped, so it must be
    // bookkeeping alone -- that is the whole saving over one VMO per
    // `brk`, and glibc's malloc asks in 4 to 32 KiB steps.
    let proc = a_process(48_702);
    let base = a_heap(&proc);
    #[allow(clippy::type_complexity)]
    let (grown, reserved, held, again, held_again, shrunk, reserved_after, held_after) =
        on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                let grown = sys.sys_brk(base + 8 * PAGE);
                let reserved = sys.linux_process().mapped_brk();
                let held = mapped(&sys.zircon_process());
                let again = sys.sys_brk(base + 16 * PAGE + 100);
                let held_again = mapped(&sys.zircon_process());
                let shrunk = sys.sys_brk(base + 2 * PAGE);
                let reserved_after = sys.linux_process().mapped_brk();
                let held_after = mapped(&sys.zircon_process());
                (
                    grown,
                    reserved,
                    held,
                    again,
                    held_again,
                    shrunk,
                    reserved_after,
                    held_after,
                )
            })
        });

    assert_eq!(grown, Ok(base + 8 * PAGE), "a plain grow of eight pages");
    assert_eq!(
        reserved,
        base + BRK_CHUNK,
        "eight pages of break did not reserve the chunk ahead of it"
    );
    assert_eq!(
        again,
        Ok(base + 16 * PAGE + 100),
        "a grow inside the reservation was answered with the page it ends on \
         rather than the break asked for, which is the `sbrk(100)` bug again \
         on the path that skips the mapping"
    );
    assert_eq!(
        held, held_again,
        "a grow inside the reservation mapped something again"
    );
    assert_eq!(
        shrunk,
        Ok(base + 2 * PAGE),
        "a shrink did not report the break it was asked for"
    );
    assert_eq!(
        reserved_after,
        base + 2 * PAGE,
        "the reservation stayed above the new break, so the next grow would \
         hand back the old heap instead of fresh pages"
    );
    assert_eq!(
        held - held_after,
        BRK_CHUNK - 2 * PAGE,
        "the shrink did not give back everything above the new break"
    );
}

#[test]
fn a_break_on_the_same_page_is_bookkeeping_and_the_number_asked_for_is_kept() {
    // Two things in one, because they are the same mistake seen twice. The
    // break stored and returned is the one the caller NAMED, not the
    // page-aligned one: storing the aligned one made `sbrk(100)` move the
    // break by 4096 and `sbrk(0)` never agree with `start + n`. And a second
    // break on that same page maps and unmaps nothing at all.
    let proc = a_process(48_703);
    let base = a_heap(&proc);
    let (first, held, same_page, held_after, stored) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let first = sys.sys_brk(base + 100);
            let held = mapped(&sys.zircon_process());
            let same_page = sys.sys_brk(base + 200);
            let held_after = mapped(&sys.zircon_process());
            (
                first,
                held,
                same_page,
                held_after,
                sys.linux_process().brk(),
            )
        })
    });

    assert_eq!(
        first,
        Ok(base + 100),
        "a break of a hundred bytes was rounded up to a page"
    );
    assert_eq!(
        same_page,
        Ok(base + 200),
        "a second break on the same page was not answered with itself"
    );
    assert_eq!(
        stored,
        base + 200,
        "the break stored is not the one asked for"
    );
    assert_eq!(
        held, held_after,
        "a break that stays on its page mapped or unmapped something"
    );
}

#[test]
fn a_grow_whose_reservation_would_be_absurd_hands_back_the_old_break() {
    // The reservation ahead is rounded up to `BRK_CHUNK`, so a break far
    // enough away makes it bigger than any mapping this kernel will make:
    // `MAX_MMAP_LEN` is the same cap `mmap` refuses with, and over it `brk`
    // keeps the break it has rather than attempting the map. The address is
    // inside the address space -- that is what makes this a different
    // refusal from the one above.
    //
    // The distance is measured from where the first grow left the
    // reservation, and it has to pass the cap rather than reach it: a break
    // exactly at it is a 64 GiB mapping this kernel does attempt, which
    // means sixteen million page frames and sixteen seconds of them before
    // the answer comes back the same way for a different reason.
    let proc = a_process(48_704);
    let base = a_heap(&proc);
    let far = base + BRK_CHUNK + MAX_MMAP_LEN + PAGE;
    assert!(
        far <= USER_ASPACE_END,
        "this test needs a reachable address to be refused for its SIZE and \
         not for being outside the address space"
    );
    let (grown, far_away, stored) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let grown = sys.sys_brk(base + PAGE);
            let far_away = sys.sys_brk(far);
            (grown, far_away, sys.linux_process().brk())
        })
    });

    assert_eq!(grown, Ok(base + PAGE), "a grow of one page");
    assert_eq!(
        far_away,
        Ok(base + PAGE),
        "a break whose reservation passes the mmap cap moved anyway"
    );
    assert_eq!(stored, base + PAGE, "the refused break was stored");
}
