//! What `mmap` of a file hands back, and the three refusals of its own that
//! had never been asked for.
//!
//! `sys_mmap` is the largest body in this file and the only one whose
//! file-backed arm had never been reached with `MAP_SHARED`. The rule that
//! arm exists for is not about an errno: **every mapper of one file must
//! receive the SAME object**, because that is what makes one process's
//! stores visible through another's mapping. The comment on it names the
//! price of getting it wrong (clients render into a mapped memfd and the
//! compositor reads the pixels through its own mapping), and nothing in the
//! tree asked whether two mappings of one file share an object or copy it.
//! An mmap that quietly hands out per-process copies returns a valid
//! address, maps the right length and reads the right bytes; what it loses
//! is the writes, and only on the other side.
//!
//! The same for `MAP_SHARED | MAP_ANONYMOUS`: without the marker, `fork`
//! privatizes the region and a preforked pool's shared counter page stops
//! being shared, which no return value shows either. Both are asked here of
//! the address space rather than of `mmap`: the object behind the mapping,
//! and whether it is marked to survive a fork as one.
//!
//! The three refusals are a length past what the kernel will place, an
//! unaligned address where the placement is not a hint, and
//! `MAP_FIXED_NOREPLACE` over a range that is taken. Each is paired here
//! with the call that must NOT be refused, because every one of the three
//! sits one bit away from an ordinary mapping.
//!
//! What a pipe answers is here too: the one descriptor in this bench with no
//! object behind it at all, which is how the error arms of both `get_vmo`
//! calls are reached.
//!
//! Fixed pids live in 49_39x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use linux_object::fs::{File, OpenFlags, Pipe};
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

const PAGE: usize = PAGE_SIZE;
/// `PROT_READ | PROT_WRITE`, and `PROT_READ` alone.
const RW: usize = 0x3;
const R: usize = 0x1;
/// The flag words, written out as userspace passes them rather than built
/// from `MmapFlags`, whose bit numbering is part of what is under test.
/// `MAP_ANONYMOUS` is `0x20` on x86-64, which is where this bench runs.
const ANON_PRIVATE: usize = 0x22;
const ANON_SHARED: usize = 0x21;
const FILE_PRIVATE: usize = 0x2;
const FILE_SHARED: usize = 0x1;
const FIXED_ANON_PRIVATE: usize = 0x32;
const NOREPLACE_ANON_PRIVATE: usize = 0x10_0022;
const FIXED_FILE_PRIVATE: usize = 0x12;

/// `MAX_MMAP_LEN` plus one byte: the length the kernel refuses to place.
/// Written out rather than read from the constant, which is what the refusal
/// is made of.
const PAST_WHAT_IS_PLACED: usize = 64 * 1024 * 1024 * 1024 + 1;

/// What the file holds. Its size only has to be non-zero for the mapping to
/// have something to be backed by.
const WHAT_THE_FILE_HOLDS: &[u8; 16] = b"sixteen bytes...";

/// The descriptor `mmap` is handed for an anonymous mapping, which ignores
/// it entirely.
fn no_descriptor() -> FileDesc {
    FileDesc::from(-1i32 as usize)
}

/// A process whose root holds a `/<top>` of its own, with TWO descriptors
/// open on one file of it -- the two mappers whose objects are compared.
///
/// The directory is per-process because a lookup from the root goes through
/// a path cache every process in this binary shares.
fn a_process_with_one_file_open_twice(pid: KoID, top: &str) -> (Arc<Process>, i32, i32) {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let dir = lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    let inode = dir.create("f", FileType::File, 0o644).unwrap();
    inode.write_at(0, WHAT_THE_FILE_HOLDS).unwrap();
    let path = alloc::format!("/{}/f", top);
    let one: i32 = lp
        .add_file(File::new(inode.clone(), OpenFlags::RDWR, path.clone()))
        .unwrap()
        .into();
    let two: i32 = lp
        .add_file(File::new(inode, OpenFlags::RDWR, path))
        .unwrap()
        .into();
    (
        Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "t", lp).unwrap(),
        one,
        two,
    )
}

/// A process holding the read end of a pipe, which is readable -- so it
/// passes the descriptor's own access check -- and has no object behind it.
fn a_process_with_a_pipe(pid: KoID) -> (Arc<Process>, i32) {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let (read_end, _write_end) = Pipe::create_pair();
    let fd: i32 = lp
        .add_file(File::new(
            Arc::new(read_end),
            OpenFlags::RDONLY,
            String::from("/pipe"),
        ))
        .unwrap()
        .into();
    (
        Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "t", lp).unwrap(),
        fd,
    )
}

fn a_process(pid: KoID) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "t", LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// What the process has mapped now, asked of the address space.
fn mapped(proc: &Process) -> usize {
    proc.vmar().mapped_bytes_in(0, usize::MAX)
}

/// The object the mapping at `at` covers, asked of the address space and not
/// of the call under test.
fn the_object_behind(proc: &Process, at: usize) -> Arc<zircon_object::vm::VmObject> {
    proc.vmar()
        .find_mapping(at)
        .expect("a mapping at the address mmap returned")
        .vmo()
        .clone()
}

async fn anon(sys: &Syscall<'_>, pages: usize, flags: usize) -> SysResult {
    sys.sys_mmap(0, pages * PAGE, RW, flags, no_descriptor(), 0)
        .await
}

/// An address inside the user address space that is mapped by nobody: one
/// the kernel itself picked, handed back, and then took away again.
async fn a_free_address(sys: &Syscall<'_>) -> usize {
    let at = anon(sys, 1, ANON_PRIVATE).await.expect("a fresh mapping");
    sys.sys_munmap(at, PAGE).expect("and it goes away");
    at
}

#[test]
fn a_length_past_what_the_kernel_will_place_is_refused_and_maps_nothing() {
    // The cap is read before anything is placed, so the refusal cannot cost
    // the process a byte of address space. The length is the only thing
    // wrong with the call: the same flags one page long go through.
    let proc = a_process(49_390);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let held = mapped(sys.zircon_process());
            assert_eq!(
                sys.sys_mmap(0, PAST_WHAT_IS_PLACED, RW, ANON_PRIVATE, no_descriptor(), 0)
                    .await,
                Err(LxError::ENOMEM),
                "a length past the cap was accepted"
            );
            assert_eq!(
                mapped(sys.zircon_process()),
                held,
                "the refused length still cost the process address space"
            );
            assert!(
                anon(sys, 1, ANON_PRIVATE).await.is_ok(),
                "and one page of the same call is an ordinary mapping"
            );
        })
    });
}

#[test]
fn an_unaligned_address_is_refused_where_it_is_an_order_and_rounded_where_it_is_a_hint() {
    // Only `addr` has to be page-aligned, and only where the placement is
    // an order. A hint is rounded DOWN to its page instead, which is what
    // `round_hint_to_min` does and what ld.so's zero-fill mmap relies on:
    // holding a hint to the same rule bounced every glibc binary at load.
    let proc = a_process(49_391);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let free = a_free_address(sys).await;
            let one_past_the_page = free + 1;

            assert_eq!(
                sys.sys_mmap(
                    one_past_the_page,
                    PAGE,
                    RW,
                    FIXED_ANON_PRIVATE,
                    no_descriptor(),
                    0
                )
                .await,
                Err(LxError::EINVAL),
                "MAP_FIXED took an address that is not a page boundary"
            );
            assert_eq!(
                sys.sys_mmap(
                    one_past_the_page,
                    PAGE,
                    RW,
                    ANON_PRIVATE,
                    no_descriptor(),
                    0
                )
                .await,
                Ok(free),
                "the same address as a hint was not rounded down to its page"
            );
        })
    });
}

#[test]
fn a_fixed_noreplace_over_a_taken_range_is_eexist_and_replaces_nothing() {
    // `MAP_FIXED_NOREPLACE` is placed like `MAP_FIXED` and replaces nothing,
    // so the one thing that must not happen is the mapping already there
    // being taken away. The call that proves the flag is placing at all is
    // the second one: at a free address it lands exactly there, which a
    // hint would not promise.
    let proc = a_process(49_392);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let taken = anon(sys, 1, ANON_PRIVATE).await.expect("one page");
            let was_there = the_object_behind(sys.zircon_process(), taken);
            let held = mapped(sys.zircon_process());

            assert_eq!(
                sys.sys_mmap(taken, PAGE, RW, NOREPLACE_ANON_PRIVATE, no_descriptor(), 0)
                    .await,
                Err(LxError::EEXIST),
                "MAP_FIXED_NOREPLACE replaced a range that was taken"
            );
            assert!(
                Arc::ptr_eq(&was_there, &the_object_behind(sys.zircon_process(), taken)),
                "the refusal swapped the object that was already mapped there"
            );
            assert_eq!(
                mapped(sys.zircon_process()),
                held,
                "the refusal changed what the process has mapped"
            );

            let free = a_free_address(sys).await;
            assert_eq!(
                sys.sys_mmap(free, PAGE, RW, NOREPLACE_ANON_PRIVATE, no_descriptor(), 0)
                    .await,
                Ok(free),
                "and at a free address it has to land exactly where it was told"
            );
        })
    });
}

#[test]
fn a_shared_anonymous_mapping_is_marked_to_stay_one_object_across_a_fork() {
    // Nothing a caller can see says which of the two this is: both return an
    // address and both read as zeros. The difference only appears in a
    // child, where the unmarked one has been privatized and the counter
    // page a preforked pool shares stops being shared. So it is asked of
    // the object.
    let proc = a_process(49_393);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let shared = anon(sys, 1, ANON_SHARED).await.expect("MAP_SHARED");
            let private = anon(sys, 1, ANON_PRIVATE).await.expect("MAP_PRIVATE");

            assert!(
                the_object_behind(sys.zircon_process(), shared).is_share_on_fork(),
                "a MAP_SHARED anonymous mapping was not marked to be shared on a fork"
            );
            assert!(
                !the_object_behind(sys.zircon_process(), private).is_share_on_fork(),
                "a MAP_PRIVATE anonymous mapping was marked as shared"
            );
        })
    });
}

#[test]
fn two_shared_mappings_of_one_file_are_the_same_object() {
    // The rule the file arm exists for. Two descriptors, two calls, one
    // object: a store through either mapping has to be a store the other
    // one sees, and a per-call copy would pass every other check -- the
    // address is valid, the length is right and the bytes read correctly.
    let (proc, one, two) = a_process_with_one_file_open_twice(49_394, "mmapshared");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let first = sys
                .sys_mmap(0, PAGE, RW, FILE_SHARED, FileDesc::from(one), 0)
                .await
                .expect("the first shared mapping");
            let second = sys
                .sys_mmap(0, PAGE, RW, FILE_SHARED, FileDesc::from(two), 0)
                .await
                .expect("the second shared mapping");
            assert_ne!(first, second, "the two mappings are at two addresses");

            let a = the_object_behind(sys.zircon_process(), first);
            let b = the_object_behind(sys.zircon_process(), second);
            assert!(
                Arc::ptr_eq(&a, &b),
                "two MAP_SHARED mappings of one file got two objects, so stores through one are invisible through the other"
            );
            assert!(
                a.is_share_on_fork(),
                "a shared file mapping was not marked to be shared on a fork either"
            );
        })
    });
}

#[test]
fn a_private_mapping_of_the_same_file_is_a_snapshot_of_its_own() {
    // The other side of the same decision, and the reason the shared one
    // cannot be satisfied by handing everybody a private object: MAP_PRIVATE
    // has to be the per-call snapshot, and it must not be the object the
    // shared mappers are writing through.
    let (proc, one, two) = a_process_with_one_file_open_twice(49_395, "mmapprivate");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let shared = sys
                .sys_mmap(0, PAGE, RW, FILE_SHARED, FileDesc::from(one), 0)
                .await
                .expect("a shared mapping");
            let private = sys
                .sys_mmap(0, PAGE, RW, FILE_PRIVATE, FileDesc::from(two), 0)
                .await
                .expect("a private mapping");

            let s = the_object_behind(sys.zircon_process(), shared);
            let p = the_object_behind(sys.zircon_process(), private);
            assert!(
                !Arc::ptr_eq(&s, &p),
                "a private mapping of the file got the shared object, so its writes reach every other mapper"
            );
            assert!(
                !p.is_share_on_fork(),
                "a private file mapping was marked to be shared on a fork"
            );
        })
    });
}

#[test]
fn a_shared_mapping_of_a_pipe_is_refused_by_the_pipe_and_not_by_the_descriptor() {
    // The pipe passes everything mmap checks before it asks for an object:
    // the read end is readable, so the access check lets a PROT_READ
    // MAP_SHARED through, and the length and the offset are ordinary. What
    // refuses it is the pipe itself, which has no object to hand over, and
    // the errno has to be that answer and not an invented one.
    let (proc, fd) = a_process_with_a_pipe(49_396);
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let held = mapped(sys.zircon_process());
            assert_eq!(
                sys.sys_mmap(0, PAGE, R, FILE_SHARED, FileDesc::from(fd), 0)
                    .await,
                Err(LxError::ENOSYS),
                "a shared mapping of a pipe was not refused by the pipe"
            );
            assert_eq!(
                mapped(sys.zircon_process()),
                held,
                "the refused mapping of a pipe still took address space"
            );
        })
    });
}

#[test]
fn a_private_mapping_of_a_pipe_is_refused_too_and_by_its_own_call() {
    // `MAP_PRIVATE` asks a different method of the descriptor, with its own
    // error path, so it is its own question. A descriptor that cannot be
    // mapped shared cannot be mapped private either.
    let (proc, fd) = a_process_with_a_pipe(49_397);
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            assert_eq!(
                sys.sys_mmap(0, PAGE, R, FILE_PRIVATE, FileDesc::from(fd), 0)
                    .await,
                Err(LxError::ENOSYS),
                "a private mapping of a pipe was not refused by the pipe"
            );
        })
    });
}

#[test]
fn a_fixed_mapping_of_a_file_outside_the_address_space_is_refused_and_maps_nothing() {
    // The placement is the last thing the file arm can fail on, and it fails
    // with the object already in hand -- so the one thing to check is that a
    // mapping that could not be placed leaves nothing behind, neither a
    // partial mapping nor the address space it asked for.
    //
    // The address is the one past the end of this process's own region,
    // which is outside it whatever the host thinks of low addresses: a
    // mapping under `mmap_min_addr` is not the same refusal here and in the
    // runner, and a test must not depend on which.
    let (proc, one, _two) = a_process_with_one_file_open_twice(49_398, "mmapoutside");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let held = mapped(sys.zircon_process());
            let past_the_end = sys.zircon_process().vmar().end_addr();
            let answer = sys
                .sys_mmap(
                    past_the_end,
                    PAGE,
                    R,
                    FIXED_FILE_PRIVATE,
                    FileDesc::from(one),
                    0,
                )
                .await;
            assert!(
                answer.is_err(),
                "a file mapping was placed outside the address space: {:?}",
                answer
            );
            assert_eq!(
                mapped(sys.zircon_process()),
                held,
                "the mapping that could not be placed still took address space"
            );
        })
    });
}
