//! `pselect6(2)`, `ppoll(2)`, `epoll_wait(2)` and `epoll_pwait2(2)` called by
//! their own numbers: the four entry points of this file that had never run.
//!
//! `select_core` and `sys_poll` are reached by `end_to_end_tests`, and every
//! pure helper around them -- `select_time_left`, `select_nfds`,
//! `epoll_maxevents`, `PSelectSigsetArg`'s layout, `TimeSpec::
//! try_into_poll_msecs` -- has tests of its own. What had never been asked
//! for even once is the wiring between them: `sys_pselect6` sat at 37 of its
//! lines at zero and `sys_ppoll` at 22, which is the whole of both bodies,
//! and `sys_epoll_wait` at 10, which is the whole of the legacy four
//! argument form every static binary in the tree actually calls.
//!
//! That matters more here than the line count says, because the one bug this
//! file records as fixed lives in exactly that gap. `pselect6`'s timeout is a
//! `timespec`, in NANOseconds; it was once parsed as `select`'s `timeval`, in
//! MICROseconds, and glibc routes plain `select()` through `pselect6`, so
//! every timeout came out a thousand times too long and a 100 ms select slept
//! 100 seconds. busybox ash's line editor and every terminal probe sat
//! wedged that way. The helper that converts correctly is tested where it
//! lives; nothing said which of the two this syscall hands its bytes to.
//!
//! What makes the timeout observable without waiting out any of it is the
//! write-back: `poll_select_finish` hands the caller the time LEFT, so a call
//! that returns at once on a ready descriptor reports very nearly the whole
//! timeout -- in the units the kernel read it in. A timeout of two seconds
//! comes back as one second and change, and which field holds the change says
//! whether it was read as nanoseconds or as microseconds.
//!
//! A node on RamFS is always ready both ways, so every wait here finishes on
//! its first pass: no network stack, no device and no timer are involved, and
//! a wait that did not finish would be caught by `on_a_thread`'s own timeout
//! rather than block the suite.
//!
//! Fixed pids live in 48_6xx.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use linux_object::fs::{File, OpenFlags};
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// `POLLIN | POLLOUT`, the two bits a reader and a writer ask for.
const IN_AND_OUT: u16 = 0x0001 | 0x0004;

/// `EPOLL_CTL_ADD` (`/usr/include/linux/eventpoll.h`).
const CTL_ADD: i32 = 1;

/// A descriptor no process here holds: the sets and the entries below never
/// reach it, and nothing can hand it out, so it is closed by construction.
const A_CLOSED_FD: usize = 100;

/// `sizeof(sigset_t)` as this kernel and every libc agree on it. Written out
/// rather than taken from `size_of::<Sigset>()`, which would still match
/// itself if the type changed under it.
const SIGSETSIZE: usize = 8;

/// A process whose root holds a `/<top>` of its own, because a lookup from
/// the root goes through a path cache every process in this binary shares.
fn a_process(pid: KoID, name: &str, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

/// A node of `dir_path` opened read-write, and the descriptor it got. Any
/// kind of node on RamFS answers ready for reading and for writing, which is
/// what makes these answers knowable with nothing else running.
fn a_ready_node(lp: &LinuxProcess, dir_path: &str, type_: FileType) -> usize {
    let dir = lp.lookup_inode(dir_path).unwrap();
    let inode = dir.create("f", type_, 0o644).unwrap();
    let path = alloc::format!("{}/f", dir_path);
    let fd: i32 = lp
        .add_file(File::new(inode, OpenFlags::RDWR, path))
        .unwrap()
        .into();
    fd as usize
}

/// An `fd_set` in host memory: one `u32` per 32 descriptors, sized by `nfds`
/// as the array a caller declares is.
fn a_set(nfds: usize, fds: &[usize]) -> Vec<u32> {
    let mut words = alloc::vec![0u32; nfds.div_ceil(FD_PER_ITEM).max(1)];
    for &fd in fds {
        words[fd / FD_PER_ITEM] |= 1 << (fd % FD_PER_ITEM);
    }
    words
}

/// One `pollfd` of a caller's array.
fn an_entry(fd: i32, events: u16) -> PollFd {
    PollFd {
        fd: FileDesc::from(fd),
        events: PollEvents::from_bits_truncate(events),
        revents: PollEvents::empty(),
    }
}

/// `pselect6(nfds, &read, &write, &except, &timeout, sigset_arg)` by the
/// syscall, with all four of its pointers aimed at the caller's own memory.
///
/// `sys_pselect6` wants `&mut self` and `on_a_thread` lends a shared one, so
/// this builds its own out of the same three parts, as `dispatch_tests::
/// by_number` does.
#[allow(clippy::too_many_arguments)]
async fn pselect6(
    sys: &Syscall<'_>,
    nfds: usize,
    read: &mut [u32],
    write: &mut [u32],
    except: &mut [u32],
    timeout: Option<&mut TimeSpec>,
    sigset_arg: usize,
) -> SysResult {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    let at = match timeout {
        Some(ts) => ts as *mut TimeSpec as usize,
        None => 0,
    };
    own.sys_pselect6(
        nfds,
        UserInOutPtr::from(read.as_mut_ptr() as usize),
        UserInOutPtr::from(write.as_mut_ptr() as usize),
        UserInOutPtr::from(except.as_mut_ptr() as usize),
        UserInOutPtr::from(at),
        sigset_arg,
    )
    .await
}

/// `select(nfds, &read, &write, &except, &timeout)` by the syscall, for the
/// one thing it does that `pselect6` does not: hand the time left back in a
/// `timeval`.
async fn select(
    sys: &Syscall<'_>,
    nfds: usize,
    read: &mut [u32],
    write: &mut [u32],
    except: &mut [u32],
    timeout: &mut TimeVal,
) -> SysResult {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    own.sys_select(
        nfds,
        UserInOutPtr::from(read.as_mut_ptr() as usize),
        UserInOutPtr::from(write.as_mut_ptr() as usize),
        UserInOutPtr::from(except.as_mut_ptr() as usize),
        UserInOutPtr::from(timeout as *mut TimeVal as usize),
    )
    .await
}

/// `ppoll(ufds, nfds, &timeout, &sigmask, sigsetsize)` by the syscall. A
/// `None` timeout is the null pointer that means no limit, and a `None` mask
/// the null pointer that means "leave mine alone".
async fn ppoll(
    sys: &Syscall<'_>,
    set: &mut [PollFd],
    timeout: Option<&TimeSpec>,
    sigmask: Option<&Sigset>,
    sigsetsize: usize,
) -> SysResult {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    let ts = match timeout {
        Some(t) => t as *const TimeSpec as usize,
        None => 0,
    };
    let mask = match sigmask {
        Some(m) => m as *const Sigset as usize,
        None => 0,
    };
    own.sys_ppoll(
        UserInOutPtr::from(set.as_mut_ptr() as usize),
        set.len(),
        UserInPtr::from(ts),
        UserInPtr::from(mask),
        sigsetsize,
    )
    .await
}

/// An epoll watching one ready node of `dir_path`, and its descriptor.
fn an_epoll_on_a_ready_node(sys: &Syscall<'_>, dir_path: &str) -> usize {
    let lp = sys.linux_process();
    // A device node and not a regular file: `can_epoll` refuses the latter.
    let fd = a_ready_node(&lp, dir_path, FileType::CharDevice);
    let epfd = sys.sys_epoll_create1(0).unwrap();
    let watch = EpollEvent {
        events: u32::from(IN_AND_OUT),
        data: 0,
    };
    sys.sys_epoll_ctl(
        FileDesc::from(epfd),
        CTL_ADD,
        FileDesc::from(fd),
        UserInPtr::from(&watch as *const EpollEvent as usize),
    )
    .unwrap();
    epfd
}

#[test]
fn pselect6_reads_its_timeout_in_nanoseconds_and_not_microseconds() {
    // The bug this file records as fixed, pinned for the first time. A
    // hundred milliseconds is `{0, 100_000_000}` as a `timespec`; read as a
    // `timeval` the same bytes are a hundred million MICROseconds, which is
    // a hundred seconds, and that is what glibc's `select()` used to get.
    //
    // The timeout is never waited out -- the descriptor is ready, so the
    // call returns on its first pass -- but the write-back reports what was
    // left of it, so the units are visible without spending any of them.
    // Under the old reading the answer would be a hundred seconds minus a
    // few microseconds: `sec` would be 99, not 0.
    const A_HUNDRED_MS: usize = 100_000_000;
    let proc = a_process(48_600, "pselect-ns", "pselect_ns");
    let dir_path = String::from("/pselect_ns");
    let (got, left) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = a_ready_node(&lp, &dir_path, FileType::File);
            let nfds = fd + 1;
            let mut read = a_set(nfds, &[fd]);
            let mut write = a_set(nfds, &[]);
            let mut except = a_set(nfds, &[]);
            let mut timeout = TimeSpec {
                sec: 0,
                nsec: A_HUNDRED_MS,
            };
            let got = pselect6(
                sys,
                nfds,
                &mut read,
                &mut write,
                &mut except,
                Some(&mut timeout),
                0,
            )
            .await;
            (got, timeout)
        })
    });

    assert_eq!(
        got,
        Ok(1),
        "a descriptor ready for reading was not reported"
    );
    assert_eq!(
        left.sec, 0,
        "a 100 ms timeout came back with whole seconds left of it, \
         which is what reading its nanoseconds as microseconds does"
    );
    assert!(
        left.nsec <= A_HUNDRED_MS,
        "more was left of the timeout than was asked for: {} ns",
        left.nsec
    );
    assert!(
        left.nsec > 0,
        "a timeout that was never waited on came back spent"
    );
}

#[test]
fn the_two_shapes_hand_back_the_time_left_each_in_its_own_unit() {
    // `select` and `pselect6` share a body and differ in the type of the
    // timeout they read and write: microseconds and nanoseconds. Asked for
    // two seconds and answered at once, each must report one second and
    // change left -- and the change is the discriminating part, because the
    // same remainder is about 999_900 in one unit and about 999_900_000 in
    // the other. A write-back in the wrong type would still look plausible
    // in `sec` alone.
    let proc = a_process(48_601, "time-left", "time_left");
    let dir_path = String::from("/time_left");
    let (selected, tv, pselected, ts) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = a_ready_node(&lp, &dir_path, FileType::File);
            let nfds = fd + 1;
            let mut read = a_set(nfds, &[fd]);
            let mut write = a_set(nfds, &[]);
            let mut except = a_set(nfds, &[]);
            let mut tv = TimeVal { sec: 2, usec: 0 };
            let selected = select(sys, nfds, &mut read, &mut write, &mut except, &mut tv).await;
            // The sets come back holding the answer, so they are built
            // again for the second call rather than reused.
            let mut read = a_set(nfds, &[fd]);
            let mut write = a_set(nfds, &[]);
            let mut except = a_set(nfds, &[]);
            let mut ts = TimeSpec { sec: 2, nsec: 0 };
            let pselected = pselect6(
                sys,
                nfds,
                &mut read,
                &mut write,
                &mut except,
                Some(&mut ts),
                0,
            )
            .await;
            (selected, tv, pselected, ts)
        })
    });

    assert_eq!(
        selected,
        Ok(1),
        "select did not report the ready descriptor"
    );
    assert_eq!(
        pselected,
        Ok(1),
        "pselect6 did not report the ready descriptor"
    );
    assert_eq!(
        tv.sec, 1,
        "select handed back {}s of a 2s timeout it never waited on",
        tv.sec
    );
    assert_eq!(
        ts.sec, 1,
        "pselect6 handed back {}s of a 2s timeout it never waited on",
        ts.sec
    );
    assert!(
        tv.usec > 900_000,
        "select's remainder is not a count of microseconds: {}",
        tv.usec
    );
    assert!(
        ts.nsec > 900_000_000,
        "pselect6's remainder is not a count of nanoseconds: {}",
        ts.nsec
    );
}

#[test]
fn the_sticky_timeouts_persona_leaves_the_callers_struct_alone() {
    // `STICKY_TIMEOUTS` is the one persona under which the write-back above
    // must not happen at all: the caller's struct comes back bit for bit as
    // it went in, however long the call took. Asked of both entry points,
    // and with the same two second timeout as the test above, so the pair
    // says that the persona is what decides and not the shape of the call.
    let proc = a_process(48_602, "sticky", "sticky");
    let dir_path = String::from("/sticky");
    let (selected, tv, pselected, ts) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = a_ready_node(&lp, &dir_path, FileType::File);
            lp.set_personality(STICKY_TIMEOUTS);
            let nfds = fd + 1;
            let mut read = a_set(nfds, &[fd]);
            let mut write = a_set(nfds, &[]);
            let mut except = a_set(nfds, &[]);
            let mut tv = TimeVal { sec: 2, usec: 0 };
            let selected = select(sys, nfds, &mut read, &mut write, &mut except, &mut tv).await;
            let mut read = a_set(nfds, &[fd]);
            let mut write = a_set(nfds, &[]);
            let mut except = a_set(nfds, &[]);
            let mut ts = TimeSpec { sec: 2, nsec: 0 };
            let pselected = pselect6(
                sys,
                nfds,
                &mut read,
                &mut write,
                &mut except,
                Some(&mut ts),
                0,
            )
            .await;
            (selected, tv, pselected, ts)
        })
    });

    assert_eq!(
        selected,
        Ok(1),
        "select did not report the ready descriptor"
    );
    assert_eq!(
        pselected,
        Ok(1),
        "pselect6 did not report the ready descriptor"
    );
    assert_eq!(
        (tv.sec, tv.usec),
        (2, 0),
        "the sticky persona did not stop select writing its timeout back"
    );
    assert_eq!(
        (ts.sec, ts.nsec),
        (2, 0),
        "the sticky persona did not stop pselect6 writing its timeout back"
    );
}

#[test]
fn pselect6_reads_its_sixth_argument_as_a_pointer_and_then_a_length() {
    // `pselect6`'s sixth argument is not a sigset but a pointer to
    // `{ const sigset_t *ss; size_t ss_len; }`, read out of the caller's
    // memory with one `read`, so the order of the two words IS the ABI.
    // `pselect_sigset_arg_layout_tests` pins the struct's offsets; nothing
    // said which word this syscall takes for which, and the two are
    // indistinguishable from inside the struct.
    //
    // Three answers say it: a pack whose length is the right one is
    // accepted, the same pack with a wrong length is EINVAL, and a null
    // sixth argument is no mask at all and asks nothing about either word.
    // Read the other way round, the accepted case would hand a pointer
    // value in as the length and be EINVAL too.
    let proc = a_process(48_603, "sigset-pack", "sigset_pack");
    let dir_path = String::from("/sigset_pack");
    let (with_a_mask, bad_length, no_pack) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = a_ready_node(&lp, &dir_path, FileType::File);
            let nfds = fd + 1;
            let mask = Sigset::new(0);
            let good = PSelectSigsetArg {
                ss: &mask as *const Sigset as usize,
                ss_len: SIGSETSIZE,
            };
            let short = PSelectSigsetArg {
                ss: &mask as *const Sigset as usize,
                ss_len: SIGSETSIZE / 2,
            };
            // Each call gets its own sets: the answer is written back into
            // them, so a reused one would be asking a different question.
            let mut read = a_set(nfds, &[fd]);
            let mut write = a_set(nfds, &[]);
            let mut except = a_set(nfds, &[]);
            let mut ts = TimeSpec { sec: 0, nsec: 0 };
            let with_a_mask = pselect6(
                sys,
                nfds,
                &mut read,
                &mut write,
                &mut except,
                Some(&mut ts),
                &good as *const PSelectSigsetArg as usize,
            )
            .await;
            let mut read = a_set(nfds, &[fd]);
            let mut write = a_set(nfds, &[]);
            let mut except = a_set(nfds, &[]);
            let mut ts = TimeSpec { sec: 0, nsec: 0 };
            let bad_length = pselect6(
                sys,
                nfds,
                &mut read,
                &mut write,
                &mut except,
                Some(&mut ts),
                &short as *const PSelectSigsetArg as usize,
            )
            .await;
            let mut read = a_set(nfds, &[fd]);
            let mut write = a_set(nfds, &[]);
            let mut except = a_set(nfds, &[]);
            let mut ts = TimeSpec { sec: 0, nsec: 0 };
            let no_pack = pselect6(
                sys,
                nfds,
                &mut read,
                &mut write,
                &mut except,
                Some(&mut ts),
                0,
            )
            .await;
            (with_a_mask, bad_length, no_pack)
        })
    });

    assert_eq!(
        with_a_mask,
        Ok(1),
        "a well formed sigset argpack was not accepted, which is what \
         reading its length out of its pointer word does"
    );
    assert_eq!(
        bad_length,
        Err(LxError::EINVAL),
        "a sigset argpack with the wrong length was accepted"
    );
    assert_eq!(
        no_pack,
        Ok(1),
        "a null sixth argument was read as a pack anyway"
    );
}

#[test]
fn ppoll_takes_a_timespec_and_a_null_one_is_no_limit_at_all() {
    // `ppoll` differs from `poll` in the type of its timeout and in the mask
    // it installs around the wait; everything else it hands straight on. The
    // timeout is the half this test asks about, in the three shapes that are
    // answerable without waiting: a null pointer, which means no limit and
    // must not be read; a zero timeout, which must not wait; and one that
    // does not name a duration, which is EINVAL before anything is waited
    // on.
    let proc = a_process(48_604, "ppoll-timeout", "ppoll_timeout");
    let dir_path = String::from("/ppoll_timeout");
    let (no_limit, spent, out_of_range) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = a_ready_node(&lp, &dir_path, FileType::File);
            // Ready, so a wait with no limit still comes back at once.
            let mut ready = alloc::vec![an_entry(fd as i32, IN_AND_OUT)];
            let no_limit = ppoll(sys, &mut ready, None, None, 0).await;
            // A negative descriptor is poll(2)'s "skip this entry", so this
            // set is never ready and a zero timeout is the only reason the
            // call can return.
            let mut never = alloc::vec![an_entry(-1, IN_AND_OUT)];
            let zero = TimeSpec { sec: 0, nsec: 0 };
            let spent = ppoll(sys, &mut never, Some(&zero), None, 0).await;
            // A `timespec` is a whole-seconds field and a nanoseconds field
            // below a second; two thousand million nanoseconds does not name
            // a duration, and Linux refuses it rather than sleeping for some
            // other length of time.
            let nonsense = TimeSpec {
                sec: 0,
                nsec: 2_000_000_000,
            };
            let out_of_range = ppoll(sys, &mut never, Some(&nonsense), None, 0).await;
            (no_limit, spent, out_of_range)
        })
    });

    assert_eq!(
        no_limit,
        Ok(1),
        "a null timeout was read instead of taken for no limit"
    );
    assert_eq!(spent, Ok(0), "a zero timeout did not come back empty");
    assert_eq!(
        out_of_range,
        Err(LxError::EINVAL),
        "a timespec that names no duration was waited on anyway"
    );
}

#[test]
fn ppoll_forwards_the_mask_and_its_size_and_a_null_mask_asks_nothing() {
    // The other half of `ppoll`: the mask and its size reach
    // `install_temp_sigmask` as given, so a size that is not
    // `sizeof(sigset_t)` is EINVAL -- and a null mask short-circuits before
    // that check, so the same wrong size is not an error when there is no
    // mask to apply it to. That pair is what says both arguments are
    // forwarded rather than one of them replaced.
    let proc = a_process(48_605, "ppoll-mask", "ppoll_mask");
    let dir_path = String::from("/ppoll_mask");
    let (with_a_mask, short_size, no_mask) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let fd = a_ready_node(&lp, &dir_path, FileType::File);
            let mask = Sigset::new(0);
            let zero = TimeSpec { sec: 0, nsec: 0 };
            let mut set = alloc::vec![an_entry(fd as i32, IN_AND_OUT)];
            let with_a_mask = ppoll(sys, &mut set, Some(&zero), Some(&mask), SIGSETSIZE).await;
            let short_size = ppoll(sys, &mut set, Some(&zero), Some(&mask), SIGSETSIZE / 2).await;
            let no_mask = ppoll(sys, &mut set, Some(&zero), None, SIGSETSIZE / 2).await;
            (with_a_mask, short_size, no_mask)
        })
    });

    assert_eq!(
        with_a_mask,
        Ok(1),
        "a mask of the right size was not accepted"
    );
    assert_eq!(
        short_size,
        Err(LxError::EINVAL),
        "a mask of half the size of a sigset was accepted"
    );
    assert_eq!(
        no_mask,
        Ok(1),
        "a size that goes with no mask at all was checked anyway"
    );
}

#[test]
fn epoll_wait_is_the_same_wait_without_a_mask_and_pwait2_counts_nanoseconds() {
    // The two forms nothing had called. `epoll_wait(2)` is the four argument
    // one every static binary in the tree uses and it is pure forwarding, so
    // what it has to get right is the ORDER: its `maxevents` and its timeout
    // are an `int` each and swapping them turns a wait for one event into
    // `epoll_maxevents(0)`, which is EINVAL. `epoll_pwait2(2)` differs only
    // in reading that timeout out of a `timespec`, with the same two edges
    // as `ppoll`: a null pointer is no limit, and one that names no duration
    // is EINVAL.
    let proc = a_process(48_606, "epoll-forms", "epoll_forms");
    let dir_path = String::from("/epoll_forms");
    let (waited, no_limit, out_of_range, closed) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let epfd = an_epoll_on_a_ready_node(sys, &dir_path);
            let mut out = alloc::vec![EpollEvent { events: 0, data: 0 }; 8];
            let at = out.as_mut_ptr() as usize;
            let waited = sys
                .sys_epoll_wait(FileDesc::from(epfd), UserOutPtr::from(at), out.len(), 0)
                .await;
            let no_limit = sys
                .sys_epoll_pwait2(
                    FileDesc::from(epfd),
                    UserOutPtr::from(at),
                    out.len(),
                    UserInPtr::from(0),
                    UserInPtr::from(0),
                    0,
                )
                .await;
            let nonsense = TimeSpec {
                sec: 0,
                nsec: 2_000_000_000,
            };
            let out_of_range = sys
                .sys_epoll_pwait2(
                    FileDesc::from(epfd),
                    UserOutPtr::from(at),
                    out.len(),
                    UserInPtr::from(&nonsense as *const TimeSpec as usize),
                    UserInPtr::from(0),
                    0,
                )
                .await;
            // The legacy form reaches the same lookup as the one with a
            // mask: a descriptor the process does not have is EBADF, and it
            // is answered before the wait rather than inside it.
            let closed = sys
                .sys_epoll_wait(
                    FileDesc::from(A_CLOSED_FD),
                    UserOutPtr::from(at),
                    out.len(),
                    0,
                )
                .await;
            (waited, no_limit, out_of_range, closed)
        })
    });

    assert_eq!(
        waited,
        Ok(1),
        "epoll_wait did not report the one ready descriptor it watches"
    );
    assert_eq!(
        no_limit,
        Ok(1),
        "a null timespec was read instead of taken for no limit"
    );
    assert_eq!(
        out_of_range,
        Err(LxError::EINVAL),
        "a timespec that names no duration was waited on anyway"
    );
    assert_eq!(
        closed,
        Err(LxError::EBADF),
        "waiting on a descriptor the process does not have was not EBADF"
    );
}
