use super::*;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use lock::Mutex;
use zircon_object::object::*;

/// Max nesting depth for one epoll watching another (mirrors Linux's
/// `EP_MAX_NESTS`). Also bounds the cycle-detection walk in
/// `Epoll::contains_epoll` itself, so a bug elsewhere that let a cycle slip
/// past this check couldn't make the check's own recursion unbounded either.
const EPOLL_MAX_NEST_DEPTH: usize = 4;

/// `epoll_ctl(2)` operations.
const EPOLL_CTL_ADD: i32 = 1;
/// See [`EPOLL_CTL_ADD`].
const EPOLL_CTL_DEL: i32 = 2;
/// See [`EPOLL_CTL_ADD`].
const EPOLL_CTL_MOD: i32 = 3;

/// The four `epoll_ctl(2)` event bits that live above the 16 bits a
/// [`PollEvents`] can hold.
///
/// `EpollEvent::events` is a `u32` because Linux's `struct epoll_event` is,
/// but every reader in this file used to narrow it with `event.events as u16`
/// before handing it to `PollEvents::from_bits_truncate`. That cast dropped
/// all four on the floor, and the one that matters is `EPOLLONESHOT`: a
/// thread pool arms an fd with it precisely so that **one** worker is handed
/// the event and the rest are not, and re-arms with `EPOLL_CTL_MOD` when it
/// is done. Losing the bit turned that guarantee into its opposite -- the fd
/// stayed armed and every waiter got it, which is the race the flag exists to
/// prevent.
const EPOLLEXCLUSIVE: u32 = 1 << 28;
/// See [`EPOLLEXCLUSIVE`]. Keeps the system awake while the event is pending;
/// this kernel has no suspend, so it is accepted and does nothing.
const EPOLLWAKEUP: u32 = 1 << 29;
/// See [`EPOLLEXCLUSIVE`]. Report the fd once, then disable the entry until
/// an `EPOLL_CTL_MOD` re-arms it.
const EPOLLONESHOT: u32 = 1 << 30;
/// See [`EPOLLEXCLUSIVE`]. Edge-triggered. **Still level-triggered here**:
/// the bit is stored and round-trips, but the readiness scan reports a level.
/// A half-done edge trigger is worse than none -- a program that misses an
/// edge waits forever -- so it is left honest and untouched.
const EPOLLET: u32 = 1 << 31;

/// The bits `EPOLLEXCLUSIVE` may be combined with (Linux's
/// `EPOLLEXCLUSIVE_OK_BITS`): the two readiness bits, the two that are always
/// reported, and the three flags that do not change who gets woken.
const EPOLLEXCLUSIVE_OK_BITS: u32 = PollEvents::IN.bits() as u32
    | PollEvents::OUT.bits() as u32
    | PollEvents::ERR.bits() as u32
    | PollEvents::HUP.bits() as u32
    | EPOLLWAKEUP
    | EPOLLET
    | EPOLLEXCLUSIVE;

/// The events one interest-list entry reports for a readiness status.
///
/// The stored mask decides all four bits, `EPOLLERR`/`EPOLLHUP` included.
/// Those two are reported whether or not the caller asked for them, which is
/// why [`epoll_ctl_events`] puts them into every mask it stores -- so that the
/// one mask that reports nothing at all is the empty one, which only an
/// `EPOLLONESHOT` delivery can produce. `ep_item_poll` is the same `&`.
fn ready_events(events: u32, status: &PollStatus) -> u32 {
    let interest = PollEvents::from_bits_truncate(events as u16);
    let mut ready = PollEvents::empty();
    if status.read {
        ready |= PollEvents::IN;
    }
    if status.write {
        ready |= PollEvents::OUT;
    }
    if status.error {
        ready |= PollEvents::ERR;
    }
    if status.hangup {
        ready |= PollEvents::HUP;
    }
    (ready & interest).bits() as u32
}

/// The event mask `epoll_ctl` stores for one interest-list entry, or the
/// error Linux reports for the request.
///
/// `EPOLLEXCLUSIVE` is the only bit `do_epoll_ctl` validates, and it validates
/// it three ways: it cannot be added by `EPOLL_CTL_MOD`, it cannot be put on a
/// nested epoll, and it cannot be mixed with a bit outside
/// [`EPOLLEXCLUSIVE_OK_BITS`]. All three exist because the flag changes *who*
/// is woken, and a request whose wake-up set is ambiguous is refused rather
/// than guessed at.
///
/// `EPOLLERR` and `EPOLLHUP` are forced in, as `do_epoll_ctl` does, so that
/// the stored mask alone says what the entry reports. That is what lets an
/// `EPOLLONESHOT` entry be disabled by zeroing its mask, the way
/// `ep_send_events` does it, instead of carrying a second flag that every
/// reader would have to remember to consult.
fn epoll_ctl_events(op: i32, events: u32, target_is_epoll: bool) -> LxResult<u32> {
    if events & EPOLLEXCLUSIVE != 0 {
        if op == EPOLL_CTL_MOD {
            return Err(LxError::EINVAL);
        }
        if target_is_epoll || events & !EPOLLEXCLUSIVE_OK_BITS != 0 {
            return Err(LxError::EINVAL);
        }
    }
    Ok(events | PollEvents::ERR.bits() as u32 | PollEvents::HUP.bits() as u32)
}

/// The two refusals `do_epoll_ctl` makes of the target file before it
/// looks at the op or the mask, in its order: a file with no poll
/// operation (`!file_can_poll`, `EPERM`), then the epoll itself
/// (`f.file == tf.file`, `EINVAL`).
///
/// A regular file was accepted and reported always ready, so an event
/// loop handed one (a log written by another process, a config file)
/// spun at full speed where Linux tells it `EPERM` and it falls back to
/// inotify or a timer. And adding an epoll to itself was `ELOOP`, the
/// answer for a cycle through other epolls, not the `EINVAL` Linux gives
/// the direct case.
fn epoll_target_verdict(target_can_epoll: bool, target_is_self: bool) -> LxResult<()> {
    if !target_can_epoll {
        return Err(LxError::EPERM);
    }
    if target_is_self {
        return Err(LxError::EINVAL);
    }
    Ok(())
}

lazy_static::lazy_static! {
    /// Serializes EPOLL_CTL_ADD of a nested epoll (one epoll fd watching
    /// another) so the cycle/depth check and the insert happen as one atomic
    /// step. Without this, two concurrent adds in opposite directions
    /// (thread 1: add B to A; thread 2: add A to B) could each pass
    /// `contains_epoll` before either inserts, still creating a cycle.
    /// Mirrors Linux's global `epmutex`, used for the same reason.
    static ref EPOLL_NEST_LOCK: Mutex<()> = Mutex::new(());
}

/// epoll implementation
pub struct Epoll {
    base: KObjectBase,
    inner: Mutex<EpollInner>,
    flags: OpenFlags,
}

struct EpollInner {
    /// Each watched fd maps to its requested event mask plus a handle to the
    /// underlying file. The file handle lets `Epoll::poll()` report readiness
    /// of the watched fds *without* a process context — which is what makes a
    /// nested epoll (an epoll fd added to another epoll's interest list) work.
    /// wlroots/labwc relies on exactly this: it adds `libinput_get_fd()` (an
    /// epoll fd) to its `wl_event_loop` epoll, so the outer epoll must surface
    /// the inner epoll's readiness or input events are never dispatched.
    interest_list: BTreeMap<FileDesc, (EpollEvent, Arc<dyn FileLike>)>,
}

/// epoll event
#[repr(C)]
#[cfg_attr(target_arch = "x86_64", repr(packed))]
#[derive(Clone, Copy, Debug)]
pub struct EpollEvent {
    /// events
    pub events: u32,
    /// data
    pub data: u64,
}

impl_kobject!(Epoll);

impl Epoll {
    /// create an epoll instance
    pub fn new(flags: OpenFlags) -> Arc<Self> {
        Arc::new(Epoll {
            base: KObjectBase::new(),
            inner: Mutex::new(EpollInner {
                interest_list: BTreeMap::new(),
            }),
            flags,
        })
    }

    /// add, modify, or remove a file descriptor from the interest list. `file`
    /// is the resolved handle for `fd` (required for ADD/MOD; ignored for DEL).
    pub fn ctl(
        &self,
        op: i32,
        fd: FileDesc,
        event: EpollEvent,
        file: Option<Arc<dyn FileLike>>,
    ) -> LxResult<usize> {
        // The target is judged first, for every op, as `do_epoll_ctl` does
        // it: a file that cannot be polled is EPERM and this epoll itself is
        // EINVAL, before the op, the mask or the interest list.
        if let Some(target) = file.as_ref() {
            let target_is_self = core::ptr::eq(
                Arc::as_ptr(target) as *const u8,
                self as *const Self as *const u8,
            );
            epoll_target_verdict(target.can_epoll(), target_is_self)?;
        }
        // The mask is settled before the interest list is touched, as
        // `do_epoll_ctl` does it: a request this kernel will not honour is
        // EINVAL whether or not the fd happens to be watched already, and the
        // stored mask is the one the readiness scan will be held to.
        let event = if op == EPOLL_CTL_ADD || op == EPOLL_CTL_MOD {
            let target = file.as_ref().ok_or(LxError::EBADF)?;
            let target_is_epoll = target.clone().downcast_arc::<Epoll>().is_ok();
            EpollEvent {
                events: epoll_ctl_events(op, event.events, target_is_epoll)?,
                data: event.data,
            }
        } else {
            event
        };
        let mut inner = self.inner.lock();
        match op {
            EPOLL_CTL_ADD => {
                let file = file.ok_or(LxError::EBADF)?;
                // Linux keys the interest list by (file, fd), so `EEXIST` is
                // for the SAME description under the same number. An entry
                // whose description is no longer the one at `fd` is a number
                // that was closed and handed out again: the caller cannot
                // reach the old description through `fd` any more, and it
                // cannot `EPOLL_CTL_DEL` a file it has no descriptor for.
                if let Some((_, watched)) = inner.interest_list.get(&fd) {
                    if Arc::ptr_eq(watched, &file) {
                        return Err(LxError::EEXIST);
                    }
                    inner.interest_list.remove(&fd);
                }
                // If the target is itself an epoll, reject a self-add or any
                // nesting that would create a cycle or exceed
                // EPOLL_MAX_NEST_DEPTH. any_ready()'s recursion through
                // file.poll() has no other terminating condition -- a cycle
                // recurses forever, and even an acyclic chain deep enough can
                // overflow a coroutine's guard-page-less stack (the same bug
                // class root-caused once already for unbounded path
                // recursion, commit b448c77e).
                if let Ok(target) = file.clone().downcast_arc::<Epoll>() {
                    // Serialize against a concurrent nested add elsewhere so
                    // the check and the insert are atomic as a unit (see
                    // EPOLL_NEST_LOCK's doc comment) -- drop our own lock
                    // first since contains_epoll never needs to re-lock
                    // `self` (it returns as soon as it reaches `self` by
                    // pointer, before locking that node), but a *different*
                    // epoll's `ctl` could still be walking through us.
                    drop(inner);
                    let _nest_guard = EPOLL_NEST_LOCK.lock();
                    if target.contains_epoll(self, 0) {
                        return Err(LxError::ELOOP);
                    }
                    inner = self.inner.lock();
                    if let Some((_, watched)) = inner.interest_list.get(&fd) {
                        if Arc::ptr_eq(watched, &file) {
                            return Err(LxError::EEXIST);
                        }
                    }
                }
                inner.interest_list.insert(fd, (event, file));
            }
            EPOLL_CTL_DEL => {
                inner.interest_list.remove(&fd).ok_or(LxError::ENOENT)?;
            }
            EPOLL_CTL_MOD => {
                let file = file.ok_or(LxError::EBADF)?;
                let e = inner.interest_list.get_mut(&fd).ok_or(LxError::ENOENT)?;
                *e = (event, file);
            }
            _ => return Err(LxError::EINVAL),
        }
        Ok(0)
    }

    /// Disable the entries an `EPOLLONESHOT` delivery has just used up.
    ///
    /// `ep_send_events` clears the event bits of a one-shot entry as it hands
    /// the event out, leaving the entry in the interest list reporting nothing
    /// until an `EPOLL_CTL_MOD` puts a mask back. Zeroing the mask is the
    /// whole mechanism, which is why `epoll_ctl` forces `EPOLLERR`/`EPOLLHUP`
    /// in: an armed entry can never have an empty mask by accident.
    ///
    /// The scan runs on a snapshot taken without the lock, so the file handle
    /// is compared by pointer before anything is written: between the scan and
    /// here, another thread may have dropped this fd and handed the number to
    /// a different file, and that one was never delivered anything.
    fn disarm_oneshot(&self, delivered: &[(FileDesc, Arc<dyn FileLike>)]) {
        if delivered.is_empty() {
            return;
        }
        let mut inner = self.inner.lock();
        for (fd, file) in delivered {
            if let Some((event, current)) = inner.interest_list.get_mut(fd) {
                if event.events & EPOLLONESHOT != 0 && Arc::ptr_eq(current, file) {
                    event.events = 0;
                }
            }
        }
    }

    /// Drop every entry that watches `file`: a descriptor of it was closed,
    /// and it was the last one in its table holding that description.
    ///
    /// This is `eventpoll_release`, which `__fput` runs when the last
    /// reference to an open file description goes away, and it is what
    /// epoll(7) promises under "Will closing a file descriptor cause it to be
    /// removed from all epoll interest lists?". Nothing here did it, so a
    /// process that closed a watched descriptor without an `EPOLL_CTL_DEL`
    /// first (most of them: the manual says close is enough) kept it in three
    /// ways at once. The interest list held the description alive, so the
    /// peer of a closed socket or pipe never saw EOF or `EPIPE` while the
    /// event loop lived. `epoll_wait` kept returning the dead entry with its
    /// old `data`, and the `read` the program then did on that number was
    /// `EBADF`, forever, at full speed. And when the number came back from
    /// `accept` or `open`, `EPOLL_CTL_ADD` on it was `EEXIST`.
    ///
    /// Matched by description, not by number: the entry is keyed by the
    /// number it was added under, and the descriptor whose close ended the
    /// description may be a `dup` under another one. Compared by pointer, as
    /// `disarm_oneshot` does, because a number that was closed and reopened
    /// may already watch its new description. Returns how many entries went.
    pub fn forget_closed(&self, file: &Arc<dyn FileLike>) -> usize {
        let mut inner = self.inner.lock();
        let before = inner.interest_list.len();
        inner
            .interest_list
            .retain(|_, (_, watched)| !Arc::ptr_eq(watched, file));
        before - inner.interest_list.len()
    }

    /// Whether `needle` (some other epoll, compared by identity) is
    /// reachable by walking outward from `self` through nested epolls --
    /// i.e. whether `self` already (transitively) watches `needle`. Used by
    /// `ctl`'s ADD path to reject a nesting that would create a cycle.
    /// `depth >= EPOLL_MAX_NEST_DEPTH` conservatively answers "yes" (refuse
    /// rather than risk missing a cycle further down) rather than growing
    /// unbounded itself.
    fn contains_epoll(&self, needle: *const Epoll, depth: usize) -> bool {
        if core::ptr::eq(self, needle) {
            return true;
        }
        if depth >= EPOLL_MAX_NEST_DEPTH {
            return true;
        }
        // Snapshot then drop the lock before recursing, same reasoning as
        // any_ready(): a watched fd can itself be an epoll that re-enters.
        let entries: Vec<Arc<dyn FileLike>> = self
            .inner
            .lock()
            .interest_list
            .values()
            .map(|(_, f)| f.clone())
            .collect();
        for f in entries {
            if let Ok(child) = f.downcast_arc::<Epoll>() {
                if child.contains_epoll(needle, depth + 1) {
                    return true;
                }
            }
        }
        false
    }

    /// Returns whether any watched fd is currently ready for its requested
    /// events. Shared by `poll`/`async_poll` so a nested epoll surfaces its
    /// inner readiness to an outer epoll/poll.
    ///
    /// Nested epolls (libinput's fd inside wlroots) are walked **iteratively**
    /// with an explicit heap stack — recursive `file.poll() → any_ready()`
    /// stacked a fresh interest-list snapshot frame per nest level on the
    /// coroutine stack during labwc bring-up.
    fn any_ready(&self) -> bool {
        let mut stack: Vec<(EpollEvent, Arc<dyn FileLike>, usize)> = self
            .inner
            .lock()
            .interest_list
            .values()
            .cloned()
            .map(|(e, f)| (e, f, 0))
            .collect();
        while let Some((event, file, depth)) = stack.pop() {
            if let Ok(child) = file.clone().downcast_arc::<Epoll>() {
                if depth >= EPOLL_MAX_NEST_DEPTH {
                    continue;
                }
                let nested: Vec<(EpollEvent, Arc<dyn FileLike>, usize)> = child
                    .inner
                    .lock()
                    .interest_list
                    .values()
                    .cloned()
                    .map(|(e, f)| (e, f, depth + 1))
                    .collect();
                stack.extend(nested);
                continue;
            }
            let interest = PollEvents::from_bits_truncate(event.events as u16);
            if let Ok(status) = file.poll(interest) {
                if ready_events(event.events, &status) != 0 {
                    return true;
                }
            }
        }
        false
    }
}

#[async_trait]
impl FileLike for Epoll {
    fn flags(&self) -> OpenFlags {
        self.flags
    }

    fn set_flags(&self, _f: OpenFlags) -> LxResult {
        Ok(())
    }

    async fn read(&self, _buf: &mut [u8]) -> LxResult<usize> {
        Err(LxError::ENOSYS)
    }

    fn write(&self, _buf: &[u8]) -> LxResult<usize> {
        Err(LxError::ENOSYS)
    }

    async fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> LxResult<usize> {
        Err(LxError::ENOSYS)
    }

    fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        // An epoll fd is readable iff any watched fd is ready. Surfacing this is
        // what lets a nested epoll (e.g. libinput's fd inside wlroots' event
        // loop) wake an outer epoll/poll.
        Ok(PollStatus {
            read: self.any_ready(),
            write: false,
            error: false,
            hangup: false,
        })
    }

    async fn async_poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        Ok(PollStatus {
            read: self.any_ready(),
            write: false,
            error: false,
            hangup: false,
        })
    }
}

impl Epoll {
    /// wait for events on the interest list
    pub async fn wait(&self, maxevents: usize, timeout_msecs: isize) -> LxResult<Vec<EpollEvent>> {
        let begin_time = kernel_hal::timer::timer_now();
        loop {
            // Snapshot the interest list, keeping each watched file's OWN
            // `Arc<dyn FileLike>`. Two reasons this is the handle to poll,
            // rather than re-resolving the fd number through the process:
            //  * Correctness — epoll watches the open file *description*, not
            //    the fd. If userspace closes the fd and reuses the number for
            //    something else, Linux keeps reporting the original
            //    description until EPOLL_CTL_DEL; re-resolving by number
            //    watched the wrong file.
            //  * Lifetime — polling the stored Arc means this future holds NO
            //    `&LinuxProcess` borrow across its `.await` points. The borrow
            //    reached the process through the thread, and a stale
            //    net/timer waker re-polling a parked epoll after the owning
            //    thread had torn down dereferenced freed process memory
            //    (a use-after-free #GP with the free-poison pattern in the
            //    faulting register). The Arc keeps exactly what we touch
            //    alive for as long as we touch it.
            let interest_list: Vec<(FileDesc, EpollEvent, Arc<dyn FileLike>)> = self
                .inner
                .lock()
                .interest_list
                .iter()
                .map(|(fd, (event, file))| (*fd, *event, file.clone()))
                .collect();
            let watch_net = interest_list
                .iter()
                .any(|(fd, _, _)| crate::net::fd_is_socket(*fd));
            let watch_interactive = interest_list
                .iter()
                .any(|(fd, _, _)| crate::net::fd_is_interactive(*fd));
            crate::net::io_wait_tick(watch_net, watch_interactive);
            // Sync readiness scan. Do NOT call `async_poll` here: each scan used
            // to Box::pin+poll+drop a future per watched fd (libinput epoll in
            // labwc touches dozens). The futures never stayed parked across
            // the await below anyway (Dropped before IoMultiplexWait), but
            // constructing them blew the guard-page-less coroutine stack → #DF
            // in `__from_user` at session start. Wakeups come from the 4 ms
            // IoMultiplexWait tick (and HID/NET IRQ registration there).
            let mut events = Vec::new();
            let mut delivered = Vec::new();
            for (fd, event, file) in &interest_list {
                let interest = PollEvents::from_bits_truncate(event.events as u16);
                let status = match file.poll(interest) {
                    Ok(status) => status,
                    Err(err) => return Err(err),
                };
                let ready = ready_events(event.events, &status);
                if ready != 0 {
                    events.push(EpollEvent {
                        events: ready,
                        data: event.data,
                    });
                    // Which of these is an `EPOLLONESHOT` entry is decided in
                    // `disarm_oneshot`, under the lock: this snapshot was
                    // taken without it, and a mask read from it can already
                    // be stale.
                    delivered.push((*fd, file.clone()));
                    if events.len() >= maxevents {
                        break;
                    }
                }
            }

            if !events.is_empty() {
                self.disarm_oneshot(&delivered);
                return Ok(events);
            }

            if timeout_msecs >= 0 {
                let deadline = begin_time + core::time::Duration::from_millis(timeout_msecs as u64);
                if kernel_hal::timer::timer_now() >= deadline {
                    return Ok(Vec::new());
                }
            }

            // A blocking wait may be interrupted by a deliverable signal, but a
            // signal must not hide events that are already ready, nor turn a
            // non-blocking/expired wait into `EINTR`.
            crate::process::check_signals()?;

            // Park a readiness waker on every watched fd that can carry one, so
            // a pipe write / unix-socket send / timerfd expiry wakes this task
            // the moment it happens instead of on the next re-scan tick. The
            // subscriptions are flat, synchronous registrations (see
            // `FileLike::subscribe_readiness` — NOT nested async_poll futures,
            // which overflowed the coroutine stack at desktop start). They are
            // taken AFTER the scan above found nothing: an event racing in
            // between is caught by the EventBus's latched flags, which fire the
            // waker at subscribe time.
            //
            // When every fd subscribed, the fallback re-scan stretches from
            // 4 ms to the covered tick — that alone removes ~96 % of the idle
            // wakeups of a parked desktop process. Any fd without an event
            // source (evdev nodes, signalfd, nested epolls) keeps the short
            // tick for its whole set, preserving the old behavior exactly.
            let waker =
                core::future::poll_fn(|cx| core::task::Poll::Ready(cx.waker().clone())).await;
            let mut subs = alloc::vec::Vec::with_capacity(interest_list.len());
            let mut covered = !interest_list.is_empty();
            for (_fd, event, file) in &interest_list {
                let interest = PollEvents::from_bits_truncate(event.events as u16);
                match file.subscribe_readiness(interest, &waker) {
                    Some(sub) => subs.push(sub),
                    None => covered = false,
                }
            }
            let tick_ms = if covered {
                crate::net::wait::IO_WAIT_COVERED_TICK_MS
            } else {
                crate::net::wait::IO_WAIT_TICK_MS
            };
            crate::net::wait::IoMultiplexWait::with_tick(
                timeout_msecs,
                watch_net,
                watch_interactive,
                tick_ms,
            )
            .await;
            drop(subs);
        }
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod abi_tests {
    use super::*;
    use core::mem::size_of;

    #[test]
    fn epoll_event_matches_linux_uapi() {
        assert_eq!(size_of::<EpollEvent>(), 12);
    }
}

#[cfg(test)]
mod tests {
    //! Host tests for `EPOLL_CTL_*` and the readiness walk.
    //!
    //! `Epoll::wait` needs a process and the timer, but everything a
    //! regression would break first does not: the interest list bookkeeping,
    //! and `any_ready`, which is what makes a **nested** epoll surface its
    //! inner readiness. wlroots/labwc depends on that exact behaviour — it
    //! puts `libinput_get_fd()` (an epoll fd) inside its own event loop, so
    //! losing it means input events are silently never dispatched.
    //!
    //! The watched files are real `EventFd`s: they are `FileLike`, they need
    //! no process context, and a `write` makes one readable.

    use super::*;
    use crate::fs::eventfd::EventFd;

    fn epoll() -> Arc<Epoll> {
        Epoll::new(OpenFlags::empty())
    }

    /// An eventfd, readable iff `ready`.
    fn evfd(ready: bool) -> Arc<dyn FileLike> {
        let fd = EventFd::new(0, OpenFlags::empty());
        if ready {
            fd.write(&1u64.to_ne_bytes()).unwrap();
        }
        fd
    }

    const ADD: i32 = 1;
    const DEL: i32 = 2;
    const MOD: i32 = 3;

    fn ev(mask: PollEvents, data: u64) -> EpollEvent {
        EpollEvent {
            events: mask.bits() as u32,
            data,
        }
    }

    fn readable(e: &Epoll) -> bool {
        e.poll(PollEvents::IN).unwrap().read
    }

    #[test]
    fn ctl_add_del_mod_report_the_linux_errors() {
        let ep = epoll();
        let fd = FileDesc::from(5);
        let f = evfd(false);
        // MOD and DEL before the fd is in the interest list.
        assert_eq!(
            ep.ctl(MOD, fd, ev(PollEvents::IN, 0), Some(f.clone())),
            Err(LxError::ENOENT)
        );
        assert_eq!(
            ep.ctl(DEL, fd, ev(PollEvents::IN, 0), None),
            Err(LxError::ENOENT)
        );
        // ADD needs the resolved handle.
        assert_eq!(
            ep.ctl(ADD, fd, ev(PollEvents::IN, 0), None),
            Err(LxError::EBADF)
        );
        assert_eq!(
            ep.ctl(ADD, fd, ev(PollEvents::IN, 0), Some(f.clone())),
            Ok(0)
        );
        // A second ADD of the same fd is EEXIST, not a silent replace.
        assert_eq!(
            ep.ctl(ADD, fd, ev(PollEvents::IN, 0), Some(f.clone())),
            Err(LxError::EEXIST)
        );
        // An unknown op is EINVAL.
        assert_eq!(
            ep.ctl(9, fd, ev(PollEvents::IN, 0), Some(f)),
            Err(LxError::EINVAL)
        );
        // DEL removes it, and is ENOENT the second time.
        assert_eq!(ep.ctl(DEL, fd, ev(PollEvents::IN, 0), None), Ok(0));
        assert_eq!(
            ep.ctl(DEL, fd, ev(PollEvents::IN, 0), None),
            Err(LxError::ENOENT)
        );
    }

    #[test]
    fn readiness_follows_the_requested_mask() {
        let ep = epoll();
        let fd = FileDesc::from(3);
        let quiet = evfd(false);
        ep.ctl(ADD, fd, ev(PollEvents::IN, 7), Some(quiet.clone()))
            .unwrap();
        assert!(!readable(&ep));
        // An eventfd is always writable, so watching it for OUT is ready now.
        ep.ctl(MOD, fd, ev(PollEvents::OUT, 7), Some(quiet.clone()))
            .unwrap();
        assert!(readable(&ep));
        // Back to IN, and make the eventfd actually readable.
        ep.ctl(MOD, fd, ev(PollEvents::IN, 7), Some(quiet.clone()))
            .unwrap();
        assert!(!readable(&ep));
        quiet.write(&1u64.to_ne_bytes()).unwrap();
        assert!(readable(&ep));
        // Removing the only watched fd makes it quiet again.
        ep.ctl(DEL, fd, ev(PollEvents::IN, 7), None).unwrap();
        assert!(!readable(&ep));
    }

    #[test]
    fn an_empty_interest_list_is_never_ready() {
        assert!(!readable(&epoll()));
    }

    #[test]
    fn a_nested_epoll_surfaces_its_inner_readiness() {
        // The wlroots shape: inner watches a device fd, outer watches inner.
        let inner = epoll();
        let outer = epoll();
        let dev = evfd(false);
        inner
            .ctl(
                ADD,
                FileDesc::from(4),
                ev(PollEvents::IN, 1),
                Some(dev.clone()),
            )
            .unwrap();
        outer
            .ctl(
                ADD,
                FileDesc::from(5),
                ev(PollEvents::IN, 2),
                Some(inner.clone()),
            )
            .unwrap();
        assert!(!readable(&outer));
        // An event on the device must reach the outer epoll.
        dev.write(&1u64.to_ne_bytes()).unwrap();
        assert!(readable(&inner));
        assert!(readable(&outer));
    }

    /// A file with no poll operation, the way a regular file on ext4 is.
    struct Unpollable {
        base: KObjectBase,
    }

    impl_kobject!(Unpollable);

    #[async_trait]
    impl FileLike for Unpollable {
        fn flags(&self) -> OpenFlags {
            OpenFlags::empty()
        }
        fn set_flags(&self, _f: OpenFlags) -> LxResult {
            Ok(())
        }
        async fn read(&self, _buf: &mut [u8]) -> LxResult<usize> {
            Err(LxError::ENOSYS)
        }
        fn write(&self, _buf: &[u8]) -> LxResult<usize> {
            Err(LxError::ENOSYS)
        }
        async fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> LxResult<usize> {
            Err(LxError::ENOSYS)
        }
        fn can_epoll(&self) -> bool {
            false
        }
        fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
            Ok(PollStatus {
                read: true,
                write: true,
                error: false,
                hangup: false,
            })
        }
        async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
            self.poll(events)
        }
    }

    /// `do_epoll_ctl`: a target with no poll operation is `EPERM` for
    /// every op, judged before the op, the mask and the interest list; and
    /// it is never added, so `poll(2)` on the epoll stays quiet.
    #[test]
    fn a_file_that_cannot_be_polled_is_eperm_for_every_op() {
        let ep = epoll();
        let plain: Arc<dyn FileLike> = Arc::new(Unpollable {
            base: KObjectBase::new(),
        });
        for op in [ADD, MOD, DEL, 9] {
            assert_eq!(
                ep.ctl(
                    op,
                    FileDesc::from(7),
                    ev(PollEvents::IN, 0),
                    Some(plain.clone())
                ),
                Err(LxError::EPERM),
                "op {op}"
            );
        }
        // EPERM comes before the mask is looked at.
        assert_eq!(
            ep.ctl(
                MOD,
                FileDesc::from(7),
                EpollEvent {
                    events: EPOLLEXCLUSIVE,
                    data: 0
                },
                Some(plain)
            ),
            Err(LxError::EPERM)
        );
        assert!(!readable(&ep), "a refused file was watched anyway");
        assert_eq!(epoll_target_verdict(true, false), Ok(()));
        assert_eq!(epoll_target_verdict(false, true), Err(LxError::EPERM));
    }

    #[test]
    fn an_epoll_cannot_watch_itself_or_close_a_cycle() {
        let a = epoll();
        // The direct case is `f.file == tf.file`, EINVAL; a cycle through
        // another epoll is `ep_loop_check`, ELOOP.
        assert_eq!(
            a.ctl(
                ADD,
                FileDesc::from(1),
                ev(PollEvents::IN, 0),
                Some(a.clone())
            ),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            a.ctl(
                DEL,
                FileDesc::from(1),
                ev(PollEvents::IN, 0),
                Some(a.clone())
            ),
            Err(LxError::EINVAL),
            "for DEL too: the target is judged before the op"
        );
        let b = epoll();
        // a watches b is fine...
        a.ctl(
            ADD,
            FileDesc::from(2),
            ev(PollEvents::IN, 0),
            Some(b.clone()),
        )
        .unwrap();
        // ...but b watching a would close the loop, and `any_ready` would
        // then walk it forever.
        assert_eq!(
            b.ctl(
                ADD,
                FileDesc::from(3),
                ev(PollEvents::IN, 0),
                Some(a.clone())
            ),
            Err(LxError::ELOOP)
        );
    }

    #[test]
    fn nesting_deeper_than_ep_max_nests_is_refused() {
        // A chain a0 -> a1 -> ... -> a4, five levels deep.
        let chain: Vec<Arc<Epoll>> = (0..=EPOLL_MAX_NEST_DEPTH).map(|_| epoll()).collect();
        for i in 0..EPOLL_MAX_NEST_DEPTH {
            chain[i]
                .ctl(
                    ADD,
                    FileDesc::from(i as i32),
                    ev(PollEvents::IN, 0),
                    Some(chain[i + 1].clone()),
                )
                .unwrap();
        }
        // Hanging that whole chain off one more epoll exceeds the limit, and
        // is refused conservatively rather than walked.
        let root = epoll();
        assert_eq!(
            root.ctl(
                ADD,
                FileDesc::from(99),
                ev(PollEvents::IN, 0),
                Some(chain[0].clone())
            ),
            Err(LxError::ELOOP)
        );
        // The chain itself still works: readiness at the bottom reaches the top.
        let dev = evfd(true);
        chain[EPOLL_MAX_NEST_DEPTH]
            .ctl(ADD, FileDesc::from(50), ev(PollEvents::IN, 0), Some(dev))
            .unwrap();
        assert!(readable(&chain[0]));
    }

    /// `dup(epfd)` gives a second descriptor for the SAME `struct eventpoll`
    /// (`fs/eventpoll.c` never copies one), so `epoll_ctl` through either fd
    /// is visible through the other. The fd table hands out this very `Arc`
    /// for a dup, which is what makes that true here.
    #[test]
    fn a_dup_of_an_epoll_fd_watches_the_same_interest_list() {
        let ep = epoll();
        let dev = evfd(true);
        ep.ctl(ADD, FileDesc::from(6), ev(PollEvents::IN, 0), Some(dev))
            .unwrap();
        let copy: Arc<dyn FileLike> = ep.clone();
        assert!(copy.poll(PollEvents::IN).unwrap().read);
        ep.ctl(DEL, FileDesc::from(6), ev(PollEvents::IN, 0), None)
            .unwrap();
        assert!(!readable(&ep));
        assert!(
            !copy.poll(PollEvents::IN).unwrap().read,
            "a watch dropped through one fd is dropped for both"
        );
    }
}

#[cfg(test)]
mod flag_tests {
    //! The four `epoll_ctl(2)` bits that live above the sixteen a
    //! [`PollEvents`] holds, and what `EPOLLONESHOT` owes the caller once its
    //! event has been handed out.
    //!
    //! `Epoll::wait` needs a process and the timer, so the decisions it makes
    //! are tested where they live: [`epoll_ctl_events`] for what a request
    //! stores, [`ready_events`] for what an entry reports, and
    //! `disarm_oneshot` for what a delivery uses up.

    use super::*;
    use crate::fs::eventfd::EventFd;

    const ADD: i32 = EPOLL_CTL_ADD;
    const DEL: i32 = EPOLL_CTL_DEL;
    const MOD: i32 = EPOLL_CTL_MOD;

    const IN: u32 = PollEvents::IN.bits() as u32;
    const OUT: u32 = PollEvents::OUT.bits() as u32;
    const ERR: u32 = PollEvents::ERR.bits() as u32;
    const HUP: u32 = PollEvents::HUP.bits() as u32;

    fn epoll() -> Arc<Epoll> {
        Epoll::new(OpenFlags::empty())
    }

    /// An eventfd, readable iff `ready`. Always writable.
    fn evfd(ready: bool) -> Arc<dyn FileLike> {
        let fd = EventFd::new(0, OpenFlags::empty());
        if ready {
            fd.write(&1u64.to_ne_bytes()).unwrap();
        }
        fd
    }

    fn ev(events: u32) -> EpollEvent {
        EpollEvent { events, data: 0 }
    }

    /// The mask `ep` currently holds for `fd`, or `None` if it holds none.
    fn stored(ep: &Epoll, fd: FileDesc) -> Option<u32> {
        ep.inner
            .lock()
            .interest_list
            .get(&fd)
            .map(|(event, _)| event.events)
    }

    fn status(read: bool, write: bool, error: bool, hangup: bool) -> PollStatus {
        PollStatus {
            read,
            write,
            error,
            hangup,
        }
    }

    /// A `FileLike` whose readiness is whatever the test says it is. An
    /// `EventFd` can be made readable, but nothing in the host tests can be
    /// made to report `POLLERR`/`POLLHUP` -- which is exactly what an entry's
    /// stored mask now decides.
    struct Fixed {
        base: KObjectBase,
        status: PollStatus,
    }

    impl_kobject!(Fixed);

    impl Fixed {
        fn new(status: PollStatus) -> Arc<Self> {
            Arc::new(Fixed {
                base: KObjectBase::new(),
                status,
            })
        }
    }

    #[async_trait]
    impl FileLike for Fixed {
        fn flags(&self) -> OpenFlags {
            OpenFlags::empty()
        }
        fn set_flags(&self, _f: OpenFlags) -> LxResult {
            Ok(())
        }
        async fn read(&self, _buf: &mut [u8]) -> LxResult<usize> {
            Err(LxError::ENOSYS)
        }
        fn write(&self, _buf: &[u8]) -> LxResult<usize> {
            Err(LxError::ENOSYS)
        }
        async fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> LxResult<usize> {
            Err(LxError::ENOSYS)
        }
        fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
            Ok(status(
                self.status.read,
                self.status.write,
                self.status.error,
                self.status.hangup,
            ))
        }
        async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus> {
            self.poll(events)
        }
    }

    /// Their `uapi` values, and the reason they went missing: `events` is a
    /// `u32` and every reader narrowed it to a `u16` first.
    #[test]
    fn the_four_flags_above_sixteen_bits_have_their_linux_values() {
        assert_eq!(EPOLLEXCLUSIVE, 0x1000_0000);
        assert_eq!(EPOLLWAKEUP, 0x2000_0000);
        assert_eq!(EPOLLONESHOT, 0x4000_0000);
        assert_eq!(EPOLLET, 0x8000_0000);
        for flag in [EPOLLEXCLUSIVE, EPOLLWAKEUP, EPOLLONESHOT, EPOLLET] {
            assert_eq!(flag as u16, 0, "a u16 cannot carry {flag:#x}");
        }
    }

    /// A stored mask always reports `EPOLLERR`/`EPOLLHUP`, so the one mask
    /// that reports nothing is the empty one -- which is what makes zeroing a
    /// used-up `EPOLLONESHOT` entry mean "disabled" and nothing else.
    #[test]
    fn epoll_ctl_forces_err_and_hup_into_every_stored_mask() {
        assert_eq!(epoll_ctl_events(ADD, IN, false), Ok(IN | ERR | HUP));
        assert_eq!(epoll_ctl_events(ADD, 0, false), Ok(ERR | HUP));
        assert_eq!(epoll_ctl_events(MOD, OUT, false), Ok(OUT | ERR | HUP));
    }

    /// The bits a `PollEvents` cannot hold reach the interest list intact.
    #[test]
    fn oneshot_and_edge_trigger_survive_the_trip_through_epoll_ctl() {
        let ep = epoll();
        let fd = FileDesc::from(3);
        ep.ctl(ADD, fd, ev(IN | EPOLLONESHOT | EPOLLET), Some(evfd(false)))
            .unwrap();
        let mask = stored(&ep, fd).unwrap();
        assert_eq!(mask & EPOLLONESHOT, EPOLLONESHOT);
        assert_eq!(mask & EPOLLET, EPOLLET);
        assert_eq!(mask, IN | EPOLLONESHOT | EPOLLET | ERR | HUP);
        // A MOD is held to the same mask as an ADD: it replaces the entry
        // outright, so anything it forgets is forgotten for good.
        ep.ctl(MOD, fd, ev(OUT | EPOLLET), Some(evfd(false)))
            .unwrap();
        assert_eq!(stored(&ep, fd), Some(OUT | EPOLLET | ERR | HUP));
    }

    /// `EPOLLEXCLUSIVE` changes *who* is woken, so Linux refuses every request
    /// whose wake-up set would be ambiguous instead of guessing.
    #[test]
    fn exclusive_is_refused_by_mod_by_a_nested_epoll_and_by_a_bit_it_cannot_share() {
        assert_eq!(
            epoll_ctl_events(MOD, IN | EPOLLEXCLUSIVE, false),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            epoll_ctl_events(ADD, IN | EPOLLEXCLUSIVE, true),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            epoll_ctl_events(ADD, IN | EPOLLEXCLUSIVE | EPOLLONESHOT, false),
            Err(LxError::EINVAL)
        );
    }

    /// ...and accepts the ones it can share a wake-up with.
    #[test]
    fn exclusive_is_accepted_with_the_bits_linux_allows_beside_it() {
        assert_eq!(
            epoll_ctl_events(
                ADD,
                IN | OUT | EPOLLEXCLUSIVE | EPOLLET | EPOLLWAKEUP,
                false
            ),
            Ok(IN | OUT | EPOLLEXCLUSIVE | EPOLLET | EPOLLWAKEUP | ERR | HUP)
        );
        // And the refusal reaches userspace through `ctl`, not just the helper.
        let ep = epoll();
        assert_eq!(
            ep.ctl(
                ADD,
                FileDesc::from(4),
                ev(IN | EPOLLEXCLUSIVE),
                Some(epoll())
            ),
            Err(LxError::EINVAL)
        );
    }

    /// An entry reports only what its mask asks for -- `EPOLLERR`/`EPOLLHUP`
    /// included, which is what lets an empty mask mean "report nothing".
    #[test]
    fn an_entry_reports_the_readiness_its_mask_asked_for() {
        let loud = status(true, true, true, true);
        assert_eq!(ready_events(IN | ERR | HUP, &loud), IN | ERR | HUP);
        assert_eq!(ready_events(OUT | ERR | HUP, &loud), OUT | ERR | HUP);
        assert_eq!(ready_events(0, &loud), 0);
        // The high flags are not readiness and are never reported back.
        assert_eq!(
            ready_events(IN | ERR | HUP | EPOLLONESHOT | EPOLLET, &loud),
            IN | ERR | HUP
        );
        // Nothing ready is nothing reported, however wide the mask.
        assert_eq!(
            ready_events(IN | OUT | ERR | HUP, &status(false, false, false, false)),
            0
        );
        // And each of the four stands on its own: a peer that hung up without
        // leaving anything to read is still a hangup, which is the whole
        // reason `poll(2)` has a bit for it.
        let mask = IN | OUT | ERR | HUP;
        assert_eq!(ready_events(mask, &status(false, false, false, true)), HUP);
        assert_eq!(ready_events(mask, &status(false, false, true, false)), ERR);
        assert_eq!(ready_events(mask, &status(false, true, false, false)), OUT);
        assert_eq!(ready_events(mask, &status(true, false, false, false)), IN);
    }

    /// The point of the flag: one delivery, then the entry is off until the
    /// caller re-arms it. Losing the bit meant every waiter kept being handed
    /// the same fd, which is the race `EPOLLONESHOT` exists to prevent.
    #[test]
    fn a_oneshot_entry_stops_reporting_once_its_event_has_been_handed_out() {
        let ep = epoll();
        let fd = FileDesc::from(7);
        let file = evfd(true);
        ep.ctl(ADD, fd, ev(IN | EPOLLONESHOT), Some(file.clone()))
            .unwrap();
        assert!(ep.poll(PollEvents::IN).unwrap().read);

        ep.disarm_oneshot(&[(fd, file.clone())]);
        assert_eq!(stored(&ep, fd), Some(0), "disabled, not removed");
        assert!(
            !ep.poll(PollEvents::IN).unwrap().read,
            "the fd is still readable; the entry is the thing that is spent"
        );

        // A re-arm through EPOLL_CTL_MOD brings it back, which is the whole
        // handshake a thread pool relies on.
        ep.ctl(MOD, fd, ev(IN | EPOLLONESHOT), Some(file)).unwrap();
        assert!(ep.poll(PollEvents::IN).unwrap().read);
    }

    /// A spent entry is silent about everything, errors and hangups included.
    #[test]
    fn a_spent_oneshot_entry_reports_neither_error_nor_hangup() {
        let ep = epoll();
        let fd = FileDesc::from(8);
        let file: Arc<dyn FileLike> = Fixed::new(status(false, false, true, true));
        ep.ctl(ADD, fd, ev(IN | EPOLLONESHOT), Some(file.clone()))
            .unwrap();
        assert!(
            ep.poll(PollEvents::IN).unwrap().read,
            "an error is reported even though only EPOLLIN was asked for"
        );
        ep.disarm_oneshot(&[(fd, file)]);
        assert!(!ep.poll(PollEvents::IN).unwrap().read);
    }

    /// Everything else keeps firing for as long as it is ready.
    #[test]
    fn an_entry_that_did_not_ask_for_oneshot_is_never_disabled() {
        let ep = epoll();
        let fd = FileDesc::from(9);
        let file = evfd(true);
        ep.ctl(ADD, fd, ev(IN), Some(file.clone())).unwrap();
        ep.disarm_oneshot(&[(fd, file)]);
        assert_eq!(stored(&ep, fd), Some(IN | ERR | HUP));
        assert!(ep.poll(PollEvents::IN).unwrap().read);
    }

    /// The scan runs on a snapshot taken without the lock, so between the
    /// delivery and the disarm the fd number can have been dropped and reused
    /// for another file -- one that was never handed anything.
    #[test]
    fn disarming_leaves_alone_an_fd_that_now_holds_a_different_file() {
        let ep = epoll();
        let fd = FileDesc::from(10);
        let old = evfd(true);
        ep.ctl(ADD, fd, ev(IN | EPOLLONESHOT), Some(old.clone()))
            .unwrap();
        let new = evfd(true);
        ep.ctl(DEL, fd, ev(0), None).unwrap();
        ep.ctl(ADD, fd, ev(IN | EPOLLONESHOT), Some(new)).unwrap();

        ep.disarm_oneshot(&[(fd, old)]);
        assert_eq!(
            stored(&ep, fd),
            Some(IN | EPOLLONESHOT | ERR | HUP),
            "the replacement entry keeps its mask"
        );
        assert!(ep.poll(PollEvents::IN).unwrap().read);
    }
}

/// Closing a watched descriptor is what removes it from the interest list,
/// as epoll(7) promises; nothing here did it.
#[cfg(test)]
mod close_forgets_tests {
    use super::*;
    use crate::fs::eventfd::EventFd;
    use crate::process::LinuxProcess;
    use rcore_fs_ramfs::RamFS;

    const ADD: i32 = 1;
    const DEL: i32 = 2;

    fn ev(data: u64) -> EpollEvent {
        EpollEvent {
            events: PollEvents::IN.bits() as u32,
            data,
        }
    }

    fn ready_eventfd() -> Arc<dyn FileLike> {
        let fd = EventFd::new(0, OpenFlags::empty());
        fd.write(&1u64.to_ne_bytes()).unwrap();
        fd
    }

    /// A process with an epoll at `epfd` watching a readable eventfd at `fd`.
    fn watching() -> (LinuxProcess, Arc<Epoll>, FileDesc, Arc<dyn FileLike>) {
        let proc = LinuxProcess::new(RamFS::new(), 0);
        let ep = Epoll::new(OpenFlags::empty());
        proc.add_file(ep.clone()).unwrap();
        let file = ready_eventfd();
        let fd = proc.add_file(file.clone()).unwrap();
        ep.ctl(ADD, fd, ev(7), Some(file.clone())).unwrap();
        assert!(ep.poll(PollEvents::IN).unwrap().read);
        (proc, ep, fd, file)
    }

    fn watches(ep: &Epoll, fd: FileDesc) -> bool {
        ep.inner.lock().interest_list.contains_key(&fd)
    }

    #[test]
    fn closing_the_watched_descriptor_removes_it_from_the_interest_list() {
        let (proc, ep, fd, file) = watching();
        // The table and the interest list hold it, besides this test.
        assert_eq!(Arc::strong_count(&file), 3);
        proc.close_file(fd).unwrap();
        assert!(
            !watches(&ep, fd),
            "close is enough, no EPOLL_CTL_DEL needed"
        );
        assert!(!ep.poll(PollEvents::IN).unwrap().read);
        assert_eq!(ep.ctl(DEL, fd, ev(0), None), Err(LxError::ENOENT));
        // And the interest list no longer keeps the description alive: this
        // is what lets the other end of a closed socket or pipe see EOF.
        assert_eq!(Arc::strong_count(&file), 1);
    }

    #[test]
    fn a_dup_keeps_the_entry_until_the_last_descriptor_of_the_description_closes() {
        let (proc, ep, fd, file) = watching();
        let dup = proc.add_file_cloexec(file.clone(), false).unwrap();
        assert_ne!(dup, fd);
        proc.close_file(fd).unwrap();
        assert!(
            watches(&ep, fd),
            "the description is still open through the dup, as in Linux"
        );
        assert!(ep.poll(PollEvents::IN).unwrap().read);
        proc.close_file(dup).unwrap();
        assert!(!watches(&ep, fd));
        assert_eq!(Arc::strong_count(&file), 1);
    }

    #[test]
    fn the_number_can_be_watched_again_once_it_names_a_new_file() {
        let (proc, ep, fd, _old) = watching();
        proc.close_file(fd).unwrap();
        let new = ready_eventfd();
        // The lowest free number is the one just closed.
        assert_eq!(proc.add_file(new.clone()).unwrap(), fd);
        assert_eq!(ep.ctl(ADD, fd, ev(8), Some(new.clone())), Ok(0));
        // The same description under the same number is still EEXIST.
        assert_eq!(ep.ctl(ADD, fd, ev(8), Some(new)), Err(LxError::EEXIST));
    }

    /// The interest list is keyed by (file, fd) in Linux, so a stale entry
    /// under a reused number never stops `EPOLL_CTL_ADD` of the new file,
    /// whichever path let the entry survive.
    #[test]
    fn add_replaces_an_entry_whose_file_is_no_longer_the_one_at_that_number() {
        let ep = Epoll::new(OpenFlags::empty());
        let fd = FileDesc::from(5);
        let old = ready_eventfd();
        ep.ctl(ADD, fd, ev(1), Some(old.clone())).unwrap();
        let new: Arc<dyn FileLike> = EventFd::new(0, OpenFlags::empty());
        assert_eq!(ep.ctl(ADD, fd, ev(2), Some(new.clone())), Ok(0));
        let (event, watched) = ep.inner.lock().interest_list[&fd].clone();
        let data = event.data;
        assert_eq!(data, 2);
        assert!(Arc::ptr_eq(&watched, &new));
        assert!(!ep.poll(PollEvents::IN).unwrap().read, "old is not watched");
        assert_eq!(ep.ctl(ADD, fd, ev(3), Some(new)), Err(LxError::EEXIST));
    }

    /// The nested-epoll path re-checks after taking the nesting lock; it
    /// answers the same two ways as the plain one.
    #[test]
    fn a_nested_epoll_is_eexist_twice_under_its_number_and_replaces_a_stale_one() {
        let outer = Epoll::new(OpenFlags::empty());
        let fd = FileDesc::from(6);
        let stale = ready_eventfd();
        outer.ctl(ADD, fd, ev(1), Some(stale)).unwrap();
        let inner: Arc<dyn FileLike> = Epoll::new(OpenFlags::empty());
        assert_eq!(outer.ctl(ADD, fd, ev(2), Some(inner.clone())), Ok(0));
        assert!(!outer.poll(PollEvents::IN).unwrap().read, "stale is gone");
        assert_eq!(outer.ctl(ADD, fd, ev(3), Some(inner)), Err(LxError::EEXIST));
    }

    #[test]
    fn dup2_over_a_watched_descriptor_closes_it_for_the_epoll_too() {
        let (proc, ep, fd, file) = watching();
        let other = EventFd::new(0, OpenFlags::empty());
        let old = proc.replace_file(fd, other, false).unwrap().unwrap();
        assert!(Arc::ptr_eq(&old, &file));
        drop(old);
        assert!(!watches(&ep, fd));
        assert_eq!(Arc::strong_count(&file), 1);
    }

    #[test]
    fn dup2_of_a_descriptor_onto_itself_changes_nothing() {
        let (proc, ep, fd, file) = watching();
        proc.replace_file(fd, file.clone(), false).unwrap();
        assert!(watches(&ep, fd));
    }

    #[test]
    fn close_range_and_the_exec_sweep_forget_what_they_close() {
        let (proc, ep, fd, file) = watching();
        proc.close_range(fd, fd);
        assert!(!watches(&ep, fd));
        assert_eq!(Arc::strong_count(&file), 1);

        let (proc, ep, fd, file) = watching();
        proc.set_fd_cloexec(fd, true).unwrap();
        proc.remove_cloexec_files();
        assert!(!watches(&ep, fd));
        assert_eq!(Arc::strong_count(&file), 1);
    }

    #[test]
    fn every_entry_of_the_description_goes_whatever_number_it_was_added_under() {
        let (proc, ep, fd, file) = watching();
        let dup = proc.add_file_cloexec(file.clone(), false).unwrap();
        ep.ctl(ADD, dup, ev(9), Some(file.clone())).unwrap();
        proc.close_file(dup).unwrap();
        assert!(
            watches(&ep, fd) && watches(&ep, dup),
            "still open through fd"
        );
        proc.close_file(fd).unwrap();
        assert!(!watches(&ep, fd) && !watches(&ep, dup));
        assert_eq!(Arc::strong_count(&file), 1);
    }

    /// An epoll that is itself closed takes its list with it; one that lives
    /// on in a dup keeps watching.
    #[test]
    fn closing_the_epoll_itself_is_not_a_watched_descriptor_going_away() {
        let (proc, ep, fd, _file) = watching();
        let epfd = proc.add_file_cloexec(ep.clone(), false).unwrap();
        proc.close_file(epfd).unwrap();
        assert!(watches(&ep, fd));
    }
}
