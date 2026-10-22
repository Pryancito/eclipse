//! The wait that actually parks: `poll(2)` and `select(2)` asked about a
//! descriptor that is not ready yet.
//!
//! Every other test of this file finishes on the future's FIRST pass. A node
//! on RamFS answers ready for reading and for writing, so `events > 0` and the
//! scan returns; a timeout of zero takes `PollWait::ReturnEmpty` before a
//! waker is parked anywhere. Between them they cover the two arms that return
//! at once and nothing else, which left the whole of the blocking path -- 129
//! lines across the two futures, plus `arm_io_wait`, `clear_poll_io`,
//! `io_wait_interval` and `schedule_poll_wakeup` -- never executed by anything.
//! That path is what `poll` and `select` ARE: a caller that knew its
//! descriptor was ready would not be calling them.
//!
//! The read end of a fresh pipe is the descriptor this needs. With the write
//! end still open and the buffer empty, `Pipe::poll` reports `read: false`
//! (and the read end never reports `write`), so the scan finds nothing and
//! the future goes on to park -- which is the one thing no RamFS node can be
//! made to do.
//!
//! What the park then does is three decisions, and each test here is one of
//! them:
//!
//!  * **A waker on the descriptor's own event bus.** `subscribe_readiness`
//!    hands back a subscription for a pipe, so the write that fills it fires
//!    this task's waker at once instead of leaving it to the re-scan tick.
//!  * **A timer, for the timeout and as the backstop.** The call must come
//!    back when the timeout runs out and not before it.
//!  * **A signal check on every pass.** A parked wait has to be interruptible;
//!    a ready descriptor (or a spent timeout) must be answered instead, which
//!    is why the check sits where it does and not at the top of the call.
//!
//! And one set has no event source at all: `poll` with nothing but the
//! negative descriptors POSIX says to ignore. The body records that as a bug
//! it fixed -- counting ignored slots as watched made every one of them look
//! covered, which "stretched the backstop to the covered tick with no event
//! source anywhere in the set", so a plain timeout woke 25 times slower than
//! it asked to. What the answer cannot show is which tick was chosen: the
//! timer is armed for `min(timeout, tick)`, so for any timeout shorter than
//! the tick the two readings behave alike. The test below pins what is
//! observable -- that such a set parks through this path and waits its
//! timeout out -- and the tick choice itself stays with
//! `io_wait_interval_for`'s own tests, where it is a pure function.
//!
//! The waits here are bounded by their own timeouts (20 ms where the timeout
//! is the point) or ended by a write (a 5 s ceiling that is never reached), so
//! nothing depends on how loaded the machine is. The one assertion about time
//! is a lower bound -- a call that came back BEFORE its timeout did not wait.
//!
//! Fixed pids live in 49_30x.

extern crate std;

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use linux_object::fs::{File, OpenFlags, Pipe};
use linux_object::process::LinuxProcess;
use linux_object::thread::ThreadExt;
use rcore_fs::vfs::INode;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// `POLLIN`, the one bit every reader here asks for.
const POLLIN: u16 = 0x0001;

/// A timeout short enough to wait out in a test and long enough that a call
/// returning before it has plainly not waited.
const A_SHORT_WAIT: isize = 20;

/// A ceiling for a wait that something else is going to end. Nothing here
/// reaches it; a wait that does is a failure, and says so.
const A_CEILING: isize = 5_000;

/// A gap between tries, long enough that the retries are not a spin.
const A_MOMENT: Duration = Duration::from_millis(10);

/// How long a helper thread waits for the park it is meant to interrupt: two
/// thousand tries of [`A_MOMENT`], twenty seconds, the harness's own cap for
/// one call.
///
/// Deliberately a budget that does NOTHING when it runs out. A thread that
/// gives up and acts anyway acts at the worst possible moment -- after the
/// wait it was aimed at has already finished -- and turns a test into an
/// intermittent one that blames whatever parks next.
const A_WAIT_BUDGET: usize = 2_000;

/// A process, the two descriptors of a pipe it holds (read end first, as
/// `pipe(2)` writes them), and the write end itself for a host thread to
/// fill.
///
/// Built here rather than with `sys_pipe2` inside the call because the
/// descriptors are needed BEFORE the thread that waits on them starts: the
/// process keeps its file table across its threads, so a pipe put in it now
/// is the same pipe the call sees.
fn a_process_with_a_pipe(pid: KoID, name: &str) -> (Arc<Process>, usize, usize, Arc<Pipe>) {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let (read, write) = Pipe::create_pair();
    let write = Arc::new(write);
    let read_fd: i32 = lp
        .add_file(File::new(
            Arc::new(read),
            OpenFlags::RDONLY,
            String::from("pipe_r:[]"),
        ))
        .unwrap()
        .into();
    // The write end is kept in the table as well as in hand: a pipe with no
    // writer left hangs up, and a hung-up read end reports POLLIN at once,
    // which is the opposite of what every test here needs.
    let write_fd: i32 = lp
        .add_file(File::new(
            write.clone(),
            OpenFlags::WRONLY,
            String::from("pipe_w:[]"),
        ))
        .unwrap()
        .into();
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap();
    (proc, read_fd as usize, write_fd as usize, write)
}

/// A process with nothing in it, for the set that watches nothing.
fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// One `pollfd` of a caller's array. A negative descriptor is poll(2)'s
/// "ignore this entry" and is passed through as given.
fn an_entry(fd: i32, events: u16) -> PollFd {
    PollFd {
        fd: FileDesc::from(fd),
        events: PollEvents::from_bits_truncate(events),
        revents: PollEvents::empty(),
    }
}

/// `poll(ufds, nfds, timeout_msecs)` by the syscall.
///
/// `sys_poll` wants `&mut self` and `on_a_thread` lends a shared one, so this
/// builds its own out of the same three parts, as `dispatch_tests::by_number`
/// does.
async fn poll_them(sys: &Syscall<'_>, set: &mut [PollFd], timeout_msecs: isize) -> SysResult {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    let nfds = set.len();
    own.sys_poll(
        UserInOutPtr::from(set.as_mut_ptr() as usize),
        nfds,
        timeout_msecs,
    )
    .await
}

/// An `fd_set` in host memory: one `u32` per 32 descriptors.
fn a_set(nfds: usize, fds: &[usize]) -> Vec<u32> {
    let mut words = alloc::vec![0u32; nfds.div_ceil(FD_PER_ITEM).max(1)];
    for &fd in fds {
        words[fd / FD_PER_ITEM] |= 1 << (fd % FD_PER_ITEM);
    }
    words
}

/// `select(nfds, &read, &write, &except, &timeout)` by the syscall, with a
/// timeout in milliseconds for the sake of the table above.
async fn select_them(
    sys: &Syscall<'_>,
    nfds: usize,
    read: &mut [u32],
    timeout_msecs: isize,
) -> SysResult {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    let mut write = a_set(nfds, &[]);
    let mut except = a_set(nfds, &[]);
    let mut timeout = TimeVal {
        sec: (timeout_msecs / 1000) as usize,
        usec: (timeout_msecs % 1000) as usize * 1000,
    };
    own.sys_select(
        nfds,
        UserInOutPtr::from(read.as_mut_ptr() as usize),
        UserInOutPtr::from(write.as_mut_ptr() as usize),
        UserInOutPtr::from(except.as_mut_ptr() as usize),
        UserInOutPtr::from(&mut timeout as *mut TimeVal as usize),
    )
    .await
}

/// `sizeof(sigset_t)` as this kernel and every libc agree on it.
const SIGSETSIZE: usize = 8;

/// What the helpers below send to interrupt a park, and what the masks here
/// deliberately do NOT block.
const A_SIGNAL: linux_object::signal::Signal = linux_object::signal::Signal::SIGUSR1;

/// A signal the tests never send, used only to make an installed mask
/// recognisable: a mask that still contains it after the call is a mask that
/// was kept rather than restored.
const A_SIGNAL_NOBODY_SENDS: linux_object::signal::Signal = linux_object::signal::Signal::SIGUSR2;

/// A host thread that waits for `parked` and then signals the process.
///
/// A SIGNAL and not `Process::exit`: an exit takes the process out from under
/// the call, and one that lands before the park is reached answers for a
/// process that is gone instead of being interrupted. A signal cannot lose
/// that race in either direction -- queued before the park it is answered by
/// the check at the top of the next pass, queued after, by the wake. The
/// budget does NOTHING if it runs out: a helper that gives up and acts anyway
/// interrupts whatever parks next.
fn a_signaller(pid: usize, parked: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for _ in 0..A_WAIT_BUDGET {
            if parked.load(Ordering::SeqCst) {
                std::thread::sleep(A_MOMENT);
                linux_object::process::send_signal_to_process(pid, A_SIGNAL).unwrap();
                return;
            }
            std::thread::sleep(A_MOMENT);
        }
    })
}

/// A mask that blocks one signal: non-null, so `install_temp_sigmask` makes a
/// guard, and recognisable afterwards.
fn a_mask(sig: linux_object::signal::Signal) -> Sigset {
    let mut mask = Sigset::empty();
    mask.insert(sig);
    mask
}

/// This thread's signal mask right now, as the guard left it.
fn the_mask_now(sys: &Syscall<'_>) -> Sigset {
    // The same way `install_temp_sigmask` gets at it: `Syscall::thread` is a
    // `CurrentThread`, and the Linux signal state hangs off the `Arc<Thread>`.
    Arc::clone(sys.thread).lock_linux().signal_mask()
}

/// `ppoll(ufds, nfds, &timeout, &sigmask, sigsetsize)` by the syscall. A
/// `None` timeout is the null pointer that means no limit, and a `None` mask
/// the null pointer that means "leave mine alone".
async fn ppoll(
    sys: &Syscall<'_>,
    set: &mut [PollFd],
    timeout: Option<&TimeSpec>,
    sigmask: Option<&Sigset>,
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
    let nfds = set.len();
    own.sys_ppoll(
        UserInOutPtr::from(set.as_mut_ptr() as usize),
        nfds,
        UserInPtr::from(ts),
        UserInPtr::from(mask),
        SIGSETSIZE,
    )
    .await
}

/// `pselect6(nfds, &read, NULL, NULL, &timeout, sigset_arg)` by the syscall,
/// with only the read set the tests here care about.
async fn pselect6(
    sys: &Syscall<'_>,
    nfds: usize,
    read: &mut [u32],
    timeout: Option<&TimeSpec>,
    sigmask: Option<&Sigset>,
) -> SysResult {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    let at = match timeout {
        Some(t) => t as *const TimeSpec as usize,
        None => 0,
    };
    // The sixth argument is a pointer to `{ const sigset_t *ss; size_t
    // ss_len; }`, not to the mask itself.
    let arg = PSelectSigsetArg {
        ss: match sigmask {
            Some(m) => m as *const Sigset as usize,
            None => 0,
        },
        ss_len: SIGSETSIZE,
    };
    own.sys_pselect6(
        nfds,
        UserInOutPtr::from(read.as_mut_ptr() as usize),
        UserInOutPtr::from(0),
        UserInOutPtr::from(0),
        UserInOutPtr::from(at),
        match sigmask {
            Some(_) => &arg as *const PSelectSigsetArg as usize,
            None => 0,
        },
    )
    .await
}

/// `select(nfds, &read, NULL, NULL, NULL)`: no timeout at all, which is the
/// null pointer that means "for ever".
async fn select_for_ever(sys: &Syscall<'_>, nfds: usize, read: &mut [u32]) -> SysResult {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    own.sys_select(
        nfds,
        UserInOutPtr::from(read.as_mut_ptr() as usize),
        UserInOutPtr::from(0),
        UserInOutPtr::from(0),
        UserInOutPtr::from(0),
    )
    .await
}

/// `select(nfds, NULL, &write, NULL, &timeout)`: the write set alone.
async fn select_for_writing(
    sys: &Syscall<'_>,
    nfds: usize,
    write: &mut [u32],
    timeout_msecs: isize,
) -> SysResult {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    let mut timeout = TimeVal {
        sec: (timeout_msecs / 1000) as usize,
        usec: (timeout_msecs % 1000) as usize * 1000,
    };
    own.sys_select(
        nfds,
        UserInOutPtr::from(0),
        UserInOutPtr::from(write.as_mut_ptr() as usize),
        UserInOutPtr::from(0),
        UserInOutPtr::from(&mut timeout as *mut TimeVal as usize),
    )
    .await
}

/// A host thread that waits for `parked` and then puts one byte in the pipe.
///
/// The byte goes in through the write end's own `write_at`, which is what
/// `write(2)` calls and where the readable event is published; this is a test
/// of the waiting side, so the writing side is not asked to go round the
/// syscall table as well.
fn a_writer(write: Arc<Pipe>, parked: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for _ in 0..A_WAIT_BUDGET {
            if parked.load(Ordering::SeqCst) {
                // One more gap, so the byte lands after the first pass has
                // parked rather than racing it: either way the answer is the
                // same, but only the later one exercises the wake.
                std::thread::sleep(A_MOMENT);
                assert_eq!(write.write_at(0, b"x"), Ok(1), "the pipe took no byte");
                return;
            }
            std::thread::sleep(A_MOMENT);
        }
    })
}

#[test]
fn a_poll_of_an_empty_pipe_parks_and_the_write_that_fills_it_wakes_the_wait() {
    // The whole point of the readiness subscription: a `poll` that found
    // nothing parks a waker on each watched descriptor's event bus, and the
    // write publishes READABLE into that bus. What is asserted is the answer
    // -- one descriptor, POLLIN -- which can only be reached by going round
    // the future a second time, because on the first pass the pipe was empty.
    //
    // Deliberately not asserted: how long it took. A wake through the bus and
    // a wake on the backstop tick give the same answer, a few milliseconds
    // apart, and a test that tried to tell them apart by the clock would be
    // measuring the machine's load. The tick is a fallback for exactly this
    // case and is allowed to work.
    let (proc, read_fd, _write_fd, write) = a_process_with_a_pipe(49_300, "poll-woken");
    let parked = Arc::new(AtomicBool::new(false));
    let writer = a_writer(write, parked.clone());
    let (got, revents) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut set = [an_entry(read_fd as i32, POLLIN)];
            parked.store(true, Ordering::SeqCst);
            let got = poll_them(sys, &mut set, A_CEILING).await;
            (got, set[0].revents)
        })
    });
    writer.join().unwrap();
    assert_eq!(
        got,
        Ok(1),
        "a poll parked on an empty pipe did not come back on the write that filled it"
    );
    assert!(
        revents.contains(PollEvents::IN),
        "the descriptor came back without POLLIN: {:?}",
        revents
    );
}

#[test]
fn a_poll_that_nothing_ever_feeds_comes_back_when_its_timeout_runs_out() {
    // The timeout, reached from a park instead of from the spent-timeout
    // shortcut. `nothing_lands_in_exceptfds_and_a_spent_timeout_hands_the_
    // sets_back_cleared` already asks for a timeout of zero, which answers
    // `Ok(0)` on the first pass without arming anything; this one arms the
    // timer, parks, and is woken by that timer with the time gone.
    //
    // The lower bound is the assertion that matters. A kernel that armed the
    // timer for the re-scan tick and forgot the deadline, or one that treated
    // `Poll::Pending` as "come back at once", answers zero straight away --
    // and `Ok(0)` alone cannot tell that from a wait that was actually waited.
    let (proc, read_fd, _write_fd, _write) = a_process_with_a_pipe(49_301, "poll-timeout");
    let (got, waited, revents) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut set = [an_entry(read_fd as i32, POLLIN)];
            let began = mono_now();
            let got = poll_them(sys, &mut set, A_SHORT_WAIT).await;
            (got, mono_now() - began, set[0].revents)
        })
    });
    assert_eq!(got, Ok(0), "a poll of an empty pipe answered a descriptor");
    assert!(
        waited >= Duration::from_millis(A_SHORT_WAIT as u64),
        "the call came back in {:?}, before the {} ms it was given",
        waited,
        A_SHORT_WAIT
    );
    assert!(
        revents.is_empty(),
        "a descriptor that never became ready came back with {:?}",
        revents
    );
}

#[test]
fn a_set_of_nothing_but_ignored_slots_has_no_event_source_and_waits_its_timeout_out() {
    // The bug the body records: an ignored slot was counted as a watched
    // descriptor with a waker parked on it, so a set made only of them looked
    // fully covered and took the long backstop -- with nothing in the set
    // that could ever publish an event. udhcpc6 leaves -1 in its array, and
    // this is the shape a caller using poll(2) as a sleep hands in.
    //
    // There is no descriptor here at all, so the only way out is the timer:
    // the test asserts that the way out exists and is the timeout, which is
    // the pass through `io_wait_interval` that a covered set never takes.
    let proc = a_process(49_302, "poll-ignored");
    let (got, waited, revents) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut set = [an_entry(-1, POLLIN), an_entry(-7, POLLIN)];
            let began = mono_now();
            let got = poll_them(sys, &mut set, A_SHORT_WAIT).await;
            (got, mono_now() - began, [set[0].revents, set[1].revents])
        })
    });
    assert_eq!(
        got,
        Ok(0),
        "an ignored slot was counted as a descriptor with something to say"
    );
    assert!(
        waited >= Duration::from_millis(A_SHORT_WAIT as u64),
        "a set of ignored slots came back in {:?}, before its timeout",
        waited
    );
    assert!(
        revents[0].is_empty() && revents[1].is_empty(),
        "an ignored entry was written back with {:?}",
        revents
    );
}

#[test]
fn a_signal_ends_a_poll_that_is_parked_with_nothing_to_wake_it() {
    // A parked wait must be interruptible. The check sits after the scan and
    // after the ready return, which is the order Linux keeps: a caller that
    // already has ready descriptors is given them, and only a wait that is
    // about to block again is turned into EINTR. Nothing had ever reached it
    // here, because nothing had ever blocked again.
    //
    // The ceiling is five seconds and the pipe is never written, so the only
    // two ways out are the signal and a failure. The helper does nothing at
    // all if the park never happens, which is what keeps this test from
    // interrupting whatever parks next. What arrives is a signal and not the
    // end of the process: see [`a_signaller`].
    let (proc, read_fd, _write_fd, _write) = a_process_with_a_pipe(49_303, "poll-eintr");
    let parked = Arc::new(AtomicBool::new(false));
    a_signaller(49_303, parked.clone());
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut set = [an_entry(read_fd as i32, POLLIN)];
            parked.store(true, Ordering::SeqCst);
            poll_them(sys, &mut set, A_CEILING).await
        })
    });
    assert_eq!(
        got,
        Err(LxError::EINTR),
        "a poll parked on a pipe nobody writes did not come out on a signal"
    );
}

#[test]
fn a_select_of_an_empty_pipe_parks_and_the_write_wakes_it_with_the_set_marked() {
    // `select` keeps the same three decisions in its own future, and answers
    // through bitmaps instead of an array: the descriptor that woke it has to
    // come back SET in the read set, and the count is the number of bits, so
    // one. A kernel that woke correctly and committed the wrong bitmap would
    // answer `Ok(1)` with an empty set.
    let (proc, read_fd, _write_fd, write) = a_process_with_a_pipe(49_304, "select-woken");
    let parked = Arc::new(AtomicBool::new(false));
    let writer = a_writer(write, parked.clone());
    let (got, read) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let nfds = read_fd + 1;
            let mut read = a_set(nfds, &[read_fd]);
            parked.store(true, Ordering::SeqCst);
            let got = select_them(sys, nfds, &mut read, A_CEILING).await;
            (got, read)
        })
    });
    writer.join().unwrap();
    assert_eq!(
        got,
        Ok(1),
        "a select parked on an empty pipe did not come back on the write"
    );
    assert_eq!(
        read[read_fd / FD_PER_ITEM] & (1 << (read_fd % FD_PER_ITEM)),
        1 << (read_fd % FD_PER_ITEM),
        "the descriptor that woke the select came back clear in the read set"
    );
}

#[test]
fn a_select_whose_timeout_runs_out_while_parked_hands_the_sets_back_cleared() {
    // The other end of the same future: the timeout reached from a park, and
    // the bitmaps committed on the way out. Linux answers 0 AND writes the
    // (empty) ready sets back, so the caller's bit is gone from the set it
    // handed in -- the same contract the spent-timeout test pins on the first
    // pass, here across a real wait.
    let (proc, read_fd, _write_fd, _write) = a_process_with_a_pipe(49_305, "select-timeout");
    let (got, waited, read) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let nfds = read_fd + 1;
            let mut read = a_set(nfds, &[read_fd]);
            let began = mono_now();
            let got = select_them(sys, nfds, &mut read, A_SHORT_WAIT).await;
            (got, mono_now() - began, read)
        })
    });
    assert_eq!(got, Ok(0), "a select of an empty pipe found a descriptor");
    assert!(
        waited >= Duration::from_millis(A_SHORT_WAIT as u64),
        "the select came back in {:?}, before the {} ms it was given",
        waited,
        A_SHORT_WAIT
    );
    assert_eq!(
        read[read_fd / FD_PER_ITEM],
        0,
        "a descriptor that never became ready came back set in the read set"
    );
}

#[test]
fn a_ppoll_that_a_signal_interrupts_keeps_the_mask_it_installed() {
    // `ppoll`'s whole reason for existing: it installs the caller's mask for
    // the duration of the wait, and when the wait ends in EINTR the mask has
    // to STAY installed, because the handler is about to run and it must run
    // under the mask the caller asked for. The guard that does that
    // (`keep_for_signal`) sat at zero: every test of `ppoll` so far passed a
    // null mask, which makes no guard at all, or finished on a ready
    // descriptor, which is not EINTR.
    //
    // What makes it visible is a mask holding a signal nobody here sends.
    // The guard is dropped when `sys_ppoll` returns, so by the time the mask
    // is read back it has either been restored (the signal was never
    // noticed) or kept (it was). The signal that does the interrupting is a
    // different one, and deliberately not in the mask: a blocked signal is
    // not deliverable and would not interrupt anything.
    let (proc, read_fd, _write_fd, _write) = a_process_with_a_pipe(49_310, "ppoll-mask");
    let parked = Arc::new(AtomicBool::new(false));
    a_signaller(49_310, parked.clone());
    let (got, mask) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut set = [an_entry(read_fd as i32, POLLIN)];
            let mask = a_mask(A_SIGNAL_NOBODY_SENDS);
            let timeout = TimeSpec {
                sec: (A_CEILING / 1000) as usize,
                nsec: 0,
            };
            parked.store(true, Ordering::SeqCst);
            let got = ppoll(sys, &mut set, Some(&timeout), Some(&mask)).await;
            (got, the_mask_now(sys))
        })
    });
    assert_eq!(
        got,
        Err(LxError::EINTR),
        "a ppoll parked on an empty pipe did not come out on a signal"
    );
    assert!(
        mask.contains(A_SIGNAL_NOBODY_SENDS),
        "the mask ppoll installed was restored before the handler could run"
    );
}

#[test]
fn a_pselect6_that_a_signal_interrupts_keeps_the_mask_it_installed() {
    // The same contract in `pselect6`, which is where it matters most: glibc
    // routes plain `select()` through this syscall. It is written out
    // separately there -- its own `install_temp_sigmask`, its own guard, and
    // a sixth argument that is a pointer to a STRUCT holding the mask
    // pointer rather than the mask itself -- so a fix in `ppoll` says
    // nothing about it.
    let (proc, read_fd, _write_fd, _write) = a_process_with_a_pipe(49_311, "pselect-mask");
    let parked = Arc::new(AtomicBool::new(false));
    a_signaller(49_311, parked.clone());
    let (got, mask) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let nfds = read_fd + 1;
            let mut read = a_set(nfds, &[read_fd]);
            let mask = a_mask(A_SIGNAL_NOBODY_SENDS);
            let timeout = TimeSpec {
                sec: (A_CEILING / 1000) as usize,
                nsec: 0,
            };
            parked.store(true, Ordering::SeqCst);
            let got = pselect6(sys, nfds, &mut read, Some(&timeout), Some(&mask)).await;
            (got, the_mask_now(sys))
        })
    });
    assert_eq!(
        got,
        Err(LxError::EINTR),
        "a pselect6 parked on an empty pipe did not come out on a signal"
    );
    assert!(
        mask.contains(A_SIGNAL_NOBODY_SENDS),
        "the mask pselect6 installed was restored before the handler could run"
    );
}

#[test]
fn a_select_with_no_timeout_at_all_waits_until_something_happens() {
    // A null timeout pointer is select(2)'s "block indefinitely", and it is a
    // different arm from any finite one: the pair that carries the deadline
    // and the time to write back is built as `(-1, None)` instead of from a
    // `timeval`. Nothing had ever taken it -- a test cannot pass a null
    // timeout and also know the call will come back, unless something ends
    // the wait, which is what the pipe is for.
    let (proc, read_fd, _write_fd, write) = a_process_with_a_pipe(49_312, "select-for-ever");
    let parked = Arc::new(AtomicBool::new(false));
    let writer = a_writer(write, parked.clone());
    let (got, read) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let nfds = read_fd + 1;
            let mut read = a_set(nfds, &[read_fd]);
            parked.store(true, Ordering::SeqCst);
            let got = select_for_ever(sys, nfds, &mut read).await;
            (got, read)
        })
    });
    writer.join().unwrap();
    assert_eq!(
        got,
        Ok(1),
        "a select with no deadline did not come back on the write that fed it"
    );
    assert_eq!(
        read[read_fd / FD_PER_ITEM] & (1 << (read_fd % FD_PER_ITEM)),
        1 << (read_fd % FD_PER_ITEM),
        "the descriptor that ended the wait came back clear in the read set"
    );
}

#[test]
fn a_pselect6_with_no_timeout_at_all_waits_until_something_happens() {
    // `pselect6`'s own copy of the same arm, for the same reason the mask is
    // tested twice: the two syscalls read their timeout through different
    // calls and build the pair in different places.
    let (proc, read_fd, _write_fd, write) = a_process_with_a_pipe(49_313, "pselect-for-ever");
    let parked = Arc::new(AtomicBool::new(false));
    let writer = a_writer(write, parked.clone());
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let nfds = read_fd + 1;
            let mut read = a_set(nfds, &[read_fd]);
            parked.store(true, Ordering::SeqCst);
            pselect6(sys, nfds, &mut read, None, None).await
        })
    });
    writer.join().unwrap();
    assert_eq!(
        got,
        Ok(1),
        "a pselect6 with no deadline did not come back on the write that fed it"
    );
}

#[test]
fn a_signal_ends_a_select_that_is_parked_with_nothing_to_wake_it() {
    // `select_core` keeps its own signal check, written out separately from
    // `sys_poll`'s, and it was at zero for the same reason: nothing had ever
    // parked here. A select that cannot be interrupted is the same bug as a
    // poll that cannot be, and it is the one every shell's line editor sits
    // in.
    let (proc, read_fd, _write_fd, _write) = a_process_with_a_pipe(49_314, "select-eintr");
    let parked = Arc::new(AtomicBool::new(false));
    a_signaller(49_314, parked.clone());
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let nfds = read_fd + 1;
            let mut read = a_set(nfds, &[read_fd]);
            parked.store(true, Ordering::SeqCst);
            select_them(sys, nfds, &mut read, A_CEILING).await
        })
    });
    assert_eq!(
        got,
        Err(LxError::EINTR),
        "a select parked on a pipe nobody writes did not come out on a signal"
    );
}

#[test]
fn a_select_that_watches_no_descriptor_at_all_still_waits_out_its_timeout() {
    // `select(1, &empty, &empty, &empty, &timeout)`: an `nfds` above zero
    // with no bit set anywhere. That is a sleep, and it is the shape that
    // makes `any_watched` false, which is the one way a parked select takes
    // the re-scan interval instead of the stretched backstop -- the pass
    // through `io_wait_interval` that a set with a subscribable descriptor
    // never reaches.
    let proc = a_process(49_315, "select-nothing");
    let (got, waited) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut read = a_set(1, &[]);
            let began = mono_now();
            let got = select_them(sys, 1, &mut read, A_SHORT_WAIT).await;
            (got, mono_now() - began)
        })
    });
    assert_eq!(
        got,
        Ok(0),
        "a select watching nothing found a ready descriptor"
    );
    assert!(
        waited >= Duration::from_millis(A_SHORT_WAIT as u64),
        "a select watching nothing came back in {:?}, before its timeout",
        waited
    );
}

#[test]
fn a_descriptor_that_will_never_be_writable_is_still_watched_for_writing() {
    // The write set of a parked select. The read end of a pipe is never
    // writable -- it does not write -- so asking about it in `writefds` is a
    // wait that can only end on its timeout, and it is the one shape that
    // puts `POLLOUT` into the interest a parked select subscribes with.
    // Every select test before this one asked about reading.
    let (proc, read_fd, _write_fd, _write) = a_process_with_a_pipe(49_316, "select-writefds");
    let (got, waited, write) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let nfds = read_fd + 1;
            let mut write = a_set(nfds, &[read_fd]);
            let began = mono_now();
            let got = select_for_writing(sys, nfds, &mut write, A_SHORT_WAIT).await;
            (got, mono_now() - began, write)
        })
    });
    assert_eq!(got, Ok(0), "the read end of a pipe was reported writable");
    assert!(
        waited >= Duration::from_millis(A_SHORT_WAIT as u64),
        "the select came back in {:?}, before its timeout",
        waited
    );
    assert_eq!(
        write[read_fd / FD_PER_ITEM],
        0,
        "a descriptor that never became writable came back set in the write set"
    );
}
