//! The eight calls a process makes about pages it already has: `msync`,
//! `mincore`, `mlock`, `mlock2`, `munlock`, `mlockall`, `munlockall` and
//! `madvise`. **Not one of them was ever called by a test.**
//!
//! Every verdict they take has a test module of its own in `vm.rs`
//! (`mm_walk_tests`, `mlockall_flag_tests`, `madvise_tests`,
//! `user_range_tests`), and all of them call the helper directly: the walk,
//! the flag words and the ranges are decided on. What only a syscall can
//! show is the four things those cannot:
//!
//! 1. **Which range each call hands to the walk.** `mlock` and `munlock`
//!    round the address down and stretch the length to cover the rest of
//!    its page, so what has to be mapped is the SUM and not the length: a
//!    call that passed `(addr, addr + len)` instead would pass every pure
//!    test there is.
//! 2. **The order the arguments are judged in.** `mlock2` reads its flag
//!    before it looks at the range, and `madvise` refuses an advice this
//!    kernel cannot honour before it even notices that the range is empty.
//!    An empty range is success, so a refusal with a length of zero can
//!    only come from the advice having been judged first.
//! 3. **That the answer reaches the caller's buffer, whole and in place.**
//!    `mincore` hands its range over a page's worth of answers at a time,
//!    and that loop is the one place where the answer and the buffer can
//!    drift apart. A range that fits in one chunk cannot see it.
//! 4. **That `MADV_DONTNEED` really discards.** The promise is that the
//!    range reads back as zero on next access, which is a thing a program
//!    can look at and a return value cannot.
//! 5. **That a hole in the range is reported, and reported late.** Linux
//!    applies the advice to the mappings it finds and answers `ENOMEM` for
//!    the gap afterwards, so the only way to tell that apart from a refusal
//!    is to read the bytes of the half that WAS mapped.
//!
//! Fixed pids live in 47_43x and 47_44x here.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::vec::Vec;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};
use zircon_object::vm::MMUFlags;

const PAGE: usize = PAGE_SIZE;

/// `MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED`, and `PROT_READ | PROT_WRITE`.
const ANON_FIXED: usize = 0x32;
const RW: usize = 0x3;

/// `msync(2)`'s flags, by their own numbers, and a bit nobody has.
const MS_ASYNC: usize = 1;
const MS_INVALIDATE: usize = 2;
const MS_SYNC: usize = 4;
const A_MSYNC_FLAG_OF_NOBODY: usize = 8;

/// `mlock2(2)`'s only flag, and one it does not have.
const MLOCK_ONFAULT: usize = 1;
const AN_MLOCK_FLAG_OF_NOBODY: usize = 2;

/// `mlockall(2)`'s three flags.
const MCL_CURRENT: usize = 1;
const MCL_FUTURE: usize = 2;
const MCL_ONFAULT: usize = 4;

/// The `madvise(2)` advice numbers used here: the two that discard, the one
/// that is only a hint, the five Linux knows and this kernel cannot honour,
/// and one nobody has ever defined.
const MADV_NORMAL: usize = 0;
const MADV_DONTNEED: usize = 4;
const MADV_FREE: usize = 8;
const MADV_REMOVE: usize = 9;
const MADV_DONTFORK: usize = 10;
const MADV_DOFORK: usize = 11;
const MADV_WIPEONFORK: usize = 18;
const MADV_KEEPONFORK: usize = 19;
const AN_ADVICE_OF_NOBODY: usize = 7;

/// How many pages' worth of `mincore` answers this kernel holds at once, as
/// a program would have to work it out: a page of them, one byte each.
const A_CHUNK: usize = 4096;

/// What a buffer is seeded with, so that "wrote nothing" cannot pass for
/// "wrote zero".
const A_SEED: u8 = 0xaa;

/// A descriptor of nobody, which is what an anonymous mapping passes.
fn no_file() -> FileDesc {
    FileDesc::from(-1i32 as usize)
}

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// `pages` pages mapped the way a program maps a heap -- through `mmap` --
/// at an address of this test's own.
///
/// Every test passes an `offset` of its own, and `MAP_FIXED` is what makes
/// that stick. Under libos these mappings are backed by the *host* process's
/// address space and every process here shares one base (`USER_ASPACE_BASE`:
/// `aspace-separate` is off in this crate), so two tests that map at the same
/// place and then touch a page fight over the same host mapping -- and
/// `cargo test` runs them at once, while the CI job pins `--test-threads=1`,
/// which is the one arrangement where that never shows. The offsets here
/// start past the band the pure modules of `vm.rs` use.
async fn mapped(sys: &Syscall<'_>, offset: usize, pages: usize) -> usize {
    assert!(offset >= 0x2000_0000 && offset.is_multiple_of(PAGE));
    let at = sys.zircon_process().vmar().addr() + offset;
    let got = sys
        .sys_mmap(at, pages * PAGE, RW, ANON_FIXED, no_file(), 0)
        .await
        .expect("a fresh address space has room for this");
    assert_eq!(got, at, "MAP_FIXED did not map where it was told to");
    at
}

/// `mincore` over `len` bytes from `at`, into a buffer of `pages + 1` bytes
/// seeded with [`A_SEED`]: the extra byte is a guard, so a write past the
/// end of the answer shows up.
fn mincore_of(sys: &Syscall<'_>, at: usize, len: usize, pages: usize) -> (SysResult, Vec<u8>) {
    let mut buf = alloc::vec![A_SEED; pages + 1];
    let answer = sys.sys_mincore(at, len, UserOutPtr::from(buf.as_mut_ptr() as usize));
    (answer, buf)
}

/// A page touched the way a program touches it: the fault a write takes,
/// which is what commits the frame and installs the page-table entry. Under
/// libos there is no CPU to take it, so the test asks the VMAR itself.
fn touch(sys: &Syscall<'_>, page: usize) {
    sys.zircon_process()
        .vmar()
        .handle_page_fault(page, MMUFlags::WRITE | MMUFlags::USER)
        .expect("a mapped, writable page takes a write fault");
}

/// Whether a page has an entry of its own, which is what `mincore` reports
/// and what a discard takes away.
fn is_resident(sys: &Syscall<'_>, page: usize) -> bool {
    let (answer, buf) = mincore_of(sys, page, PAGE, 1);
    assert_eq!(answer, Ok(0), "mincore over one mapped page");
    buf[0] == 1
}

/// Bytes a program leaves in a page it has touched.
fn write_bytes(at: usize, bytes: &[u8]) {
    let mut out: UserOutPtr<u8> = UserOutPtr::from(at);
    out.write_array(bytes).expect("a touched page is writable");
}

/// And what it reads back out of one.
fn read_bytes(at: usize, len: usize) -> Vec<u8> {
    UserInPtr::<u8>::from(at)
        .read_array(len)
        .expect("a touched page is readable")
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

#[test]
fn the_range_these_calls_are_given_has_to_be_mapped_end_to_end() {
    // Four of them answer the same question before anything else happens --
    // is every page of this range mapped? -- and each one has to ask it of
    // the range the caller named, not of the mapping it happens to find.
    let proc = a_process(47_430, "held-mapped");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let at = mapped(sys, 0x2000_0000, 2).await;
            let two = 2 * PAGE;
            let three = 3 * PAGE;

            assert_eq!(sys.sys_mlock(at, two), Ok(0));
            assert_eq!(sys.sys_munlock(at, two), Ok(0));
            assert_eq!(sys.sys_msync(at, two, MS_SYNC), Ok(0));
            assert_eq!(mincore_of(sys, at, two, 2).0, Ok(0));

            // One page past the end of the mapping, and the answer is the
            // one an allocator probing address-space layout reads.
            assert_eq!(sys.sys_mlock(at, three), Err(LxError::ENOMEM));
            assert_eq!(sys.sys_munlock(at, three), Err(LxError::ENOMEM));
            assert_eq!(sys.sys_msync(at, three, MS_SYNC), Err(LxError::ENOMEM));
            assert_eq!(mincore_of(sys, at, three, 3).0, Err(LxError::ENOMEM));

            // An empty range is nothing to walk, so it is success wherever
            // it points, as on Linux.
            let nowhere = at + two;
            assert_eq!(sys.sys_mlock(nowhere, 0), Ok(0));
            assert_eq!(sys.sys_munlock(nowhere, 0), Ok(0));
            assert_eq!(sys.sys_msync(nowhere, 0, MS_SYNC), Ok(0));
            assert_eq!(mincore_of(sys, nowhere, 0, 0).0, Ok(0));

            // And a buffer the kernel cannot write is the caller's fault,
            // not the range's.
            assert_eq!(
                sys.sys_mincore(at, two, UserOutPtr::from(0usize)),
                Err(LxError::EFAULT),
                "mincore wrote its answer to a null pointer"
            );
        })
    });
}

#[test]
fn mlock_and_munlock_stretch_a_range_to_the_page_it_starts_in() {
    // `len = PAGE_ALIGN(len + offset_in_page(start)); start &= PAGE_MASK;`
    // (`mm/mlock.c`): with an unaligned address the range is a page longer
    // than the length, so what has to be mapped is the sum. A call that
    // walked `(addr, addr + len)` instead would say this one page is
    // locked, and `mlock` is the promise a key handler (gpg, ssh-agent)
    // relies on when it says a secret never reaches backing store.
    let proc = a_process(47_431, "held-stretch");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let at = mapped(sys, 0x2400_0000, 1).await;

            assert_eq!(
                sys.sys_mlock(at + 1, PAGE),
                Err(LxError::ENOMEM),
                "a range stretched past the one mapped page was locked"
            );
            assert_eq!(sys.sys_munlock(at + 1, PAGE), Err(LxError::ENOMEM));

            // One byte shorter, and it stays inside the page it started in.
            assert_eq!(sys.sys_mlock(at + 1, PAGE - 1), Ok(0));
            assert_eq!(sys.sys_munlock(at + 1, PAGE - 1), Ok(0));
        })
    });
}

#[test]
fn mlock2_judges_its_flag_before_it_looks_at_the_range() {
    // `mlock2` is `mlock` plus a flag word, and the word is read first: a
    // caller probing for `MLOCK_ONFAULT` on an older kernel passes a real
    // range and has to be told about the flag.
    let proc = a_process(47_432, "held-mlock2");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let at = mapped(sys, 0x2800_0000, 1).await;
            let nowhere = at + 0x10_0000;

            assert_eq!(sys.sys_mlock2(at, PAGE, 0), Ok(0));
            assert_eq!(sys.sys_mlock2(at, PAGE, MLOCK_ONFAULT), Ok(0));
            assert_eq!(
                sys.sys_mlock2(at, PAGE, AN_MLOCK_FLAG_OF_NOBODY),
                Err(LxError::EINVAL)
            );
            assert_eq!(sys.sys_mlock2(at, PAGE, usize::MAX), Err(LxError::EINVAL));

            // The order: an unmapped range AND a flag nobody has is the
            // flag's error.
            assert_eq!(
                sys.sys_mlock2(nowhere, PAGE, AN_MLOCK_FLAG_OF_NOBODY),
                Err(LxError::EINVAL),
                "the range was looked at before the flag"
            );
            // And with a flag that is fine, the range is reached, so the
            // second half of the call is really `mlock`.
            assert_eq!(
                sys.sys_mlock2(nowhere, PAGE, 0),
                Err(LxError::ENOMEM),
                "mlock2 never got as far as the range"
            );
        })
    });
}

#[test]
fn mlockall_wants_a_flag_that_means_something_and_munlockall_never_argues() {
    // `mlockall` has no range: the flag word is the whole of what it can
    // get wrong. `MCL_ONFAULT` says WHEN to lock, so on its own there is
    // nothing for it to be locking, which is the one combination a reader
    // would not guess.
    let proc = a_process(47_433, "held-mlockall");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            assert_eq!(sys.sys_mlockall(MCL_CURRENT), Ok(0));
            assert_eq!(sys.sys_mlockall(MCL_FUTURE), Ok(0));
            assert_eq!(
                sys.sys_mlockall(MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT),
                Ok(0)
            );

            assert_eq!(sys.sys_mlockall(0), Err(LxError::EINVAL));
            assert_eq!(sys.sys_mlockall(MCL_ONFAULT), Err(LxError::EINVAL));
            assert_eq!(sys.sys_mlockall(usize::MAX), Err(LxError::EINVAL));

            // `munlockall` has nothing to validate at all.
            assert_eq!(sys.sys_munlockall(), Ok(0));
        })
    });
}

#[test]
fn mincore_writes_one_byte_a_page_and_draws_the_line_at_what_was_touched() {
    // One byte per page, bit 0 set when the page is resident, and resident
    // means "has a page-table entry": a demand-paged page nobody has
    // touched reports absent. That is the distinction `mincore` exists to
    // draw and the one allocators probe for.
    let proc = a_process(47_434, "held-mincore");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let at = mapped(sys, 0x2c00_0000, 3).await;

            let (answer, buf) = mincore_of(sys, at, 3 * PAGE, 3);
            assert_eq!(answer, Ok(0));
            assert_eq!(
                buf,
                [0, 0, 0, A_SEED],
                "a mapping nobody has touched read as resident"
            );

            // The LAST of the three pages: a fault also maps the fifteen
            // pages after it (`fault_around`), never the ones before, so
            // touching the last one leaves the first two alone.
            touch(sys, at + 2 * PAGE);
            let (answer, buf) = mincore_of(sys, at, 3 * PAGE, 3);
            assert_eq!(answer, Ok(0));
            assert_eq!(
                buf,
                [0, 0, 1, A_SEED],
                "the byte that moved is not the byte of the page that was touched"
            );

            // And it is the length that says how many bytes to write, not
            // the size of the mapping.
            let (answer, buf) = mincore_of(sys, at, PAGE, 3);
            assert_eq!(answer, Ok(0));
            assert_eq!(
                buf,
                [0, A_SEED, A_SEED, A_SEED],
                "one page asked about, more than one byte written"
            );
        })
    });
}

#[test]
fn a_mincore_answer_longer_than_one_chunk_is_written_whole_and_in_place() {
    // `sys_mincore` hands the range over a page's worth of answers at a
    // time, the way `do_mincore` does out of its one `__get_free_page`, so
    // that a `mincore` over a 64 GiB mapping does not grow a 16 MiB kernel
    // allocation inside the syscall. That loop is the only place where the
    // answer and the buffer can drift apart -- a chunk written at the wrong
    // offset, or asked about the wrong page -- and no range that fits in
    // one chunk can see it.
    const PAGES: usize = A_CHUNK + 64;
    const TOUCHED: usize = A_CHUNK + 10;
    let proc = a_process(47_435, "held-chunks");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            assert_eq!(
                MINCORE_CHUNK_PAGES, A_CHUNK,
                "the chunk is not a page of answers any more"
            );
            let at = mapped(sys, 0x3000_0000, PAGES).await;
            touch(sys, at + TOUCHED * PAGE);

            let (answer, buf) = mincore_of(sys, at, PAGES * PAGE, PAGES);
            assert_eq!(answer, Ok(0));
            assert_eq!(
                buf[PAGES], A_SEED,
                "mincore wrote a byte past the answer it was asked for"
            );
            assert!(
                buf[..PAGES].iter().all(|&b| b == 0 || b == 1),
                "a page of the answer was never written at all"
            );
            assert!(
                buf[..A_CHUNK].iter().all(|&b| b == 0),
                "the second chunk answered about the first chunk's pages"
            );
            assert_eq!(
                buf[TOUCHED], 1,
                "the touched page's byte is not at the touched page's index"
            );
            assert_eq!(
                buf[TOUCHED - 1],
                0,
                "a fault maps the pages after it, not the ones before"
            );
        })
    });
}

#[test]
fn msync_takes_its_flags_and_its_range_from_the_man_page() {
    // `msync` is what SQLite and a Firefox profile write through, so what
    // it refuses matters as much as what it flushes.
    let proc = a_process(47_436, "held-msync");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let at = mapped(sys, 0x3400_0000, 1).await;

            for flags in [
                MS_ASYNC,
                MS_SYNC,
                MS_INVALIDATE,
                MS_SYNC | MS_INVALIDATE,
                MS_ASYNC | MS_INVALIDATE,
            ] {
                assert_eq!(sys.sys_msync(at, PAGE, flags), Ok(0), "flags {:#x}", flags);
            }

            // The two that contradict each other, and a bit nobody has.
            assert_eq!(
                sys.sys_msync(at, PAGE, MS_SYNC | MS_ASYNC),
                Err(LxError::EINVAL)
            );
            assert_eq!(
                sys.sys_msync(at, PAGE, A_MSYNC_FLAG_OF_NOBODY),
                Err(LxError::EINVAL)
            );
            // An address that is not the start of a page.
            assert_eq!(sys.sys_msync(at + 1, PAGE, MS_SYNC), Err(LxError::EINVAL));

            // The length that used to report success over a range it never
            // looked at: `addr + roundup_pages(len)` wrapped to zero, so
            // the walk ran zero times and `msync(p, -1, MS_SYNC)` said the
            // file was written back.
            assert_eq!(
                sys.sys_msync(at, usize::MAX, MS_SYNC),
                Err(LxError::ENOMEM),
                "a length that covers the whole address space was accepted"
            );
            assert_eq!(
                sys.sys_msync(at, usize::MAX - PAGE, MS_SYNC),
                Err(LxError::ENOMEM)
            );
        })
    });
}

#[test]
fn madvise_discards_the_page_it_is_given_and_leaves_its_neighbour_alone() {
    // What `MADV_DONTNEED` promises: the range reads back as zero on next
    // access. Treated as the no-op it used to be, it leaves STALE bytes,
    // and an allocator that decommits a region and later reuses it
    // assuming zeros finds its old metadata -- mimalloc aborted mid-`apk
    // update` ("trying to free from an invalid arena") and mozjemalloc's
    // red-black tree corrupted in Firefox. A return value cannot show any
    // of that; the bytes can.
    let proc = a_process(47_437, "held-madvise");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let at = mapped(sys, 0x3800_0000, 2).await;
            touch(sys, at);
            touch(sys, at + PAGE);
            write_bytes(at, b"first");
            write_bytes(at + PAGE, b"second");

            assert_eq!(sys.sys_madvise(at + PAGE, PAGE, MADV_DONTNEED), Ok(0));
            // The discard drops the page's entry, so "on next access" means
            // "on the fault the next access takes". Under libos there is no
            // CPU to take it, so the test is the CPU again -- and a page
            // whose entry is gone is itself a thing a reader can check.
            assert!(
                !is_resident(sys, at + PAGE),
                "MADV_DONTNEED left the page mapped, so nothing re-reads it"
            );
            touch(sys, at + PAGE);
            assert_eq!(
                read_bytes(at + PAGE, 6),
                [0u8; 6],
                "MADV_DONTNEED left the old bytes behind"
            );
            assert_eq!(
                read_bytes(at, 5),
                *b"first",
                "MADV_DONTNEED discarded a page it was not given"
            );

            // `MADV_FREE` is the same promise with a lazier contract, and
            // this kernel keeps it the same way.
            write_bytes(at + PAGE, b"again");
            assert_eq!(sys.sys_madvise(at + PAGE, PAGE, MADV_FREE), Ok(0));
            touch(sys, at + PAGE);
            assert_eq!(read_bytes(at + PAGE, 5), [0u8; 5]);

            // And an advice that is only a hint changes nothing at all.
            assert_eq!(sys.sys_madvise(at, PAGE, MADV_NORMAL), Ok(0));
            assert_eq!(
                read_bytes(at, 5),
                *b"first",
                "a hint this kernel does not act on discarded the page"
            );
        })
    });
}

#[test]
fn the_advice_this_kernel_cannot_honour_is_refused_before_anything_else() {
    // These five are not hints: each changes what a `fork` or the backing
    // store does afterwards, and a caller told 0 relies on it. BoringSSL
    // and AWS-LC (`fork_detect.c`, behind rustls' default backend) ask for
    // `MADV_WIPEONFORK` on a page and, told yes, trust that page to read as
    // zero in a child; here the child kept it, so parent and child went on
    // producing the same DRBG output after a fork.
    let proc = a_process(47_438, "held-advice");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let at = mapped(sys, 0x3c00_0000, 1).await;

            for advice in [
                MADV_REMOVE,
                MADV_DONTFORK,
                MADV_DOFORK,
                MADV_WIPEONFORK,
                MADV_KEEPONFORK,
            ] {
                assert_eq!(
                    sys.sys_madvise(at, PAGE, advice),
                    Err(LxError::EINVAL),
                    "advice {} was accepted and ignored",
                    advice
                );
            }
            assert_eq!(
                sys.sys_madvise(at, PAGE, AN_ADVICE_OF_NOBODY),
                Err(LxError::EINVAL)
            );

            // The order, which is what makes this a wiring test: an empty
            // range is success for every advice this kernel honours, so a
            // refusal with a length of zero can only come from the advice
            // having been judged first.
            assert_eq!(sys.sys_madvise(at, 0, MADV_NORMAL), Ok(0));
            assert_eq!(
                sys.sys_madvise(at, 0, MADV_WIPEONFORK),
                Err(LxError::EINVAL),
                "the length was looked at before the advice"
            );
        })
    });
}

#[test]
fn the_arguments_arrive_in_the_order_the_syscall_table_names_them() {
    // Six calls by number, through the table, and three of them checked by
    // what happens rather than by what they return: the bytes `mincore`
    // writes, the page `madvise` discards, and the flag word `mlockall`
    // refuses.
    let proc = a_process(47_439, "held-by-number");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let at = mapped(sys, 0x4000_0000, 3).await;
            touch(sys, at + 2 * PAGE);

            // `mincore(addr, len, vec)`: the answer lands in the third
            // argument and it is about the pages of the first.
            let mut buf = alloc::vec![A_SEED; 4];
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::MINCORE,
                    [at, 3 * PAGE, buf.as_mut_ptr() as usize, 0, 0, 0]
                )
                .await,
                0
            );
            assert_eq!(buf, [0, 0, 1, A_SEED], "mincore by number");

            // `madvise(addr, len, advice)`: with the advice third, the
            // discard happens, and it happens to the page the first names.
            touch(sys, at);
            touch(sys, at + PAGE);
            write_bytes(at, b"here");
            write_bytes(at + PAGE, b"next");
            assert_eq!(
                by_number(sys, crate::Sys::MADVISE, [at, PAGE, MADV_DONTNEED, 0, 0, 0]).await,
                0
            );
            touch(sys, at);
            assert_eq!(read_bytes(at, 4), [0u8; 4], "madvise by number");
            assert_eq!(
                read_bytes(at + PAGE, 4),
                *b"next",
                "madvise by number took the wrong length"
            );

            // `mlock2(addr, len, flags)` and `msync(addr, len, flags)`: the
            // flags are third in both.
            //
            // A call that FAILS cannot tell an argument swap: with the length
            // and the flag word exchanged, a length of one page reads as a
            // flag word of 4096 and is refused too, for the wrong reason. So
            // each of these is asked once in a shape that has to SUCCEED --
            // and a swap turns that success into a refusal.
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::MLOCK2,
                    [at, PAGE, AN_MLOCK_FLAG_OF_NOBODY, 0, 0, 0]
                )
                .await,
                -(LxError::EINVAL as isize)
            );
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::MLOCK2,
                    [at, 3 * PAGE, MLOCK_ONFAULT, 0, 0, 0]
                )
                .await,
                0,
                "mlock2 by number read its length or its flag somewhere else"
            );
            assert_eq!(
                by_number(
                    sys,
                    crate::Sys::MSYNC,
                    [at, PAGE, MS_SYNC | MS_ASYNC, 0, 0, 0]
                )
                .await,
                -(LxError::EINVAL as isize)
            );
            assert_eq!(
                by_number(sys, crate::Sys::MSYNC, [at, 3 * PAGE, MS_SYNC, 0, 0, 0]).await,
                0,
                "msync by number read its length or its flags somewhere else"
            );
            // `mlock(addr, len)` takes two and no more.
            assert_eq!(
                by_number(sys, crate::Sys::MLOCK, [at, 3 * PAGE, 0, 0, 0, 0]).await,
                0,
                "mlock by number read its address or its length somewhere else"
            );
            assert_eq!(
                by_number(sys, crate::Sys::MUNLOCK, [at, 3 * PAGE, 0, 0, 0, 0]).await,
                0,
                "munlock by number read its address or its length somewhere else"
            );
            assert_eq!(
                by_number(sys, crate::Sys::MUNLOCK, [at, 4 * PAGE, 0, 0, 0, 0]).await,
                -(LxError::ENOMEM as isize),
                "munlock by number read its length somewhere else"
            );

            // `mlockall(flags)` reads the first argument, and `munlockall`
            // reads none of them: here it is handed six words of rubbish.
            assert_eq!(
                by_number(sys, crate::Sys::MLOCKALL, [MCL_ONFAULT, 0, 0, 0, 0, 0]).await,
                -(LxError::EINVAL as isize)
            );
            assert_eq!(
                by_number(sys, crate::Sys::MLOCKALL, [MCL_CURRENT, 0, 0, 0, 0, 0]).await,
                0
            );
            assert_eq!(
                by_number(sys, crate::Sys::MUNLOCKALL, [usize::MAX; 6]).await,
                0
            );
        })
    });
}

#[test]
fn a_hole_in_the_madvise_range_is_reported_after_the_advice_is_honoured() {
    // `madvise_walk_vmas` walks the VMAs the range covers, applies the
    // advice to each one, and returns its `unmapped_error` -- ENOMEM -- for
    // the gaps between them. So both halves of the contract are visible at
    // once: the page that WAS mapped comes back zero, and the call still
    // fails. A kernel that refused the whole range at the hole would pass a
    // test that only read the errno, and leave behind exactly the stale
    // bytes an allocator reuses.
    let proc = a_process(47_440, "held-hole");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            // Two one-page mappings side by side, so that dropping the
            // second one leaves a hole without having to split a mapping.
            let at = mapped(sys, 0x4400_0000, 1).await;
            let next = mapped(sys, 0x4400_1000, 1).await;
            assert_eq!(next, at + PAGE, "the second page went somewhere else");
            touch(sys, at);
            write_bytes(at, b"stale");

            // Mapped end to end, this is the success the four calls around
            // it already promise, and it has to stay one.
            assert_eq!(sys.sys_madvise(at, 2 * PAGE, MADV_NORMAL), Ok(0));

            assert_eq!(sys.sys_munmap(next, PAGE), Ok(0));
            assert_eq!(
                sys.sys_madvise(at, 2 * PAGE, MADV_DONTNEED),
                Err(LxError::ENOMEM),
                "madvise reported success for a range with a hole in it"
            );

            // And the half that was mapped was discarded all the same.
            assert!(
                !is_resident(sys, at),
                "the error came back before the advice was applied"
            );
            touch(sys, at);
            assert_eq!(
                read_bytes(at, 5),
                [0u8; 5],
                "the page that was mapped kept its old bytes"
            );

            // A range wholly unmapped is the same answer, for a discard and
            // for a hint alike: the walk is about the range, not the advice.
            assert_eq!(
                sys.sys_madvise(next, PAGE, MADV_DONTNEED),
                Err(LxError::ENOMEM)
            );
            assert_eq!(
                sys.sys_madvise(next, PAGE, MADV_NORMAL),
                Err(LxError::ENOMEM)
            );

            // An empty range names no page, so it is still success wherever
            // it points: the walk never runs, as in the other four.
            assert_eq!(sys.sys_madvise(next, 0, MADV_DONTNEED), Ok(0));
        })
    });
}
