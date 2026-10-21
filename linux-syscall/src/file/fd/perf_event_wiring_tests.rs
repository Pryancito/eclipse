//! `perf_event_open`, through the syscall: what it judges before it reads
//! anything from user memory, how much of the `perf_event_attr` it reads, and
//! what of that image reaches the event.
//!
//! `perf_open_flag_tests` next door covers `perf_open_plan` and
//! `perf_attr_size_tests` covers `perf_attr_size`, which between them are
//! every decision this call makes about its five arguments -- and both call
//! those functions directly. What they cannot see:
//!
//! **The order.** `SYSCALL_DEFINE5(perf_event_open)` reads the flag word
//! before the attr, so a flag this kernel has never heard of has to be
//! `EINVAL` even when there is nothing behind the pointer, and the `EFAULT`
//! of a null pointer has to come after it.
//!
//! **That the attr image arrives whole.** `read_format` lives at byte 32 of
//! the struct and decides how many bytes a `read(2)` of the event hands back,
//! so a `read` is how a test sees that the bytes the syscall copied are the
//! bytes the event was built from.
//!
//! **That a `pid` of 0 becomes the caller's own.** `perf_event_open(2)` reads
//! 0 as "the calling process", and `event_matches` in `linux-object` says
//! what the alternative is: an event that kept the literal 0 matches no
//! sample ever taken, which is silence rather than an error -- an empty
//! profile from a `perf record ./prog` that reported no trouble at all.
//!
//! Fixed pids live in 47_42x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::vec::Vec;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// `perf_event_open(2)`'s flags (`<linux/perf_event.h>`).
const PERF_FLAG_FD_NO_GROUP: usize = 1;
/// `PERF_FLAG_FD_OUTPUT`: the event writes into the leader's ring buffer.
/// This kernel gives every event a ring of its own, so the flag is accepted
/// and not modelled -- refusing it would be a rule Linux does not have.
const PERF_FLAG_FD_OUTPUT: usize = 2;
const PERF_FLAG_PID_CGROUP: usize = 4;
const PERF_FLAG_FD_CLOEXEC: usize = 8;
/// A bit outside `PERF_FLAG_ALL`, which is how a caller finds out this kernel
/// is older than the flag it asked for.
const A_PERF_FLAG_OF_NOBODY: usize = 16;

/// The three arguments' "no such thing" value, which is -1 for all of them
/// and 0 for none of them.
const ANY_PID: i32 = -1;
const ANY_CPU: i32 = -1;
const NO_GROUP: i32 = -1;
/// `pid = 0`: the calling process, and not the process whose id is zero.
const THE_CALLER: i32 = 0;
/// A descriptor nobody opened.
const A_FD_OF_NOBODY: i32 = 90;

/// Byte offsets inside `struct perf_event_attr`. They are UABI and they live
/// in a private module of `linux-object`, so they are written out here, which
/// is also what a program building the struct does.
const ATTR_OFF_SIZE: usize = 4;
const ATTR_OFF_SAMPLE_PERIOD: usize = 16;
const ATTR_OFF_READ_FORMAT: usize = 32;

/// `read_format` bits: each one adds eight bytes to what a `read(2)` of the
/// event hands back.
const PERF_FORMAT_TOTAL_TIME_ENABLED: u64 = 1 << 0;
const PERF_FORMAT_TOTAL_TIME_RUNNING: u64 = 1 << 1;
const PERF_FORMAT_ID: u64 = 1 << 2;

/// A sample period that is not 1, so the count a sample adds is recognisable
/// and not a number the event could have arrived at by itself.
const A_PERIOD: u64 = 7;
/// A cpu a sample can be taken on without an event having been opened for
/// it, which is the other half of the match an event makes.
const A_CPU_THAT_IS_NOT_ZERO: u32 = 5;

/// `fcntl(2)` commands.
const F_GETFD: usize = 1;
const F_GETFL: usize = 3;
const FD_CLOEXEC: usize = 1;
/// `O_RDWR`, which is what every perf event is opened as.
const O_RDWR: usize = 2;

/// A process of its own, because the registry of perf events is shared by
/// every process in this binary and a sample reaches all of them.
fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

/// A `perf_event_attr` image as a program builds one: `bytes` long, saying in
/// its `size` field how much of itself the kernel may read.
fn an_attr(size: usize, bytes: usize) -> Vec<u8> {
    let mut attr = alloc::vec![0u8; bytes];
    attr[ATTR_OFF_SIZE..ATTR_OFF_SIZE + 4].copy_from_slice(&(size as u32).to_ne_bytes());
    attr
}

fn put_u64(attr: &mut [u8], offset: usize, value: u64) {
    attr[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
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

fn perf_open(
    sys: &Syscall<'_>,
    attr: &[u8],
    pid: i32,
    cpu: i32,
    group_fd: i32,
    flags: usize,
) -> SysResult {
    perf_open_at(sys, attr.as_ptr() as usize, pid, cpu, group_fd, flags)
}

/// The same with the pointer spelled out, so a test can pass the null one.
fn perf_open_at(
    sys: &Syscall<'_>,
    attr_ptr: usize,
    pid: i32,
    cpu: i32,
    group_fd: i32,
    flags: usize,
) -> SysResult {
    sys.sys_perf_event_open(attr_ptr, pid, cpu, group_fd, flags)
}

/// How many bytes a `read(2)` of the event hands back into a buffer of `len`,
/// which is what `read_format` decides.
async fn read_of(sys: &Syscall<'_>, fd: usize, len: usize) -> SysResult {
    let mut buf = alloc::vec![0u8; len];
    sys.sys_read(
        FileDesc::from(fd),
        UserOutPtr::from(buf.as_mut_ptr() as usize),
        len,
    )
    .await
}

/// The count of a counting event, which is what `perf stat` reads.
async fn count_of(sys: &Syscall<'_>, fd: usize) -> Result<u64, LxError> {
    let mut buf = [0u8; 8];
    let got = sys
        .sys_read(
            FileDesc::from(fd),
            UserOutPtr::from(buf.as_mut_ptr() as usize),
            8,
        )
        .await?;
    assert_eq!(got, 8, "a counting event reads back eight bytes or nothing");
    Ok(u64::from_ne_bytes(buf))
}

async fn is_cloexec(sys: &Syscall<'_>, fd: usize) -> Result<bool, LxError> {
    Ok(sys.sys_fcntl(FileDesc::from(fd), F_GETFD, 0).await? & FD_CLOEXEC != 0)
}

async fn status_flags(sys: &Syscall<'_>, fd: usize) -> SysResult {
    sys.sys_fcntl(FileDesc::from(fd), F_GETFL, 0).await
}

#[test]
fn the_flag_word_is_judged_before_anything_is_read_from_user_memory() {
    // `perf_event_open` used to read its flags through
    // `OpenFlags::from_bits_truncate`, which reads `PERF_FLAG_*` bits as
    // `O_*` bits and drops what it does not recognise: a flag this kernel
    // had never heard of was accepted. The refusal has to come out of the
    // flag word alone, with nothing behind the attr pointer at all, because
    // a caller probing for a newer kernel passes a flag and a real struct
    // and must be told about the flag.
    let proc = a_process(47_420, "perf-flags");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let free = lp.get_free_fd();
            assert!(
                kernel_hal::online_cpu_count() >= 1,
                "this test wants at least one cpu to open an event on"
            );

            assert_eq!(
                perf_open_at(sys, 0, ANY_PID, 0, NO_GROUP, A_PERF_FLAG_OF_NOBODY),
                Err(LxError::EINVAL),
                "the flag word is read before the attr pointer"
            );
            assert_eq!(
                perf_open_at(sys, 0, ANY_PID, 0, NO_GROUP, 0),
                Err(LxError::EFAULT),
                "a null attr pointer, with a flag word that is fine"
            );

            // `PERF_FLAG_PID_CGROUP` makes `pid` the descriptor of a cgroup
            // directory: both arguments are refused at -1 first, and then
            // the `fdget` of `perf_cgroup_connect` finds no cgroup, because
            // this kernel has none. Both answers arrive without the attr
            // being read.
            assert_eq!(
                perf_open_at(sys, 0, ANY_PID, 0, NO_GROUP, PERF_FLAG_PID_CGROUP),
                Err(LxError::EINVAL),
                "a cgroup event wants a pid"
            );
            assert_eq!(
                perf_open_at(sys, 0, 1, ANY_CPU, NO_GROUP, PERF_FLAG_PID_CGROUP),
                Err(LxError::EINVAL),
                "a cgroup event wants a cpu"
            );
            assert_eq!(
                perf_open_at(sys, 0, 1, 0, NO_GROUP, PERF_FLAG_PID_CGROUP),
                Err(LxError::EBADF),
                "there are no cgroups here"
            );

            assert_eq!(
                lp.get_free_fd(),
                free,
                "a refused perf_event_open spent a descriptor anyway"
            );
        })
    });
}

#[test]
fn an_event_needs_a_task_or_a_cpu_and_the_cpu_has_to_exist() {
    // `find_get_context`: an event with no task (`pid == -1`) needs a CPU,
    // and `cpu >= nr_cpu_ids` is `EINVAL` whether or not there is a task.
    // The count of CPUs is asked of the HAL by the syscall, so a test that
    // called `perf_open_plan` itself would be choosing the answer.
    let proc = a_process(47_421, "perf-where");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let ncpus = kernel_hal::online_cpu_count();
            assert!(ncpus >= 1, "this test wants at least one cpu");
            let own = sys.zircon_process().id() as i32;
            let attr = an_attr(0, PERF_ATTR_SIZE_VER0);

            assert_eq!(
                perf_open(sys, &attr, ANY_PID, ANY_CPU, NO_GROUP, 0),
                Err(LxError::EINVAL),
                "an event with no task and no cpu profiles nothing"
            );
            assert_eq!(
                perf_open(sys, &attr, ANY_PID, ncpus as i32, NO_GROUP, 0),
                Err(LxError::EINVAL),
                "a cpu one past the last one"
            );
            assert_eq!(
                perf_open(sys, &attr, own, ncpus as i32, NO_GROUP, 0),
                Err(LxError::EINVAL),
                "a task event is held to the same cpu test"
            );
            assert!(
                perf_open(sys, &attr, ANY_PID, 0, NO_GROUP, 0).is_ok(),
                "a system-wide event on cpu 0"
            );
            assert!(
                perf_open(sys, &attr, own, ANY_CPU, NO_GROUP, 0).is_ok(),
                "a task event on any cpu, which is what perf record opens"
            );
        })
    });
}

#[test]
fn the_attr_size_the_caller_wrote_decides_how_much_is_read() {
    // `perf_copy_attr`: a zero `size` means version 0, and anything else out
    // of range is `E2BIG`. What this used to do was clamp, which answers a
    // request the kernel cannot honour by quietly reading a different struct
    // than the one the caller built.
    let proc = a_process(47_422, "perf-attr-size");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let own = sys.zircon_process().id() as i32;

            // Zero means version 0, which is 64 bytes: a buffer of exactly
            // that is enough, so nothing beyond it was read.
            let version_zero = an_attr(0, PERF_ATTR_SIZE_VER0);
            assert!(
                perf_open(sys, &version_zero, own, ANY_CPU, NO_GROUP, 0).is_ok(),
                "a size of 0 is version 0 and reads 64 bytes"
            );

            let too_small = an_attr(PERF_ATTR_SIZE_VER0 - 1, PERF_ATTR_SIZE_VER0);
            assert_eq!(
                perf_open(sys, &too_small, own, ANY_CPU, NO_GROUP, 0),
                Err(LxError::E2BIG),
                "a struct smaller than version 0 is not version 0"
            );

            let past_a_page = an_attr(PAGE_SIZE + 1, PERF_ATTR_SIZE_VER0);
            assert_eq!(
                perf_open(sys, &past_a_page, own, ANY_CPU, NO_GROUP, 0),
                Err(LxError::E2BIG),
                "a struct larger than a page"
            );

            // A whole page is the most it reads, and it does read all of it.
            let a_page = an_attr(PAGE_SIZE, PAGE_SIZE);
            assert!(
                perf_open(sys, &a_page, own, ANY_CPU, NO_GROUP, 0).is_ok(),
                "a page is the largest attr there is"
            );
        })
    });
}

#[test]
fn a_tail_past_version_zero_that_is_not_empty_is_e2big() {
    // `perf_copy_attr` ends by requiring that everything past what this
    // kernel understands be zero: a caller that set a field the kernel does
    // not know about asked for something it is not getting, and an event
    // that ignores it is an event behaving differently from what was asked
    // with no way to tell.
    let proc = a_process(47_423, "perf-attr-tail");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let own = sys.zircon_process().id() as i32;
            let bigger_than_this_kernel_reads = PERF_ATTR_SIZE_VER0 * 2;

            let empty_tail = an_attr(bigger_than_this_kernel_reads, bigger_than_this_kernel_reads);
            assert!(
                perf_open(sys, &empty_tail, own, ANY_CPU, NO_GROUP, 0).is_ok(),
                "a zero tail is a caller asking for nothing new"
            );

            let mut first_byte = empty_tail.clone();
            first_byte[PERF_ATTR_SIZE_VER0] = 1;
            assert_eq!(
                perf_open(sys, &first_byte, own, ANY_CPU, NO_GROUP, 0),
                Err(LxError::E2BIG),
                "the first byte past version 0 was set and ignored"
            );

            let mut last_byte = empty_tail;
            last_byte[bigger_than_this_kernel_reads - 1] = 0x80;
            assert_eq!(
                perf_open(sys, &last_byte, own, ANY_CPU, NO_GROUP, 0),
                Err(LxError::E2BIG),
                "the last byte of the attr is part of the tail too"
            );
        })
    });
}

#[test]
fn what_the_attr_asks_to_be_read_back_is_what_a_read_hands_over() {
    // `read_format` is the `u64` at byte 32 of the struct, and it decides
    // how many eight-byte fields a `read(2)` of the event hands back. So a
    // read is how a test sees that the bytes the syscall copied out of user
    // memory are the bytes the event was built from -- there is no getter
    // for the attr, and that is the point: this is the way a program sees
    // it too.
    let proc = a_process(47_424, "perf-read-format");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let own = sys.zircon_process().id() as i32;

            let plain = an_attr(0, PERF_ATTR_SIZE_VER0);
            let counting = perf_open(sys, &plain, own, ANY_CPU, NO_GROUP, 0).unwrap();
            assert_eq!(
                read_of(sys, counting, 8).await,
                Ok(8),
                "a counting event hands back its count and nothing else"
            );
            assert_eq!(
                read_of(sys, counting, 4).await,
                Err(LxError::ENOSPC),
                "half of a count is not a smaller count"
            );

            let mut asking_for_three = an_attr(0, PERF_ATTR_SIZE_VER0);
            put_u64(
                &mut asking_for_three,
                ATTR_OFF_READ_FORMAT,
                PERF_FORMAT_TOTAL_TIME_ENABLED | PERF_FORMAT_TOTAL_TIME_RUNNING | PERF_FORMAT_ID,
            );
            let scaled = perf_open(sys, &asking_for_three, own, ANY_CPU, NO_GROUP, 0).unwrap();
            assert_eq!(
                read_of(sys, scaled, 32).await,
                Ok(32),
                "the three extras of read_format did not arrive at the event"
            );
            assert_eq!(
                read_of(sys, scaled, 24).await,
                Err(LxError::ENOSPC),
                "a buffer one field short is refused, not filled with a prefix"
            );
        })
    });
}

#[test]
fn a_group_fd_that_is_not_an_open_perf_event_is_ebadf() {
    // `perf_fget_light`: a `group_fd` other than -1 has to be an open perf
    // event. It was not looked at at all, so a bogus one opened an event
    // anyway -- and the check runs before the flags are read, so
    // `PERF_FLAG_FD_NO_GROUP` does not mean "do not look at it": it means
    // the event is not joined to the leader.
    let proc = a_process(47_425, "perf-group");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let own = sys.zircon_process().id() as i32;
            let attr = an_attr(0, PERF_ATTR_SIZE_VER0);
            let leader = perf_open(sys, &attr, own, ANY_CPU, NO_GROUP, 0).unwrap() as i32;
            let not_a_perf_event = sys.sys_eventfd2(0, 0).unwrap() as i32;

            assert!(
                perf_open(sys, &attr, own, ANY_CPU, leader, 0).is_ok(),
                "an open perf event is a group leader"
            );
            assert_eq!(
                perf_open(sys, &attr, own, ANY_CPU, not_a_perf_event, 0),
                Err(LxError::EBADF),
                "an eventfd is not a perf event"
            );
            assert_eq!(
                perf_open(sys, &attr, own, ANY_CPU, A_FD_OF_NOBODY, 0),
                Err(LxError::EBADF),
                "a descriptor nobody opened is not a perf event"
            );
            assert_eq!(
                perf_open(
                    sys,
                    &attr,
                    own,
                    ANY_CPU,
                    not_a_perf_event,
                    PERF_FLAG_FD_NO_GROUP
                ),
                Err(LxError::EBADF),
                "the leader is checked whatever the flags say"
            );
            assert!(
                perf_open(sys, &attr, own, ANY_CPU, NO_GROUP, PERF_FLAG_FD_NO_GROUP).is_ok(),
                "-1 is no leader at all, which is not an error"
            );
        })
    });
}

#[test]
fn a_pid_of_zero_is_the_calling_process() {
    // What `perf record ./prog` and every program that profiles itself
    // opens. Passing the literal 0 through made the sampler compare it
    // against the real pid of whoever was running, so it matched nothing and
    // the profile came out empty -- and an empty profile is not an error
    // anywhere: the call succeeds, the fd works, and `perf report` says
    // there are no samples.
    //
    // Asked of two events at once, one for this process and one for a pid
    // nobody has, so what is being tested is the match and not that samples
    // arrive at all.
    let proc = a_process(47_426, "perf-pid-zero");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let own = sys.zircon_process().id() as i32;
            let mut attr = an_attr(0, PERF_ATTR_SIZE_VER0);
            put_u64(&mut attr, ATTR_OFF_SAMPLE_PERIOD, A_PERIOD);

            let mine = perf_open(sys, &attr, THE_CALLER, ANY_CPU, NO_GROUP, 0).unwrap();
            let somebody_elses = perf_open(sys, &attr, own + 1000, ANY_CPU, NO_GROUP, 0).unwrap();
            let on_cpu_zero = perf_open(sys, &attr, THE_CALLER, 0, NO_GROUP, 0).unwrap();
            assert_eq!(count_of(sys, mine).await, Ok(0), "a new event has no count");
            assert_eq!(count_of(sys, on_cpu_zero).await, Ok(0));

            // One sample taken on this process on cpu 0, as the
            // timer-interrupt return path takes them.
            linux_object::fs::perf_sample_user(own, own, 0, 0xdead_beef);

            assert_eq!(
                count_of(sys, mine).await,
                Ok(A_PERIOD),
                "a pid of 0 did not resolve to the calling process"
            );
            assert_eq!(
                count_of(sys, somebody_elses).await,
                Ok(0),
                "an event opened for another process took this one's sample"
            );
            assert_eq!(
                count_of(sys, on_cpu_zero).await,
                Ok(A_PERIOD),
                "an event bound to cpu 0 missed a sample taken on cpu 0"
            );

            // And the cpu side of the same match: the event bound to cpu 0
            // does not take a sample from another cpu, where the one opened
            // for any cpu takes both.
            linux_object::fs::perf_sample_user(own, own, A_CPU_THAT_IS_NOT_ZERO, 0xdead_beef);

            assert_eq!(
                count_of(sys, on_cpu_zero).await,
                Ok(A_PERIOD),
                "an event bound to cpu 0 took a sample from another cpu"
            );
            assert_eq!(
                count_of(sys, mine).await,
                Ok(A_PERIOD * 2),
                "an event opened for any cpu takes every sample"
            );
            assert_eq!(count_of(sys, somebody_elses).await, Ok(0));
        })
    });
}

#[test]
fn perf_flag_fd_cloexec_is_the_only_flag_that_reaches_the_descriptor() {
    // The perf flags are not open flags, and reading them as such is how
    // `PERF_FLAG_FD_CLOEXEC` (8, which is no open flag at all) left the fd
    // `perf record` opens alive across the `execve` of the profiled
    // program, while `PERF_FLAG_FD_NO_GROUP` (1) was read as `O_WRONLY`.
    let proc = a_process(47_427, "perf-cloexec");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let own = sys.zircon_process().id() as i32;
            let attr = an_attr(0, PERF_ATTR_SIZE_VER0);

            let plain = perf_open(sys, &attr, own, ANY_CPU, NO_GROUP, 0).unwrap();
            assert_eq!(
                is_cloexec(sys, plain).await,
                Ok(false),
                "an event nobody asked to close on exec"
            );
            assert_eq!(
                status_flags(sys, plain).await,
                Ok(O_RDWR),
                "a perf event is opened read-write"
            );

            let closing =
                perf_open(sys, &attr, own, ANY_CPU, NO_GROUP, PERF_FLAG_FD_CLOEXEC).unwrap();
            assert_eq!(
                is_cloexec(sys, closing).await,
                Ok(true),
                "PERF_FLAG_FD_CLOEXEC did not reach the descriptor"
            );

            let no_group =
                perf_open(sys, &attr, own, ANY_CPU, NO_GROUP, PERF_FLAG_FD_NO_GROUP).unwrap();
            assert_eq!(
                status_flags(sys, no_group).await,
                Ok(O_RDWR),
                "a perf flag was read as an open flag"
            );
            assert_eq!(
                is_cloexec(sys, no_group).await,
                Ok(false),
                "PERF_FLAG_FD_NO_GROUP is not PERF_FLAG_FD_CLOEXEC"
            );

            // The flag this kernel accepts and does not model: every event
            // has a ring of its own, so there is nothing to share, and
            // Linux takes the flag all the same.
            let shared_ring =
                perf_open(sys, &attr, own, ANY_CPU, NO_GROUP, PERF_FLAG_FD_OUTPUT).unwrap();
            assert_eq!(
                status_flags(sys, shared_ring).await,
                Ok(O_RDWR),
                "PERF_FLAG_FD_OUTPUT is not an open flag either"
            );
            assert_eq!(is_cloexec(sys, shared_ring).await, Ok(false));
        })
    });
}

#[test]
fn the_arguments_arrive_in_the_order_the_syscall_table_names_them() {
    // Five arguments, through the table, the way userspace calls it. Two of
    // them are checked by what the event turns out to be: the flag word by
    // the close-on-exec bit of the descriptor, and the pid by a sample for
    // this process reaching the event.
    let proc = a_process(47_428, "perf-by-number");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let own = sys.zircon_process().id() as i32;
            let mut attr = an_attr(0, PERF_ATTR_SIZE_VER0);
            put_u64(&mut attr, ATTR_OFF_SAMPLE_PERIOD, A_PERIOD);

            let fd = by_number(
                sys,
                crate::Sys::PERF_EVENT_OPEN,
                [
                    attr.as_ptr() as usize,
                    THE_CALLER as usize,
                    ANY_CPU as usize,
                    NO_GROUP as usize,
                    PERF_FLAG_FD_CLOEXEC,
                    0,
                ],
            )
            .await;
            assert!(fd >= 0, "perf_event_open by number answered {}", fd);
            let fd = fd as usize;
            assert_eq!(
                is_cloexec(sys, fd).await,
                Ok(true),
                "the fifth argument is the flag word"
            );

            linux_object::fs::perf_sample_user(own, own, 0, 0xdead_beef);
            assert_eq!(
                count_of(sys, fd).await,
                Ok(A_PERIOD),
                "the second argument is the pid"
            );
        })
    });
}
