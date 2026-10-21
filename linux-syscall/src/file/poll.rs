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

/// `poll(2)` / `ppoll(2)`'s `nfds`: an `unsigned int`, refused when it
/// exceeds the soft `RLIMIT_NOFILE` (`do_sys_poll`: `if (nfds >
/// rlimit(RLIMIT_NOFILE)) return -EINVAL`).
///
/// Unchecked, a huge `nfds` made `read_array(nfds)` try to copy an absurd
/// `pollfd` vector out of userspace before anything else ran — and the
/// man page's documented `EINVAL` never answered.
fn poll_nfds(raw: usize, nofile: u64) -> Result<usize, LxError> {
    let n = raw as u32 as usize;
    if n as u64 > nofile {
        return Err(LxError::EINVAL);
    }
    Ok(n)
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

/// One entry of a poll set as [`watch_kinds`] reads it: `None` for an entry
/// poll(2) ignores, `Some(is_socket)` for one it watches.
///
/// poll(2): "if fd is negative then the corresponding events field is
/// ignored". Classifying such an entry as a non-socket -- which is what it
/// looks like once the descriptor cannot be resolved -- made a set of
/// nothing but ignored entries ask for the interactive path, so a plain
/// `poll(fds, n, timeout)` used to wake on every tty interrupt for the
/// length of its timeout.
fn poll_entry_kind(raw: i32, is_socket: impl FnOnce() -> bool) -> Option<bool> {
    (raw >= 0).then(is_socket)
}

/// What a wait must pump, from whether each watched fd is a socket:
/// `(watch_net, watch_interactive)`.
///
/// Both halves, from one pass, because resolving a descriptor to its file
/// costs a table lookup and the two questions are the same question: a set
/// with a socket in it needs the network stack driven, and a set with
/// anything else in it needs the interactive path. A set can need both, and
/// an empty set needs neither.
///
/// It used to be two reads of the fd NUMBER (`fd_is_socket`: `>= SOCKET_FD`,
/// and `fd_is_interactive`: `< SOCKET_FD` and not negative). The first of
/// those cast the descriptor to `usize`, so a negative fd -- the "ignore
/// this entry" value poll(2) documents -- came out enormous, read as a
/// socket, and pumped the stack for nothing.
fn watch_kinds(sockets: impl Iterator<Item = bool>) -> (bool, bool) {
    let mut net = false;
    let mut interactive = false;
    for is_socket in sockets {
        if is_socket {
            net = true;
        } else {
            interactive = true;
        }
        if net && interactive {
            break;
        }
    }
    (net, interactive)
}

/// Whether a wait's fd set is the one shape [`io_wait_interval`] may demote to
/// the background-VT tick: at least one fd that counts, and every one of them
/// a terminal. `None` is an fd that does not count -- a negative `pollfd`
/// (POSIX says they are ignored), or an fd in none of `select`'s three sets;
/// `Some` is whether that fd is a terminal, an fd the process cannot resolve
/// counting as not one.
///
/// Written once because both wait loops computed it inline, and the decision
/// it feeds has regressed in both directions. Keyed on `watch_interactive`
/// alone it demoted PulseAudio's ALSA sink thread (`[pcm, timer]`, never on
/// the active VT) to a 100 ms re-scan, and its 108 ms buffer underran on every
/// wake; and a set with no terminal in it at all -- DRM fds, timerfds, pipes,
/// the shape of a compositor's startup waits -- must not be demoted either, or
/// every roundtrip is gated at a tenth of a second. An empty set is not the
/// shape: there is no terminal in it to be on a background VT.
fn only_terminals(fds: impl Iterator<Item = Option<bool>>) -> bool {
    let mut any = false;
    for is_terminal in fds {
        match is_terminal {
            None => continue,
            Some(true) => any = true,
            Some(false) => return false,
        }
    }
    any
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

fn arm_io_wait(
    cx: &mut Context,
    watch_net: bool,
    watch_interactive: bool,
    watch_hid: bool,
    io_armed: &mut bool,
) {
    if *io_armed {
        linux_object::net::retain_io_wait_wakers_hid(
            cx.waker(),
            watch_net,
            watch_interactive,
            watch_hid,
        );
        *io_armed = false;
        return;
    }
    linux_object::net::register_io_wait_wakers_hid(
        cx.waker(),
        watch_net,
        watch_interactive,
        watch_hid,
    );
    *io_armed = true;
}

/// Did an input frame land in the window between the descriptor scan above and
/// the arming that follows it?
///
/// `hid_seq` is [`linux_object::fs::devfs::input::wait::input_seq`] as it stood
/// *before* the scan. A frame in that window is the one wake that could be
/// lost: its own wake drained a list this task had not joined yet, and the
/// scan that would have seen the packet had already run -- so the task parked
/// with a packet queued and woke on the fallback tick instead, which is the
/// latency libinput reports as "your system is too slow".
///
/// `true` means go round again rather than park; the task is self-woken here,
/// so the caller's `Poll::Pending` is re-polled at once. `false` when there is
/// no input device in this set, which must not consult the counter at all:
/// another process's keystroke is none of this wait's business.
fn hid_frame_missed(cx: &mut Context, watch_hid: bool, hid_seq: u64) -> bool {
    if !watch_hid || linux_object::fs::devfs::input::wait::input_seq() == hid_seq {
        return false;
    }
    cx.waker().wake_by_ref();
    true
}

fn clear_poll_io(
    timer: &mut Option<kernel_hal::timer_waker::TimerWakerSlot>,
    io_waker: &mut Option<core::task::Waker>,
    watch_net: bool,
    watch_interactive: bool,
) {
    kill_poll_timer(timer);
    if let Some(w) = io_waker.take() {
        // `true` for the input list, whatever this pass computed. These
        // futures live across every pass of the wait and overwrite their
        // watch flags on each one, so an input fd closed mid-wait flips
        // `watch_hid` from true to false and a clear gated on it would walk
        // past the registration the earlier pass made -- leaving a waker that
        // a later input frame fires into a wait that is over. Clearing one
        // that was never made is a no-op (`clearing_an_unregistered_waker_
        // does_nothing`), so the unconditional clear costs nothing and cannot
        // strand anything.
        linux_object::net::clear_io_wait_wakers_hid(&w, watch_net, watch_interactive, true);
    }
}

/// The sixth argument of `pselect6` is a pointer to
/// `{ const sigset_t *ss; size_t ss_len; }` (`struct sigset_argpack` in
/// `fs/select.c`). It is read out of the caller's memory with a single
/// `UserInPtr::read`, so the order of the two words is the ABI: read the wrong
/// way round, the length is used as a pointer and the pointer as a length.
///
/// It used to be declared inside `sys_pselect6`, where nothing outside that
/// function body could name it and therefore nothing could pin it.
#[repr(C)]
struct PSelectSigsetArg {
    ss: usize,
    ss_len: usize,
}

impl Syscall<'_> {
    /// Wait for some event on a file descriptor
    pub async fn sys_poll(
        &mut self,
        mut ufds: UserInOutPtr<PollFd>,
        nfds: usize,
        timeout_msecs: isize,
    ) -> SysResult {
        // `nfds > RLIMIT_NOFILE` is EINVAL before the array is touched.
        let nfds = poll_nfds(nfds, self.linux_process().file_limit().cur)?;
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
            /// This set holds an input device (directly, or inside a nested
            /// epoll), so an input frame must wake this wait rather than
            /// leaving it to the re-scan tick. See
            /// `linux_object::fs::devfs::input::wait`.
            watch_hid: bool,
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
                // Asked of the FILE and not of the fd number. Sockets are
                // handed out from `SOCKET_FD` only while the process may
                // hold a descriptor that high (see
                // `LinuxProcess::add_socket`), so "fd >= 1000" stopped
                // meaning "socket" the moment a process with a small
                // `RLIMIT_NOFILE` opened one -- and a wait that does not
                // know it is watching a socket never pumps the stack
                // underneath it.
                let (watch_net, watch_interactive) = {
                    let proc = this.syscall.linux_process();
                    // A negative fd is poll(2)'s "ignore this entry" and is
                    // skipped, not classified: counted as a non-socket it
                    // made a list of nothing but ignored entries ask for the
                    // interactive path, and a plain timeout then woke on
                    // every tty interrupt.
                    let sockets = this.polls.iter().filter_map(|p| {
                        poll_entry_kind(p.fd.into(), || {
                            proc.get_file_like(p.fd)
                                .map(|f| f.is_socket())
                                .unwrap_or(false)
                        })
                    });
                    watch_kinds(sockets)
                };
                // Read BEFORE the scan below (see
                // `linux_object::fs::devfs::input::wait::input_seq`): a frame
                // landing between the scan and `arm_io_wait` drains a list
                // this task is not in yet, and the wait would sleep out its
                // fallback tick for an event that had already arrived.
                let hid_seq = linux_object::fs::devfs::input::wait::input_seq();
                let watch_hid = {
                    let proc = this.syscall.linux_process();
                    this.polls.iter().any(|p| {
                        <FileDesc as Into<i32>>::into(p.fd) >= 0
                            && proc
                                .get_file_like(p.fd)
                                .map(|f| f.is_input_device())
                                .unwrap_or(false)
                    })
                };
                this.watch_net = watch_net;
                this.watch_interactive = watch_interactive;
                this.watch_hid = watch_hid;
                if this.io_armed {
                    arm_io_wait(
                        cx,
                        watch_net,
                        watch_interactive,
                        watch_hid,
                        &mut this.io_armed,
                    );
                }
                linux_object::net::io_wait_tick(watch_net, watch_interactive);
                let proc = this.syscall.linux_process();
                this.terminal_only = only_terminals(this.polls.iter().map(|p| {
                    if <FileDesc as Into<i32>>::into(p.fd) < 0 {
                        None
                    } else {
                        Some(
                            proc.get_file_like(p.fd)
                                .map(|f| f.is_terminal())
                                .unwrap_or(false),
                        )
                    }
                }));
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
                        // POLLHUP/POLLERR are return-only: always reported
                        // when set, even if the caller did not list them in
                        // `events`; the rest is what the caller asked for.
                        poll.revents = PE::revents(&status, poll.events);
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
                    // Counted over the fds that could park a waker, not over
                    // the array: a set of nothing but negative `pollfd`s is
                    // all ignored slots, and calling that "every watched fd
                    // has a subscription doing the real waking" stretched the
                    // backstop to the covered tick with no event source
                    // anywhere in the set. `select` already counted it this
                    // way (`covered &= any_watched`).
                    let mut any_watched = false;
                    covered = true;
                    for p in this.polls.iter() {
                        if <FileDesc as Into<i32>>::into(p.fd) < 0 {
                            continue; // ignored slot (POSIX): nothing to wake on
                        }
                        any_watched = true;
                        match proc
                            .get_file_like(p.fd)
                            .ok()
                            .and_then(|f| f.subscribe_readiness(p.events, cx.waker()))
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
                        arm_io_wait(
                            cx,
                            watch_net,
                            watch_interactive,
                            watch_hid,
                            &mut this.io_armed,
                        );
                        this.io_waker = Some(cx.waker().clone());
                        if hid_frame_missed(cx, watch_hid, hid_seq) {
                            return Poll::Pending;
                        }
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
            watch_hid: false,
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
        // Asked of the FILE and not of the fd number: a socket lands on a
        // low descriptor whenever the process may not hold one at
        // `SOCKET_FD` (see `LinuxProcess::add_socket`).
        let (watch_net, watch_interactive) = {
            let files = self.linux_process().get_files()?;
            watch_kinds((0..nfds).filter_map(|fd| {
                let fd = FileDesc::from(fd);
                (read_fds.contains(fd) || write_fds.contains(fd) || err_fds.contains(fd))
                    .then(|| files.get(&fd).map(|f| f.is_socket()).unwrap_or(false))
            }))
        };
        // Same for whether the set holds an input device: an evdev node has
        // no event bus to subscribe to, so without this registration a key or
        // a mouse frame would be found only by the re-scan tick.
        let watch_hid = {
            let files = self.linux_process().get_files()?;
            (0..nfds).any(|fd| {
                let fd = FileDesc::from(fd);
                (read_fds.contains(fd) || write_fds.contains(fd) || err_fds.contains(fd))
                    && files.get(&fd).map(|f| f.is_input_device()).unwrap_or(false)
            })
        };
        // Membership is fixed, so the "only terminals" shape is too — the one
        // shape `io_wait_interval` may demote to the background-VT tick.
        let terminal_only = {
            let files = self.linux_process().get_files()?;
            only_terminals((0..nfds).map(|fd| {
                let fd = FileDesc::from(fd);
                if !(read_fds.contains(fd) || write_fds.contains(fd) || err_fds.contains(fd)) {
                    None
                } else {
                    Some(files.get(&fd).map(|f| f.is_terminal()).unwrap_or(false))
                }
            }))
        };

        #[must_use = "future does nothing unless polled/`await`-ed"]
        struct SelectFuture<'a> {
            read_fds: &'a mut FdSet,
            write_fds: &'a mut FdSet,
            err_fds: &'a mut FdSet,
            nfds: usize,
            watch_net: bool,
            watch_interactive: bool,
            /// This set holds an input device (directly, or inside a nested
            /// epoll), so an input frame must wake this wait rather than
            /// leaving it to the re-scan tick. See
            /// `linux_object::fs::devfs::input::wait`.
            watch_hid: bool,
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
                let watch_hid = this.watch_hid;
                // Read BEFORE the scan below, for the reason `sys_poll`'s copy
                // of this line gives: a frame between the scan and the arming
                // drains a list this task is not in yet.
                let hid_seq = linux_object::fs::devfs::input::wait::input_seq();
                let terminal_only = this.terminal_only;
                if this.io_armed {
                    arm_io_wait(
                        cx,
                        watch_net,
                        watch_interactive,
                        watch_hid,
                        &mut this.io_armed,
                    );
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
                        arm_io_wait(
                            cx,
                            watch_net,
                            watch_interactive,
                            watch_hid,
                            &mut this.io_armed,
                        );
                        this.io_waker = Some(cx.waker().clone());
                        if hid_frame_missed(cx, watch_hid, hid_seq) {
                            return Poll::Pending;
                        }
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
            watch_hid,
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
        let epoll = Epoll::new(flags | OpenFlags::RDWR);
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
        // `do_epoll_ctl`'s order: the event is copied in for ADD and MOD
        // (EFAULT), then `epfd` and `fd` are both looked up, for every op
        // (EBADF), then the target is judged, and only then is `epfd` not
        // being an epoll EINVAL. It was EBADF, the answer for a closed fd,
        // and DEL never looked `fd` up, so DEL of a closed fd was ENOENT.
        let event = if op == 2 {
            EpollEvent { events: 0, data: 0 }
        } else {
            event.read()?
        };
        let proc = self.linux_process();
        let epoll_file = proc.get_file_like(epfd)?;
        let file = proc.get_file_like(fd)?;
        let epoll = epoll_file.downcast_ref::<Epoll>().ok_or(LxError::EINVAL)?;
        epoll.ctl(op, fd, event, Some(file))
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
        // `get_file_like` answers EBADF for a closed fd; a live fd that is
        // not an epoll is EINVAL (`do_epoll_wait`), same as `epoll_ctl`
        // above. Mapping the downcast to EBADF used to erase that distinction.
        let epoll = match self
            .linux_process()
            .get_file_like(epfd)
            .and_then(|f| f.downcast_arc::<Epoll>().map_err(|_| LxError::EINVAL))
        {
            Ok(e) => e,
            Err(e) => {
                linux_object::process::trace_wait_error("epoll_wait/epfd", e);
                return Err(e);
            }
        };

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

    /// `epoll_pwait2(2)`: [`sys_epoll_pwait`](Self::sys_epoll_pwait) with its
    /// timeout as a `timespec`. A null one waits for ever and `{0, 0}` does
    /// not wait. One out of range is `EINVAL`, read and checked before
    /// anything else, as `epoll_pwait2` does before `do_epoll_pwait`. The
    /// wait counts in milliseconds, as every wait here does, so a timeout is
    /// rounded up to the next one and never comes back early.
    pub async fn sys_epoll_pwait2(
        &self,
        epfd: FileDesc,
        events: UserOutPtr<EpollEvent>,
        maxevents: usize,
        timeout: UserInPtr<TimeSpec>,
        sigmask: UserInPtr<Sigset>,
        sigsetsize: usize,
    ) -> SysResult {
        let timeout = if timeout.is_null() {
            -1
        } else {
            timeout.read()?.try_into_poll_msecs()?
        };
        self.sys_epoll_pwait(epfd, events, maxevents, timeout, sigmask, sigsetsize)
            .await
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

/// The one decision `poll(2)` and `select(2)` share about input frames, and the
/// one both used to carry as a copy of the same four lines. A regression that
/// drops or reorders it here fails these; a regression that drops the *call*
/// from one of the two futures is what the call sites' single shared helper is
/// for.
#[cfg(test)]
mod hid_frame_missed_tests {
    use super::*;
    use core::sync::atomic::{AtomicBool, Ordering};
    use core::task::{RawWaker, RawWakerVTable, Waker};
    use linux_object::fs::devfs::input::wait::{input_seq, wake_input_waiters};

    /// A waker that remembers whether it was woken: the self-wake is half of
    /// what this function has to do, and a `true` without it would park the
    /// task for ever.
    #[allow(unsafe_code)]
    fn watching_waker(flag: &AtomicBool) -> Waker {
        unsafe fn clone(p: *const ()) -> RawWaker {
            RawWaker::new(p, &VTABLE)
        }
        unsafe fn wake(p: *const ()) {
            // SAFETY: the pointer is the `&AtomicBool` this waker was built
            // from, which outlives it.
            unsafe { &*(p as *const AtomicBool) }.store(true, Ordering::SeqCst);
        }
        unsafe fn drop_fn(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake, drop_fn);
        // SAFETY: as above; the vtable only ever reads through that pointer.
        unsafe {
            Waker::from_raw(RawWaker::new(
                flag as *const AtomicBool as *const (),
                &VTABLE,
            ))
        }
    }

    #[test]
    fn a_frame_after_the_scan_sends_the_caller_round_again() {
        let woken = AtomicBool::new(false);
        let waker = watching_waker(&woken);
        let mut cx = Context::from_waker(&waker);
        // Read before the scan, as both futures do.
        let seq = input_seq();
        // The frame that used to be lost: queued after the scan, woken before
        // this task was in the list.
        wake_input_waiters();
        assert!(
            hid_frame_missed(&mut cx, true, seq),
            "a frame in the window has to stop this pass from parking"
        );
        assert!(
            woken.load(Ordering::SeqCst),
            "and has to self-wake, or the `Poll::Pending` that follows parks              the task on the fallback tick after all"
        );
    }

    #[test]
    fn an_empty_window_parks() {
        let woken = AtomicBool::new(false);
        let waker = watching_waker(&woken);
        let mut cx = Context::from_waker(&waker);
        assert!(
            !hid_frame_missed(&mut cx, true, input_seq()),
            "nothing arrived, so this pass sleeps on the input waker list"
        );
        assert!(!woken.load(Ordering::SeqCst), "and nothing woke it");
    }

    #[test]
    fn a_set_without_an_input_device_ignores_the_counter() {
        let woken = AtomicBool::new(false);
        let waker = watching_waker(&woken);
        let mut cx = Context::from_waker(&waker);
        let seq = input_seq();
        wake_input_waiters();
        assert!(
            !hid_frame_missed(&mut cx, false, seq),
            "no evdev node in this set: somebody else's keystroke must not cut              this wait's sleep short"
        );
        assert!(!woken.load(Ordering::SeqCst), "and must not wake it");
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

    /// The arm that decides a finite timeout is `1..`, and the arm under it
    /// is "any other value, wait for ever". Narrowing it to `2..` dropped a
    /// one-millisecond poll straight through to that arm and parked the
    /// caller with no deadline at all -- green, because every other case
    /// here asks for ten milliseconds or more. `poll(fds, n, 1)` is what a
    /// caller writes when it wants one pass and the shortest wait there is,
    /// and it must not be the call that never comes back.
    #[test]
    fn the_shortest_finite_timeout_is_still_finite() {
        assert_eq!(
            poll_wait_decision(1, T0, T0),
            PollWait::Sleep(Some(Duration::from_millis(1)))
        );
        // And it runs out after its one millisecond, rather than never.
        assert_eq!(
            poll_wait_decision(1, T0, T0 + Duration::from_millis(1)),
            PollWait::ReturnEmpty
        );
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

    /// The two intervals themselves, written out rather than compared
    /// against the constants that hold them.
    ///
    /// Every other test here asks *which* tick is picked and asserts against
    /// the constant it is checking, so both follow it: moving
    /// `SLOW_IO_WAIT_TICK` to 10 ms passed green. Neither number is
    /// arbitrary. The slow tick has to stay far enough above the fast one to
    /// be worth demoting to at all -- that is the whole point of it -- and
    /// far enough below the buffers that get re-scanned at this rate:
    /// PulseAudio's is 108 ms, which is what 100 ms was chosen against, and
    /// what the last regression here underran.
    #[test]
    fn the_fast_tick_is_four_milliseconds_and_the_slow_one_a_hundred() {
        assert_eq!(
            IO_WAIT_TICK,
            Duration::from_millis(4),
            "the fast re-poll tick moved"
        );
        assert_eq!(
            SLOW_IO_WAIT_TICK,
            Duration::from_millis(100),
            "the slow re-poll tick moved"
        );
        assert!(
            SLOW_IO_WAIT_TICK >= IO_WAIT_TICK * 10,
            "the slow tick stopped being slower than the fast one by enough to pay for itself"
        );
    }

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

    // ---- the shape that feeds that decision ------------------------------

    /// `terminal_only` is the *input* the two regressions above turned on, and
    /// both wait loops computed it inline. A set of terminals alone is the
    /// shape; one fd that is not a terminal takes the whole set out of it,
    /// wherever in the set it sits.
    #[test]
    fn a_set_of_terminals_alone_is_the_shape_and_one_stranger_ends_it() {
        let t = Some(true);
        let f = Some(false);
        assert!(only_terminals([t].iter().copied()));
        assert!(only_terminals([t, t, t].iter().copied()));
        assert!(!only_terminals([f, t, t].iter().copied()), "first");
        assert!(!only_terminals([t, f, t].iter().copied()), "middle");
        assert!(!only_terminals([t, t, f].iter().copied()), "last");
        assert!(!only_terminals([f].iter().copied()));
    }

    /// An fd that does not count must not be able to *make* the shape: a
    /// `poll` set of nothing but the `-1`s udhcpc6 leaves behind, or a
    /// `select` whose three sets are empty below `nfds`, has no terminal in it
    /// to be on a background VT.
    #[test]
    fn fds_that_do_not_count_cannot_make_the_shape_on_their_own() {
        assert!(!only_terminals(core::iter::empty()));
        assert!(!only_terminals([None].iter().copied()));
        assert!(!only_terminals([None, None, None].iter().copied()));
    }

    /// But they do not break it either: one real terminal beside them is
    /// still a shell on a VT.
    #[test]
    fn an_ignored_fd_beside_a_terminal_leaves_the_shape_alone() {
        assert!(only_terminals([None, Some(true), None].iter().copied()));
        assert!(
            !only_terminals([None, Some(false), None].iter().copied()),
            "an ignored fd does not excuse the one that is not a terminal"
        );
    }

    /// The decision this shape feeds, end to end: it is only ever a demotion
    /// for a set of terminals that is not on the active VT.
    #[test]
    fn only_a_background_set_of_terminals_reaches_the_slow_tick() {
        let demoted = |fds: &[Option<bool>], on_active_vt| {
            io_wait_interval_for(
                false,
                true,
                only_terminals(fds.iter().copied()),
                on_active_vt,
            ) == SLOW_IO_WAIT_TICK
        };
        assert!(demoted(&[Some(true)], false));
        assert!(!demoted(&[Some(true)], true));
        assert!(!demoted(&[Some(true), Some(false)], false));
        assert!(!demoted(&[None], false));
        assert!(!demoted(&[], false));
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

    // ---- poll(2)'s nfds vs RLIMIT_NOFILE ---------------------------------

    #[test]
    fn poll_nfds_at_or_under_the_limit_is_fine() {
        assert_eq!(poll_nfds(0, 1024), Ok(0));
        assert_eq!(poll_nfds(1024, 1024), Ok(1024));
        assert_eq!(poll_nfds(1, 1), Ok(1));
    }

    #[test]
    fn poll_nfds_past_rlimit_nofile_is_einval() {
        assert_eq!(poll_nfds(1025, 1024), Err(LxError::EINVAL));
        assert_eq!(poll_nfds(u32::MAX as usize, 1024), Err(LxError::EINVAL));
    }

    #[test]
    fn poll_nfds_is_an_unsigned_int_not_a_whole_usize() {
        // High half discarded: same narrowing as the syscall's `unsigned int`.
        assert_eq!(poll_nfds(0x1_0000_0008, 1024), Ok(8));
        // Soft limit unlimited: every unsigned int fits.
        assert_eq!(
            poll_nfds(u32::MAX as usize, u64::MAX),
            Ok(u32::MAX as usize)
        );
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
mod watch_kinds_tests {
    //! What a wait decides to pump, now that it asks the file and not the
    //! descriptor number.
    //!
    //! The number stopped answering the question when `add_socket` started
    //! falling back to a low descriptor: a process whose `RLIMIT_NOFILE` is
    //! below `SOCKET_FD` (PulseAudio sets 256) cannot hold a descriptor at
    //! 1000 at all, so its sockets live down among stdin and stdout. Under
    //! the old test such a socket never pumped the network stack, and a
    //! `poll` on it would have waited for a packet nobody was moving.

    use super::watch_kinds;

    #[test]
    fn un_socket_en_un_descriptor_bajo_sigue_pidiendo_red() {
        assert_eq!(watch_kinds([true].iter().copied()), (true, false));
    }

    #[test]
    fn solo_ficheros_no_piden_red() {
        assert_eq!(watch_kinds([false, false].iter().copied()), (false, true));
    }

    #[test]
    fn una_mezcla_pide_las_dos_cosas() {
        assert_eq!(
            watch_kinds([false, true, false].iter().copied()),
            (true, true)
        );
    }

    /// A set with nothing in it asks for neither: a `poll(NULL, 0, t)` is a
    /// sleep, and pumping the stack for it is work nobody asked for.
    #[test]
    fn un_conjunto_vacio_no_pide_nada() {
        assert_eq!(watch_kinds(core::iter::empty()), (false, false));
    }

    /// poll(2)'s "ignore this entry": a negative descriptor is skipped, not
    /// counted as a non-socket. Counted, a set of nothing but ignored
    /// entries asked for the interactive path and woke on tty traffic for
    /// the length of a timeout nobody meant to make interactive.
    #[test]
    fn un_descriptor_negativo_no_se_clasifica_siquiera() {
        assert_eq!(super::poll_entry_kind(-1, || panic!("no se mira")), None);
        assert_eq!(
            watch_kinds(
                [-1, -2]
                    .iter()
                    .filter_map(|fd| super::poll_entry_kind(*fd, || panic!("no se mira")))
            ),
            (false, false)
        );
    }

    /// And an entry that IS watched is classified by its file, including the
    /// one the process cannot resolve (a closed fd), which is not a socket.
    #[test]
    fn un_descriptor_valido_se_clasifica_por_su_fichero() {
        assert_eq!(super::poll_entry_kind(3, || true), Some(true));
        assert_eq!(super::poll_entry_kind(0, || false), Some(false));
    }

    /// One pass, and it stops as soon as both answers are known: the walk is
    /// per wake-up on every `poll` in the system.
    #[test]
    fn no_recorre_de_mas_cuando_ya_sabe_las_dos_respuestas() {
        let mut seen = 0;
        let kinds = [false, true, false, true, false];
        let out = watch_kinds(kinds.iter().copied().inspect(|_| seen += 1));
        assert_eq!(out, (true, true));
        assert_eq!(seen, 2, "siguio mirando fds despues de saber la respuesta");
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

#[cfg(test)]
mod benches {
    //! What `select`, `pselect6`, `poll`, `ppoll`, `epoll_create` and
    //! `epoll_wait` spend before they ever look at a file: narrowing their
    //! arguments, building the `fd_set` bitmaps, and the per-fd scans that
    //! run once per pass of the wait loop.
    //!
    //! The rows that end in a count (`_of_64`, `_of_1024`) are a family on
    //! purpose: the slope says whether the work is per call or per fd, which
    //! is the whole question for a process sitting in `select` with a wide
    //! `nfds`.
    //!
    //! Every row black-boxes its *inputs* as well as its result; with a
    //! constant argument the compiler folds these narrowings away entirely
    //! and the row reports loop overhead instead.

    use super::*;
    use test::{black_box, Bencher};

    fn user_fdset(words: &mut [u32]) -> UserInOutPtr<u32> {
        UserInOutPtr::from(words.as_mut_ptr() as usize)
    }

    /// `(asked about, still open)` for `nfds` descriptors: every fourth one
    /// is in a set, and all of them are open, so the scan runs to the end.
    fn scan_tables(nfds: usize) -> (Vec<bool>, Vec<bool>) {
        (
            (0..nfds).map(|fd| fd % 4 == 0).collect(),
            alloc::vec![true; nfds],
        )
    }

    /// `select`'s three `FdSet::new` calls copy the caller's bitmap in and
    /// allocate a second one for the answer, per call. `FD_PER_ITEM` is
    /// `u32::BITS`, so 64 descriptors are two words.
    #[bench]
    fn fdset_new_of_64(b: &mut Bencher) {
        let mut words = [0xffff_ffffu32; 2];
        // `UserInOutPtr` is not `Copy`, so the pointer is rebuilt from its
        // black-boxed address inside the loop. That is one `from` per
        // iteration, which is a move of a `usize`.
        let addr = user_fdset(&mut words).as_addr();
        b.iter(|| {
            black_box(FdSet::new(UserInOutPtr::from(black_box(addr)), black_box(64)).unwrap())
        });
    }

    /// 256 fds: eight words in, eight allocated out.
    #[bench]
    fn fdset_new_of_256(b: &mut Bencher) {
        let mut words = [0xffff_ffffu32; 8];
        // `UserInOutPtr` is not `Copy`, so the pointer is rebuilt from its
        // black-boxed address inside the loop. That is one `from` per
        // iteration, which is a move of a `usize`.
        let addr = user_fdset(&mut words).as_addr();
        b.iter(|| {
            black_box(FdSet::new(UserInOutPtr::from(black_box(addr)), black_box(256)).unwrap())
        });
    }

    /// The widest set this kernel accepts, `MAX_SELECT_NFDS`. Three of these
    /// are the fixed cost of one `select` call with a full set.
    #[bench]
    fn fdset_new_of_1024(b: &mut Bencher) {
        let mut words = [0xffff_ffffu32; MAX_FDSET_SIZE];
        // `UserInOutPtr` is not `Copy`, so the pointer is rebuilt from its
        // black-boxed address inside the loop. That is one `from` per
        // iteration, which is a move of a `usize`.
        let addr = user_fdset(&mut words).as_addr();
        b.iter(|| {
            black_box(
                FdSet::new(
                    UserInOutPtr::from(black_box(addr)),
                    black_box(MAX_SELECT_NFDS),
                )
                .unwrap(),
            )
        });
    }

    /// A NULL set, which is the common case for two of the three: no copy,
    /// no allocation. The gap to the rows above is what a set *costs*.
    #[bench]
    fn fdset_new_null(b: &mut Bencher) {
        b.iter(|| {
            black_box(FdSet::new(black_box(UserInOutPtr::from(0)), black_box(1024)).unwrap())
        });
    }

    /// `contains`, which the scan asks once per fd per set per pass.
    #[bench]
    fn fdset_contains(b: &mut Bencher) {
        let mut words = [0xffff_ffffu32; MAX_FDSET_SIZE];
        let set = FdSet::new(user_fdset(&mut words), MAX_SELECT_NFDS).unwrap();
        b.iter(|| black_box(black_box(&set).contains(FileDesc::from(black_box(777usize)))));
    }

    /// The scan `select` runs before it waits, looking for a descriptor the
    /// caller asked about and does not hold: `EBADF` for the whole call.
    /// With nothing closed it is the full sweep, which is the cost that is
    /// always paid.
    ///
    /// Both predicates read a black-boxed table rather than computing an
    /// answer from `fd`. With plain arithmetic in them the compiler can see
    /// that the scan finds nothing and deletes it: the 1024-fd row read the
    /// same 6 ns as the 64-fd one, which is one `find` that never ran.
    #[bench]
    fn select_closed_fd_scan_of_64(b: &mut Bencher) {
        let (asked, open) = scan_tables(64);
        b.iter(|| {
            let (asked, open) = (black_box(&asked), black_box(&open));
            black_box(select_closed_fd(
                black_box(64),
                |fd| asked[fd],
                |fd| open[fd],
            ))
        });
    }

    /// The same sweep over the widest set: sixteen times the fds.
    #[bench]
    fn select_closed_fd_scan_of_1024(b: &mut Bencher) {
        let (asked, open) = scan_tables(MAX_SELECT_NFDS);
        b.iter(|| {
            let (asked, open) = (black_box(&asked), black_box(&open));
            black_box(select_closed_fd(
                black_box(MAX_SELECT_NFDS),
                |fd| asked[fd],
                |fd| open[fd],
            ))
        });
    }

    /// Mapping one file's poll status onto `select`'s three sets, once per
    /// ready fd per pass.
    #[bench]
    fn select_ready_of_status(b: &mut Bencher) {
        let status = PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        };
        b.iter(|| black_box(select_ready(black_box(&status))));
    }

    /// `select`'s `int n`, narrowed and clamped.
    #[bench]
    fn select_nfds_arg(b: &mut Bencher) {
        b.iter(|| black_box(select_nfds(black_box(64))));
    }

    /// `poll`'s `nfds`, which is checked against `RLIMIT_NOFILE`.
    #[bench]
    fn poll_nfds_arg(b: &mut Bencher) {
        b.iter(|| black_box(poll_nfds(black_box(64), black_box(1024))));
    }

    /// `poll`'s and `ppoll`'s `int timeout`, narrowed so that a negative one
    /// is a wait with no deadline.
    #[bench]
    fn poll_timeout_arg(b: &mut Bencher) {
        b.iter(|| black_box(poll_timeout_msecs(black_box(usize::MAX))));
    }

    /// `epoll_wait`'s `maxevents` and `epoll_create`'s `size`.
    #[bench]
    fn epoll_arg_checks(b: &mut Bencher) {
        b.iter(|| {
            black_box((
                epoll_maxevents(black_box(64)),
                epoll_create_size(black_box(1)),
            ))
        });
    }

    /// The decision at the end of a pass that found nothing: return empty or
    /// sleep, and for how long. Once per pass, for every waiting process.
    #[bench]
    fn poll_wait_decision_with_deadline(b: &mut Bencher) {
        let begin = Duration::from_secs(10);
        let now = Duration::from_millis(10_050);
        b.iter(|| {
            black_box(poll_wait_decision(
                black_box(1000),
                black_box(begin),
                black_box(now),
            ))
        });
    }

    /// The shape test that decides whether a wait may be demoted to the
    /// background-VT tick. It walks the whole fd set, so it is a family too;
    /// this is the answer-on-the-first-fd case, which is the common one.
    #[bench]
    fn only_terminals_of_64_mixed(b: &mut Bencher) {
        let fds: Vec<Option<bool>> = (0..64)
            .map(|i| match i % 3 {
                0 => None,
                1 => Some(true),
                _ => Some(false),
            })
            .collect();
        b.iter(|| black_box(only_terminals(black_box(&fds).iter().copied())));
    }

    /// The same set with every fd a terminal: no early answer, the whole
    /// walk.
    #[bench]
    fn only_terminals_of_64_all_ttys(b: &mut Bencher) {
        let fds: Vec<Option<bool>> = (0..64).map(|_| Some(true)).collect();
        b.iter(|| black_box(only_terminals(black_box(&fds).iter().copied())));
    }
}

#[cfg(test)]
mod pselect_sigset_arg_layout_tests {
    use super::PSelectSigsetArg;
    use core::mem::{align_of, offset_of, size_of};

    /// `struct sigset_argpack`: the pointer and then the length, both a word
    /// wide. Swapping them is the worst shape this operator produces, because
    /// the two values stay individually plausible: `ss_len` would be read from
    /// the bytes holding a userspace address, so the size check that guards
    /// the sigmask sees a huge number, and `ss` would be read from the bytes
    /// holding a small length and used as the address to read the mask from.
    #[test]
    fn the_sixth_argument_is_the_pointer_and_then_its_length() {
        assert_eq!(size_of::<PSelectSigsetArg>(), 16);
        assert_eq!(align_of::<PSelectSigsetArg>(), 8);
        assert_eq!(offset_of!(PSelectSigsetArg, ss), 0);
        assert_eq!(offset_of!(PSelectSigsetArg, ss_len), 8);
    }
}

#[cfg(test)]
mod pollfd_layout_tests {
    use super::PollFd;
    use core::mem::{align_of, offset_of, size_of};

    /// `struct pollfd`: the descriptor, the mask the caller asks for, and the
    /// mask the kernel answers with. The last two are the same width and sit
    /// next to each other, so the eight bytes stay eight bytes if they trade
    /// places -- and then `poll(2)` reads the caller's request out of the slot
    /// it is supposed to write the answer into, and writes the answer over the
    /// request. Every `poll` test in the crate passed with them swapped,
    /// because the kernel stays consistent with itself: both sides go through
    /// the field name.
    #[test]
    fn struct_pollfd_asks_in_one_mask_and_answers_in_the_next() {
        assert_eq!(size_of::<PollFd>(), 8);
        assert_eq!(align_of::<PollFd>(), 4);
        assert_eq!(offset_of!(PollFd, fd), 0);
        assert_eq!(offset_of!(PollFd, events), 4);
        assert_eq!(offset_of!(PollFd, revents), 6);
        // Two bytes each, and the answer comes after the question.
        assert!(offset_of!(PollFd, revents) > offset_of!(PollFd, events));
    }
}

#[cfg(test)]
mod end_to_end_tests {
    //! `poll(2)` made the way a program makes it, which nothing did.
    //!
    //! The pure helpers above are well covered -- `poll_nfds`,
    //! `poll_wait_decision`, `watch_kinds`, `only_terminals` and the rest all
    //! have tests of their own. What had never run is the body that uses
    //! them: `PollFuture::poll`, 260 lines that scan the set, classify every
    //! descriptor, count the ready ones and decide whether to park. Nothing
    //! in this crate called `sys_poll`, `sys_ppoll`, `sys_select`,
    //! `sys_pselect6`, `sys_epoll_wait` or `sys_epoll_pwait` even once, so
    //! the file sat at 21% of its production lines.
    //!
    //! A file on RamFS is always ready for both directions, and that is
    //! enough to ask the three things this body decides on its own, without
    //! a network stack, a device or a timer: what the return VALUE counts,
    //! what an fd the process does not have answers, and what a negative fd
    //! does.

    use super::*;
    use crate::on_a_thread::on_a_thread;
    use alloc::boxed::Box;
    use alloc::string::String;
    use linux_object::fs::{File, OpenFlags};
    use linux_object::process::LinuxProcess;
    use rcore_fs::vfs::FileType;
    use rcore_fs_ramfs::RamFS;
    use zircon_object::task::{Process, ROOT_JOB};

    /// Ask for nothing and the entry is still scanned; these are the two
    /// bits a reader and a writer ask for.
    const IN_AND_OUT: u16 = 0x0001 | 0x0004;
    /// The same question written the long way, with the stream names beside
    /// the generic ones, which is what glibc's `poll` passes through.
    const IN_AND_OUT_WITH_STREAMS: u16 = 0x0001 | 0x0040 | 0x0004 | 0x0100;
    /// `POLLNVAL`, which is an answer and not an error.
    const NVAL: u16 = 0x0020;

    fn an_entry(fd: i32, events: u16) -> PollFd {
        PollFd {
            fd: FileDesc::from(fd),
            events: PollEvents::from_bits_truncate(events),
            revents: PollEvents::empty(),
        }
    }

    /// A process whose root holds a `/<top>` of its own, because a lookup
    /// from the root goes through a path cache every process in this binary
    /// shares.
    fn a_process(pid: KoID, name: &str, top: &str) -> Arc<Process> {
        let lp = LinuxProcess::new(RamFS::new(), 0);
        lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
        Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
    }

    /// `poll(ufds, nfds, timeout)` by the syscall, with the set in memory it
    /// reads and writes back through a pointer.
    ///
    /// `sys_poll` wants `&mut self` and `on_a_thread` lends a shared one, so
    /// this builds its own out of the same three parts, as
    /// `dispatch_tests::by_number` does.
    async fn poll_them(sys: &Syscall<'_>, set: &mut [PollFd], timeout_msecs: isize) -> SysResult {
        let mut own = Syscall {
            thread: sys.thread,
            thread_fn: sys.thread_fn,
            syscall_entry: sys.syscall_entry,
        };
        own.sys_poll(
            UserInOutPtr::from(set.as_mut_ptr() as usize),
            set.len(),
            timeout_msecs,
        )
        .await
    }

    /// Open a file of `top` and poll `set`, whose first entry is given that
    /// file's descriptor. Returns what the syscall returned and the set as
    /// the syscall wrote it back.
    fn poll_with_a_file(
        proc: &Arc<Process>,
        top: &str,
        mut set: alloc::vec::Vec<PollFd>,
        at: usize,
        timeout_msecs: isize,
    ) -> (SysResult, alloc::vec::Vec<PollFd>) {
        // Owned before the closure: `on_a_thread` wants a `'static` one.
        let dir_path = alloc::format!("/{}", top);
        let path = alloc::format!("/{}/f", top);
        on_a_thread(proc, move |sys| {
            Box::pin(async move {
                let lp = sys.linux_process();
                let dir = lp.lookup_inode(&dir_path).unwrap();
                let inode = dir.create("f", FileType::File, 0o644).unwrap();
                let fd: i32 = lp
                    .add_file(File::new(inode, OpenFlags::RDWR, String::from(&path)))
                    .unwrap()
                    .into();
                set[at] = an_entry(fd, set[at].events.bits());
                let got = poll_them(sys, &mut set, timeout_msecs).await;
                (got, set)
            })
        })
    }

    #[test]
    fn the_answer_counts_descriptors_and_not_event_bits() {
        // Linux's poll(2) returns "the number of fds with nonzero revents,
        // not the number of event bits set". One file ready both ways is
        // ONE, however many bits come back in it -- and asking the question
        // the long way must not turn it into more.
        let proc = a_process(45_560, "poll-count", "poll_count");
        let (got, set) = poll_with_a_file(
            &proc,
            "poll_count",
            alloc::vec![an_entry(0, IN_AND_OUT_WITH_STREAMS)],
            0,
            0,
        );

        assert_eq!(got, Ok(1), "a file ready both ways was not one descriptor");
        assert_eq!(
            set[0].revents.bits(),
            IN_AND_OUT_WITH_STREAMS,
            "the four bits asked for did not all come back"
        );
    }

    #[test]
    fn a_descriptor_the_process_does_not_have_is_pollnval_and_is_counted() {
        // POLLNVAL is a well-defined answer and not a kernel error: the
        // syscall succeeds, the entry carries the bit, and it counts towards
        // the return value like any other ready entry.
        let proc = a_process(45_561, "poll-nval", "poll_nval");
        let mut set = alloc::vec![an_entry(999, IN_AND_OUT)];
        let (got, set) = on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                let got = poll_them(sys, &mut set, 0).await;
                (got, set)
            })
        });

        assert_eq!(got, Ok(1), "a stale descriptor was not counted");
        assert_eq!(
            set[0].revents.bits(),
            NVAL,
            "a stale descriptor did not come back as POLLNVAL alone"
        );
    }

    #[test]
    fn a_negative_descriptor_is_ignored_and_counts_for_nothing() {
        // poll(2)'s "skip this entry", which udhcpc6 leaves in the set. A
        // set of nothing but ignored entries, with a timeout of zero, is a
        // return of zero and not a wait.
        let proc = a_process(45_562, "poll-skip", "poll_skip");
        let mut set = alloc::vec![an_entry(-1, IN_AND_OUT), an_entry(-1, IN_AND_OUT)];
        let (got, set) = on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                let got = poll_them(sys, &mut set, 0).await;
                (got, set)
            })
        });

        assert_eq!(got, Ok(0), "an ignored entry was counted or waited on");
        assert_eq!(set[0].revents.bits(), 0, "an ignored entry got an answer");
        assert_eq!(set[1].revents.bits(), 0, "an ignored entry got an answer");
    }

    #[test]
    fn the_three_kinds_of_entry_are_counted_separately_in_one_set() {
        // The three paths of the scan, in the one set, so that the count is
        // read against entries that answer differently: the ready file and
        // the stale descriptor each count, the ignored one does not.
        let proc = a_process(45_563, "poll-mixed", "poll_mixed");
        let (got, set) = poll_with_a_file(
            &proc,
            "poll_mixed",
            alloc::vec![
                an_entry(-1, IN_AND_OUT),
                an_entry(0, IN_AND_OUT),
                an_entry(999, IN_AND_OUT),
            ],
            1,
            0,
        );

        assert_eq!(got, Ok(2), "the mixed set did not count two of three");
        assert_eq!(set[0].revents.bits(), 0, "the ignored entry got an answer");
        assert_eq!(
            set[1].revents.bits(),
            IN_AND_OUT,
            "the ready file did not answer both directions"
        );
        assert_eq!(set[2].revents.bits(), NVAL, "the stale entry was not NVAL");
    }

    // ---- select(2) and epoll_wait(2) -------------------------------------
    //
    // `select_core` (the shared body of `select` and `pselect6`) and
    // `Epoll::wait` behind `sys_epoll_pwait` were as unexecuted as
    // `PollFuture::poll` was. They are not more of the same: select counts
    // its answer differently from poll, hands the answer back by rewriting
    // the caller's own bitmaps, and has a whole check -- `max_select_fd` --
    // that poll does not.

    /// `EPOLL_CTL_ADD`. `/usr/include/linux/eventpoll.h` says 1, and the
    /// number is written here rather than read back out of the kernel under
    /// test: a test that takes its numbers from the table it is checking
    /// agrees with whatever that table says.
    const CTL_ADD: i32 = 1;

    /// A descriptor the process does not have. Above stdio and above the one
    /// file these tests open, so nothing can hand it out by accident.
    const A_CLOSED_FD: usize = 100;

    /// A `fd_set` in host memory: one `u32` per 32 descriptors, which is how
    /// select reads it and how `commit` writes it back. Sized by `nfds`,
    /// like the array a caller declares -- so every `fd` named here must sit
    /// inside those words, including the ones past `nfds` that a test puts
    /// there on purpose.
    fn a_set(nfds: usize, fds: &[usize]) -> alloc::vec::Vec<u32> {
        let mut words = alloc::vec![0u32; nfds.div_ceil(FD_PER_ITEM).max(1)];
        for &fd in fds {
            words[fd / FD_PER_ITEM] |= 1 << (fd % FD_PER_ITEM);
        }
        words
    }

    /// A node of `dir_path` opened read-write, and the descriptor it got.
    ///
    /// A node on RamFS is always ready for reading and for writing, whatever
    /// kind it is, which is what makes these answers knowable with no
    /// device, no network stack and no timer in the way. The kind still
    /// matters to epoll, which refuses a regular file outright: see
    /// `epoll_refuses_the_regular_file_that_poll_reports_always_ready`.
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

    /// `select(nfds, &read, &write, &except, &timeout)` by the syscall, with
    /// the four arguments in memory it reads and writes back through
    /// pointers.
    ///
    /// All three sets are real buffers, never null: a null set is a
    /// different path of `FdSet::new` and an empty slice's pointer is
    /// dangling rather than null, so passing one would hand production a
    /// pointer it is entitled to read.
    ///
    /// `sys_select` wants `&mut self` and `on_a_thread` lends a shared one,
    /// so this builds its own out of the same three parts, as `poll_them`
    /// does.
    async fn select_them(
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

    /// `epoll_pwait` with no signal mask, which is what `sys_epoll_wait`
    /// passes. The output array is the caller's, and the answer says how
    /// many of its entries were filled.
    async fn wait_on(sys: &Syscall<'_>, epfd: usize, out: &mut [EpollEvent]) -> SysResult {
        sys.sys_epoll_pwait(
            FileDesc::from(epfd),
            UserOutPtr::from(out.as_mut_ptr() as usize),
            out.len(),
            0,
            // Built here and not shared: `UserInPtr` is not `Copy`, so a
            // null kept in a local would be moved by the first call.
            UserInPtr::from(0),
            0,
        )
        .await
    }

    #[test]
    fn selects_answer_counts_bits_and_polls_counts_descriptors() {
        // The same file, ready the same two ways, is TWO to select(2) and
        // ONE to poll(2): select "returns the total number of bits that are
        // set in readfds, writefds and exceptfds", poll "the number of fds
        // with nonzero revents". Neither count had a test, and the pair is
        // asked here in one go so that a change which made the two agree
        // cannot pass.
        let proc = a_process(45_564, "select-bits", "select_bits");
        let dir_path = String::from("/select_bits");
        let (selected, polled) = on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                let lp = sys.linux_process();
                let fd = a_ready_node(&lp, &dir_path, FileType::File);
                let nfds = fd + 1;
                let mut read = a_set(nfds, &[fd]);
                let mut write = a_set(nfds, &[fd]);
                let mut except = a_set(nfds, &[]);
                let mut timeout = TimeVal { sec: 0, usec: 0 };
                let selected =
                    select_them(sys, nfds, &mut read, &mut write, &mut except, &mut timeout).await;
                let mut set = alloc::vec![an_entry(fd as i32, IN_AND_OUT)];
                let polled = poll_them(sys, &mut set, 0).await;
                (selected, polled)
            })
        });

        assert_eq!(selected, Ok(2), "select did not count both of its bits");
        assert_eq!(polled, Ok(1), "poll counted bits instead of descriptors");
    }

    #[test]
    fn a_closed_descriptor_in_a_set_is_ebadf_before_anything_is_waited_on() {
        // `max_select_fd` (fs/select.c): a descriptor in any of the three
        // sets that the process does not have makes the whole call EBADF,
        // and it does so before the wait begins. It used to be skipped
        // instead, so a program that had closed a socket and left its bit
        // set waited out the timeout -- or for ever -- rather than being
        // told which descriptor to drop.
        //
        // Asked with a timeout of zero, so that a kernel which skipped the
        // check answers Ok(0) at once instead of hanging the suite.
        let proc = a_process(45_565, "select-badfd", "select_badfd");
        let nfds = A_CLOSED_FD + 1;
        let mut read = a_set(nfds, &[A_CLOSED_FD]);
        let mut write = a_set(nfds, &[]);
        let mut except = a_set(nfds, &[]);
        let got = on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                let mut timeout = TimeVal { sec: 0, usec: 0 };
                select_them(sys, nfds, &mut read, &mut write, &mut except, &mut timeout).await
            })
        });

        assert_eq!(
            got,
            Err(LxError::EBADF),
            "a closed descriptor in readfds did not stop the call"
        );
    }

    #[test]
    fn nothing_lands_in_exceptfds_and_a_spent_timeout_hands_the_sets_back_cleared() {
        // Three things that only show up together. `select_ready` reports
        // `except: false` whatever the status says, because POLLEX_SET is
        // PRI alone and nothing here raises it -- so a file that is ready
        // both ways is still not ready in exceptfds. That makes this a
        // select which finds nothing, which is the one path that answers
        // through `PollWait::ReturnEmpty`: Linux returns 0 AND writes the
        // (empty) ready sets back, so the caller's bit is gone from the set
        // it handed in.
        let proc = a_process(45_566, "select-except", "select_except");
        let dir_path = String::from("/select_except");
        let (got, read, write, except) = on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                let lp = sys.linux_process();
                let fd = a_ready_node(&lp, &dir_path, FileType::File);
                let nfds = fd + 1;
                let mut read = a_set(nfds, &[]);
                let mut write = a_set(nfds, &[]);
                let mut except = a_set(nfds, &[fd]);
                let mut timeout = TimeVal { sec: 0, usec: 0 };
                let got =
                    select_them(sys, nfds, &mut read, &mut write, &mut except, &mut timeout).await;
                (got, read, write, except)
            })
        });

        assert_eq!(got, Ok(0), "a file was reported ready in exceptfds");
        assert_eq!(except[0], 0, "the exception bit was handed back still set");
        assert_eq!(read[0], 0, "a set select was not asked about came back set");
        assert_eq!(
            write[0], 0,
            "a set select was not asked about came back set"
        );
    }

    #[test]
    fn a_bit_above_nfds_is_neither_an_error_nor_looked_at() {
        // `nfds` is the count of descriptors to consider, not the size of
        // the words the caller passed: a `fd_set` is always a whole number
        // of 32-bit words, so bits above `nfds` are there whether the
        // caller meant them or not. They must be neither scanned nor
        // EBADF'd, and they must not survive into the answer.
        let proc = a_process(45_567, "select-clamp", "select_clamp");
        let dir_path = String::from("/select_clamp");
        let (got, read, only_the_file) = on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                let lp = sys.linux_process();
                let fd = a_ready_node(&lp, &dir_path, FileType::File);
                let nfds = fd + 1;
                // The second one is past `nfds` and belongs to nobody: it
                // shares the first word, as the words a caller declares do.
                let mut read = a_set(nfds, &[fd, fd + 8]);
                let mut write = a_set(nfds, &[]);
                let mut except = a_set(nfds, &[]);
                let mut timeout = TimeVal { sec: 0, usec: 0 };
                let got =
                    select_them(sys, nfds, &mut read, &mut write, &mut except, &mut timeout).await;
                (got, read[0], 1u32 << fd)
            })
        });

        assert_eq!(got, Ok(1), "a bit above nfds was counted or made an error");
        assert_eq!(
            read, only_the_file,
            "the answer kept a bit that is past nfds"
        );
    }

    #[test]
    fn an_epoll_hands_back_the_cookie_the_caller_registered() {
        // The point of `epoll_data`: the kernel keeps 64 bits on behalf of
        // the caller and reports them untouched, so a program can hang its
        // own pointer off a watch instead of keeping a second descriptor to
        // object map. Nothing had ever read one back out.
        let proc = a_process(45_568, "epoll-cookie", "epoll_cookie");
        let dir_path = String::from("/epoll_cookie");
        const A_COOKIE: u64 = 0x0bad_c0de_dead_beef;
        let (got, events, cookie) = on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                let lp = sys.linux_process();
                // A device node and not a regular file: `can_epoll`
                // refuses the latter, which is the next test. Not a FIFO
                // either -- an empty one answers writable alone, on purpose
                // (`fifo_node_poll`, already covered where it lives).
                let fd = a_ready_node(&lp, &dir_path, FileType::CharDevice);
                let epfd = sys.sys_epoll_create1(0).unwrap();
                let watch = EpollEvent {
                    events: u32::from(IN_AND_OUT),
                    data: A_COOKIE,
                };
                sys.sys_epoll_ctl(
                    FileDesc::from(epfd),
                    CTL_ADD,
                    FileDesc::from(fd),
                    UserInPtr::from(&watch as *const EpollEvent as usize),
                )
                .unwrap();
                let mut out = alloc::vec![EpollEvent { events: 0, data: 0 }; 8];
                let got = wait_on(sys, epfd, &mut out).await;
                // Copied out of the array before being asserted on:
                // `EpollEvent` is packed, so a reference to a field of one
                // (which is what `assert_eq!` takes) does not exist.
                (got, out[0].events, out[0].data)
            })
        });

        assert_eq!(got, Ok(1), "a ready file was not reported once");
        assert_eq!(cookie, A_COOKIE, "the caller's cookie came back changed");
        assert_eq!(
            events,
            u32::from(IN_AND_OUT),
            "a file ready both ways was not reported both ways"
        );
    }

    #[test]
    fn an_epoll_wait_tells_a_closed_descriptor_from_one_that_is_not_an_epoll() {
        // Two different answers, and they were once the same one: the
        // downcast was mapped to EBADF, so a live descriptor that simply
        // was not an epoll reported itself closed. `do_epoll_wait` gives
        // EBADF for a descriptor the process does not have and EINVAL for
        // one it has that is of the wrong kind -- the difference between
        // "you closed it" and "that is a file".
        let proc = a_process(45_569, "epoll-kind", "epoll_kind");
        let dir_path = String::from("/epoll_kind");
        let (not_an_epoll, closed) = on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                let lp = sys.linux_process();
                let fd = a_ready_node(&lp, &dir_path, FileType::File);
                let mut out = alloc::vec![EpollEvent { events: 0, data: 0 }; 4];
                let not_an_epoll = wait_on(sys, fd, &mut out).await;
                let closed = wait_on(sys, A_CLOSED_FD, &mut out).await;
                (not_an_epoll, closed)
            })
        });

        assert_eq!(
            not_an_epoll,
            Err(LxError::EINVAL),
            "waiting on a plain file did not say it is the wrong kind"
        );
        assert_eq!(
            closed,
            Err(LxError::EBADF),
            "waiting on a descriptor the process does not have was not EBADF"
        );
    }

    #[test]
    fn epoll_refuses_the_regular_file_that_poll_reports_always_ready() {
        // The one place where the three multiplexers disagree about the same
        // descriptor, and it is not a quirk of this kernel: a regular file
        // has no `poll` operation on any disk filesystem, so `epoll_ctl`
        // answers EPERM for every op before it looks at anything else about
        // the request, while `poll(2)` and `select(2)` are required to report
        // that same file always ready. A file watched by epoll is the shape
        // of the mistake this refuses -- the watch would never fire, because
        // there is nothing to fire it.
        //
        // Both halves are asked of ONE descriptor in one process, so that a
        // change which made epoll accept the file, or poll refuse it, cannot
        // pass by moving the disagreement somewhere else.
        let proc = a_process(45_570, "epoll-regular", "epoll_regular");
        let dir_path = String::from("/epoll_regular");
        let (watched, polled) = on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                let lp = sys.linux_process();
                let fd = a_ready_node(&lp, &dir_path, FileType::File);
                let epfd = sys.sys_epoll_create1(0).unwrap();
                let watch = EpollEvent {
                    events: u32::from(IN_AND_OUT),
                    data: 0,
                };
                let watched = sys.sys_epoll_ctl(
                    FileDesc::from(epfd),
                    CTL_ADD,
                    FileDesc::from(fd),
                    UserInPtr::from(&watch as *const EpollEvent as usize),
                );
                let mut set = alloc::vec![an_entry(fd as i32, IN_AND_OUT)];
                let polled = poll_them(sys, &mut set, 0).await;
                (watched, polled)
            })
        });

        assert_eq!(
            watched,
            Err(LxError::EPERM),
            "epoll took a watch on a file that has nothing to wake it"
        );
        assert_eq!(
            polled,
            Ok(1),
            "poll did not report a regular file always ready"
        );
    }
}

#[cfg(test)]
mod pselect_ppoll_wiring_tests;
