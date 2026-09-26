//! IO Multiplex operations
//!
//! - select, pselect
//! - poll, ppoll

use super::*;
use alloc::vec::Vec;
use bitvec::prelude::{BitVec, Lsb0};
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;
use kernel_hal::timer;
use linux_object::fs::{FileDesc, PollEvents, PollStatus};
use linux_object::signal::Sigset;
use linux_object::time::*;

use super::fd::{anon_fd_flags, ANON_CLOEXEC};

/// Monotonic time since boot — must match `timer::timer_set` deadlines (not wall clock).
fn mono_now() -> Duration {
    timer::timer_now()
}

/// `STICKY_TIMEOUTS` (`include/uapi/linux/personality.h`): the persona bit
/// under which `select` leaves its timeout alone.
const STICKY_TIMEOUTS: u32 = 0x0400_0000;

/// What `poll_select_finish` (`fs/select.c`) writes back into the caller's
/// `timeval`/`timespec` once a `select` or `pselect6` returns, on every
/// path, a timeout and `EINTR` included: the time LEFT of the timeout,
/// zero once it has run out. `None` means the struct is not touched: no
/// timeout (NULL), a zero timeout ("No update for zero timeout"), or the
/// `STICKY_TIMEOUTS` persona. It used to be never written, so glibc's
/// `select()`, which is `pselect6` on a local `timespec` copied back into
/// the caller's `timeval`, handed back the original timeout: a
/// `while (select(...) < 0 && errno == EINTR)` loop restarted the whole
/// wait after each signal.
pub(crate) fn select_time_left(
    timeout: Option<Duration>,
    elapsed: Duration,
    sticky: bool,
) -> Option<Duration> {
    let timeout = timeout?;
    if sticky || timeout.is_zero() {
        return None;
    }
    Some(timeout.saturating_sub(elapsed))
}

fn schedule_poll_wakeup(
    cx: &mut Context,
    after: Duration,
    timer_token: &mut Option<kernel_hal::timer_waker::TimerWakerSlot>,
) {
    let deadline = mono_now() + after;
    // Refresh in place while the previous tick is still pending; re-arm only
    // after it fired. Avoids AtomicBool TOCTOU + timer-heap churn.
    kernel_hal::timer_waker::ensure_timer_waker(timer_token, deadline, cx);
}

fn kill_poll_timer(timer_token: &mut Option<kernel_hal::timer_waker::TimerWakerSlot>) {
    kernel_hal::timer_waker::kill_timer_waker(timer_token);
}

/// Wakeup granularity for select/poll/epoll (IRQ wakes can arrive earlier).
const IO_WAIT_TICK: Duration = Duration::from_millis(linux_object::net::wait::IO_WAIT_TICK_MS);

/// Slow re-poll granularity for an interactive wait whose terminal is NOT the
/// active VT. A background-VT shell sits in poll(stdin) for keyboard input that
/// can only ever arrive on the *active* terminal, so re-polling it at the fast
/// 4 ms tick just burns CPU (the busy/heat with several spare VT shells). Poll
/// it slowly instead; it still wakes immediately on a real event, and within
/// this bound it notices its VT becoming active and resumes fast polling.
const SLOW_IO_WAIT_TICK: Duration = Duration::from_millis(100);

/// Pick the io-wait re-poll interval, given whether the caller's terminal is
/// the one the user is looking at. Split from [`io_wait_interval`] so the
/// decision itself — which has regressed twice, in both directions — is a
/// pure function the host tests can ask every combination of.
fn io_wait_interval_for(
    watch_net: bool,
    watch_interactive: bool,
    terminal_only: bool,
    on_active_vt: bool,
) -> Duration {
    let background_interactive = watch_interactive && terminal_only && !on_active_vt;
    if !watch_net && background_interactive {
        SLOW_IO_WAIT_TICK
    } else {
        IO_WAIT_TICK
    }
}

/// The timeout of `poll(2)`/`epoll_wait(2)`, as it must be read out of the
/// syscall register.
///
/// Two things happen here that `as isize` does not do:
///
///  * **It is an `int`.** Linux declares `SYSCALL_DEFINE3(poll, ..., int,
///    timeout_msecs)`, and the `SYSCALL_DEFINE` macros cast each register to
///    the declared type, so the top 32 bits are not part of the number. Taking
///    the whole register instead turned a `-1` that arrived zero-extended
///    (0xffff_ffff — what a caller that keeps the timeout in a 32-bit slot
///    leaves in the register) into a finite 49-day wait, and any value with a
///    high bit set into a wait of up to 292 million years.
///  * **Every negative value means "for ever"**, not just `-1`. poll(2) says
///    so, and both [`Epoll::wait`] and `IoMultiplexWait` already read it that
///    way (`timeout_msecs >= 0`). The poll/select futures did not: they matched
///    `-1` alone and let everything else fall through to an arm that returned
///    `Poll::Pending` **without arming a timer, an io-wait waker or a readiness
///    subscription**. A plain `poll(fds, n, -2)` from any process parked that
///    thread with nothing left in the kernel that could ever wake it.
///
/// Normalizing at the syscall boundary keeps both readings in one place.
pub(crate) fn poll_timeout_msecs(raw: usize) -> isize {
    let msecs = raw as u32 as i32;
    if msecs < 0 {
        -1
    } else {
        msecs as isize
    }
}

/// The widest fd number `select(2)` can be asked about here: what the
/// `fd_set` this kernel accepts can hold.
const MAX_SELECT_NFDS: usize = MAX_FDSET_SIZE * FD_PER_ITEM;

/// `select(2)`'s first argument, as Linux reads it: `int n`, negative is
/// `EINVAL`, and anything past the table of open files is **clamped**, not
/// refused (`if (n > max_fds) n = max_fds;` in `core_sys_select`).
///
/// Read as a whole `usize` it was neither. `select(nfds, NULL, NULL, NULL,
/// &tv)` is a portable way to sleep, and a caller that passed a large `nfds`
/// with no fd sets sent the kernel scanning `0..nfds` — twice per pass, plus
/// once per watched fd — for two billion iterations with no lock held and no
/// way out: one unprivileged call burned a core until the machine was
/// rebooted. Clamping bounds the scan by the same thing that bounds the
/// `fd_set` itself.
fn select_nfds(raw: usize) -> Result<usize, LxError> {
    let n = raw as u32 as i32;
    if n < 0 {
        return Err(LxError::EINVAL);
    }
    Ok((n as usize).min(MAX_SELECT_NFDS))
}

/// Linux's `EP_MAX_EVENTS`: as many events as can be counted in the `int`
/// that `epoll_wait(2)` returns.
const EP_MAX_EVENTS: usize = (i32::MAX as usize) / core::mem::size_of::<EpollEvent>();

/// `epoll_wait(2)`'s `maxevents`, as Linux reads it: `int`, and
/// `maxevents <= 0 || maxevents > EP_MAX_EVENTS` is `EINVAL` before anything
/// else happens.
///
/// Taken as a whole `usize` and never checked, `maxevents = 0` was not "no
/// events": the wait loop pushes an event first and only then tests
/// `events.len() >= maxevents`, so it returned **one** event and wrote it into
/// a buffer the caller had sized for none — a 12-byte write past the end of
/// whatever userspace allocated, from a syscall any process can make.
fn epoll_maxevents(raw: usize) -> Result<usize, LxError> {
    let n = raw as u32 as i32;
    if n <= 0 || n as usize > EP_MAX_EVENTS {
        return Err(LxError::EINVAL);
    }
    Ok(n as usize)
}

/// `epoll_create(2)`'s `size`, which is obsolete and must still be positive.
///
/// The argument has been ignored since Linux 2.6.8 -- it used to size the
/// interest list -- but `ep_alloc` still rejects a non-positive one, so
/// `epoll_create(0)` is `EINVAL` on every Linux there is. Here it was not
/// read at all, which made this kernel the one place that accepted it.
///
/// `size` arrives as a machine word and the syscall's parameter is an `int`,
/// so it is narrowed the same way `epoll_maxevents` narrows `maxevents`:
/// `epoll_create(-1)` is a negative `int`, not four billion.
fn epoll_create_size(raw: usize) -> Result<usize, LxError> {
    let n = raw as u32 as i32;
    if n <= 0 {
        return Err(LxError::EINVAL);
    }
    Ok(n as usize)
}

/// What a poll/select pass does once it has scanned every fd and found
/// nothing ready.
#[derive(Debug, PartialEq, Eq)]
enum PollWait {
    /// Return 0 now: the caller asked not to block, or its deadline has
    /// already passed.
    ReturnEmpty,
    /// Block. `Some(remaining)` is how much of the caller's timeout is left;
    /// `None` is a wait with no deadline.
    Sleep(Option<Duration>),
}

/// Decide between returning empty and blocking again. Written once for both
/// `poll` and `select`, which had the same four-armed `match` copied into each
/// future — and the copy is where the arm for "any other negative timeout"
/// went missing.
fn poll_wait_decision(timeout_msecs: isize, begin_time: Duration, now: Duration) -> PollWait {
    match timeout_msecs {
        0 => PollWait::ReturnEmpty,
        1.. => {
            let deadline = begin_time + Duration::from_millis(timeout_msecs as u64);
            if now >= deadline {
                PollWait::ReturnEmpty
            } else {
                PollWait::Sleep(Some(deadline.saturating_sub(now)))
            }
        }
        // Negative: wait for ever (poll(2)).
        _ => PollWait::Sleep(None),
    }
}

/// How long to sleep before re-scanning: the re-poll tick, or the rest of the
/// caller's timeout when that is shorter — a wait must not overshoot its own
/// deadline by a tick.
fn wake_after(limit: Option<Duration>, tick: Duration) -> Duration {
    match limit {
        Some(remaining) => remaining.min(tick),
        None => tick,
    }
}

/// Pick the io-wait re-poll interval. The slow tick exists for exactly one
/// pattern: a shell parked in poll(stdin) on a *background* VT, whose input can
/// only ever arrive once its VT becomes active — re-polling that at 4 ms just
/// burns CPU. Everything else gets the fast tick: in particular a poll set with
/// *no* interactive fd at all (DRM fds, timerfds, pipes, device fds — the shape
/// of a compositor's startup waits) must NOT be demoted to 100 ms, or every
/// such roundtrip is gated at a tenth of a second and startup takes minutes.
///
/// `terminal_only` is that pattern's signature: every fd in the set is a
/// terminal ([`FileLike::is_terminal`]). `watch_interactive` alone is not —
/// it is true for any non-socket fd, since it also drives the HID/TTY IRQ
/// registration — and keying the demotion on it put PulseAudio's ALSA sink
/// thread (`[pcm, timer]`, in a process that is never on the active VT) on
/// a 100 ms re-scan: its 108 ms buffer underran on every wake, and the
/// `[alsa-timer]` probe saw its 2.7 ms period answered in 105 ms.
fn io_wait_interval(
    s: &Syscall,
    watch_net: bool,
    watch_interactive: bool,
    terminal_only: bool,
) -> Duration {
    let on_active_vt = s.linux_process().vt() == kernel_hal::console::active_vt();
    io_wait_interval_for(watch_net, watch_interactive, terminal_only, on_active_vt)
}

fn arm_io_wait(cx: &mut Context, watch_net: bool, watch_interactive: bool, io_armed: &mut bool) {
    if *io_armed {
        linux_object::net::retain_io_wait_wakers(cx.waker(), watch_net, watch_interactive);
        *io_armed = false;
        return;
    }
    linux_object::net::register_io_wait_wakers(cx.waker(), watch_net, watch_interactive);
    *io_armed = true;
}

fn clear_poll_io(
    timer: &mut Option<kernel_hal::timer_waker::TimerWakerSlot>,
    io_waker: &mut Option<core::task::Waker>,
    watch_net: bool,
    watch_interactive: bool,
) {
    kill_poll_timer(timer);
    if let Some(w) = io_waker.take() {
        linux_object::net::clear_io_wait_wakers(&w, watch_net, watch_interactive);
    }
}

impl Syscall<'_> {
    /// Wait for some event on a file descriptor
    pub async fn sys_poll(
        &mut self,
        mut ufds: UserInOutPtr<PollFd>,
        nfds: usize,
        timeout_msecs: isize,
    ) -> SysResult {
        let _ = self.maybe_handle_tty_intr()?;
        let mut polls = ufds.read_array(nfds)?;
        info!(
            "poll: ufds: {:?}, nfds: {:?}, timeout_msecs: {}",
            polls, nfds, timeout_msecs
        );

        let begin_time = mono_now();
        #[must_use = "future does nothing unless polled/`await`-ed"]
        struct PollFuture<'a> {
            polls: &'a mut Vec<PollFd>,
            timeout_msecs: isize,
            begin_time: Duration,
            syscall: &'a Syscall<'a>,
            io_armed: bool,
            timer: Option<kernel_hal::timer_waker::TimerWakerSlot>,
            /// Last watch flags + waker parked in IRQ lists (for Drop cleanup).
            watch_net: bool,
            watch_interactive: bool,
            /// Every fd in the set is a terminal: the only shape that may be
            /// demoted to the slow background-VT tick (see `io_wait_interval`).
            terminal_only: bool,
            io_waker: Option<core::task::Waker>,
            /// Readiness wakers parked on the watched fds' event buses
            /// (pipes, unix sockets, ptys, eventfd/timerfd, DRM). Refreshed
            /// each pass; RAII-unsubscribed on drop.
            subs: Vec<linux_object::sync::ReadinessSub>,
        }
        impl Drop for PollFuture<'_> {
            fn drop(&mut self) {
                let wn = self.watch_net;
                let wi = self.watch_interactive;
                clear_poll_io(&mut self.timer, &mut self.io_waker, wn, wi);
            }
        }
        impl<'a> Future for PollFuture<'a> {
            type Output = SysResult;

            fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
                use PollEvents as PE;
                let this = self.get_mut();
                // Unsubscribe last pass's readiness wakers before re-scanning;
                // fresh ones are parked below if we wait again.
                this.subs.clear();
                let watch_net = this
                    .polls
                    .iter()
                    .any(|p| linux_object::net::fd_is_socket(p.fd));
                let watch_interactive = this
                    .polls
                    .iter()
                    .any(|p| linux_object::net::fd_is_interactive(p.fd));
                this.watch_net = watch_net;
                this.watch_interactive = watch_interactive;
                if this.io_armed {
                    arm_io_wait(cx, watch_net, watch_interactive, &mut this.io_armed);
                }
                linux_object::net::io_wait_tick(watch_net, watch_interactive);
                let proc = this.syscall.linux_process();
                this.terminal_only = !this.polls.is_empty()
                    && this.polls.iter().all(|p| {
                        <FileDesc as Into<i32>>::into(p.fd) < 0
                            || proc
                                .get_file_like(p.fd)
                                .map(|f| f.is_terminal())
                                .unwrap_or(false)
                    });
                let terminal_only = this.terminal_only;
                let mut events = 0;
                let mut early_err = None;

                // iterate each poll to check whether it is ready
                for poll in this.polls.iter_mut() {
                    poll.revents = PE::empty();
                    if let Ok(file_like) = proc.get_file_like(poll.fd) {
                        debug!("get file like: {:?}", file_like);
                        // Sync poll only — see Epoll::wait. async_poll per fd
                        // here nested huge futures on the coroutine stack and
                        // #DF'd `__from_user` when labwc/libinput started.
                        let status = match file_like.poll(poll.events) {
                            Ok(ret) => ret,
                            Err(err) => {
                                debug!("poll ret err: {:?}", err);
                                early_err = Some(err);
                                break;
                            }
                        };
                        if status.error {
                            poll.revents |= PE::ERR;
                        }
                        if status.read && poll.events.contains(PE::IN) {
                            poll.revents |= PE::IN;
                        }
                        if status.write && poll.events.contains(PE::OUT) {
                            poll.revents |= PE::OUT;
                        }
                        // POLLHUP/POLLERR are return-only: always report when set,
                        // even if the caller did not list them in `events`.
                        if status.hangup {
                            poll.revents |= PE::HUP;
                        }
                        // Linux poll(2): return value is the number of fds with
                        // nonzero revents, not the number of event bits set.
                        if !poll.revents.is_empty() {
                            events += 1;
                        }
                    } else if <FileDesc as Into<i32>>::into(poll.fd) < 0 {
                        // POSIX poll(2): negative fds are ignored (udhcpc6 leaves -1 in the set).
                        poll.revents = PE::empty();
                    } else {
                        // POLLNVAL is a well-defined answer, not a kernel
                        // error; keep the diagnostic off the (synchronous,
                        // slow) warn channel — a program polling a stale fd in
                        // a loop would stall on serial output otherwise.
                        debug!("can not find filelike object from fd: {:?}", poll.fd);
                        poll.revents |= PE::INVAL;
                        events += 1;
                    }
                }
                if let Some(err) = early_err {
                    clear_poll_io(
                        &mut this.timer,
                        &mut this.io_waker,
                        watch_net,
                        watch_interactive,
                    );
                    return Poll::Ready(Err(err));
                }
                // some event happens, so evoke the process
                if events > 0 {
                    clear_poll_io(
                        &mut this.timer,
                        &mut this.io_waker,
                        watch_net,
                        watch_interactive,
                    );
                    return Poll::Ready(Ok(events));
                }

                // A signal may interrupt only when we are actually about to
                // block again. If the caller already has ready fds (or a
                // timeout of 0 / an expired timeout), Linux returns that result
                // instead of synthesizing `EINTR`.
                if let Err(e) = linux_object::process::check_signals() {
                    clear_poll_io(
                        &mut this.timer,
                        &mut this.io_waker,
                        watch_net,
                        watch_interactive,
                    );
                    return Poll::Ready(Err(e));
                }

                // Nothing ready and we are about to wait: park a readiness
                // waker on every fd that can carry one (flat registration —
                // see `FileLike::subscribe_readiness`), so a pipe write or a
                // unix-socket send wakes this task the moment it happens
                // instead of on the next re-scan tick. Refreshed every pass
                // (stale subscriptions were dropped by the `clear()` above);
                // events racing in after the scan are caught by the
                // EventBus's latched flags, which fire the waker at
                // subscribe time. With full coverage the backstop stretches
                // from 4 ms to the covered tick; any unsubscribable fd keeps
                // the short tick for the whole set.
                let mut covered = false;
                if this.timeout_msecs != 0 {
                    covered = !this.polls.is_empty();
                    for p in this.polls.iter() {
                        if <FileDesc as Into<i32>>::into(p.fd) < 0 {
                            continue; // ignored slot (POSIX): nothing to wake on
                        }
                        match proc
                            .get_file_like(p.fd)
                            .ok()
                            .and_then(|f| f.subscribe_readiness(p.events, cx.waker()))
                        {
                            Some(sub) => this.subs.push(sub),
                            None => covered = false,
                        }
                    }
                }
                let covered_tick =
                    Duration::from_millis(linux_object::net::wait::IO_WAIT_COVERED_TICK_MS);

                match poll_wait_decision(this.timeout_msecs, this.begin_time, mono_now()) {
                    PollWait::ReturnEmpty => {
                        clear_poll_io(
                            &mut this.timer,
                            &mut this.io_waker,
                            watch_net,
                            watch_interactive,
                        );
                        return Poll::Ready(Ok(0));
                    }
                    PollWait::Sleep(limit) => {
                        let tick = if covered {
                            covered_tick
                        } else {
                            io_wait_interval(
                                this.syscall,
                                watch_net,
                                watch_interactive,
                                terminal_only,
                            )
                        };
                        let wake_in = wake_after(limit, tick);
                        arm_io_wait(cx, watch_net, watch_interactive, &mut this.io_armed);
                        this.io_waker = Some(cx.waker().clone());
                        schedule_poll_wakeup(cx, wake_in, &mut this.timer);
                    }
                }

                Poll::Pending
            }
        }

        let future = PollFuture {
            polls: &mut polls,
            timeout_msecs,
            begin_time,
            syscall: self,
            io_armed: false,
            timer: None,
            watch_net: false,
            watch_interactive: false,
            terminal_only: false,
            io_waker: None,
            subs: Vec::new(),
        };
        let result = future.await;
        if let Err(e) = &result {
            linux_object::process::trace_wait_error("poll", *e);
        }
        if let Err(e) = ufds.write_array(&polls) {
            let e: LxError = e.into();
            linux_object::process::trace_wait_error("poll/write", e);
            return Err(e);
        }
        info!("return ufds: {:?}", polls);
        result
    }

    /// Wait for some event on a file descriptor
    ///
    /// ppoll() allows an application to safely wait until either a file descriptor becomes ready or until a signal is caught
    pub async fn sys_ppoll(
        &mut self,
        ufds: UserInOutPtr<PollFd>,
        nfds: usize,
        timeout: UserInPtr<TimeSpec>,
        sigmask: UserInPtr<Sigset>,
        sigsetsize: usize,
    ) -> SysResult {
        let timeout_msecs = if timeout.is_null() {
            -1
        } else {
            let timeout = timeout.read()?;
            info!("sys_ppoll: timeout: {:?}", timeout);
            // Validated, not cast: a `timespec` out of range is EINVAL, and
            // a huge one must stay finite rather than turn into the
            // negative value that means "wait for ever".
            timeout.try_into_poll_msecs()?
        };

        let mut guard = self.install_temp_sigmask(sigmask, sigsetsize)?;
        let result = self.sys_poll(ufds, nfds, timeout_msecs).await;
        if matches!(result, Err(LxError::EINTR)) {
            if let Some(g) = guard.as_mut() {
                g.keep_for_signal();
            }
        }
        result
    }

    /// similar to select, but have sigmask argument
    pub async fn sys_pselect6(
        &mut self,
        nfds: usize,
        read: UserInOutPtr<u32>,
        write: UserInOutPtr<u32>,
        err: UserInOutPtr<u32>,
        mut timeout: UserInOutPtr<TimeSpec>,
        sigset_arg: usize,
    ) -> SysResult {
        // pselect6's timeout is a `timespec` (NANOseconds). It was previously
        // parsed as select(2)'s `timeval` (MICROseconds), inflating every
        // timeout by 1000x: glibc routes plain select() through pselect6, so a
        // 100 ms select slept 100 SECONDS. busybox ash's line editor and every
        // terminal-probe wait sat "wedged" exactly this way.
        let (timeout_msecs, timeout_left) = if timeout.is_null() {
            (-1, None)
        } else {
            let ts = timeout.read()?;
            (ts.try_into_poll_msecs()?, Some(ts.try_into_duration()?))
        };
        let sticky = self.linux_process().personality() & STICKY_TIMEOUTS != 0;
        let begin = mono_now();
        // 6th arg is a pointer to `{ const sigset_t *ss; size_t ss_len; }`.
        let (sigmask, sigsetsize) = if sigset_arg == 0 {
            (UserInPtr::<Sigset>::from(0), 0)
        } else {
            #[repr(C)]
            struct PSelectSigsetArg {
                ss: usize,
                ss_len: usize,
            }
            let arg: PSelectSigsetArg = UserInPtr::from(sigset_arg).read()?;
            (UserInPtr::from(arg.ss), arg.ss_len)
        };
        let mut guard = self.install_temp_sigmask(sigmask, sigsetsize)?;
        let result = self
            .select_core(nfds, read, write, err, timeout_msecs)
            .await;
        if matches!(result, Err(LxError::EINTR)) {
            if let Some(g) = guard.as_mut() {
                g.keep_for_signal();
            }
        }
        if let Some(left) = select_time_left(timeout_left, mono_now().saturating_sub(begin), sticky)
        {
            // A `timespec` in read-only memory must not turn a completed
            // select into EFAULT (`fs/select.c`, "sticky"): the write's
            // failure is ignored.
            let _ = timeout.write(TimeSpec::from_duration(left));
        }
        result
    }

    /// allow a program to monitor multiple file descriptors,
    /// waiting until one or more of the file descriptors become "ready" for some class of I/O operation.
    ///
    /// A file descriptor is considered ready if it is possible to perform the corresponding I/O operation (e.g., read) without blocking.
    pub async fn sys_select(
        &mut self,
        nfds: usize,
        read: UserInOutPtr<u32>,
        write: UserInOutPtr<u32>,
        err: UserInOutPtr<u32>,
        mut timeout: UserInOutPtr<TimeVal>,
    ) -> SysResult {
        let _ = self.maybe_handle_tty_intr()?;
        info!(
            "select: nfds: {}, read: {:?}, write: {:?}, err: {:?}, timeout: {:?}",
            nfds, read, write, err, timeout
        );
        /* nfds = 0 is a valid way to sleep in POSIX
        if nfds as u64 == 0 {
            return Ok(0);
        } */
        let (timeout_msecs, timeout_left) = if !timeout.is_null() {
            let tv = timeout.read()?;
            (tv.try_into_poll_msecs()?, Some(tv.try_into_duration()?))
        } else {
            // infinity
            (-1, None)
        };
        let sticky = self.linux_process().personality() & STICKY_TIMEOUTS != 0;
        let begin = mono_now();
        let result = self
            .select_core(nfds, read, write, err, timeout_msecs)
            .await;
        if let Some(left) = select_time_left(timeout_left, mono_now().saturating_sub(begin), sticky)
        {
            // See `sys_pselect6`: a failed write-back is ignored.
            let _ = timeout.write(TimeVal::from(left));
        }
        result
    }

    /// Shared body of `select`/`pselect6` once the timeout is normalized to
    /// milliseconds (`-1` = infinite). The two entry points differ only in the
    /// timeout's user-space type: `timeval` (us) vs `timespec` (ns).
    pub async fn select_core(
        &mut self,
        nfds: usize,
        read: UserInOutPtr<u32>,
        write: UserInOutPtr<u32>,
        err: UserInOutPtr<u32>,
        timeout_msecs: isize,
    ) -> SysResult {
        // `nfds` is an `int` that Linux clamps to the caller's open-file
        // table; unclamped it made `0..nfds` a two-billion-iteration scan on
        // every pass. See `select_nfds`.
        let nfds = select_nfds(nfds)?;
        let mut read_fds = FdSet::new(read, nfds)?;
        let mut write_fds = FdSet::new(write, nfds)?;
        let mut err_fds = FdSet::new(err, nfds)?;
        // `max_select_fd`: a closed fd in any of the three sets is EBADF
        // before the wait begins. It used to be skipped, so a program that
        // closed a socket and left its bit set waited for the timeout (or for
        // ever) instead of learning which fd to drop.
        {
            let files = self.linux_process().get_files()?;
            let in_any = |fd: usize| {
                let fd = FileDesc::from(fd);
                read_fds.contains(fd) || write_fds.contains(fd) || err_fds.contains(fd)
            };
            if select_closed_fd(nfds, in_any, |fd| files.contains_key(&FileDesc::from(fd)))
                .is_some()
            {
                return Err(LxError::EBADF);
            }
        }
        let begin_time = mono_now();

        // The select set membership (`origin`) does not change while the future
        // is being polled, so whether we need to pump the network / interactive
        // I/O is invariant. Compute it once here instead of on every wakeup.
        let watch_net = (0..nfds).any(|fd| {
            fd >= linux_object::net::SOCKET_FD
                && (read_fds.contains(FileDesc::from(fd))
                    || write_fds.contains(FileDesc::from(fd))
                    || err_fds.contains(FileDesc::from(fd)))
        });
        let watch_interactive = (0..nfds).any(|fd| {
            linux_object::net::fd_is_interactive(FileDesc::from(fd))
                && (read_fds.contains(FileDesc::from(fd))
                    || write_fds.contains(FileDesc::from(fd))
                    || err_fds.contains(FileDesc::from(fd)))
        });
        // Membership is fixed, so the "only terminals" shape is too — the one
        // shape `io_wait_interval` may demote to the background-VT tick.
        let terminal_only = {
            let files = self.linux_process().get_files()?;
            let mut any = false;
            let all = (0..nfds).all(|fd| {
                let fd = FileDesc::from(fd);
                if !(read_fds.contains(fd) || write_fds.contains(fd) || err_fds.contains(fd)) {
                    return true;
                }
                any = true;
                files.get(&fd).map(|f| f.is_terminal()).unwrap_or(false)
            });
            any && all
        };

        #[must_use = "future does nothing unless polled/`await`-ed"]
        struct SelectFuture<'a> {
            read_fds: &'a mut FdSet,
            write_fds: &'a mut FdSet,
            err_fds: &'a mut FdSet,
            nfds: usize,
            watch_net: bool,
            watch_interactive: bool,
            /// See `PollFuture::terminal_only`.
            terminal_only: bool,
            timeout_msecs: isize,
            begin_time: Duration,
            syscall: &'a Syscall<'a>,
            io_armed: bool,
            timer: Option<kernel_hal::timer_waker::TimerWakerSlot>,
            io_waker: Option<core::task::Waker>,
            /// Readiness wakers parked on the watched fds (see PollFuture).
            subs: Vec<linux_object::sync::ReadinessSub>,
        }

        impl Drop for SelectFuture<'_> {
            fn drop(&mut self) {
                let wn = self.watch_net;
                let wi = self.watch_interactive;
                clear_poll_io(&mut self.timer, &mut self.io_waker, wn, wi);
            }
        }

        impl<'a> Future for SelectFuture<'a> {
            type Output = SysResult;

            fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
                let this = self.get_mut();
                // Unsubscribe last pass's readiness wakers before re-scanning.
                this.subs.clear();
                let watch_net = this.watch_net;
                let watch_interactive = this.watch_interactive;
                let terminal_only = this.terminal_only;
                if this.io_armed {
                    arm_io_wait(cx, watch_net, watch_interactive, &mut this.io_armed);
                }
                linux_object::net::io_wait_tick(watch_net, watch_interactive);
                let files = this.syscall.linux_process().get_files()?;

                let mut events = 0;
                let mut early_err = None;
                // Iterate only the fds in the select set instead of every open
                // fd in the process.
                for fd in 0..this.nfds {
                    let fd = FileDesc::from(fd);
                    if !this.err_fds.contains(fd)
                        && !this.read_fds.contains(fd)
                        && !this.write_fds.contains(fd)
                    {
                        continue;
                    }
                    let file_like = match files.get(&fd) {
                        Some(f) => f,
                        None => continue,
                    };
                    // Sync poll only — same stack-overflow rationale as sys_poll /
                    // Epoll::wait (labwc bring-up #DF in `__from_user`).
                    let status = match file_like.poll(PollEvents::all()) {
                        Ok(ret) => ret,
                        Err(err) => {
                            early_err = Some(err);
                            break;
                        }
                    };
                    let ready = select_ready(&status);
                    if ready.except && this.err_fds.contains(fd) {
                        this.err_fds.set(fd);
                        events += 1;
                    }
                    if ready.read && this.read_fds.contains(fd) {
                        this.read_fds.set(fd);
                        events += 1;
                    }
                    if ready.write && this.write_fds.contains(fd) {
                        this.write_fds.set(fd);
                        events += 1;
                    }
                }
                if let Some(err) = early_err {
                    clear_poll_io(
                        &mut this.timer,
                        &mut this.io_waker,
                        watch_net,
                        watch_interactive,
                    );
                    return Poll::Ready(Err(err));
                }

                // some event happens, so evoke the process
                if events > 0 {
                    clear_poll_io(
                        &mut this.timer,
                        &mut this.io_waker,
                        watch_net,
                        watch_interactive,
                    );
                    // Flush the ready bitmaps to user space once.
                    this.read_fds.commit();
                    this.write_fds.commit();
                    this.err_fds.commit();
                    return Poll::Ready(Ok(events));
                }

                if let Err(e) = linux_object::process::check_signals() {
                    clear_poll_io(
                        &mut this.timer,
                        &mut this.io_waker,
                        watch_net,
                        watch_interactive,
                    );
                    return Poll::Ready(Err(e));
                }

                // Same readiness-subscription scheme as PollFuture: park a
                // waker per watched fd, stretch the backstop when every fd
                // took one.
                let mut covered = false;
                if this.timeout_msecs != 0 {
                    covered = true;
                    let mut any_watched = false;
                    for fd in 0..this.nfds {
                        let fd = FileDesc::from(fd);
                        let mut interest = PollEvents::empty();
                        if this.read_fds.contains(fd) {
                            interest |= PollEvents::IN;
                        }
                        if this.write_fds.contains(fd) {
                            interest |= PollEvents::OUT;
                        }
                        if interest.is_empty() && !this.err_fds.contains(fd) {
                            continue;
                        }
                        any_watched = true;
                        match files
                            .get(&fd)
                            .and_then(|f| f.subscribe_readiness(interest, cx.waker()))
                        {
                            Some(sub) => this.subs.push(sub),
                            None => covered = false,
                        }
                    }
                    covered &= any_watched;
                }
                let covered_tick =
                    Duration::from_millis(linux_object::net::wait::IO_WAIT_COVERED_TICK_MS);

                match poll_wait_decision(this.timeout_msecs, this.begin_time, mono_now()) {
                    PollWait::ReturnEmpty => {
                        // A select that ran out of time still answers with the
                        // (empty) ready sets, as Linux does.
                        this.read_fds.commit();
                        this.write_fds.commit();
                        this.err_fds.commit();
                        clear_poll_io(
                            &mut this.timer,
                            &mut this.io_waker,
                            watch_net,
                            watch_interactive,
                        );
                        return Poll::Ready(Ok(0));
                    }
                    PollWait::Sleep(limit) => {
                        let tick = if covered {
                            covered_tick
                        } else {
                            io_wait_interval(
                                this.syscall,
                                watch_net,
                                watch_interactive,
                                terminal_only,
                            )
                        };
                        let wake_in = wake_after(limit, tick);
                        arm_io_wait(cx, watch_net, watch_interactive, &mut this.io_armed);
                        this.io_waker = Some(cx.waker().clone());
                        schedule_poll_wakeup(cx, wake_in, &mut this.timer);
                    }
                }
                Poll::Pending
            }
        }
        let future = SelectFuture {
            read_fds: &mut read_fds,
            write_fds: &mut write_fds,
            err_fds: &mut err_fds,
            nfds,
            watch_net,
            watch_interactive,
            terminal_only,
            timeout_msecs,
            begin_time,
            syscall: self,
            io_armed: false,
            timer: None,
            io_waker: None,
            subs: Vec::new(),
        };
        future.await
    }

    /// creates an epoll instance
    pub fn sys_epoll_create1(&self, flags: usize) -> SysResult {
        info!("epoll_create1: flags={:#x}", flags);
        // `EPOLL_CLOEXEC` is the only flag there is.
        let flags = anon_fd_flags(flags, ANON_CLOEXEC)?;
        let proc = self.linux_process();
        let epoll = Epoll::new(flags);
        let fd = proc.add_file(epoll)?;
        Ok(fd.into())
    }

    /// opens an epoll file descriptor
    pub fn sys_epoll_create(&self, size: usize) -> SysResult {
        info!("epoll_create: size={}", size);
        epoll_create_size(size)?;
        self.sys_epoll_create1(0)
    }

    /// control interface for an epoll file descriptor
    pub fn sys_epoll_ctl(
        &self,
        epfd: FileDesc,
        op: i32,
        fd: FileDesc,
        event: UserInPtr<EpollEvent>,
    ) -> SysResult {
        info!(
            "epoll_ctl: epfd={:?}, op={}, fd={:?}, event={:?}",
            epfd, op, fd, event
        );
        let proc = self.linux_process();
        let epoll_file = proc.get_file_like(epfd)?;
        let epoll = epoll_file.downcast_ref::<Epoll>().ok_or(LxError::EBADF)?;
        let (event, file) = if op == 2 {
            // EPOLL_CTL_DEL: no event payload, no file handle needed.
            (EpollEvent { events: 0, data: 0 }, None)
        } else {
            // ADD/MOD: resolve the target fd so the epoll can poll it directly
            // (required for nested-epoll readiness).
            (event.read()?, Some(proc.get_file_like(fd)?))
        };
        epoll.ctl(op, fd, event, file)
    }

    /// wait for an I/O event on an epoll file descriptor
    pub async fn sys_epoll_pwait(
        &self,
        epfd: FileDesc,
        mut events: UserOutPtr<EpollEvent>,
        maxevents: usize,
        timeout: isize,
        sigmask: UserInPtr<Sigset>,
        sigsetsize: usize,
    ) -> SysResult {
        log::trace!(
            "epoll_pwait: epfd={:?}, maxevents={}, timeout={}",
            epfd,
            maxevents,
            timeout
        );
        // Validated before anything else, as Linux does: `maxevents` sizes the
        // caller's output array, and the wait loop fills it without a second
        // bound. See `epoll_maxevents`.
        let maxevents = epoll_maxevents(maxevents)?;
        let mut guard = self.install_temp_sigmask(sigmask, sigsetsize)?;
        // Resolve the epoll object to an owned Arc (not a borrow of a local):
        // `wait` awaits, and a stale net/timer waker re-polling this future
        // after teardown must not dereference a freed process/file. The Arc
        // keeps the epoll object itself alive for the whole wait; `wait`
        // likewise holds each watched file by Arc, so the future carries no
        // reference that outlives what it points at.
        let epoll = match self
            .linux_process()
            .get_file_like(epfd)
            .and_then(|f| f.downcast_arc::<Epoll>().map_err(|_| LxError::EBADF))
        {
            Ok(e) => e,
            Err(e) => {
                linux_object::process::trace_wait_error("epoll_wait/epfd", e);
                return Err(e);
            }
        };

        // TODO: handle timeout
        let result = match epoll.wait(maxevents, timeout).await {
            Ok(v) => {
                if let Err(e) = events.write_array(&v) {
                    let e: LxError = e.into();
                    linux_object::process::trace_wait_error("epoll_wait/write", e);
                    Err(e)
                } else {
                    Ok(v.len())
                }
            }
            Err(e) => {
                linux_object::process::trace_wait_error("epoll_wait", e);
                Err(e)
            }
        };
        if matches!(result, Err(LxError::EINTR)) {
            if let Some(g) = guard.as_mut() {
                g.keep_for_signal();
            }
        }
        result
    }

    /// wait for an I/O event on an epoll file descriptor
    pub async fn sys_epoll_wait(
        &self,
        epfd: FileDesc,
        events: UserOutPtr<EpollEvent>,
        maxevents: usize,
        timeout: isize,
    ) -> SysResult {
        self.sys_epoll_pwait(epfd, events, maxevents, timeout, 0.into(), 0)
            .await
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct PollFd {
    fd: FileDesc,
    events: PollEvents,
    revents: PollEvents,
}

/// fd size per item
const FD_PER_ITEM: usize = u32::BITS as usize;
/// max Fdset size
const MAX_FDSET_SIZE: usize = 1024 / FD_PER_ITEM;

/// The lowest fd below `nfds` that is in one of the three sets and not
/// open: `max_select_fd` (fs/select.c) makes it EBADF before anything is
/// waited on. `in_any` says whether an fd is in a set, `is_open` whether the
/// process has it.
pub(crate) fn select_closed_fd(
    nfds: usize,
    in_any: impl Fn(usize) -> bool,
    is_open: impl Fn(usize) -> bool,
) -> Option<usize> {
    (0..nfds).find(|&fd| in_any(fd) && !is_open(fd))
}

/// Which of the three sets a poll status lights up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SelectReady {
    /// `readfds`.
    pub read: bool,
    /// `writefds`.
    pub write: bool,
    /// `exceptfds`.
    pub except: bool,
}

/// `do_select`'s three masks: `POLLIN_SET` is IN, HUP and ERR; `POLLOUT_SET`
/// is OUT and ERR; `POLLEX_SET` is PRI alone (which nothing here reports).
///
/// An error used to go to `exceptfds` only, and a hangup nowhere: a pipe
/// whose reader had gone (`write: false, error: true`) never left
/// `select(writefds)`, when Linux returns it writable so the write can fail
/// with EPIPE; and a socket that hung up was not readable to `select`, though
/// its `read` of 0 was waiting.
pub(crate) fn select_ready(status: &PollStatus) -> SelectReady {
    SelectReady {
        read: status.read || status.hangup || status.error,
        write: status.write || status.error,
        except: false,
    }
}

/// FdSet data struct for select
struct FdSet {
    /// input addr, for update Fdset use
    addr: UserInOutPtr<u32>,
    /// FdSet bit buffer
    origin: BitVec<Lsb0, u32>,
    /// Ready bit buffer
    ready: BitVec<Lsb0, u32>,
}

impl FdSet {
    /// Initialize a `FdSet` from pointer and number of fds
    /// Check if the array is large enough
    fn new(addr: UserInOutPtr<u32>, nfds: usize) -> Result<FdSet, LxError> {
        if addr.is_null() {
            Ok(FdSet {
                addr,
                origin: BitVec::new(),
                ready: BitVec::new(),
            })
        } else {
            let len = nfds.div_ceil(FD_PER_ITEM);
            if len > MAX_FDSET_SIZE {
                return Err(LxError::EINVAL);
            }
            // Save the caller's set. Do NOT clear it here: the result is
            // written once, by `commit`, and only on the paths where select
            // actually has an answer. Zeroing it up front meant a select that
            // failed — `EINTR` from a signal, or `EFAULT`/`EINVAL` raised
            // while building one of the *later* two sets — handed the caller
            // back an emptied `fd_set` that Linux leaves untouched. The usual
            // `while (select(...) < 0 && errno == EINTR) continue;` loop then
            // re-entered on a set with no fds in it and waited for an event
            // that could no longer be asked for.
            let origin = BitVec::from_slice(addr.as_slice(len)?).unwrap();
            let ready = BitVec::from_slice(&alloc::vec![0; len]).unwrap();
            Ok(FdSet {
                addr,
                origin,
                ready,
            })
        }
    }

    /// Mark `fd` as ready in this `FdSet`.
    ///
    /// This only updates the in-memory bitmap; the result is flushed to user
    /// space once via [`commit`](Self::commit) instead of rewriting the whole
    /// bit buffer on every ready fd.
    /// Fd should be less than nfds
    fn set(&mut self, fd: FileDesc) {
        let fd: usize = fd.into();
        if fd < self.ready.len() {
            self.ready.set(fd, true);
        }
    }

    /// Write the ready bitmap back to user memory once.
    fn commit(&mut self) {
        if self.ready.is_empty() {
            return;
        }
        let vec: Vec<u32> = self.ready.clone().into();
        let _ = self.addr.write_array(&vec);
    }

    /// Check to see whether `fd` is in original `FdSet`
    /// Fd should be less than nfds
    fn contains(&self, fd: FileDesc) -> bool {
        let fd: usize = fd.into();
        if fd < self.origin.len() {
            self.origin[fd]
        } else {
            false
        }
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod abi_tests {
    use super::*;
    use core::mem::size_of;

    #[test]
    fn pollfd_matches_linux_uapi() {
        assert_eq!(size_of::<PollFd>(), 8);
    }
}

#[cfg(test)]
mod select_ready_tests {
    //! `select`'s answer for a closed fd, and which set each condition lands
    //! in.

    use super::*;

    fn status(read: bool, write: bool, error: bool, hangup: bool) -> PollStatus {
        PollStatus {
            read,
            write,
            error,
            hangup,
        }
    }

    /// A closed fd in any set is found, and only below `nfds`; an fd in no
    /// set is nobody's business, open or not.
    #[test]
    fn a_closed_fd_in_a_set_is_found_below_nfds() {
        let in_set = |fd: usize| fd == 3 || fd == 7;
        let open = |fd: usize| fd != 7;
        assert_eq!(select_closed_fd(8, in_set, open), Some(7));
        assert_eq!(
            select_closed_fd(7, in_set, open),
            None,
            "7 is not below nfds"
        );
        assert_eq!(select_closed_fd(8, in_set, |_| true), None);
        assert_eq!(select_closed_fd(8, |_| false, |_| false), None, "in no set");
        assert_eq!(select_closed_fd(0, |_| true, |_| false), None);
        // The first of several, as `max_select_fd` stops at the first.
        assert_eq!(select_closed_fd(8, |_| true, |fd| fd > 4), Some(0));
    }

    /// Error is readable and writable, hangup readable, and `exceptfds` is
    /// for urgent data alone.
    #[test]
    fn error_and_hangup_land_where_do_select_puts_them() {
        let ready = |st| select_ready(&st);
        let r = |read, write, except| SelectReady {
            read,
            write,
            except,
        };
        assert_eq!(
            ready(status(true, false, false, false)),
            r(true, false, false)
        );
        assert_eq!(
            ready(status(false, true, false, false)),
            r(false, true, false)
        );
        // The write end of a pipe whose reader is gone.
        assert_eq!(
            ready(status(false, false, true, true)),
            r(true, true, false)
        );
        // A socket both sides of which are shut.
        assert_eq!(
            ready(status(false, false, false, true)),
            r(true, false, false)
        );
        assert_eq!(
            ready(status(false, false, true, false)),
            r(true, true, false)
        );
        assert_eq!(
            ready(status(false, false, false, false)),
            r(false, false, false)
        );
    }
}

#[cfg(test)]
mod select_time_left_tests {
    //! The Linux-only write-back of `select`'s timeout, which never happened.

    use super::*;

    const S: fn(u64) -> Duration = Duration::from_secs;

    /// The time left of the timeout, and zero once it has run out: never
    /// "nothing" for a timeout that was given.
    #[test]
    fn the_time_left_is_written_back_zero_once_it_ran_out() {
        assert_eq!(select_time_left(Some(S(5)), S(2), false), Some(S(3)));
        assert_eq!(select_time_left(Some(S(5)), S(5), false), Some(S(0)));
        assert_eq!(select_time_left(Some(S(5)), S(7), false), Some(S(0)));
        assert_eq!(
            select_time_left(
                Some(Duration::from_millis(100)),
                Duration::from_millis(1),
                false
            ),
            Some(Duration::from_millis(99))
        );
    }

    /// `fs/select.c`: no pointer, "No update for zero timeout", and the
    /// `STICKY_TIMEOUTS` persona all leave the caller's struct alone.
    #[test]
    fn null_zero_and_sticky_timeouts_are_left_alone() {
        assert_eq!(select_time_left(None, S(2), false), None);
        assert_eq!(select_time_left(Some(S(0)), S(0), false), None);
        assert_eq!(select_time_left(Some(S(5)), S(2), true), None);
        assert_eq!(STICKY_TIMEOUTS, 0x0400_0000);
    }
}

#[cfg(test)]
mod poll_tests {
    //! Host tests for the four numbers `poll`/`select`/`epoll_wait` take from
    //! userspace, and for the decision every blocking pass makes.
    //!
    //! This is the file a desktop lives in: a compositor, its clients and
    //! every shell spend nearly all of their time parked in one of these three
    //! syscalls, so a wrong answer here shows up as "the machine is slow" or
    //! "it hung" and never as an error. None of it needs a process, a timer or
    //! an fd — the parts that decide are plain functions of their arguments,
    //! which is why they were split out.

    use super::*;

    /// The register a syscall argument arrives in, holding the given `int`.
    /// The C caller only ever set the low 32 bits; what is above them is not
    /// part of the value.
    fn reg(value: i32) -> usize {
        value as u32 as usize
    }

    // ---- poll(2) / epoll_wait(2) timeout --------------------------------

    #[test]
    fn a_timeout_in_milliseconds_is_taken_as_it_is() {
        assert_eq!(poll_timeout_msecs(reg(0)), 0);
        assert_eq!(poll_timeout_msecs(reg(1)), 1);
        assert_eq!(poll_timeout_msecs(reg(250)), 250);
        // The longest finite wait an `int` of milliseconds can name: ~24 days.
        assert_eq!(poll_timeout_msecs(reg(i32::MAX)), i32::MAX as isize);
    }

    #[test]
    fn minus_one_is_the_wait_with_no_deadline() {
        assert_eq!(poll_timeout_msecs(-1isize as usize), -1);
    }

    /// The hang. poll(2): "a negative value means an infinite timeout" — any
    /// negative value. The futures matched `-1` alone and let the rest fall
    /// into an arm that returned `Pending` with no timer and no waker, so
    /// `poll(fds, n, -2)` parked the caller with nothing left in the kernel
    /// that could ever wake it.
    #[test]
    fn every_negative_timeout_is_infinite_not_just_minus_one() {
        for raw in [-2i32, -3, -1000, i32::MIN] {
            assert_eq!(
                poll_timeout_msecs(reg(raw)),
                -1,
                "timeout {} must mean for ever",
                raw
            );
        }
    }

    /// `SYSCALL_DEFINE3(poll, ..., int, timeout_msecs)`: the top half of the
    /// register is not part of the number.
    #[test]
    fn the_high_half_of_the_register_is_not_part_of_the_timeout() {
        assert_eq!(poll_timeout_msecs(0x1_0000_0000), 0);
        assert_eq!(poll_timeout_msecs(0x1_0000_0064), 100);
        assert_eq!(poll_timeout_msecs(0xdead_beef_0000_000a), 10);
    }

    /// A `-1` that arrives zero-extended — what a caller keeping the timeout
    /// in a 32-bit slot leaves in the register — is still "for ever", not a
    /// finite 49-day wait.
    #[test]
    fn a_minus_one_that_arrives_zero_extended_still_means_for_ever() {
        assert_eq!(poll_timeout_msecs(0x0000_0000_ffff_ffff), -1);
    }

    // ---- the blocking decision ------------------------------------------

    const T0: Duration = Duration::from_secs(100);

    #[test]
    fn a_zero_timeout_never_blocks() {
        assert_eq!(poll_wait_decision(0, T0, T0), PollWait::ReturnEmpty);
    }

    #[test]
    fn a_live_deadline_sleeps_for_what_is_left_of_it() {
        let now = T0 + Duration::from_millis(30);
        assert_eq!(
            poll_wait_decision(100, T0, now),
            PollWait::Sleep(Some(Duration::from_millis(70)))
        );
    }

    /// The deadline is measured from the first pass, not from this one: a
    /// re-scan must not restart the caller's timeout.
    #[test]
    fn the_deadline_is_measured_from_the_first_pass() {
        let later = T0 + Duration::from_millis(90);
        assert_eq!(
            poll_wait_decision(100, T0, later),
            PollWait::Sleep(Some(Duration::from_millis(10)))
        );
    }

    #[test]
    fn an_expired_deadline_returns_instead_of_sleeping_again() {
        let now = T0 + Duration::from_millis(101);
        assert_eq!(poll_wait_decision(100, T0, now), PollWait::ReturnEmpty);
    }

    /// Exactly on the deadline the wait is over, not "zero more milliseconds":
    /// sleeping for `Duration::ZERO` would spin.
    #[test]
    fn a_deadline_that_falls_exactly_now_returns() {
        let now = T0 + Duration::from_millis(100);
        assert_eq!(poll_wait_decision(100, T0, now), PollWait::ReturnEmpty);
    }

    #[test]
    fn an_infinite_wait_has_no_deadline_to_run_out() {
        assert_eq!(poll_wait_decision(-1, T0, T0), PollWait::Sleep(None));
        let much_later = T0 + Duration::from_secs(86_400);
        assert_eq!(
            poll_wait_decision(-1, T0, much_later),
            PollWait::Sleep(None)
        );
    }

    /// The other half of the hang: whatever negative number reaches the
    /// future, it must end up blocking *with* a wakeup, never in an arm that
    /// falls through.
    #[test]
    fn any_negative_timeout_blocks_like_minus_one() {
        for t in [-2isize, -1000, i32::MIN as isize, isize::MIN] {
            assert_eq!(
                poll_wait_decision(t, T0, T0),
                PollWait::Sleep(None),
                "timeout {} must block with a wakeup armed",
                t
            );
        }
    }

    #[test]
    fn the_longest_finite_timeout_does_not_overflow_the_deadline() {
        assert_eq!(
            poll_wait_decision(i32::MAX as isize, T0, T0),
            PollWait::Sleep(Some(Duration::from_millis(i32::MAX as u64)))
        );
    }

    // ---- how long the pass actually sleeps -------------------------------

    #[test]
    fn a_wait_with_no_deadline_sleeps_one_tick_at_a_time() {
        assert_eq!(wake_after(None, IO_WAIT_TICK), IO_WAIT_TICK);
        assert_eq!(wake_after(None, SLOW_IO_WAIT_TICK), SLOW_IO_WAIT_TICK);
    }

    /// A timeout shorter than the re-poll tick must wake at the timeout. With
    /// the covered tick at 100 ms, taking the tick instead would answer a
    /// `poll(.., 5)` in 100 ms.
    #[test]
    fn a_deadline_closer_than_the_tick_wins() {
        let five = Duration::from_millis(5);
        assert_eq!(wake_after(Some(five), SLOW_IO_WAIT_TICK), five);
        assert_eq!(wake_after(Some(five), IO_WAIT_TICK), IO_WAIT_TICK);
    }

    #[test]
    fn a_deadline_exactly_one_tick_away_sleeps_one_tick() {
        assert_eq!(wake_after(Some(IO_WAIT_TICK), IO_WAIT_TICK), IO_WAIT_TICK);
    }

    // ---- the re-poll interval -------------------------------------------

    /// The shape the slow tick exists for: a shell parked in `poll(stdin)` on
    /// a VT nobody is looking at. Its keypress can only arrive once that VT is
    /// the active one, so re-scanning it every 4 ms is pure heat.
    #[test]
    fn a_shell_on_a_background_vt_is_polled_slowly() {
        assert_eq!(
            io_wait_interval_for(false, true, true, false),
            SLOW_IO_WAIT_TICK
        );
    }

    #[test]
    fn the_same_shell_on_the_active_vt_is_polled_fast() {
        assert_eq!(io_wait_interval_for(false, true, true, true), IO_WAIT_TICK);
    }

    /// First regression: a set with no interactive fd at all — DRM fds,
    /// timerfds, pipes, device fds, which is the shape of a compositor's
    /// startup waits — was demoted to 100 ms, and every roundtrip of the
    /// startup was gated at a tenth of a second.
    #[test]
    fn a_set_with_no_interactive_fd_is_never_demoted() {
        for on_active_vt in [false, true] {
            assert_eq!(
                io_wait_interval_for(false, false, true, on_active_vt),
                IO_WAIT_TICK
            );
            assert_eq!(
                io_wait_interval_for(false, false, false, on_active_vt),
                IO_WAIT_TICK
            );
        }
    }

    /// Second regression: keying the demotion on `watch_interactive` alone —
    /// true for any non-socket fd — put PulseAudio's ALSA sink thread
    /// (`[pcm, timer]`, in a process that is never on the active VT) on a
    /// 100 ms re-scan. Its 108 ms buffer underran on every wake. Only a set
    /// where *every* fd is a terminal is the pattern.
    #[test]
    fn an_interactive_set_that_is_not_all_terminals_is_polled_fast() {
        assert_eq!(
            io_wait_interval_for(false, true, false, false),
            IO_WAIT_TICK
        );
    }

    /// A socket in the set means the network stack has to be pumped, whatever
    /// else is in there.
    #[test]
    fn a_socket_in_the_set_keeps_the_fast_tick() {
        assert_eq!(io_wait_interval_for(true, true, true, false), IO_WAIT_TICK);
    }

    // ---- select(2)'s nfds -----------------------------------------------

    #[test]
    fn zero_fds_is_a_valid_way_to_sleep() {
        assert_eq!(select_nfds(reg(0)), Ok(0));
    }

    #[test]
    fn a_negative_nfds_is_einval() {
        assert_eq!(select_nfds(reg(-1)), Err(LxError::EINVAL));
        assert_eq!(select_nfds(reg(i32::MIN)), Err(LxError::EINVAL));
    }

    #[test]
    fn a_whole_fd_set_fits() {
        assert_eq!(select_nfds(reg(1024)), Ok(1024));
        assert_eq!(MAX_SELECT_NFDS, 1024);
    }

    /// Linux clamps `nfds` to the caller's open-file table rather than
    /// refusing it (`if (n > max_fds) n = max_fds;`), so a program built with
    /// a wider `fd_set` keeps working. Clamping is also what bounds the
    /// `0..nfds` scan: taken whole, `select(0x7fffffff, NULL, NULL, NULL,
    /// &tv)` — a legal way to sleep — sent the kernel round that loop two
    /// billion times per pass.
    #[test]
    fn more_fds_than_the_set_holds_are_clamped_not_refused() {
        assert_eq!(select_nfds(reg(2000)), Ok(MAX_SELECT_NFDS));
        assert_eq!(select_nfds(reg(i32::MAX)), Ok(MAX_SELECT_NFDS));
    }

    #[test]
    fn the_high_half_of_the_register_is_not_part_of_nfds() {
        assert_eq!(select_nfds(0x1_0000_0008), Ok(8));
        // 0xffff_ffff is `int` -1, not four billion fds.
        assert_eq!(select_nfds(0x0000_0000_ffff_ffff), Err(LxError::EINVAL));
    }

    // ---- epoll_wait(2)'s maxevents ---------------------------------------

    /// The overflow: room for no events is not "give me one anyway". The wait
    /// loop pushes an event before testing the cap, so `maxevents = 0`
    /// returned one and wrote it over whatever followed the caller's array.
    #[test]
    fn asking_for_no_events_is_einval() {
        assert_eq!(epoll_maxevents(reg(0)), Err(LxError::EINVAL));
    }

    #[test]
    fn a_negative_maxevents_is_einval() {
        assert_eq!(epoll_maxevents(reg(-1)), Err(LxError::EINVAL));
        assert_eq!(epoll_maxevents(reg(i32::MIN)), Err(LxError::EINVAL));
        assert_eq!(epoll_maxevents(0x0000_0000_ffff_ffff), Err(LxError::EINVAL));
    }

    #[test]
    fn one_event_is_the_smallest_ask() {
        assert_eq!(epoll_maxevents(reg(1)), Ok(1));
        assert_eq!(epoll_maxevents(reg(1024)), Ok(1024));
    }

    /// `EP_MAX_EVENTS` is `INT_MAX / sizeof(struct epoll_event)`, and the
    /// struct is the 12-byte packed one on x86_64 (asserted in
    /// `linux-object`'s `epoll::abi_tests`).
    #[test]
    fn the_cap_is_the_number_linux_uses() {
        assert_eq!(
            epoll_maxevents(reg(EP_MAX_EVENTS as i32)),
            Ok(EP_MAX_EVENTS)
        );
        assert_eq!(
            epoll_maxevents(reg(EP_MAX_EVENTS as i32 + 1)),
            Err(LxError::EINVAL)
        );
        #[cfg(target_arch = "x86_64")]
        assert_eq!(EP_MAX_EVENTS, 178_956_970);
    }

    #[test]
    fn the_high_half_of_the_register_is_not_part_of_maxevents() {
        assert_eq!(epoll_maxevents(0x1_0000_0008), Ok(8));
        assert_eq!(epoll_maxevents(0x1_0000_0000), Err(LxError::EINVAL));
    }

    // ---- the fd_set itself ------------------------------------------------
    //
    // `libos` addresses are ordinary host addresses, so a `Vec<u32>` is a
    // valid stand-in for a user-space `fd_set` and the copies below run for
    // real.

    fn user_fdset(words: &mut [u32]) -> UserInOutPtr<u32> {
        UserInOutPtr::from(words.as_mut_ptr() as usize)
    }

    /// Building the sets must not touch the caller's memory. It used to zero
    /// each one as it read it, so a select that then failed — `EINTR` from a
    /// signal, or a fault raised while reading the *second* set — handed back
    /// an emptied `fd_set` that Linux leaves exactly as it was. The
    /// `while (select(...) < 0 && errno == EINTR) continue;` loop that every
    /// other program is written with then asked about no fds at all.
    #[test]
    fn reading_a_fd_set_leaves_the_callers_copy_alone() {
        let mut words = [0b1011u32, 0xffff_ffff];
        let before = words;
        let fds = FdSet::new(user_fdset(&mut words), 64).unwrap();
        assert_eq!(words, before);
        assert!(fds.contains(FileDesc::from(0usize)));
        assert!(fds.contains(FileDesc::from(1usize)));
        assert!(!fds.contains(FileDesc::from(2usize)));
        assert!(fds.contains(FileDesc::from(3usize)));
        assert!(fds.contains(FileDesc::from(32usize)));
    }

    /// The answer is written once, and it is the ready set — not the set the
    /// caller asked about. select(2) returns the fds that fired, so the ones
    /// that did not must come back clear.
    #[test]
    fn committing_replaces_the_set_with_the_fds_that_fired() {
        let mut words = [0b1011u32, 0xffff_ffff];
        let mut fds = FdSet::new(user_fdset(&mut words), 64).unwrap();
        fds.set(FileDesc::from(3usize));
        fds.commit();
        assert_eq!(words, [0b1000u32, 0]);
    }

    /// A timeout is an answer too: nothing fired, so the caller's sets come
    /// back empty rather than still full of what it asked about.
    #[test]
    fn committing_nothing_clears_the_set() {
        let mut words = [0xffff_ffffu32];
        let mut fds = FdSet::new(user_fdset(&mut words), 32).unwrap();
        fds.commit();
        assert_eq!(words, [0u32]);
    }

    /// `select(n, NULL, NULL, &exceptfds, &tv)` is ordinary: the sets a caller
    /// does not use are null, and an fd is never in one.
    #[test]
    fn a_null_fd_set_holds_nothing_and_writes_nothing() {
        let mut fds = FdSet::new(UserInOutPtr::from(0), 64).unwrap();
        assert!(!fds.contains(FileDesc::from(0usize)));
        fds.set(FileDesc::from(0usize));
        fds.commit();
    }

    /// An fd past the end of the set is simply not in it, and marking it
    /// ready writes nothing: `select_core` walks `0..nfds` against three sets
    /// that may be different sizes.
    #[test]
    fn an_fd_past_the_end_of_the_set_is_not_in_it() {
        let mut words = [0xffff_ffffu32];
        let mut fds = FdSet::new(user_fdset(&mut words), 32).unwrap();
        assert!(!fds.contains(FileDesc::from(32usize)));
        fds.set(FileDesc::from(32usize));
        fds.commit();
        assert_eq!(words, [0u32]);
    }

    /// The `fd_set` this kernel accepts stops at 1024 fds; `select_nfds` now
    /// clamps before this is reached, so the two bounds agree.
    #[test]
    fn a_fd_set_wider_than_the_kernel_accepts_is_einval() {
        let mut words = [0u32; MAX_FDSET_SIZE + 1];
        assert_eq!(
            FdSet::new(user_fdset(&mut words), MAX_SELECT_NFDS + 1).err(),
            Some(LxError::EINVAL)
        );
        assert!(FdSet::new(user_fdset(&mut words), MAX_SELECT_NFDS).is_ok());
    }
}

#[cfg(test)]
mod epoll_create_tests {
    //! `epoll_create(2)`'s obsolete `size`, and the reason an obsolete
    //! argument still has to be read.

    use super::*;

    /// Linux has rejected a non-positive `size` since before it stopped
    /// using the value, so `epoll_create(0)` is EINVAL everywhere. Here the
    /// argument was not looked at, which made this the one kernel where a
    /// program testing its own error handling got an fd back.
    #[test]
    fn a_size_that_is_not_positive_is_refused() {
        assert_eq!(epoll_create_size(0), Err(LxError::EINVAL));
        assert_eq!(epoll_create_size(1), Ok(1));
        assert_eq!(epoll_create_size(1024), Ok(1024));
    }

    /// The syscall's parameter is an `int`, so the machine word is narrowed
    /// before it is judged — the same narrowing `epoll_maxevents` does.
    /// Read as a `usize`, `epoll_create(-1)` is four billion and passes.
    #[test]
    fn a_negative_size_is_negative_and_not_four_billion() {
        for raw in [
            usize::MAX,
            (-1i32) as u32 as usize,
            (i32::MIN as i64) as u64 as usize,
        ] {
            assert_eq!(epoll_create_size(raw), Err(LxError::EINVAL), "{:#x}", raw);
        }
        // The high half of the word is not part of the argument: a value
        // whose low 32 bits are positive is positive.
        assert_eq!(epoll_create_size(0x1_0000_0001), Ok(1));
    }
}
