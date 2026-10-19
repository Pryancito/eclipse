//! `RLIMIT_AS` was stored, read back by `getrlimit` and enforced nowhere:
//! with `ulimit -v` set, `mmap`, `brk`, `mremap` and `shmat` went on
//! growing the address space, so the `ENOMEM` path of an allocator under a
//! memory budget never ran. Each call is made here as a program makes it,
//! from a thread of the process, with the limit set a few pages above what
//! the process has mapped.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use linux_object::fs::{File, OpenFlags};
use linux_object::process::{LinuxProcess, RLimit};
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

const PAGE: usize = PAGE_SIZE;
const ANON_PRIVATE: usize = 0x22; // MAP_PRIVATE | MAP_ANONYMOUS
const ANON_PRIVATE_FIXED: usize = 0x32; // ... | MAP_FIXED
const RW: usize = 0x3;
const MREMAP_MAYMOVE: usize = 1;
const MREMAP_FIXED: usize = 2;

fn a_process(pid: KoID) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "t", LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// What the process has mapped now: the `VmSize` the limit is measured on.
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

#[test]
fn the_limit_is_counted_in_whole_pages_rounded_down() {
    assert!(within_as_rlimit(usize::MAX - PAGE, PAGE, RLIM_INFINITY));
    assert!(within_as_rlimit(2 * PAGE, 2 * PAGE, (4 * PAGE) as u64));
    assert!(
        !within_as_rlimit(2 * PAGE, 2 * PAGE, (4 * PAGE - 1) as u64),
        "the last partial page of a limit is not a page"
    );
    assert!(!within_as_rlimit(2 * PAGE, 3 * PAGE, (4 * PAGE) as u64));
}

#[test]
fn mmap_maps_up_to_the_limit_and_no_further() {
    let proc = a_process(45_201);
    room_for(&proc, 4);
    let (four, one, after_munmap) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let four = anon(sys, 4).await;
            let one = anon(sys, 1).await;
            let after = match four {
                Ok(at) => {
                    sys.sys_munmap(at, 2 * PAGE).unwrap();
                    anon(sys, 2).await
                }
                Err(e) => Err(e),
            };
            (four, one, after)
        })
    });
    assert!(four.is_ok(), "4 pages of room, 4 pages refused: {:?}", four);
    assert_eq!(
        one,
        Err(LxError::ENOMEM),
        "a page past RLIMIT_AS was mapped"
    );
    assert!(
        after_munmap.is_ok(),
        "munmap gave 2 pages back and they could not be mapped again: {:?}",
        after_munmap
    );
}

#[test]
fn a_map_fixed_counts_only_what_it_does_not_replace() {
    let proc = a_process(45_202);
    let (over_two, over_two_plus_one) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let at = anon(sys, 2).await.unwrap();
            // Full: not one page more.
            room_for(&sys.zircon_process(), 0);
            let over_two = sys
                .sys_mmap(
                    at,
                    2 * PAGE,
                    RW,
                    ANON_PRIVATE_FIXED,
                    FileDesc::from(-1i32 as usize),
                    0,
                )
                .await;
            let over_two_plus_one = sys
                .sys_mmap(
                    at,
                    3 * PAGE,
                    RW,
                    ANON_PRIVATE_FIXED,
                    FileDesc::from(-1i32 as usize),
                    0,
                )
                .await;
            (over_two, over_two_plus_one)
        })
    });
    assert!(
        over_two.is_ok(),
        "MAP_FIXED over 2 mapped pages adds nothing and was refused: {:?}",
        over_two
    );
    assert_eq!(
        over_two_plus_one,
        Err(LxError::ENOMEM),
        "MAP_FIXED over 2 pages with a third new one"
    );
}

#[test]
fn a_file_mapping_is_held_to_the_limit_too() {
    let proc = a_process(45_203);
    let lp = proc.linux();
    let inode = lp.root_inode().create("f", FileType::File, 0o644).unwrap();
    inode.resize(4 * PAGE).unwrap();
    let fd = lp
        .add_file(File::new(inode, OpenFlags::RDWR, String::from("/f")))
        .unwrap();
    room_for(&proc, 2);
    let (four, two) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let four = sys.sys_mmap(0, 4 * PAGE, RW, 0x2, fd, 0).await;
            let two = sys.sys_mmap(0, 2 * PAGE, RW, 0x2, fd, 0).await;
            (four, two)
        })
    });
    assert_eq!(four, Err(LxError::ENOMEM), "4 pages of a file in 2 of room");
    assert!(two.is_ok(), "2 pages of a file in 2 of room: {:?}", two);
}

#[test]
fn brk_grows_by_exactly_what_was_asked_when_its_chunk_does_not_fit() {
    let proc = a_process(45_204);
    let base = linux_object::loader::heap_base(&proc.vmar());
    proc.linux().set_brk(base);
    room_for(&proc, 4);
    let (small, big) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let small = sys.sys_brk(base + 3 * PAGE);
            let big = sys.sys_brk(base + 64 * PAGE);
            (small, big)
        })
    });
    assert_eq!(
        small,
        Ok(base + 3 * PAGE),
        "3 pages of break in 4 pages of room (the 1 MiB chunk does not fit, the exact grow does)"
    );
    assert_eq!(
        big,
        Ok(base + 3 * PAGE),
        "a break past RLIMIT_AS moved; it keeps the old one"
    );
}

#[test]
fn mremap_grows_up_to_the_limit_and_no_further() {
    let proc = a_process(45_205);
    let (to_three, to_six) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let at = anon(sys, 2).await.unwrap();
            room_for(&sys.zircon_process(), 1);
            let to_three = sys.sys_mremap(at, 2 * PAGE, 3 * PAGE, MREMAP_MAYMOVE, 0);
            let at = to_three.unwrap_or(at);
            let to_six = sys.sys_mremap(at, 3 * PAGE, 6 * PAGE, MREMAP_MAYMOVE, 0);
            (to_three, to_six)
        })
    });
    assert!(
        to_three.is_ok(),
        "a grow of 1 page in 1 of room: {:?}",
        to_three
    );
    assert_eq!(to_six, Err(LxError::ENOMEM), "a grow of 3 pages in none");
}

#[test]
fn mremap_to_a_fixed_address_frees_what_was_there_first() {
    let proc = a_process(45_206);
    let moved = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let from = anon(sys, 1).await.unwrap();
            let to = anon(sys, 3).await.unwrap();
            // Full: a grow from 1 page to 3 only fits because the 3 pages
            // already at the destination go first.
            room_for(&sys.zircon_process(), 0);
            sys.sys_mremap(from, PAGE, 3 * PAGE, MREMAP_MAYMOVE | MREMAP_FIXED, to)
                .map(|at| at == to)
        })
    });
    assert_eq!(moved, Ok(true));
}

#[test]
fn shmat_is_held_to_the_limit() {
    let proc = a_process(45_207);
    let (refused, taken) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            // IPC_PRIVATE, 2 pages, IPC_CREAT | 0600.
            let id = sys.sys_shmget(0, 2 * PAGE, 0o1000 | 0o600).unwrap();
            room_for(&sys.zircon_process(), 1);
            let refused = sys.sys_shmat(id, 0, 0);
            room_for(&sys.zircon_process(), 2);
            let taken = sys.sys_shmat(id, 0, 0);
            (refused, taken)
        })
    });
    assert_eq!(
        refused,
        Err(LxError::ENOMEM),
        "2 pages of segment in 1 of room"
    );
    assert!(
        taken.is_ok(),
        "2 pages of segment in 2 of room: {:?}",
        taken
    );
}
